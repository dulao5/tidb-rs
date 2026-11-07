//! Go `fmt.Sprintf` / `fmt.Sprint` for error messages (ON-DEMAND tool code, Go 1.25.12).
//!
//! go-file: $GOROOT/src/fmt/print.go
//! go-file: $GOROOT/src/fmt/format.go
//!
//! Error messages are Go printf templates (`mysql.MySQLErrName`, `terror.Error`) filled with
//! `...interface{}` arguments. Go's `fmt` is dynamically typed; the port carries the dynamic
//! type in [`Arg`] and follows `doPrintf` / `printArg` / `fmtInteger` / `fmtS` line by line
//! for the verbs and argument types that occur in TiDB error arguments:
//!
//! * verbs `v s d q x X t c b o O T` and `%%`, flags `#0+- `, width and precision
//!   (digits or `*`), argument indexes (`%[2]d`);
//! * `%!d(string=x)` for a verb the argument does not support, `%!d(MISSING)`,
//!   `%!(EXTRA string=x)`, `%!(NOVERB)`, `%!(BADWIDTH)`, `%!(BADPREC)`.
//!
//! Strings are Rust `String`s (valid UTF-8). A Go string or `[]byte` argument with invalid
//! UTF-8 is converted lossily before formatting (`Arg::from(&[u8])`), so such bytes print as
//! U+FFFD instead of raw bytes — the one known difference (see the block plan).

#![allow(clippy::needless_range_loop)]

use std::borrow::Cow;
use std::fmt::Write as _;

