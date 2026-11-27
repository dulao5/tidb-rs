//! Output sinks for `tidb-log`: stdout, rotating/gzip file writer, buffered writer, timeout guard,
//! and in-memory test sink.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{Local, NaiveDateTime};
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
    state: Mutex<RotatingState>,
    cleanup_lock: Arc<Mutex<()>>,
}

impl RotatingFileSink {
    /// Open or create a rotating file sink from `ZaplogFileLogConfig`.
    pub fn new(cfg: &ZaplogFileLogConfig, _timeout_secs: isize) -> Result<Self, Error> {
        let max_mb = if cfg.max_size <= 0 {
            DEFAULT_MAX_SIZE_MB
        } else {
            cfg.max_size
        };
        let max_bytes = (max_mb as u64).saturating_mul(1024 * 1024);
        Self::new_with_max_bytes(cfg, max_bytes, 0)
    }

    /// Constructor accepting an explicit byte rotation threshold (used by rotation tests).
    pub fn new_with_max_bytes(
        cfg: &ZaplogFileLogConfig,
        max_bytes: u64,
        _timeout_secs: isize,
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
            state: Mutex::new(RotatingState { file, size, seq: 0 }),
            cleanup_lock: Arc::new(Mutex::new(())),
        };
        sink.cleanup_old_archives();
        Ok(sink)
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
                    // Serialize compression and archive retention under one lock (matching
                    // lumberjack's single mill goroutine) so concurrent cleanups never race
                    // against an in-progress gzip or compress the same file twice.
                    let _guard = cleanup_lock.lock();
                    let _ = compress_file_gzip(&src);
                    cleanup_archives(&path_for_cleanup, max_days, max_backups, true);
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
        if self.max_days == 0 && self.max_backups == 0 && !self.compress_gzip {
            return;
        }
        let _guard = self.cleanup_lock.lock();
        cleanup_archives(
            &self.path,
            self.max_days,
            self.max_backups,
            self.compress_gzip,
        );
    }
}

impl LogSink for RotatingFileSink {
    fn write_entry(&self, data: &[u8]) -> io::Result<()> {
        let mut st = self.state.lock();
        if st.size > 0 && st.size.saturating_add(data.len() as u64) > self.max_bytes {
            self.rotate_locked(&mut st)?;
        }
        st.file.write_all(data)?;
        st.size = st.size.saturating_add(data.len() as u64);
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        let mut st = self.state.lock();
        st.file.flush()?;
        st.file.sync_data()
    }
}

fn compress_file_gzip(src: &Path) -> io::Result<()> {
    let mut dst_os = src.as_os_str().to_os_string();
    dst_os.push(".gz");
    let dst = PathBuf::from(&dst_os);
    let mut tmp_os = dst_os;
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);

    let write_res = (|| -> io::Result<()> {
        let mut reader = File::open(src)?;
        let out = File::create(&tmp)?;
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
        inner.get_ref().sync_data()?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, &dst) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    let _ = fs::remove_file(src);
    Ok(())
}

/// Lumberjack-compatible archive retention & leftover compression:
/// - Ages backups by the timestamp parsed from the filename (not filesystem mtime).
/// - Deduplicates `<ts>` and `<ts>.gz` so a backup being compressed is not counted twice.
/// - Compresses any remaining uncompressed backups when `compress_gzip` is enabled.
fn cleanup_archives(base_path: &Path, max_days: usize, max_backups: usize, compress_gzip: bool) {
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

    // Group by parsed `(NaiveDateTime, seq)` key so `<ts>` and `<ts>.gz` count as ONE backup.
    struct BackupGroup {
        ts: NaiveDateTime,
        plain: Option<PathBuf>,
        gz: Option<PathBuf>,
    }
    let mut groups: BTreeMap<(NaiveDateTime, u64), BackupGroup> = BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Some(after_prefix) = name_str.strip_prefix(&prefix) else {
            continue;
        };
        let (ts_part, is_gz) = if let Some(s) = after_prefix.strip_suffix(&format!("{ext}.gz")) {
            (s, true)
        } else if let Some(s) = after_prefix.strip_suffix(ext) {
            (s, false)
        } else {
            continue;
        };
        // Strictly validate & parse the rotated timestamp (`YYYY-MM-DDTHH-MM-SS.mmm` plus optional
        // `-<seq>`) so sibling logs like `tidb-slow.log` in the same directory are NEVER touched.
        let Some((ts, seq)) = parse_backup_timestamp(ts_part) else {
            continue;
        };
        let group = groups.entry((ts, seq)).or_insert(BackupGroup {
            ts,
            plain: None,
            gz: None,
        });
        if is_gz {
            group.gz = Some(entry.path());
        } else {
            group.plain = Some(entry.path());
        }
    }

    let now_naive = Local::now().naive_local();
    let max_age = if max_days > 0 {
        Some(chrono::Duration::days(max_days as i64))
    } else {
        None
    };

    // Iterate newest first (reverse BTreeMap order).
    let mut to_compress: Vec<PathBuf> = Vec::new();
    for (idx, (_, grp)) in groups.into_iter().rev().enumerate() {
        let too_many = max_backups > 0 && idx >= max_backups;
        let too_old = max_age.is_some_and(|limit| now_naive.signed_duration_since(grp.ts) > limit);
        if too_many || too_old {
            if let Some(p) = grp.plain {
                let _ = fs::remove_file(p);
            }
            if let Some(p) = grp.gz {
                let _ = fs::remove_file(p);
            }
        } else if compress_gzip && let Some(plain) = grp.plain {
            // Never delete a plain backup on the strength of an existing `.gz` (which could be
            // half-written from a prior crash): recompress into `.gz.tmp` and atomically rename.
            to_compress.push(plain);
        }
    }

    for leftover in to_compress {
        let _ = compress_file_gzip(&leftover);
    }
}

