//! go-file: github.com/json-iterator/go@v1.1.12/iter.go
//! go-file: github.com/json-iterator/go@v1.1.12/iter_array.go
//! go-file: github.com/json-iterator/go@v1.1.12/iter_float.go
//! go-file: github.com/json-iterator/go@v1.1.12/iter_int.go
//! go-file: github.com/json-iterator/go@v1.1.12/stream_float.go
//!
//! The part of jsoniter's `Iterator` that `ParseVectorFloat32` uses
//! (`ParseString(ConfigDefault, s)`, `ReadArrayCB`, `ReadFloat64`), ported as is: its
//! number grammar is not JSON's (a leading `+` is accepted through the slow path, digits
//! with up to 6 decimals are divided by a power of ten instead of being parsed exactly),
//! and it does not check for data after the array.
//!
//! The iterator reads from a byte slice only (Go: `reader == nil`), so `loadMore` never
//! loads anything.

#![allow(non_upper_case_globals)]

use tidb_base::gostd::strconv::parse_float;

const uint64SafeToMultiple10: u64 = u64::MAX / 10 - 1;
const maxFloat64: u64 = (1 << 53) - 1;
const maxDepth: isize = 10000;
const pow10: [u64; 7] = [1, 10, 100, 1000, 10000, 100000, 1000000];

const invalidCharForNumber: i8 = -1;
const endOfNumber: i8 = -2;
const dotInNumber: i8 = -3;

/// `floatDigits` (filled by the package `init` in Go).
// go-dep: github.com/json-iterator/go@v1.1.12.init L1
const fn float_digits() -> [i8; 256] {
    let mut t = [invalidCharForNumber; 256];
    let mut i = b'0';
    while i <= b'9' {
        t[i as usize] = (i - b'0') as i8;
        i += 1;
    }
    t[b',' as usize] = endOfNumber;
    t[b']' as usize] = endOfNumber;
    t[b'}' as usize] = endOfNumber;
    t[b' ' as usize] = endOfNumber;
    t[b'\t' as usize] = endOfNumber;
    t[b'\n' as usize] = endOfNumber;
    t[b'.' as usize] = dotInNumber;
    t
}

static floatDigits: [i8; 256] = float_digits();

/// `Iterator.Error`: `io.EOF`, or another error (its text; only jsoniter's own messages
/// and strconv's are produced here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IterError {
    Eof,
    Other(Vec<u8>),
}

/// `jsoniter.Iterator` over a byte slice.
pub struct Iterator<'a> {
    buf: &'a [u8],
    head: usize,
    tail: usize,
    depth: isize,
    pub error: Option<IterError>,
}

/// `jsoniter.ParseString(cfg, input)` / `ParseBytes`; the config is not used by the
/// functions ported here.
// go-dep: github.com/json-iterator/go@v1.1.12.ParseString L2 (config dropped: ReadArrayCB and ReadFloat64 do not read it)
// go-dep: github.com/json-iterator/go@v1.1.12.ParseBytes L2 (config dropped)
pub fn parse_string(input: &[u8]) -> Iterator<'_> {
    Iterator {
        buf: input,
        head: 0,
        tail: input.len(),
        depth: 0,
        error: None,
    }
}

impl<'a> Iterator<'a> {
    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.nextToken L1
    fn next_token(&mut self) -> u8 {
        // a variation of skip whitespaces, returning the next non-whitespace token
        loop {
            for i in self.head..self.tail {
                let c = self.buf[i];
                match c {
                    b' ' | b'\n' | b'\t' | b'\r' => continue,
                    _ => {}
                }
                self.head = i + 1;
                return c;
            }
            if !self.load_more() {
                return 0;
            }
        }
    }

    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.unreadByte L1
    fn unread_byte(&mut self) {
        if self.error.is_some() {
            return;
        }
        self.head -= 1;
    }