/// One `interface{}` argument of a Go `fmt` call, with its dynamic Go type.
#[derive(Clone, Debug, PartialEq)]
pub enum Arg {
    /// `nil` interface.
    Nil,
    /// Go `string`.
    Str(Cow<'static, str>),
    /// Go `[]byte` (converted lossily if not UTF-8).
    Bytes(Vec<u8>),
    /// Signed integer of Go type `.1` (`"int"`, `"int64"`, ...).
    Int(i64, &'static str),
    /// Unsigned integer of Go type `.1` (`"uint"`, `"uint8"`, ...).
    Uint(u64, &'static str),
    /// `float64` (`.1 == 64`) or `float32` (`.1 == 32`).
    Float(f64, u8),
    /// Go `bool`.
    Bool(bool),
    /// A value with an `Error()` or `String()` method (Go `error`, `fmt.Stringer`) and no
    /// `Format` method: the method's result and the Go type name (for `%T` and bad verbs).
    Stringer(String, &'static str),
    /// A pingcap/errors error (`errors.New`'s `*errors.fundamental`, `errors.Trace`'s
    /// `*errors.withStack`), whose `Format` method prints the message for `%v` and `%s`
    /// (ignoring width and precision), `%q` of it for `%q`, and nothing for other verbs.
    /// A tidb-rs [`crate::Error`] argument is this (Go errors are passed `Trace`d).
    Error(String),
    /// `errors.redactFormatter` (pingcap/errors, `RedactLogMarker`): formats the wrapped value
    /// with the same verb and flags, then encloses it in `‹›`, doubling `‹` and `›` inside.
    Redact(Box<Arg>),
}

impl Arg {
    /// `reflect.TypeOf(arg).String()`.
    // glue: dynamic Go type name of an argument
    fn type_string(&self) -> Cow<'static, str> {
        match self {
            Arg::Nil => Cow::Borrowed("<nil>"),
            Arg::Str(_) => Cow::Borrowed("string"),
            Arg::Bytes(_) => Cow::Borrowed("[]uint8"),
            Arg::Int(_, t) | Arg::Uint(_, t) | Arg::Stringer(_, t) => Cow::Borrowed(t),
            Arg::Error(_) => Cow::Borrowed("*errors.fundamental"),
            Arg::Float(_, 32) => Cow::Borrowed("float32"),
            Arg::Float(..) => Cow::Borrowed("float64"),
            Arg::Bool(_) => Cow::Borrowed("bool"),
            Arg::Redact(_) => Cow::Borrowed("*errors.redactFormatter"),
        }
    }

    /// `reflect.TypeOf(arg).Kind() == reflect.String` (for `Sprint`'s spacing).
    // glue: dynamic Go kind test
    fn is_string_kind(&self) -> bool {
        matches!(self, Arg::Str(_))
    }
}

impl From<&'static str> for Arg {
    // glue: Go string argument
    fn from(s: &'static str) -> Self {
        Arg::Str(Cow::Borrowed(s))
    }
}
impl From<String> for Arg {
    // glue: Go string argument
    fn from(s: String) -> Self {
        Arg::Str(Cow::Owned(s))
    }
}
impl From<&String> for Arg {
    // glue: Go string argument
    fn from(s: &String) -> Self {
        Arg::Str(Cow::Owned(s.clone()))
    }
}
impl From<&[u8]> for Arg {
    // glue: Go []byte argument
    fn from(b: &[u8]) -> Self {
        Arg::Bytes(b.to_vec())
    }
}
impl From<Vec<u8>> for Arg {
    // glue: Go []byte argument
    fn from(b: Vec<u8>) -> Self {
        Arg::Bytes(b)
    }
}
impl From<&crate::errors::Error> for Arg {
    // glue: Go error argument (a pingcap/errors value, see Arg::Error)
    fn from(e: &crate::errors::Error) -> Self {
        Arg::Error(e.error())
    }
}
impl From<crate::errors::Error> for Arg {
    // glue: Go error argument (a pingcap/errors value, see Arg::Error)
    fn from(e: crate::errors::Error) -> Self {
        Arg::Error(e.error())
    }
}
impl From<bool> for Arg {
    // glue: Go bool argument
    fn from(b: bool) -> Self {
        Arg::Bool(b)
    }
}
impl From<f64> for Arg {
    // glue: Go float64 argument
    fn from(f: f64) -> Self {
        Arg::Float(f, 64)
    }
}
impl From<f32> for Arg {
    // glue: Go float32 argument
    fn from(f: f32) -> Self {
        Arg::Float(f64::from(f), 32)
    }
}
macro_rules! int_arg {
    ($($t:ty => $v:ident $go:literal),*) => {$(
        impl From<$t> for Arg {
            // glue: Go integer argument (M-INT: isize/usize are Go int/uint)
            fn from(i: $t) -> Self {
                Arg::$v(i as _, $go)
            }
        }
    )*};
}
int_arg!(i8 => Int "int8", i16 => Int "int16", i32 => Int "int32", i64 => Int "int64",
    isize => Int "int", u8 => Uint "uint8", u16 => Uint "uint16", u32 => Uint "uint32",
    u64 => Uint "uint64", usize => Uint "uint");

/// `Sprintf`-style argument list: `go_args![a, b]` converts each with `Arg::from`.
#[macro_export]
macro_rules! go_args {
    ($($a:expr),* $(,)?) => { vec![$($crate::gofmt::Arg::from($a)),*] };
}

const LDIGITS: &[u8; 17] = b"0123456789abcdefx";
const UDIGITS: &[u8; 17] = b"0123456789ABCDEFX";

/// `fmt.fmt`: flags, width, precision.
#[derive(Clone, Copy, Default)]
struct Flags {
    wid_present: bool,
    prec_present: bool,
    minus: bool,
    plus: bool,
    sharp: bool,
    space: bool,
    zero: bool,
    plus_v: bool,
    sharp_v: bool,
    wid: isize,
    prec: isize,
}

/// `fmt.pp`.
struct Pp {
    buf: String,
    f: Flags,
    erroring: bool,
    good_arg_num: bool,
}

/// `fmt.Sprintf`.
// go-std: fmt.Sprintf L1
pub fn sprintf(format: &str, a: &[Arg]) -> String {
    let mut p = Pp::new();
    p.do_printf(format, a);
    p.buf
}

/// `fmt.Sprint`.
// go-std: fmt.Sprint L1
pub fn sprint(a: &[Arg]) -> String {
    let mut p = Pp::new();
    p.do_print(a);
    p.buf
}

impl Pp {
    // glue: newPrinter
    fn new() -> Pp {
        Pp {
            buf: String::new(),
            f: Flags::default(),
            erroring: false,
            good_arg_num: true,
        }
    }

    // go-std: fmt.pp.doPrint L1
    fn do_print(&mut self, a: &[Arg]) {
        let mut prev_string = false;
        for (arg_num, arg) in a.iter().enumerate() {
            let is_string = arg.is_string_kind();
            // Add a space between two non-string arguments.
            if arg_num > 0 && !is_string && !prev_string {
                self.buf.push(' ');
            }
            self.print_arg(arg, 'v');
            prev_string = is_string;
        }
    }

    // go-std: fmt.pp.doPrintf L1 (argument indexes `[n]` are reported as BADINDEX)
    fn do_printf(&mut self, format: &str, a: &[Arg]) {
        let fb = format.as_bytes();
        let end = fb.len();
        let mut arg_num = 0usize;
        let mut i = 0usize;
        let mut reordered = false;
        'format_loop: while i < end {
            self.good_arg_num = true;
            let lasti = i;
            while i < end && fb[i] != b'%' {
                i += 1;
            }
            if i > lasti {
                self.buf.push_str(&format[lasti..i]);
            }
            if i >= end {
                // done processing format string
                break;
            }

            // Process one verb
            i += 1;

            // Do we have flags?
            self.f = Flags::default();
            'simple_format: while i < end {
                let c = fb[i];
                match c {
                    b'#' => self.f.sharp = true,
                    b'0' => self.f.zero = true,
                    b'+' => self.f.plus = true,
                    b'-' => self.f.minus = true,
                    b' ' => self.f.space = true,
                    _ => {
                        // Fast path for common case of ascii lower case simple verbs
                        // without precision or width or argument indices.
                        if c.is_ascii_lowercase() && arg_num < a.len() {
                            if c == b'v' {
                                // Go syntax
                                self.f.sharp_v = self.f.sharp;
                                self.f.sharp = false;
                                // Struct-field syntax
                                self.f.plus_v = self.f.plus;
                                self.f.plus = false;
                            }
                            self.print_arg(&a[arg_num], char::from(c));
                            arg_num += 1;
                            i += 1;
                            continue 'format_loop;
                        }
                        // Format is more complex than simple flags and a verb or is malformed.
                        break 'simple_format;
                    }
                }
                i += 1;
            }

            // Do we have an explicit argument index?
            let mut after_index;
            (arg_num, i, after_index) =
                self.arg_number(arg_num, format, i, a.len(), &mut reordered);

            // Do we have width?
            if i < end && fb[i] == b'*' {
                i += 1;
                let (wid, present, n) = int_from_arg(a, arg_num);
                self.f.wid = wid;
                self.f.wid_present = present;
                arg_num = n;
                if !self.f.wid_present {
                    self.buf.push_str("%!(BADWIDTH)");
                }
                // We have a negative width, so take its value and ensure
                // that the minus flag is set
                if self.f.wid < 0 {
                    self.f.wid = -self.f.wid;
                    self.f.minus = true;
                    self.f.zero = false; // Do not pad with zeros to the right.
                }
                after_index = false;
            } else {
                let (wid, present, ni) = parsenum(fb, i, end);
                self.f.wid = wid;
                self.f.wid_present = present;
                i = ni;
                if after_index && self.f.wid_present {
                    // "%[3]2d"
                    self.good_arg_num = false;
                }
            }

            // Do we have precision?
            if i + 1 < end && fb[i] == b'.' {
                i += 1;
                if after_index {
                    // "%[3].2d"
                    self.good_arg_num = false;
                }
                (arg_num, i, after_index) =
                    self.arg_number(arg_num, format, i, a.len(), &mut reordered);
                if i < end && fb[i] == b'*' {
                    i += 1;
                    let (prec, present, n) = int_from_arg(a, arg_num);
                    self.f.prec = prec;
                    self.f.prec_present = present;
                    arg_num = n;
                    // Negative precision arguments don't make sense
                    if self.f.prec < 0 {
                        self.f.prec = 0;
                        self.f.prec_present = false;
                    }
                    if !self.f.prec_present {
                        self.buf.push_str("%!(BADPREC)");
                    }
                    after_index = false;
                } else {
                    let (prec, present, ni) = parsenum(fb, i, end);
                    self.f.prec = prec;
                    self.f.prec_present = present;
                    i = ni;
                    if !self.f.prec_present {
                        self.f.prec = 0;
                        self.f.prec_present = true;
                    }
                }
            }

            if !after_index {
                (arg_num, i, _) = self.arg_number(arg_num, format, i, a.len(), &mut reordered);
            }

            if i >= end {
                self.buf.push_str("%!(NOVERB)");
                break;
            }

            let verb = format[i..].chars().next().unwrap_or('\u{fffd}');
            i += verb.len_utf8();

            if verb == '%' {
                // Percent does not absorb operands and ignores f.wid and f.prec.
                self.buf.push('%');
            } else if !self.good_arg_num {
                let _ = write!(self.buf, "%!{verb}(BADINDEX)");
            } else if arg_num >= a.len() {
                // No argument left over to print for the current verb.
                let _ = write!(self.buf, "%!{verb}(MISSING)");
            } else {
                if verb == 'v' {
                    // Go syntax
                    self.f.sharp_v = self.f.sharp;
                    self.f.sharp = false;
                    // Struct-field syntax
                    self.f.plus_v = self.f.plus;
                    self.f.plus = false;
                }
                self.print_arg(&a[arg_num], verb);
                arg_num += 1;
            }
        }

        // Check for extra arguments unless the call accessed the arguments
        // out of order, in which case it's too expensive to detect if they've all
        // been used and arguably OK if they're not.
        if !reordered && arg_num < a.len() {
            self.f = Flags::default();
            self.buf.push_str("%!(EXTRA ");
            for (i, arg) in a[arg_num..].iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                if *arg == Arg::Nil {
                    self.buf.push_str("<nil>");
                } else {
                    let t = arg.type_string();
                    self.buf.push_str(&t);
                    self.buf.push('=');
                    self.print_arg(arg, 'v');
                }
            }
            self.buf.push(')');
        }
    }

    // go-std: fmt.pp.argNumber L1
    fn arg_number(
        &mut self,
        arg_num: usize,
        format: &str,
        i: usize,
        num_args: usize,
        reordered: &mut bool,
    ) -> (usize, usize, bool) {
        let fb = format.as_bytes();
        if fb.len() <= i || fb[i] != b'[' {
            return (arg_num, i, false);
        }
        *reordered = true;
        let (index, wid, ok) = parse_arg_number(&fb[i..]);
        if ok && 0 <= index && (index as usize) < num_args {
            return (index as usize, i + wid, true);
        }
        self.good_arg_num = false;
        (arg_num, i + wid, ok)
    }

    // go-std: fmt.pp.printArg L1
    fn print_arg(&mut self, arg: &Arg, verb: char) {
        if *arg == Arg::Nil {
            match verb {
                'T' | 'v' => self.pad_string("<nil>"),
                _ => self.bad_verb(arg, verb),
            }
            return;
        }
        // %T (the value's type) is special; we always do it first.
        if verb == 'T' {
            let t = arg.type_string();
            self.fmt_s(&t);
            return;
        }
        match arg {
            Arg::Bool(b) => self.fmt_bool(arg, *b, verb),
            Arg::Float(v, size) => self.fmt_float(arg, *v, *size, verb),
            Arg::Int(v, _) => self.fmt_integer(arg, *v as u64, true, verb),
            Arg::Uint(v, _) => self.fmt_integer(arg, *v, false, verb),
            Arg::Str(s) => self.fmt_string(arg, s, verb),
            Arg::Bytes(b) => self.fmt_bytes(arg, b, verb),
            Arg::Stringer(s, _) if self.erroring => {
                // Go prints the value by reflection here (printValue); the port prints the
                // method's result.
                let s = s.clone();
                self.pad(&s);
            }
            Arg::Stringer(..) | Arg::Redact(_) | Arg::Error(_) => {
                if !self.handle_methods(arg, verb) {
                    self.bad_verb(arg, verb);
                }
            }
            Arg::Nil => unreachable!(),
        }
    }

    // go-std: fmt.pp.handleMethods L1 (Formatter: only errors.redactFormatter; error/Stringer)
    fn handle_methods(&mut self, arg: &Arg, verb: char) -> bool {
        if self.erroring {
            return false;
        }
        match arg {
            // Is it a Formatter? (errors.redactFormatter.Format)
            Arg::Redact(inner) => {
                self.redact_format(inner, verb);
                true
            }
            // Is it a Formatter? (errors.fundamental.Format / withStack.Format)
            Arg::Error(msg) => {
                match verb {
                    // `%+v` also prints the stack in Go; the port has no Go stack to print.
                    'v' | 's' => self.buf.push_str(msg),
                    'q' => {
                        let q = sprintf("%q", &[Arg::from(msg.clone())]);
                        self.buf.push_str(&q);
                    }
                    _ => {}
                }
                true
            }
            Arg::Stringer(s, _) => {
                // GoStringer is not modelled: %#v of an error falls back to reflection in Go.
                if self.f.sharp_v {
                    return false;
                }
                match verb {
                    'v' | 's' | 'x' | 'X' | 'q' => {
                        let s = s.clone();
                        self.fmt_string(arg, &s, verb);
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// `errors.redactFormatter.Format`: `fmt.Sprintf(fmt.FormatString(f, verb), e.arg)`
    /// enclosed in `‹›` with `‹`/`›` doubled.
    // go-std: github.com/pingcap/errors.redactFormatter.Format L1
    fn redact_format(&mut self, arg: &Arg, verb: char) {
        let origin = sprintf(&self.format_string(verb), std::slice::from_ref(arg));
        self.buf.push('‹');
        for c in origin.chars() {
            if c == '‹' || c == '›' {
                self.buf.push(c);
                self.buf.push(c);
            } else {
                self.buf.push(c);
            }
        }
        self.buf.push('›');
    }

    /// `fmt.FormatString`: the directive that reconstructs the current flags.
    // go-std: fmt.FormatString L1
    fn format_string(&self, verb: char) -> String {
        let mut s = String::from("%");
        // Flags (the State reports sharpV / plusV as '#' / '+').
        if self.f.plus || self.f.plus_v {
            s.push('+');
        }
        if self.f.sharp || self.f.sharp_v {
            s.push('#');
        }
        if self.f.space {
            s.push(' ');
        }
        if self.f.minus {
            s.push('-');
        }
        if self.f.zero {
            s.push('0');
        }
        if self.f.wid_present {
            let _ = write!(s, "{}", self.f.wid);
        }
        if self.f.prec_present {
            let _ = write!(s, ".{}", self.f.prec);
        }
        s.push(verb);
        s
    }

    // go-std: fmt.pp.badVerb L1
    fn bad_verb(&mut self, arg: &Arg, verb: char) {
        self.erroring = true;
        let _ = write!(self.buf, "%!{verb}(");
        if *arg == Arg::Nil {
            self.buf.push_str("<nil>");
        } else {
            let t = arg.type_string();
            self.buf.push_str(&t);
            self.buf.push('=');
            let saved = self.f;
            self.print_arg(arg, 'v');
            self.f = saved;
        }
        self.buf.push(')');
        self.erroring = false;
    }

    // go-std: fmt.pp.fmtBool L1
    fn fmt_bool(&mut self, arg: &Arg, v: bool, verb: char) {
        match verb {
            't' | 'v' => self.pad_string(if v { "true" } else { "false" }),
            _ => self.bad_verb(arg, verb),
        }
    }

    // go-std: fmt.pp.fmtInteger L1 (verbs q, U not supported: bad verb)
    fn fmt_integer(&mut self, arg: &Arg, v: u64, is_signed: bool, verb: char) {
        match verb {
            'v' => {
                if self.f.sharp_v && !is_signed {
                    self.fmt_0x64(v, true);
                } else {
                    self.fmt_integer_base(v, 10, is_signed, verb, LDIGITS);
                }
            }
            'd' => self.fmt_integer_base(v, 10, is_signed, verb, LDIGITS),
            'b' => self.fmt_integer_base(v, 2, is_signed, verb, LDIGITS),
            'o' | 'O' => self.fmt_integer_base(v, 8, is_signed, verb, LDIGITS),
            'x' => self.fmt_integer_base(v, 16, is_signed, verb, LDIGITS),
            'X' => self.fmt_integer_base(v, 16, is_signed, verb, UDIGITS),
            'q' => {
                // fmt.fmtQc
                let r = if v > 0x10ffff {
                    '\u{fffd}'
                } else {
                    char::from_u32(v as u32).unwrap_or('\u{fffd}')
                };
                let q = quote_rune(r, self.f.plus);
                self.pad(&q);
            }
            'c' => {
                // fmt.fmtC
                let c = if v > u64::from(u32::MAX) {
                    '\u{fffd}'
                } else {
                    char::from_u32(v as u32).unwrap_or('\u{fffd}')
                };
                let mut b = [0u8; 4];
                let s = c.encode_utf8(&mut b).to_owned();
                self.pad(&s);
            }
            _ => self.bad_verb(arg, verb),
        }
    }

    // go-std: fmt.pp.fmt0x64 L1
    fn fmt_0x64(&mut self, v: u64, leading_0x: bool) {
        let sharp = self.f.sharp;
        self.f.sharp = leading_0x;
        self.fmt_integer_base(v, 16, false, 'v', LDIGITS);
        self.f.sharp = sharp;
    }

    // go-std: fmt.fmt.fmtInteger L1
    fn fmt_integer_base(
        &mut self,
        mut u: u64,
        base: u64,
        is_signed: bool,
        verb: char,
        digits: &[u8; 17],
    ) {
        let negative = is_signed && (u as i64) < 0;
        if negative {
            u = u.wrapping_neg();
        }
        let width = 68.max(3 + self.f.wid + self.f.prec) as usize;
        let mut buf = vec![0u8; width];

        // Two ways to ask for extra leading zero digits: %.3d or %03d.
        // If both are specified the f.zero flag is ignored and
        // padding with spaces is used instead.
        let mut prec: isize = 0;
        if self.f.prec_present {
            prec = self.f.prec;
            // Precision of 0 and value of 0 means "print nothing" but padding.
            if prec == 0 && u == 0 {
                let old_zero = self.f.zero;
                self.f.zero = false;
                self.write_padding(self.f.wid);
                self.f.zero = old_zero;
                return;
            }
        } else if self.f.zero && !self.f.minus && self.f.wid_present {
            // Zero padding is allowed only to the left.
            prec = self.f.wid;
            if negative || self.f.plus || self.f.space {
                prec -= 1; // leave room for sign
            }
        }

        let mut i = buf.len();
        while u >= base {
            i -= 1;
            buf[i] = digits[(u % base) as usize];
            u /= base;
        }
        i -= 1;
        buf[i] = digits[u as usize];
        while i > 0 && prec > (buf.len() - i) as isize {
            i -= 1;
            buf[i] = b'0';
        }

        // Various prefixes: 0x, -, etc.
        if self.f.sharp {
            match base {
                2 => {
                    i -= 1;
                    buf[i] = b'b';
                    i -= 1;
                    buf[i] = b'0';
                }
                8 => {
                    if buf[i] != b'0' {
                        i -= 1;
                        buf[i] = b'0';
                    }
                }
                16 => {
                    i -= 1;
                    buf[i] = digits[16];
                    i -= 1;
                    buf[i] = b'0';
                }
                _ => {}
            }
        }
        if verb == 'O' {
            i -= 1;
            buf[i] = b'o';
            i -= 1;
            buf[i] = b'0';
        }

        if negative {
            i -= 1;
            buf[i] = b'-';
        } else if self.f.plus {
            i -= 1;
            buf[i] = b'+';
        } else if self.f.space {
            i -= 1;
            buf[i] = b' ';
        }

        // Left padding with zeros has already been handled like precision earlier
        // or the f.zero flag is ignored due to an explicitly set precision.
        let old_zero = self.f.zero;
        self.f.zero = false;
        let s = String::from_utf8_lossy(&buf[i..]).into_owned();
        self.pad(&s);
        self.f.zero = old_zero;
    }

    // go-std: fmt.pp.fmtFloat L1
    fn fmt_float(&mut self, arg: &Arg, v: f64, size: u8, verb: char) {
        match verb {
            'v' => self.fmt_float_f(v, size, 'g', -1),
            'b' | 'g' | 'G' | 'x' | 'X' => self.fmt_float_f(v, size, verb, -1),
            'f' | 'e' | 'E' => self.fmt_float_f(v, size, verb, 6),
            'F' => self.fmt_float_f(v, size, 'f', 6),
            _ => self.bad_verb(arg, verb),
        }
    }

    /// `fmt.fmt.fmtFloat`: strconv formatting plus sign, `#` and padding handling.
    // go-std: fmt.fmt.fmtFloat L1
    fn fmt_float_f(&mut self, v: f64, size: u8, verb: char, prec: isize) {
        // Explicit precision in format specifier overrules default precision.
        let prec = if self.f.prec_present {
            self.f.prec
        } else {
            prec
        };
        // Format number, reserving space for leading + sign if needed.
        let mut num: Vec<u8> = vec![b'+'];
        num.extend(append_float(v, verb as u8, prec, size).bytes());
        if num[1] == b'-' || num[1] == b'+' {
            num.remove(0);
        } else {
            num[0] = b'+';
        }
        // f.space means to add a leading space instead of a "+" sign unless
        // the sign is explicitly asked for by f.plus.
        if self.f.space && num[0] == b'+' && !self.f.plus {
            num[0] = b' ';
        }
        // Special handling for infinities and NaN,
        // which don't look like a number so shouldn't be padded with zeros.
        if num[1] == b'I' || num[1] == b'N' {
            let old_zero = self.f.zero;
            self.f.zero = false;
            // Remove sign before NaN if not asked for.
            if num[1] == b'N' && !self.f.space && !self.f.plus {
                num.remove(0);
            }
            let n = String::from_utf8_lossy(&num).into_owned();
            self.pad(&n);
            self.f.zero = old_zero;
            return;
        }
        // The sharp flag forces printing a decimal point for non-binary formats
        // and retains trailing zeros, which we may need to restore.
        if self.f.sharp && verb != 'b' {
            let mut digits: isize = 0;
            if matches!(verb, 'v' | 'g' | 'G' | 'x') {
                digits = prec;
                // If no precision is set explicitly use a precision of 6.
                if digits == -1 {
                    digits = 6;
                }
            }
            // Buffer pre-allocated with enough room for
            // exponent notations of the form "e+123" or "p-1023".
            let mut tail: Vec<u8> = Vec::new();
            let mut has_decimal_point = false;
            let mut saw_nonzero_digit = false;
            // Starting from i = 1 to skip sign at num[0].
            let mut i = 1;
            while i < num.len() {
                match num[i] {
                    b'.' => has_decimal_point = true,
                    b'p' | b'P' => {
                        tail.extend_from_slice(&num[i..]);
                        num.truncate(i);
                    }
                    b'e' | b'E' if verb != 'x' && verb != 'X' => {
                        tail.extend_from_slice(&num[i..]);
                        num.truncate(i);
                    }
                    c => {
                        if c != b'0' {
                            saw_nonzero_digit = true;
                        }
                        // Count significant digits after the first non-zero digit.
                        if saw_nonzero_digit {
                            digits -= 1;
                        }
                    }
                }
                i += 1;
            }
            if !has_decimal_point {
                // Leading digit 0 should contribute once to digits.
                if num.len() == 2 && num[1] == b'0' {
                    digits -= 1;
                }
                num.push(b'.');
            }
            while digits > 0 {
                num.push(b'0');
                digits -= 1;
            }
            num.extend_from_slice(&tail);
        }
        // We want a sign if asked for and if the sign is not positive.
        if self.f.plus || num[0] != b'+' {
            // If we're zero padding to the left we want the sign before the leading zeros.
            // Achieve this by writing the sign out and then padding the unsigned number.
            // Zero padding is allowed only to the left.
            if self.f.zero && !self.f.minus && self.f.wid_present && self.f.wid > num.len() as isize
            {
                self.buf.push(char::from(num[0]));
                self.write_padding(self.f.wid - num.len() as isize);
                self.buf.push_str(&String::from_utf8_lossy(&num[1..]));
                return;
            }
            let n = String::from_utf8_lossy(&num).into_owned();
            self.pad(&n);
            return;
        }
        // No sign to show and the number is positive; just print the unsigned number.
        let n = String::from_utf8_lossy(&num[1..]).into_owned();
        self.pad(&n);
    }

    // go-std: fmt.pp.fmtString L1
    fn fmt_string(&mut self, arg: &Arg, v: &str, verb: char) {
        match verb {
            'v' => {
                if self.f.sharp_v {
                    self.fmt_q(v);
                } else {
                    self.fmt_s(v);
                }
            }
            's' => self.fmt_s(v),
            'x' => self.fmt_sbx(v.as_bytes(), LDIGITS),
            'X' => self.fmt_sbx(v.as_bytes(), UDIGITS),
            'q' => self.fmt_q(v),
            _ => self.bad_verb(arg, verb),
        }
    }

    // go-std: fmt.pp.fmtBytes L1 (typeString "[]byte")
    fn fmt_bytes(&mut self, _arg: &Arg, v: &[u8], verb: char) {
        match verb {
            'v' | 'd' => {
                if self.f.sharp_v {
                    self.buf.push_str("[]byte{");
                    for (i, c) in v.iter().enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.fmt_0x64(u64::from(*c), true);
                    }
                    self.buf.push('}');
                } else {
                    self.buf.push('[');
                    for (i, c) in v.iter().enumerate() {
                        if i > 0 {
                            self.buf.push(' ');
                        }
                        self.fmt_integer_base(u64::from(*c), 10, false, verb, LDIGITS);
                    }
                    self.buf.push(']');
                }
            }
            's' => {
                let s = String::from_utf8_lossy(v).into_owned();
                self.fmt_s(&s);
            }
            'x' => self.fmt_sbx(v, LDIGITS),
            'X' => self.fmt_sbx(v, UDIGITS),
            'q' => {
                let s = String::from_utf8_lossy(v).into_owned();
                self.fmt_q(&s);
            }
            _ => {
                // printValue of a []uint8 with another verb: element by element.
                if self.f.sharp_v {
                    self.buf.push_str("[]uint8{");
                } else {
                    self.buf.push('[');
                }
                for (i, c) in v.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(if self.f.sharp_v { ", " } else { " " });
                    }
                    let elem = Arg::Uint(u64::from(*c), "uint8");
                    self.fmt_integer(&elem, u64::from(*c), false, verb);
                }
                self.buf.push(if self.f.sharp_v { '}' } else { ']' });
            }
        }
    }

    // go-std: fmt.fmt.writePadding L1
    fn write_padding(&mut self, n: isize) {
        if n <= 0 {
            // No padding bytes needed.
            return;
        }
        // Zero padding is allowed only to the left.
        let pad_byte = if self.f.zero && !self.f.minus {
            '0'
        } else {
            ' '
        };
        for _ in 0..n {
            self.buf.push(pad_byte);
        }
    }

    // go-std: fmt.fmt.pad L1
    fn pad(&mut self, b: &str) {
        if !self.f.wid_present || self.f.wid == 0 {
            self.buf.push_str(b);
            return;
        }
        let width = self.f.wid - b.chars().count() as isize;
        if !self.f.minus {
            // left padding
            self.write_padding(width);
            self.buf.push_str(b);
        } else {
            // right padding
            self.buf.push_str(b);
            self.write_padding(width);
        }
    }

    // go-std: fmt.fmt.padString L1
    fn pad_string(&mut self, s: &str) {
        self.pad(s);
    }

    // go-std: fmt.fmt.truncateString L1
    fn truncate_string<'a>(&self, s: &'a str) -> &'a str {
        if self.f.prec_present {
            let mut n = self.f.prec;
            for (i, _) in s.char_indices() {
                n -= 1;
                if n < 0 {
                    return &s[..i];
                }
            }
        }
        s
    }

    // go-std: fmt.fmt.fmtS L1
    fn fmt_s(&mut self, s: &str) {
        let s = self.truncate_string(s).to_owned();
        self.pad_string(&s);
    }

    // go-std: fmt.fmt.fmtSbx L1
    fn fmt_sbx(&mut self, b: &[u8], digits: &[u8; 17]) {
        let mut length = b.len() as isize;
        // Set length to not process more bytes than the precision demands.
        if self.f.prec_present && self.f.prec < length {
            length = self.f.prec;
        }
        // Compute width of the encoding taking into account the f.sharp and f.space flag.
        let mut width = 2 * length;
        if width > 0 {
            if self.f.space {
                // Each element encoded by two hexadecimals will get a leading 0x or 0X.
                if self.f.sharp {
                    width *= 2;
                }
                // Elements will be separated by a space.
                width += length - 1;
            } else if self.f.sharp {
                // Only a leading 0x or 0X will be added for the whole string.
                width += 2;
            }
        } else {
            // The byte slice or string that should be encoded is empty.
            if self.f.wid_present {
                self.write_padding(self.f.wid);
            }
            return;
        }
        // Handle padding to the left.
        if self.f.wid_present && self.f.wid > width && !self.f.minus {
            self.write_padding(self.f.wid - width);
        }
        if self.f.sharp {
            // Add leading 0x or 0X.
            self.buf.push('0');
            self.buf.push(char::from(digits[16]));
        }
        for i in 0..length as usize {
            if self.f.space && i > 0 {
                // Separate elements with a space.
                self.buf.push(' ');
                if self.f.sharp {
                    // Add leading 0x or 0X for each element.
                    self.buf.push('0');
                    self.buf.push(char::from(digits[16]));
                }
            }
            let c = b[i];
            // Encode each byte as two hexadecimal digits.
            self.buf.push(char::from(digits[usize::from(c >> 4)]));
            self.buf.push(char::from(digits[usize::from(c & 0xF)]));
        }
        // Handle padding to the right.
        if self.f.wid_present && self.f.wid > width && self.f.minus {
            self.write_padding(self.f.wid - width);
        }
    }

    // go-std: fmt.fmt.fmtQ L1
    fn fmt_q(&mut self, s: &str) {
        let s = self.truncate_string(s).to_owned();
        if self.f.sharp && can_backquote(&s) {
            self.pad_string(&format!("`{s}`"));
            return;
        }
        let q = quote(&s, self.f.plus);
        self.pad(&q);
    }
}

