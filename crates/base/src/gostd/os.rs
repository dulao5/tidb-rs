//! `*os.PathError` texts (Go 1.25.12) for files opened by ported code (D15.3).

use std::io;
use std::io::Read;

/// The text of the `syscall.Errno` behind `e`, as Go prints it (Linux and Darwin share
/// these strings).
// go-std: syscall.Errno.Error L2 (the common errnos; others: std's text without " (os error N)")
pub fn errno_text(e: &io::Error) -> String {
    use io::ErrorKind::*;
    // ELOOP / ENAMETOOLONG (their ErrorKinds are not stable).
    let (eloop, enametoolong) = if cfg!(target_os = "linux") {
        (40, 36)
    } else {
        (62, 63)
    };
    match e.raw_os_error() {
        Some(c) if c == eloop => return "too many levels of symbolic links".to_owned(),
        Some(c) if c == enametoolong => return "file name too long".to_owned(),
        _ => {}
    }
    match e.kind() {
        NotFound => "no such file or directory".to_owned(),
        PermissionDenied => "permission denied".to_owned(),
        NotADirectory => "not a directory".to_owned(),
        IsADirectory => "is a directory".to_owned(),
        _ => {
            let s = e.to_string();
            let s = match s.find(" (os error") {
                Some(i) => s[..i].to_owned(),
                None => s,
            };
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
                None => s,
            }
        }
    }
}

/// `(*os.PathError).Error()`: `op + " " + path + ": " + errno`.
// go-std: os.PathError.Error L1
pub fn path_error(op: &str, path: &[u8], e: &io::Error) -> Vec<u8> {
    let mut out = Vec::with_capacity(op.len() + path.len() + 32);
    out.extend_from_slice(op.as_bytes());
    out.push(b' ');
    out.extend_from_slice(path);
    out.extend_from_slice(b": ");
    out.extend_from_slice(errno_text(e).as_bytes());
    out
}

#[cfg(unix)]
fn os_path(path: &[u8]) -> &std::path::Path {
    use std::os::unix::ffi::OsStrExt;
    std::path::Path::new(std::ffi::OsStr::from_bytes(path))
}

#[cfg(not(unix))]
fn os_path(path: &[u8]) -> std::path::PathBuf {
    std::path::PathBuf::from(String::from_utf8_lossy(path).into_owned())
}

/// `os.Open(path)` followed by reading everything a `bufio.Scanner` would see: the open
/// error as `*os.PathError` text, else the bytes read before the first read error (Go opens
/// a directory without error; reading it fails, so the scanner sees no data).
// go-std: os.Open L2 (O_RDONLY; read errors end the data as they end bufio.Scanner)
pub fn open_read_all(path: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
    let mut f = std::fs::File::open(os_path(path)).map_err(|e| path_error("open", path, &e))?;
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => data.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    Ok(data)
}

/// The data `io.ReadAll` returned and its read error (`*os.PathError` text), if any.
pub type ReadAll = (Vec<u8>, Option<Vec<u8>>);

/// `os.Open(path)` + `io.ReadAll`: the open error, or the data with the first read error
/// as `*os.PathError` text (`read <path>: ...`).
// go-std: io.ReadAll L2 (via os.File.Read)
pub fn open_read_all_err(path: &[u8]) -> Result<ReadAll, Vec<u8>> {
    let mut f = std::fs::File::open(os_path(path)).map_err(|e| path_error("open", path, &e))?;
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match f.read(&mut buf) {
            Ok(0) => return Ok((data, None)),
            Ok(n) => data.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Ok((data, Some(path_error("read", path, &e)))),
        }
    }
}