    /// ReportError record a error in iterator instance with current position.
    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.ReportError L1
    pub fn report_error(&mut self, operation: &str, msg: &str) {
        if let Some(e) = &self.error
            && *e != IterError::Eof
        {
            return;
        }
        let peek_start = self.head.saturating_sub(10);
        let peek_end = (self.head + 10).min(self.tail);
        let parsing = &self.buf[peek_start..peek_end];
        let context_start = self.head.saturating_sub(50);
        let context_end = (self.head + 50).min(self.tail);
        let context = &self.buf[context_start..context_end];
        let mut m = Vec::new();
        m.extend_from_slice(operation.as_bytes());
        m.extend_from_slice(b": ");
        m.extend_from_slice(msg.as_bytes());
        m.extend_from_slice(b", error found in #");
        m.extend_from_slice((self.head - peek_start).to_string().as_bytes());
        m.extend_from_slice(b" byte of ...|");
        m.extend_from_slice(parsing);
        m.extend_from_slice(b"|..., bigger context ...|");
        m.extend_from_slice(context);
        m.extend_from_slice(b"|...");
        self.error = Some(IterError::Other(m));
    }

    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.loadMore L2 (no reader: the input is one byte slice)
    fn load_more(&mut self) -> bool {
        if self.error.is_none() {
            self.head = self.tail;
            self.error = Some(IterError::Eof);
        }
        false
    }

    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.readByte L1
    fn read_byte(&mut self) -> u8 {
        if self.head == self.tail {
            if self.load_more() {
                let ret = self.buf[self.head];
                self.head += 1;
                return ret;
            }
            return 0;
        }
        let ret = self.buf[self.head];
        self.head += 1;
        ret
    }

    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.skipThreeBytes L1
    fn skip_three_bytes(&mut self, b1: u8, b2: u8, b3: u8) {
        let expect = format!("expect {}", String::from_utf8_lossy(&[b1, b2, b3]));
        if self.read_byte() != b1 {
            self.report_error("skipThreeBytes", &expect);
            return;
        }
        if self.read_byte() != b2 {
            self.report_error("skipThreeBytes", &expect);
            return;
        }
        if self.read_byte() != b3 {
            self.report_error("skipThreeBytes", &expect);
        }
    }

    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.incrementDepth L1
    fn increment_depth(&mut self) -> bool {
        self.depth += 1;
        if self.depth <= maxDepth {
            return true;
        }
        self.report_error("incrementDepth", "exceeded max depth");
        false
    }

    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.decrementDepth L1
    fn decrement_depth(&mut self) -> bool {
        self.depth -= 1;
        if self.depth >= 0 {
            return true;
        }
        self.report_error("decrementDepth", "unexpected negative nesting");
        false
    }

    /// ReadArrayCB read array with callback
    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.ReadArrayCB L1
    pub fn read_array_cb(&mut self, mut callback: impl FnMut(&mut Iterator<'a>) -> bool) -> bool {
        let mut c = self.next_token();
        if c == b'[' {
            if !self.increment_depth() {
                return false;
            }
            c = self.next_token();
            if c != b']' {
                self.unread_byte();
                if !callback(self) {
                    self.decrement_depth();
                    return false;
                }
                c = self.next_token();
                while c == b',' {
                    if !callback(self) {
                        self.decrement_depth();
                        return false;
                    }
                    c = self.next_token();
                }
                if c != b']' {
                    let msg = format!(
                        "expect ] in the end, but found {}",
                        String::from_utf8_lossy(&[c])
                    );
                    self.report_error("ReadArrayCB", &msg);
                    self.decrement_depth();
                    return false;
                }
                return self.decrement_depth();
            }
            return self.decrement_depth();
        }
        if c == b'n' {
            self.skip_three_bytes(b'u', b'l', b'l');
            return true; // null
        }
        let msg = format!("expect [ or n, but found {}", String::from_utf8_lossy(&[c]));
        self.report_error("ReadArrayCB", &msg);
        false
    }

    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.readNumberAsString L1
    fn read_number_as_string(&mut self) -> Vec<u8> {
        let mut str = Vec::with_capacity(16);
        'load_loop: loop {
            for i in self.head..self.tail {
                let c = self.buf[i];
                match c {
                    b'+' | b'-' | b'.' | b'e' | b'E' | b'0'..=b'9' => {
                        str.push(c);
                        continue;
                    }
                    _ => {
                        self.head = i;
                        break 'load_loop;
                    }
                }
            }
            if !self.load_more() {
                break;
            }
        }
        if let Some(e) = &self.error
            && *e != IterError::Eof
        {
            return str;
        }
        if str.is_empty() {
            self.report_error("readNumberAsString", "invalid number");
        }
        str
    }