// go-std: fmt.parseArgNumber L1
fn parse_arg_number(format: &[u8]) -> (isize, usize, bool) {
    // There must be at least 3 bytes: [n].
    if format.len() < 3 {
        return (0, 1, false);
    }
    // Find closing bracket.
    for i in 1..format.len() {
        if format[i] == b']' {
            let (width, ok, newi) = parsenum(format, 1, i);
            if !ok || newi != i {
                return (0, i + 1, false);
            }
            return (width - 1, i + 1, true); // arg numbers are one-indexed and skip paren.
        }
    }
    (0, 1, false)
}

// go-std: fmt.parsenum L1
fn parsenum(s: &[u8], start: usize, end: usize) -> (isize, bool, usize) {
    if start >= end {
        return (0, false, end);
    }
    let mut num: isize = 0;
    let mut isnum = false;
    let mut newi = start;
    while newi < end && s[newi].is_ascii_digit() {
        if too_large(num) {
            return (0, false, end); // Overflow; crazy long number most likely.
        }
        num = num * 10 + isize::from(s[newi] - b'0');
        isnum = true;
        newi += 1;
    }
    (num, isnum, newi)
}

// go-std: fmt.tooLarge L1
fn too_large(x: isize) -> bool {
    const MAX: isize = 1_000_000;
    !(-MAX..=MAX).contains(&x)
}