/// Parse rotated archive timestamp suffix: `YYYY-MM-DDTHH-MM-SS.mmm` optionally followed by
/// `-<digits>`.
fn parse_backup_timestamp(s: &str) -> Option<(NaiveDateTime, u64)> {
    let b = s.as_bytes();
    if b.len() < 23 {
        return None;
    }
    let ts_str = &s[..23];
    let ts = NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H-%M-%S%.3f").ok()?;
    if b.len() == 23 {
        return Some((ts, 0));
    }
    if b[23] != b'-' || b.len() <= 24 {
        return None;
    }
    let seq = s[24..].parse::<u64>().ok()?;
    Some((ts, seq))
}

/// Optional buffered wrapper with periodic background flush (Go `zapcore.BufferedWriteSyncer`).
///
/// Holds `self.buf` across `self.inner.write_entry` (matching zap's `BufferedWriteSyncer` mutex)
/// so concurrent writers and the periodic flusher never reorder chunks and `sync()` guarantees
/// every previously written entry has reached `inner`.
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
                    // Hold `bg_buf` across inner write to preserve strict FIFO ordering.
                    let mut b = bg_buf.lock();
                    if !b.is_empty() {
                        let _ = bg_inner.write_entry(&b);
                        b.clear();
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
}

impl LogSink for BufferedSink {
    fn write_entry(&self, data: &[u8]) -> io::Result<()> {
        let mut b = self.buf.lock();
        if b.len().saturating_add(data.len()) > self.capacity && !b.is_empty() {
            self.inner.write_entry(&b)?;
            b.clear();
        }
        if data.len() >= self.capacity {
            self.inner.write_entry(data)?;
        } else {
            b.extend_from_slice(data);
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        let mut b = self.buf.lock();
        if !b.is_empty() {
            self.inner.write_entry(&b)?;
            b.clear();
        }
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

/// Outermost lock-acquisition timeout wrapper matching Go `pingcap/log.LockWithTimeout`
/// (`pingcap/log/log.go:111-114, 130-190`).
///
/// Wraps the final sink (AFTER [`BufferedSink`] when `is-buffered` is enabled) so:
/// - A timeout panics ONLY when acquiring the lock exceeds `timeout` (never after a write or
///   rotation already succeeded).
/// - The background flush thread never owns this wrapper and therefore never panics on timeout.
/// - While inside the global panic hook (`tidb_base::panic_guard::is_in_panic_hook()`), lock
///   timeout returns `Err(TimedOut)` instead of panicking so the panic hook can emit its
///   single-line stderr fallback without aborting the process.
pub struct LockWithTimeoutSink {
    inner: Arc<dyn LogSink>,
    lock: Mutex<()>,
    timeout: Duration,
}

impl LockWithTimeoutSink {
    pub fn new(inner: Arc<dyn LogSink>, timeout: Duration) -> Self {
        Self {
            inner,
            lock: Mutex::new(()),
            timeout,
        }
    }

    fn acquire(&self) -> io::Result<parking_lot::MutexGuard<'_, ()>> {
        if let Some(guard) = self.lock.try_lock_for(self.timeout) {
            return Ok(guard);
        }
        if tidb_base::panic_guard::is_in_panic_hook() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("write log timeout after {:?}", self.timeout),
            ));
        }
        panic!("write log timeout after {:?}", self.timeout);
    }
}

impl LogSink for LockWithTimeoutSink {
    fn write_entry(&self, data: &[u8]) -> io::Result<()> {
        let _guard = self.acquire()?;
        self.inner.write_entry(data)
    }

    fn sync(&self) -> io::Result<()> {
        let _guard = self.acquire()?;
        self.inner.sync()
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
        Arc::new(RotatingFileSink::new(cfg, 0)?)
    };
    let buffered: Arc<dyn LogSink> = if cfg.is_buffered && !cfg.filename.is_empty() {
        Arc::new(BufferedSink::new(
            base,
            cfg.buffer_size,
            cfg.buffer_flush_interval,
        ))
    } else {
        base
    };
    if timeout_secs > 0 {
        Ok(Arc::new(LockWithTimeoutSink::new(
            buffered,
            Duration::from_secs(timeout_secs as u64),
        )))
    } else {
        Ok(buffered)
    }
}
