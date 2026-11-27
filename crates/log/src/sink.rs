//! Output sinks for `tidb-log`: stdout, rotating/gzip file writer, buffered writer, timeout guard,
//! and in-memory test sink.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use chrono::Local;
use flate2::Compression;
use flate2::write::GzEncoder;
use parking_lot::{Condvar, Mutex};
use tidb_base::Error;
use tidb_base::config::ZaplogFileLogConfig;

const DEFAULT_MAX_SIZE_MB: isize = 300;
const DEFAULT_BUFFER_SIZE: usize = 256 * 1024;
const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// Abstract log byte sink receiving fully encoded single-line entries (ending in `\n`).
pub trait LogSink: Send + Sync {
    /// Write one already-encoded log line (ending in `\n`).
    fn write_entry(&self, data: &[u8]) -> io::Result<()>;

    /// Flush any in-memory buffers to the underlying stream/file.
    fn sync(&self) -> io::Result<()>;
}

/// Synchronous stdout sink (used when `log.file.filename` is empty).
#[derive(Default)]
pub struct StdoutSink;

impl LogSink for StdoutSink {
    fn write_entry(&self, data: &[u8]) -> io::Result<()> {
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        lock.write_all(data)
    }

    fn sync(&self) -> io::Result<()> {
        io::stdout().flush()
    }
}

/// In-memory sink for golden tests and unit tests.
#[derive(Default)]
pub struct MemorySink {
    buf: Mutex<Vec<u8>>,
}

impl MemorySink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Snapshot all recorded bytes.
    pub fn snapshot(&self) -> Vec<u8> {
        self.buf.lock().clone()
    }

    /// Snapshot recorded bytes as UTF-8 (lossy only for display assertions).
    pub fn snapshot_string(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock()).into_owned()
    }

    /// Drain all recorded bytes.
    pub fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.buf.lock())
    }

    /// Clear recorded bytes.
    pub fn clear(&self) {
        self.buf.lock().clear();
    }
}

impl LogSink for MemorySink {
    fn write_entry(&self, data: &[u8]) -> io::Result<()> {
        self.buf.lock().extend_from_slice(data);
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        Ok(())
    }
}

struct RotatingState {
    file: File,
    size: u64,
    seq: u64,
}

/// Size-, backup-, and age-based rotating file sink with optional async gzip compression
/// (matching Go `lumberjack` + `pingcap/log`).
pub struct RotatingFileSink {
    path: PathBuf,
    max_bytes: u64,
    max_days: usize,
    max_backups: usize,
    compress_gzip: bool,
    timeout: Option<Duration>,
    state: Mutex<RotatingState>,
    cleanup_lock: Arc<Mutex<()>>,
}

impl RotatingFileSink {
    /// Open or create a rotating file sink from `ZaplogFileLogConfig`.
    pub fn new(cfg: &ZaplogFileLogConfig, timeout_secs: isize) -> Result<Self, Error> {
        let max_mb = if cfg.max_size <= 0 {
            DEFAULT_MAX_SIZE_MB
        } else {
            cfg.max_size
        };
        let max_bytes = (max_mb as u64).saturating_mul(1024 * 1024);
        Self::new_with_max_bytes(cfg, max_bytes, timeout_secs)
    }

    /// Constructor accepting an explicit byte rotation threshold (used by rotation tests).
    pub fn new_with_max_bytes(
        cfg: &ZaplogFileLogConfig,
        max_bytes: u64,
        timeout_secs: isize,
    ) -> Result<Self, Error> {
        let compress_gzip = match cfg.compression.as_str() {
            "" => false,
            "gzip" => true,
            other => {
                return Err(Error::plain(format!(
                    "unsupported log compression: {other}"
                )));
            }
        };

        let path = PathBuf::from(&cfg.filename);
        if path.is_dir() {
            return Err(Error::plain(format!(
                "can't use {} as log file name: is a directory",
                path.display()
            )));
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            fs::create_dir_all(parent).map_err(|e| {
                Error::plain(format!(
                    "can't make directories for new logfile {}: {e}",
                    parent.display()
                ))
            })?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::plain(format!("can't open logfile {}: {e}", path.display())))?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);