// go-std: fmt.intFromArg L1
fn int_from_arg(a: &[Arg], arg_num: usize) -> (isize, bool, usize) {
    let mut new_arg_num = arg_num;
    let (mut num, mut is_int) = (0isize, false);
    if arg_num < a.len() {
        match &a[arg_num] {
            Arg::Int(n, _) => {
                num = *n as isize;
                is_int = true;
            }
            Arg::Uint(n, _) if (*n as i64) >= 0 => {
                num = *n as isize;
                is_int = true;
            }
            _ => {}
        }
        new_arg_num = arg_num + 1;
        if too_large(num) {
            num = 0;
            is_int = false;
        }
    }
    (num, is_int, new_arg_num)
}

/// `strconv.QuoteRune` / `QuoteRuneToASCII` (`ascii`).
// go-std: strconv.appendQuotedRuneWith L2 (IsPrint approximated, see quote)
fn quote_rune(r: char, ascii: bool) -> String {
    let q = quote(&r.to_string(), ascii);
    let inner = &q[1..q.len() - 1];
    let inner = match inner {
        "\\\"" => "\"".to_owned(),
        "'" => "\\'".to_owned(),
        _ => inner.to_owned(),
    };
    format!("'{inner}'")
}

/// `strconv.CanBackquote`.
// go-std: strconv.CanBackquote L1
fn can_backquote(s: &str) -> bool {
    s.chars()
        .all(|c| c != '`' && c != '\u{feff}' && (c == '\t' || !(c < ' ' || c == '\u{7f}')))
}

