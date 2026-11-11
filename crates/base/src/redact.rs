//! go-file: pkg/util/redact/redact.go
//!
//! `pkg/util/redact` (block D00.3, ported whole per constitution 4.4 into the lowest crate
//! that uses it: the redact switch lives in [`crate::errors`], and `LazyStmtText` /
//! the slow log in `tidb-sessionctx` call `String`). `TaskInfoRedacted` (BR's
//! `StreamBackupTaskInfo`) is X.

use std::fmt::Display;
use std::io::{BufRead, Write};

use crate::errors::{
    RedactLogDisable, RedactLogEnable, RedactLogMarker, redact_log_enabled_load,
    redact_log_enabled_store,
};

/// String will redact the input string according to 'mode'. Check 'tidb_redact_log'.
// go: String L1
pub fn string(mode: &str, input: &str) -> String {
    match mode {
        "MARKER" => {
            let mut b = String::with_capacity(input.len() + 6);
            b.push('‹');
            for c in input.chars() {
                if c == '‹' || c == '›' {
                    b.push(c);
                    b.push(c);
                } else {
                    b.push(c);
                }
            }
            b.push('›');
            b
        }
        "OFF" => input.to_owned(),
        "ON" => String::new(),
        // should never happen (Go: intest.Assert(false, "invalid redact mode"); only
        // fails in Go's test builds)
        _ => String::new(),
    }
}

/// The value returned by [`stringer`] (Go's unexported `redactStringer`).
pub struct RedactStringer<T> {
    mode: String,
    stringer: T,
}

impl<T: Display> Display for RedactStringer<T> {
    // go: redactStringer.String L1
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&string(&self.mode, &self.stringer.to_string()))
    }
}

/// Stringer will redact the input stringer according to 'mode', similar to String().
// go: Stringer L1
pub fn stringer<T: Display>(mode: &str, input: T) -> RedactStringer<T> {
    RedactStringer {
        mode: mode.to_owned(),
        stringer: input,
    }
}

/// DeRedactFile will deredact the input file, either removing marked contents, or remove
/// the marker. It works line by line.
// go: DeRedactFile L1 (errors are io errors; "-" writes to stdout)
pub fn de_redact_file(remove: bool, input: &str, output: &str) -> std::io::Result<()> {
    let ifile = std::io::BufReader::new(std::fs::File::open(input)?);
    if output == "-" {
        de_redact(remove, ifile, std::io::stdout().lock(), "\n")
    } else {
        let ofile = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(output)?;
        de_redact(remove, ifile, ofile, "\n")
    }
}

/// DeRedact is similar to DeRedactFile, but act on reader/writer, it works line by line.
// go: DeRedact L2 (lines split like bufio.Scanner's ScanLines: "\n" with an optional
// preceding "\r" removed; a line is decoded as Go ranges a string: invalid UTF-8 bytes
// become U+FFFD). Go's bufio.Scanner stops silently at a line over 64 KiB (the error is
// never checked); here such lines are processed.
pub fn de_redact<R: BufRead, W: Write>(
    remove: bool,
    input: R,
    output: W,
    sep: &str,
) -> std::io::Result<()> {
    let mut out = std::io::BufWriter::new(output);
    for line in input.split(b'\n') {
        let mut line = line?;
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let text = String::from_utf8_lossy(&line);
        let chars: Vec<char> = text.chars().collect();
        let mut buf = String::new();
        let mut start = false;
        let mut i = 0;
        while i < chars.len() {
            let ch = chars[i];
            i += 1;
            if ch == '‹' {
                if start {
                    // must be '<'
                    let Some(&pch) = chars.get(i) else {
                        // Go: ReadRune returns io.EOF, which DeRedact returns as an error
                        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "EOF"));
                    };
                    i += 1;
                    if pch == ch {
                        buf.push(ch);
                    } else {
                        buf.push(ch);
                        buf.push(pch);
                    }
                } else {
                    start = true;
                    buf.clear();
                }
            } else if ch == '›' {
                if start {
                    // peek the next
                    let pch = chars.get(i).copied();
                    if pch == Some(ch) {
                        i += 1;
                        buf.push(ch);
                    } else {
                        start = false;
                        if remove {
                            out.write_all(b"?")?;
                        } else {
                            out.write_all(buf.as_bytes())?;
                            // Go: io.Copy drains the buffer
                            buf.clear();
                        }
                    }
                } else {
                    let mut tmp = [0u8; 4];
                    out.write_all(ch.encode_utf8(&mut tmp).as_bytes())?;
                }
            } else if start {
                buf.push(ch);
            } else {
                let mut tmp = [0u8; 4];
                out.write_all(ch.encode_utf8(&mut tmp).as_bytes())?;
            }
        }
        if start {
            out.write_all("‹".as_bytes())?;
            out.write_all(buf.as_bytes())?;
        }
        out.write_all(sep.as_bytes())?;
    }
    out.flush()
}

/// InitRedact inits the enableRedactLog
// go: InitRedact L1
pub fn init_redact(redact_log: bool) {
    let mode = if redact_log {
        RedactLogEnable
    } else {
        RedactLogDisable
    };
    redact_log_enabled_store(mode);
}

/// NeedRedact returns whether to redact log
// go: NeedRedact L1 (the Rust switch stores "" as OFF, which Go also treats as "no")
pub fn need_redact() -> bool {
    let mode = redact_log_enabled_load();
    mode != RedactLogDisable && !mode.is_empty()
}

/// Value receives string argument and return omitted information if redact log enabled
// go: Value L1
pub fn value(arg: &str) -> String {
    if need_redact() {
        return "?".to_owned();
    }
    arg.to_owned()
}

/// Key receives a key return omitted information if redact log enabled
// go: Key L1
pub fn key(key: &[u8]) -> String {
    if need_redact() {
        return "?".to_owned();
    }
    let mut s = String::with_capacity(key.len() * 2);
    for b in key {
        s.push_str(&format!("{b:02X}"));
    }
    s
}

/// WriteRedact is to write string with redact into `strings.Builder`
// go: WriteRedact L1
pub fn write_redact(build: &mut String, v: &str, redact: &str) {
    if redact == RedactLogMarker {
        build.push('‹');
        build.push_str(v);
        build.push('›');
        return;
    } else if redact == RedactLogEnable {
        build.push('?');
        return;
    }
    build.push_str(v);
}

/// [`string`] on Go string bytes (Go strings may hold any bytes): "OFF" returns the input
/// as is; "MARKER" ranges over it as Go does, so each byte of an invalid UTF-8 sequence
/// becomes U+FFFD.
// glue: String over Go string bytes
pub fn string_bytes(mode: &str, input: &[u8]) -> Vec<u8> {
    match mode {
        "MARKER" => {
            let mut b = Vec::with_capacity(input.len() + 6);
            let mut tmp = [0u8; 4];
            b.extend_from_slice("‹".as_bytes());
            for chunk in input.utf8_chunks() {
                for c in chunk.valid().chars() {
                    if c == '‹' || c == '›' {
                        b.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                    }
                    b.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                }
                for _ in chunk.invalid() {
                    b.extend_from_slice("\u{FFFD}".as_bytes());
                }
            }
            b.extend_from_slice("›".as_bytes());
            b
        }
        "OFF" => input.to_vec(),
        _ => Vec::new(),
    }
}