        let sink = Self {
            path,
            max_bytes: max_bytes.max(1),
            max_days: cfg.max_days.max(0) as usize,
            max_backups: cfg.max_backups.max(0) as usize,
            compress_gzip,
            timeout: if timeout_secs > 0 {
                Some(Duration::from_secs(timeout_secs as u64))
            } else {
                None
            },
            state: Mutex::new(RotatingState { file, size, seq: 0 }),
            cleanup_lock: Arc::new(Mutex::new(())),
        };
        sink.cleanup_old_archives();
        Ok(sink)
    }

    fn lock_state(&self) -> parking_lot::MutexGuard<'_, RotatingState> {
        if let Some(timeout) = self.timeout {
            self.state.try_lock_for(timeout).unwrap_or_else(|| {
                panic!(
                    "write log timeout after {:?} for {}",
                    timeout,
                    self.path.display()
                )
            })
        } else {
            self.state.lock()
        }
    }

    fn rotate_locked(&self, st: &mut RotatingState) -> io::Result<()> {
        st.file.flush()?;
        let backup = self.backup_path(&mut st.seq);
        if self.path.exists() {
            fs::rename(&self.path, &backup)?;
        }
        let new_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        st.file = new_file;
        st.size = 0;

        if self.compress_gzip && backup.exists() {
            let src = backup.clone();
            let path_for_cleanup = self.path.clone();
            let cleanup_lock = Arc::clone(&self.cleanup_lock);
            let max_days = self.max_days;
            let max_backups = self.max_backups;
            let _ = std::thread::Builder::new()
                .name("tidb-log-gzip".to_owned())
                .spawn(move || {
                    let _ = compress_file_gzip(&src);
                    let _guard = cleanup_lock.lock();
                    cleanup_archives(&path_for_cleanup, max_days, max_backups);
                });
        } else {
            self.cleanup_old_archives();
        }
        Ok(())
    }

    fn backup_path(&self, seq: &mut u64) -> PathBuf {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = self
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("tidb.log");
        let (stem, ext) = match file_name.rfind('.') {
            Some(idx) if idx > 0 => (&file_name[..idx], &file_name[idx..]),
            _ => (file_name, ""),
        };
        let ts = Local::now().format("%Y-%m-%dT%H-%M-%S%.3f");
        let candidate = dir.join(format!("{stem}-{ts}{ext}"));
        let candidate_gz = dir.join(format!("{stem}-{ts}{ext}.gz"));
        if !candidate.exists() && !candidate_gz.exists() {
            return candidate;
        }
        loop {
            let s = *seq;
            *seq = seq.wrapping_add(1);
            let numbered = dir.join(format!("{stem}-{ts}-{s}{ext}"));
            let numbered_gz = dir.join(format!("{stem}-{ts}-{s}{ext}.gz"));
            if !numbered.exists() && !numbered_gz.exists() {
                return numbered;
            }
        }
    }

    fn cleanup_old_archives(&self) {
        if self.max_days == 0 && self.max_backups == 0 {
            return;
        }
        let _guard = self.cleanup_lock.lock();
        cleanup_archives(&self.path, self.max_days, self.max_backups);
    }
}