/// `strconv.Quote` / `QuoteToASCII` (`ascii`).
// go-std: strconv.appendQuotedWith L2 (IsPrint approximated by !char::is_control, see module doc)
fn quote(s: &str, ascii: bool) -> String {
    let mut b = String::with_capacity(s.len() + 2);
    b.push('"');
    for r in s.chars() {
        if r == '"' || r == '\\' {
            b.push('\\');
            b.push(r);
            continue;
        }
        if ascii {
            if r < '\u{80}' && !r.is_control() {
                b.push(r);
                continue;
            }
        } else if !r.is_control() && r != '\u{ad}' {
            b.push(r);
            continue;
        }
        match r {
            '\u{7}' => b.push_str("\\a"),
            '\u{8}' => b.push_str("\\b"),
            '\u{c}' => b.push_str("\\f"),
            '\n' => b.push_str("\\n"),
            '\r' => b.push_str("\\r"),
            '\t' => b.push_str("\\t"),
            '\u{b}' => b.push_str("\\v"),
            _ => {
                let c = r as u32;
                if c < 0x20 || c == 0x7f {
                    let _ = write!(b, "\\x{c:02x}");
                } else if c < 0x10000 {
                    let _ = write!(b, "\\u{c:04x}");
                } else {
                    let _ = write!(b, "\\U{c:08x}");
                }
            }
        }
    }
    b.push('"');
    b
}

