//! go-file: github.com/BurntSushi/toml@v1.5.0/error.go

use crate::gofmt::{self, Arg};

/// `Position` of an error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    /// Line number, starting at 1.
    pub line: isize,
    /// Error column, starting at 1.
    pub col: isize,
    /// Start of error, as byte offset starting at 0.
    pub start: isize,
    /// Length of the error in bytes.
    pub len: isize,
}

impl Position {
    // go-dep: github.com/BurntSushi/toml@v1.5.0.Position.withCol L1
    pub(crate) fn with_col(mut self, toml_file: &[u8]) -> Position {
        let mut pos: isize = 0;
        for line in toml_file.split(|&b| b == b'\n') {
            let ll = line.len() as isize + 1; // +1 for the removed newline
            if pos + ll >= self.start {
                self.col = self.start - pos + 1;
                if self.col < 1 {
                    // Should never happen, but just in case.
                    self.col = 1;
                }
                break;
            }
            pos += ll;
        }
        self
    }
}

/// The lexer / parser errors of BurntSushi (`errLex*`, `errParse*`) and plain messages.
#[derive(Clone, Debug, PartialEq)]
pub enum LexErr {
    /// `errLexControl`.
    Control(char),
    /// `errLexEscape`.
    Escape(char),
    /// `errLexUTF8`.
    Utf8(u8),
    /// `errParseDate`.
    ParseDate(String),
    /// `errLexInlineTableNL`.
    InlineTableNL,
    /// `errLexStringNL`.
    StringNL,
    /// `errParseRange{i, size}` (i formatted with `%v`).
    ParseRange(String, &'static str),
    /// `errUnsafeFloat{i, size}`.
    UnsafeFloat(String, &'static str),
    /// `errParseDuration`.
    ParseDuration(String),
    /// `fmt.Errorf` messages and errors from TextUnmarshalers.
    Msg(String),
}

impl LexErr {
    /// `Error()` of the Go error values.
    // go-dep: github.com/BurntSushi/toml@v1.5.0.errLexControl.Error L1
    // go-dep: github.com/BurntSushi/toml@v1.5.0.errLexEscape.Error L1
    // go-dep: github.com/BurntSushi/toml@v1.5.0.errLexUTF8.Error L1
    // go-dep: github.com/BurntSushi/toml@v1.5.0.errParseDate.Error L1
    // go-dep: github.com/BurntSushi/toml@v1.5.0.errLexInlineTableNL.Error L1
    // go-dep: github.com/BurntSushi/toml@v1.5.0.errLexStringNL.Error L1
    // go-dep: github.com/BurntSushi/toml@v1.5.0.errParseRange.Error L1
    // go-dep: github.com/BurntSushi/toml@v1.5.0.errUnsafeFloat.Error L1
    // go-dep: github.com/BurntSushi/toml@v1.5.0.errParseDuration.Error L1
    pub fn error(&self) -> String {
        let s = |f: &str, a: &[Arg]| String::from_utf8_lossy(&gofmt::sprintf(f, a)).into_owned();
        match self {
            LexErr::Control(r) => s(
                "TOML files cannot contain control characters: '0x%02x'",
                &[Arg::Int(i64::from(*r as u32), "int32")],
            ),
            LexErr::Escape(r) => s(
                "invalid escape in string '\\%c'",
                &[Arg::Int(i64::from(*r as u32), "int32")],
            ),
            LexErr::Utf8(b) => s("invalid UTF-8 byte: 0x%02x", &[Arg::from(*b)]),
            LexErr::ParseDate(v) => s("invalid datetime: %q", &[Arg::from(v.clone())]),
            LexErr::InlineTableNL => "newlines not allowed within inline tables".to_owned(),
            LexErr::StringNL => "strings cannot contain newlines".to_owned(),
            LexErr::ParseRange(i, size) => format!("{i} is out of range for {size}"),
            LexErr::UnsafeFloat(i, size) => format!("{i} is out of the safe {size} range"),
            LexErr::ParseDuration(d) => s("invalid duration: %q", &[Arg::from(d.clone())]),
            LexErr::Msg(m) => m.clone(),
        }
    }
}

/// `ParseError` is returned when there is an error parsing the TOML syntax such as invalid
/// syntax, duplicate keys, etc.
#[derive(Clone, Debug, PartialEq)]
pub struct ParseError {
    /// Short technical message.
    pub message: String,
    /// Position of the error
    pub position: Position,
    /// Last parsed key, may be blank.
    pub last_key: String,
    /// Line the error occurred.
    ///
    /// Deprecated: use [Position].
    pub line: isize,
    pub(crate) err: Option<Box<LexErr>>,
}

impl ParseError {
    // go-dep: github.com/BurntSushi/toml@v1.5.0.ParseError.Error L1
    pub fn error(&self) -> String {
        if self.last_key.is_empty() {
            return format!("toml: line {}: {}", self.position.line, self.message);
        }
        let q = gofmt::sprintf("%q", &[Arg::from(self.last_key.clone())]);
        format!(
            "toml: line {} (last key {}): {}",
            self.position.line,
            String::from_utf8_lossy(&q),
            self.message
        )
    }
}

/// Errors of `Decode`: a [`ParseError`] or a `fmt.Errorf` message (`md.e`, `badtype`).
#[derive(Clone, Debug, PartialEq)]
pub enum DecodeError {
    Parse(ParseError),
    Msg(String),
    /// The file could not be read (Go: `*fs.PathError`).
    Io(String),
}

impl DecodeError {
    /// `Error()`.
    // glue: Error() of the Go error values returned by Decode
    pub fn error(&self) -> String {
        match self {
            DecodeError::Parse(p) => p.error(),
            DecodeError::Msg(m) | DecodeError::Io(m) => m.clone(),
        }
    }
}