impl LogSink for RotatingFileSink {
    fn write_entry(&self, data: &[u8]) -> io::Result<()> {
        let start = Instant::now();
        {
            let mut st = self.lock_state();
            if st.size > 0 && st.size.saturating_add(data.len() as u64) > self.max_bytes {
                self.rotate_locked(&mut st)?;
            }
            st.file.write_all(data)?;
            st.size = st.size.saturating_add(data.len() as u64);
        }
        // MutexGuard is dropped BEFORE panicking so the global FatalLog panic hook never
        // deadlocks trying to re-acquire `self.state`.
        if let Some(timeout) = self.timeout
            && start.elapsed() > timeout
        {
            panic!(
                "write log exceeded timeout {:?} for {}",
                timeout,
                self.path.display()
            );
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        let mut st = self.lock_state();
        st.file.flush()?;
        st.file.sync_data()
    }
}

fn compress_file_gzip(src: &Path) -> io::Result<()> {
    let mut dst_os = src.as_os_str().to_os_string();
    dst_os.push(".gz");
    let dst = PathBuf::from(dst_os);
    let mut reader = File::open(src)?;
    let out = File::create(&dst)?;
    let mut encoder = GzEncoder::new(BufWriter::new(out), Compression::default());
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        encoder.write_all(&buf[..n])?;
    }
    let mut inner = encoder.finish()?;
    inner.flush()?;
    drop(reader);
    let _ = fs::remove_file(src);
    Ok(())
}

fn cleanup_archives(base_path: &Path, max_days: usize, max_backups: usize) {
    let Some(dir) = base_path.parent() else {
        return;
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let Some(file_name) = base_path.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let (stem, ext) = match file_name.rfind('.') {
        Some(idx) if idx > 0 => (&file_name[..idx], &file_name[idx..]),
        _ => (file_name, ""),
    };
    let prefix = format!("{stem}-");
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Some(after_prefix) = name_str.strip_prefix(&prefix) else {
            continue;
        };
        let ts_part = if let Some(s) = after_prefix.strip_suffix(&format!("{ext}.gz")) {
            s
        } else if let Some(s) = after_prefix.strip_suffix(ext) {
            s
        } else {
            continue;
        };
        // Strictly validate the rotated timestamp (`YYYY-MM-DDTHH-MM-SS.mmm` plus optional
        // `-<seq>`) so sibling logs like `tidb-slow.log` in the same directory are NEVER deleted.
        if !is_valid_backup_timestamp(ts_part) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        backups.push((mtime, entry.path()));
    }
    // Newest first.
    backups.sort_by_key(|a| std::cmp::Reverse(a.0));

    let now = SystemTime::now();
    let max_age = if max_days > 0 {
        Some(Duration::from_secs(
            (max_days as u64).saturating_mul(86_400),
        ))
    } else {
        None
    };

    for (idx, (mtime, path)) in backups.into_iter().enumerate() {
        let too_many = max_backups > 0 && idx >= max_backups;
        let too_old =
            max_age.is_some_and(|limit| now.duration_since(mtime).is_ok_and(|age| age > limit));
        if too_many || too_old {
            let _ = fs::remove_file(path);
        }
    }
}

/// Validate rotated archive timestamp suffix: `YYYY-MM-DDTHH-MM-SS.mmm` (23 ASCII bytes)
/// optionally followed by `-<digits>`.
fn is_valid_backup_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 23 {
        return false;
    }
    let is_d = |i: usize| b[i].is_ascii_digit();
    let ts_ok = is_d(0)
        && is_d(1)
        && is_d(2)
        && is_d(3)
        && b[4] == b'-'
        && is_d(5)
        && is_d(6)
        && b[7] == b'-'
        && is_d(8)
        && is_d(9)
        && b[10] == b'T'
        && is_d(11)
        && is_d(12)
        && b[13] == b'-'
        && is_d(14)
        && is_d(15)
        && b[16] == b'-'
        && is_d(17)
        && is_d(18)
        && b[19] == b'.'
        && is_d(20)
        && is_d(21)
        && is_d(22);
    if !ts_ok {
        return false;
    }
    if b.len() == 23 {
        return true;
    }
    b[23] == b'-' && b.len() > 24 && b[24..].iter().all(u8::is_ascii_digit)
}