/// Decimal digits of `|v|` without trailing zeros and the decimal point position `dp`
/// (`v = 0.d1d2... × 10^dp`, Go's `decimalSlice`); `prec`: `None` for the shortest digits
/// that round-trip, else that many significant digits (correctly rounded).
// glue: digits from Rust's `{:e}` formatting (shortest: same digits as Go's Ryū)
fn float_digits(v: f64, size: u8, prec: Option<usize>) -> (String, i32) {
    if v == 0.0 {
        return (String::new(), 0);
    }
    let s = match prec {
        None if size == 32 => format!("{:e}", v.abs() as f32),
        None => format!("{:e}", v.abs()),
        Some(p) => format!("{:.*e}", p.saturating_sub(1), v.abs()),
    };
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0').to_owned();
    (digits, exp.parse::<i32>().unwrap_or(0) + 1)
}

/// `strconv.AppendFloat` / `FormatFloat(v, fmt, prec, size)` for fmt `b e E f g G x X`.
// go-std: strconv.genericFtoa L2 (decimal digits from Rust's float formatting, see float_digits)
fn append_float(v: f64, fmt: u8, prec: isize, size: u8) -> String {
    let (bits, mantbits, expbits, bias): (u64, u32, u32, i32) = if size == 32 {
        (u64::from((v as f32).to_bits()), 23, 8, -127)
    } else {
        (v.to_bits(), 52, 11, -1023)
    };
    let neg = (bits >> (expbits + mantbits)) != 0;
    let mut exp = ((bits >> mantbits) & ((1u64 << expbits) - 1)) as i32;
    let mut mant = bits & ((1u64 << mantbits) - 1);
    if exp == (1 << expbits) - 1 {
        // Inf, NaN
        return if mant != 0 {
            "NaN".into()
        } else if neg {
            "-Inf".into()
        } else {
            "+Inf".into()
        };
    } else if exp == 0 {
        // denormalized
        exp += 1;
    } else {
        // add implicit top bit
        mant |= 1u64 << mantbits;
    }
    exp += bias;

    // Pick off easy binary, hex formats.
    if fmt == b'b' {
        return fmt_b(neg, mant, exp, mantbits);
    }
    if fmt == b'x' || fmt == b'X' {
        return fmt_x(prec, fmt, neg, mant, exp, mantbits);
    }
    if fmt == b'f' && prec >= 0 {
        // Fixed precision: Rust's {:.*} is the exactly rounded value, as Go's bigFtoa.
        let s = format!("{:.*}", prec as usize, v.abs());
        return if neg { format!("-{s}") } else { s };
    }

    let shortest = prec < 0;
    let mut prec = prec;
    let (digs, dp) = if shortest {
        let (d, dp) = float_digits(v, size, None);
        let nd = d.len() as isize;
        let dpp = dp as isize;
        // Precision for shortest representation mode.
        match fmt {
            b'e' | b'E' => prec = (nd - 1).max(0),
            b'f' => prec = (nd - dpp).max(0),
            b'g' | b'G' => prec = nd,
            _ => {}
        }
        (d, dp)
    } else {
        // Fixed number of digits.
        let mut digits = prec;
        match fmt {
            b'e' | b'E' => digits += 1,
            b'g' | b'G' => {
                if prec == 0 {
                    prec = 1;
                }
                digits = prec;
            }
            _ => {}
        }
        float_digits(v, size, Some(digits.max(1) as usize))
    };
    format_digits(neg, &digs, dp as isize, shortest, prec, fmt)
}