    /// ReadFloat64 read float64
    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.ReadFloat64 L1
    pub fn read_float64(&mut self) -> f64 {
        let c = self.next_token();
        if c == b'-' {
            return -self.read_positive_float64();
        }
        self.unread_byte();
        self.read_positive_float64()
    }

    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.readPositiveFloat64 L1
    fn read_positive_float64(&mut self) -> f64 {
        let mut i = self.head;
        // first char
        if i == self.tail {
            return self.read_float64_slow_path();
        }
        let mut c = self.buf[i];
        i += 1;
        let ind = floatDigits[c as usize];
        match ind {
            invalidCharForNumber => return self.read_float64_slow_path(),
            endOfNumber => {
                self.report_error("readFloat64", "empty number");
                return 0.0;
            }
            dotInNumber => {
                self.report_error("readFloat64", "leading dot is invalid");
                return 0.0;
            }
            0 => {
                if i == self.tail {
                    return self.read_float64_slow_path();
                }
                c = self.buf[i];
                if c.is_ascii_digit() {
                    self.report_error("readFloat64", "leading zero is invalid");
                    return 0.0;
                }
            }
            _ => {}
        }
        let mut value = ind as u64;
        // chars before dot
        while i < self.tail {
            c = self.buf[i];
            let ind = floatDigits[c as usize];
            match ind {
                invalidCharForNumber => return self.read_float64_slow_path(),
                endOfNumber => {
                    self.head = i;
                    return value as f64;
                }
                dotInNumber => break,
                _ => {}
            }
            if value > uint64SafeToMultiple10 {
                return self.read_float64_slow_path();
            }
            value = (value << 3) + (value << 1) + ind as u64; // value = value * 10 + ind;
            i += 1;
        }
        // chars after dot
        if c == b'.' {
            i += 1;
            let mut decimal_places = 0usize;
            if i == self.tail {
                return self.read_float64_slow_path();
            }
            while i < self.tail {
                c = self.buf[i];
                let ind = floatDigits[c as usize];
                match ind {
                    endOfNumber => {
                        if decimal_places > 0 && decimal_places < pow10.len() {
                            self.head = i;
                            return value as f64 / pow10[decimal_places] as f64;
                        }
                        // too many decimal places
                        return self.read_float64_slow_path();
                    }
                    invalidCharForNumber | dotInNumber => return self.read_float64_slow_path(),
                    _ => {}
                }
                decimal_places += 1;
                if value > uint64SafeToMultiple10 {
                    return self.read_float64_slow_path();
                }
                value = (value << 3) + (value << 1) + ind as u64;
                if value > maxFloat64 {
                    return self.read_float64_slow_path();
                }
                i += 1;
            }
        }
        self.read_float64_slow_path()
    }

    // go-dep: github.com/json-iterator/go@v1.1.12.Iterator.readFloat64SlowPath L1
    fn read_float64_slow_path(&mut self) -> f64 {
        let str = self.read_number_as_string();
        if let Some(e) = &self.error
            && *e != IterError::Eof
        {
            return 0.0;
        }
        let err_msg = validate_float(&str);
        if !err_msg.is_empty() {
            self.report_error("readFloat64SlowPath", err_msg);
            return 0.0;
        }
        // The bytes are ASCII: readNumberAsString keeps only `+-.eE0-9`.
        let s = std::str::from_utf8(&str).expect("ASCII number");
        match parse_float(s) {
            Ok(v) => v,
            Err(range) => {
                let why = if range {
                    "value out of range"
                } else {
                    "invalid syntax"
                };
                self.error = Some(IterError::Other(
                    format!("strconv.ParseFloat: parsing {s:?}: {why}").into_bytes(),
                ));
                0.0
            }
        }
    }
}

// go-dep: github.com/json-iterator/go@v1.1.12.validateFloat L1
fn validate_float(str: &[u8]) -> &'static str {
    // strconv.ParseFloat is not validating `1.` or `1.e1`
    if str.is_empty() {
        return "empty number";
    }
    if str[0] == b'-' {
        return "-- is not valid";
    }
    if let Some(dot_pos) = str.iter().position(|&b| b == b'.') {
        if dot_pos == str.len() - 1 {
            return "dot can not be last character";
        }
        match str[dot_pos + 1] {
            b'0'..=b'9' => {}
            _ => return "missing digit after dot",
        }
    }
    ""
}