/// Optional buffered wrapper with periodic background flush (Go `zapcore.BufferedWriteSyncer`).
pub struct BufferedSink {
    inner: Arc<dyn LogSink>,
    buf: Arc<Mutex<Vec<u8>>>,
    capacity: usize,
    stop: Arc<(Mutex<bool>, Condvar)>,
    stopped: AtomicBool,
    flusher: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl BufferedSink {
    pub fn new(inner: Arc<dyn LogSink>, buffer_size: isize, flush_interval_ns: i64) -> Self {
        let capacity = if buffer_size > 0 {
            buffer_size as usize
        } else {
            DEFAULT_BUFFER_SIZE
        };
        let flush_interval = if flush_interval_ns > 0 {
            Duration::from_nanos(flush_interval_ns as u64)
        } else {
            DEFAULT_FLUSH_INTERVAL
        };
        let buf = Arc::new(Mutex::new(Vec::with_capacity(capacity.min(1024 * 1024))));
        let stop = Arc::new((Mutex::new(false), Condvar::new()));

        let bg_inner = Arc::clone(&inner);
        let bg_buf = Arc::clone(&buf);
        let bg_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("tidb-log-flush".to_owned())
            .spawn(move || {
                let (lock, cvar) = &*bg_stop;
                loop {
                    {
                        let mut done = lock.lock();
                        if *done {
                            break;
                        }
                        let _ = cvar.wait_for(&mut done, flush_interval);
                        if *done {
                            break;
                        }
                    }
                    // `lock` is released before taking `bg_buf` or performing disk I/O so Drop
                    // never blocks behind a slow flush.
                    let chunk = {
                        let mut b = bg_buf.lock();
                        if b.is_empty() {
                            Vec::new()
                        } else {
                            std::mem::take(&mut *b)
                        }
                    };
                    if !chunk.is_empty() {
                        let _ = bg_inner.write_entry(&chunk);
                    }
                }
            })
            .ok();

        Self {
            inner,
            buf,
            capacity,
            stop,
            stopped: AtomicBool::new(false),
            flusher: Mutex::new(handle),
        }
    }

    fn flush_buffer(&self) -> io::Result<()> {
        let chunk = {
            let mut b = self.buf.lock();
            if b.is_empty() {
                return Ok(());
            }
            std::mem::take(&mut *b)
        };
        self.inner.write_entry(&chunk)
    }
}

impl LogSink for BufferedSink {
    fn write_entry(&self, data: &[u8]) -> io::Result<()> {
        let flush_now = {
            let mut b = self.buf.lock();
            if b.len().saturating_add(data.len()) > self.capacity && !b.is_empty() {
                let chunk = std::mem::take(&mut *b);
                b.extend_from_slice(data);
                Some(chunk)
            } else {
                b.extend_from_slice(data);
                None
            }
        };
        if let Some(chunk) = flush_now {
            self.inner.write_entry(&chunk)?;
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        self.flush_buffer()?;
        self.inner.sync()
    }
}

impl Drop for BufferedSink {
    fn drop(&mut self) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            let (lock, cvar) = &*self.stop;
            *lock.lock() = true;
            cvar.notify_all();
        }
        if let Some(handle) = self.flusher.lock().take() {
            let _ = handle.join();
        }
        let _ = self.sync();
    }
}

/// Build a [`LogSink`] from a [`ZaplogFileLogConfig`] and optional `timeout` (seconds).
pub fn build_sink(
    cfg: &ZaplogFileLogConfig,
    timeout_secs: isize,
) -> Result<Arc<dyn LogSink>, Error> {
    let base: Arc<dyn LogSink> = if cfg.filename.is_empty() {
        Arc::new(StdoutSink)
    } else {
        Arc::new(RotatingFileSink::new(cfg, timeout_secs)?)
    };
    if cfg.is_buffered {
        Ok(Arc::new(BufferedSink::new(
            base,
            cfg.buffer_size,
            cfg.buffer_flush_interval,
        )))
    } else {
        Ok(base)
    }
}