/// `strconv.formatDigits`.
// go-std: strconv.formatDigits L1
fn format_digits(
    neg: bool,
    digs: &str,
    dp: isize,
    shortest: bool,
    mut prec: isize,
    fmt: u8,
) -> String {
    let nd = digs.len() as isize;
    match fmt {
        b'e' | b'E' => fmt_e(neg, digs, dp, prec, fmt),
        b'f' => fmt_f(neg, digs, dp, prec),
        _ => {
            // 'g' | 'G'
            let mut eprec = prec;
            if eprec > nd && nd >= dp {
                eprec = nd;
            }
            // %e is used if the exponent from the conversion
            // is less than -4 or greater than or equal to the precision.
            // if precision was the shortest possible, use precision 6 for this decision.
            if shortest {
                eprec = 6;
            }
            let exp = dp - 1;
            if exp < -4 || exp >= eprec {
                if prec > nd {
                    prec = nd;
                }
                return fmt_e(neg, digs, dp, prec - 1, fmt + b'e' - b'g');
            }
            if prec > dp {
                prec = nd;
            }
            fmt_f(neg, digs, dp, (prec - dp).max(0))
        }
    }
}

/// `%e`: -d.ddddde±dd
// go-std: strconv.fmtE L1
fn fmt_e(neg: bool, d: &str, dp: isize, prec: isize, fmt: u8) -> String {
    let d = d.as_bytes();
    let nd = d.len() as isize;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    // first digit
    out.push(if nd == 0 { '0' } else { char::from(d[0]) });
    // .moredigits
    if prec > 0 {
        out.push('.');
        let mut i = 1;
        let m = nd.min(prec + 1);
        if i < m {
            out.push_str(std::str::from_utf8(&d[i as usize..m as usize]).unwrap_or(""));
            i = m;
        }
        while i <= prec {
            out.push('0');
            i += 1;
        }
    }
    // e±
    out.push(char::from(fmt));
    let mut exp = dp - 1;
    if nd == 0 {
        // special case: 0 has exponent 0
        exp = 0;
    }
    if exp < 0 {
        out.push('-');
        exp = -exp;
    } else {
        out.push('+');
    }
    // dd or ddd
    if exp < 10 {
        let _ = write!(out, "0{exp}");
    } else {
        let _ = write!(out, "{exp}");
    }
    out
}

/// `%f`: -ddddddd.ddddd
// go-std: strconv.fmtF L1
fn fmt_f(neg: bool, d: &str, dp: isize, prec: isize) -> String {
    let d = d.as_bytes();
    let nd = d.len() as isize;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    // integer, padded with zeros as needed.
    if dp > 0 {
        let m = nd.min(dp);
        out.push_str(std::str::from_utf8(&d[..m as usize]).unwrap_or(""));
        for _ in m..dp {
            out.push('0');
        }
    } else {
        out.push('0');
    }
    // fraction
    if prec > 0 {
        out.push('.');
        for i in 1..=prec {
            let j = dp + i - 1;
            let ch = if 0 <= j && j < nd {
                char::from(d[j as usize])
            } else {
                '0'
            };
            out.push(ch);
        }
    }
    out
}

/// `%b`: -ddddddddp±ddd
// go-std: strconv.fmtB L1
fn fmt_b(neg: bool, mant: u64, exp: i32, mantbits: u32) -> String {
    let mut out = String::new();
    // sign
    if neg {
        out.push('-');
    }
    // mantissa
    let _ = write!(out, "{mant}");
    // p
    out.push('p');
    // ±exponent
    let exp = exp - mantbits as i32;
    if exp >= 0 {
        out.push('+');
    }
    let _ = write!(out, "{exp}");
    out
}

/// `%x`: -0x1.yyyyyyyyp±ddd or -0x0p+0. (y is hex digit, d is decimal digit)
// go-std: strconv.fmtX L1 [M-INT]
fn fmt_x(prec: isize, fmt: u8, neg: bool, mut mant: u64, mut exp: i32, mantbits: u32) -> String {
    if mant == 0 {
        exp = 0;
    }
    // Shift digits so leading 1 (if any) is at bit 1<<60.
    mant <<= 60 - mantbits;
    while mant != 0 && mant & (1 << 60) == 0 {
        mant <<= 1;
        exp -= 1;
    }
    // Round if requested.
    if (0..15).contains(&prec) {
        let shift = (prec * 4) as u32;
        let extra = (mant << shift) & ((1 << 60) - 1);
        mant >>= 60 - shift;
        if extra | (mant & 1) > 1 << 59 {
            mant += 1;
        }
        mant <<= 60 - shift;
        if mant & (1 << 61) != 0 {
            // Wrapped around.
            mant >>= 1;
            exp += 1;
        }
    }
    let hex = if fmt == b'X' { UDIGITS } else { LDIGITS };
    let mut out = String::new();
    // sign, 0x, leading digit
    if neg {
        out.push('-');
    }
    out.push('0');
    out.push(char::from(fmt));
    out.push(char::from(b'0' + ((mant >> 60) & 1) as u8));
    // .fraction
    mant <<= 4; // remove leading 0 or 1
    if prec < 0 && mant != 0 {
        out.push('.');
        while mant != 0 {
            out.push(char::from(hex[((mant >> 60) & 15) as usize]));
            mant <<= 4;
        }
    } else if prec > 0 {
        out.push('.');
        for _ in 0..prec {
            out.push(char::from(hex[((mant >> 60) & 15) as usize]));
            mant <<= 4;
        }
    }
    // p±
    out.push(if fmt.is_ascii_lowercase() { 'p' } else { 'P' });
    if exp < 0 {
        out.push('-');
        exp = -exp;
    } else {
        out.push('+');
    }
    // dd or ddd or dddd
    if exp < 100 {
        let _ = write!(out, "{exp:02}");
    } else {
        let _ = write!(out, "{exp}");
    }
    out
}

#[cfg(test)]
#[allow(clippy::useless_vec)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert_eq!(sprintf("a %s b %d", &go_args!["x", 5i64]), "a x b 5");
        assert_eq!(sprintf("%-.3s|", &go_args!["abcdef"]), "abc|");
        assert_eq!(sprintf("%d", &go_args!["?"]), "%!d(string=?)");
        assert_eq!(sprintf("%d %d", &go_args![1isize]), "1 %!d(MISSING)");
        assert_eq!(sprintf("%d", &go_args![1isize, "x"]), "1%!(EXTRA string=x)");
        assert_eq!(sprintf("%#v", &go_args!["a\"b"]), "\"a\\\"b\"");
        assert_eq!(sprintf("%T", &go_args![1u64]), "uint64");
        assert_eq!(sprintf("%v", &[Arg::Nil]), "<nil>");
        assert_eq!(sprint(&go_args![1isize, 2isize, "a", 3isize]), "1 2a3");
        assert_eq!(sprintf("%.*s", &go_args![2isize, "abc"]), "ab");
        assert_eq!(sprintf("%v", &go_args![1.5f64]), "1.5");
        assert_eq!(sprintf("%v", &go_args![1e21f64]), "1e+21");
        assert_eq!(sprintf("%v", &go_args![123456789.0f64]), "1.23456789e+08");
        assert_eq!(sprintf("%v", &go_args![0.0001f64]), "0.0001");
        assert_eq!(sprintf("%v", &go_args![0.00001f64]), "1e-05");
        assert_eq!(sprintf("%v", &go_args![123456.0f64]), "123456");
        assert_eq!(
            sprintf("%-.4s", &[Arg::Redact(Box::new(Arg::from("ab‹cdef")))]),
            "‹ab‹‹c›"
        );
    }
}
