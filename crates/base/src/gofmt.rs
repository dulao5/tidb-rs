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
//! * `%U`, float verbs `b e E f F g G x X` with Go's shortest digits (`ftoaryu.go`,
//!   ported in `gostd::ftoaryu`) and widths/precisions up to Go's limit of 1e6.
//!
//! **Go strings are byte strings**: a Go `string` argument is [`Arg::Str`] over bytes, and
//! the result is `Vec<u8>`. Invalid UTF-8 counts as one rune per byte for width and
//! precision, prints as-is with `%s`, and as `\xNN` with `%q`, exactly as in Go.

#![allow(clippy::needless_range_loop)]

use std::borrow::Cow;
use std::fmt::Write as _;

use crate::gostd;

/// One `interface{}` argument of a Go `fmt` call, with its dynamic Go type.
#[derive(Clone, Debug, PartialEq)]
pub enum Arg {
    /// `nil` interface.
    Nil,
    /// Go `string` (bytes, not necessarily UTF-8).
    Str(Cow<'static, [u8]>),
    /// Go `[]byte`.
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
    Stringer(Vec<u8>, &'static str),
    /// A pingcap/errors error (`errors.New`'s `*errors.fundamental`, `errors.Trace`'s
    /// `*errors.withStack`), whose `Format` method prints the message for `%v` and `%s`
    /// (ignoring width and precision), `%q` of it for `%q`, and nothing for other verbs.
    /// A tidb-rs [`crate::Error`] argument is this (Go errors are passed `Trace`d).
    Error(Vec<u8>),
    /// `errors.redactFormatter` (pingcap/errors, `RedactLogMarker`): formats the wrapped value
    /// with the same verb and flags, then encloses it in `‹›`, doubling `‹` and `›` inside.
    Redact(Box<Arg>),
    /// A struct value without methods that apply (e.g. a `types.MyDecimal` value, whose
    /// `String` has a pointer receiver), printed by reflection: its fields in order with the
    /// same verb and flags, `{a b c}`. The fields are unexported (no methods are called on
    /// them); `.1` is the Go type name.
    Struct(Vec<Arg>, &'static str),
    /// An array (not of bytes) printed by reflection, `[a b c]`; `.1` is the Go type name.
    Array(Vec<Arg>, &'static str),
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
            Arg::Struct(_, t) | Arg::Array(_, t) => Cow::Borrowed(t),
        }
    }

    /// `reflect.TypeOf(arg).Kind() == reflect.String` (for `Sprint`'s spacing).
    // glue: dynamic Go kind test
    fn is_string_kind(&self) -> bool {
        matches!(self, Arg::Str(_))
    }
}

impl Arg {
    /// A Go `string` argument from bytes. Bytes have no `From` conversion on purpose: a Go
    /// `string` is `Arg::go_string(b)`, a Go `[]byte` is `Arg::Bytes(b)`.
    // glue: Go string(bytes) argument
    pub fn go_string(b: impl Into<Cow<'static, [u8]>>) -> Arg {
        Arg::Str(b.into())
    }
}
impl From<&'static str> for Arg {
    // glue: Go string argument
    fn from(s: &'static str) -> Self {
        Arg::Str(Cow::Borrowed(s.as_bytes()))
    }
}
impl From<String> for Arg {
    // glue: Go string argument
    fn from(s: String) -> Self {
        Arg::Str(Cow::Owned(s.into_bytes()))
    }
}
impl From<&String> for Arg {
    // glue: Go string argument
    fn from(s: &String) -> Self {
        Arg::Str(Cow::Owned(s.clone().into_bytes()))
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

/// `fmt.buffer`: bytes, with the `String`-like appends the port uses.
#[derive(Default)]
struct Buf(Vec<u8>);

impl Buf {
    // glue: buffer.writeString
    fn push_str(&mut self, s: &str) {
        self.0.extend_from_slice(s.as_bytes());
    }
    // glue: buffer.write
    fn push_bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    // glue: buffer.writeRune
    fn push(&mut self, c: char) {
        let mut b = [0u8; 4];
        self.0.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
    }
}

impl std::fmt::Write for Buf {
    // glue: write! into the buffer
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.push_str(s);
        Ok(())
    }
}

/// `fmt.pp`.
struct Pp {
    buf: Buf,
    f: Flags,
    erroring: bool,
    good_arg_num: bool,
}

/// `fmt.Sprintf`.
// go-std: fmt.Sprintf L1
pub fn sprintf(format: impl AsRef<[u8]>, a: &[Arg]) -> Vec<u8> {
    let mut p = Pp::new();
    p.do_printf(format.as_ref(), a);
    p.buf.0
}

/// `fmt.Sprint`.
// go-std: fmt.Sprint L1
pub fn sprint(a: &[Arg]) -> Vec<u8> {
    let mut p = Pp::new();
    p.do_print(a);
    p.buf.0
}

impl Pp {
    // glue: newPrinter
    fn new() -> Pp {
        Pp {
            buf: Buf::default(),
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
    fn do_printf(&mut self, format: &[u8], a: &[Arg]) {
        let fb = format;
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
                self.buf.push_bytes(&format[lasti..i]);
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

            let (verb, size) = if fb[i] < 0x80 {
                (char::from(fb[i]), 1)
            } else {
                gostd::decode_rune(&fb[i..])
            };
            i += size;

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
        format: &[u8],
        i: usize,
        num_args: usize,
        reordered: &mut bool,
    ) -> (usize, usize, bool) {
        let fb = format;
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
                'T' | 'v' => self.pad("<nil>"),
                _ => self.bad_verb(arg, verb),
            }
            return;
        }
        // %T (the value's type) is special; we always do it first.
        if verb == 'T' {
            let t = arg.type_string();
            self.fmt_s(t.as_bytes());
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
            Arg::Struct(..) | Arg::Array(..) => self.print_value(arg, verb),
            Arg::Nil => unreachable!(),
        }
    }

    /// The struct and array cases of `printValue` for values of basic types (no field names:
    /// neither `%+v` nor `%#v` is used on such values by the ported callers).
    // go-std: fmt.pp.printValue L2 (struct and array of basic values; no %+v/%#v field names)
    fn print_value(&mut self, arg: &Arg, verb: char) {
        let (open, close, items) = match arg {
            Arg::Struct(f, _) => ('{', '}', f),
            Arg::Array(e, _) => ('[', ']', e),
            _ => unreachable!(),
        };
        self.buf.push(open);
        for (i, it) in items.iter().enumerate() {
            if i > 0 {
                self.buf.push(' ');
            }
            self.print_arg(it, verb);
        }
        self.buf.push(close);
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
                    'v' | 's' => self.buf.push_bytes(msg),
                    'q' => {
                        let q = sprintf("%q", &[Arg::go_string(msg.clone())]);
                        self.buf.push_bytes(&q);
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
    // go-dep: github.com/pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee.redactFormatter.Format L1
    fn redact_format(&mut self, arg: &Arg, verb: char) {
        let origin = sprintf(self.format_string(verb), std::slice::from_ref(arg));
        self.buf.push('‹');
        // `for _, c := range origin`: an invalid byte is U+FFFD.
        for c in gostd::runes(&origin) {
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
        // for _, c := range " +-#0" { if state.Flag(int(c)) ... } (Flag reports sharpV / plusV
        // as '#' / '+').
        if self.f.space {
            s.push(' ');
        }
        if self.f.plus || self.f.plus_v {
            s.push('+');
        }
        if self.f.minus {
            s.push('-');
        }
        if self.f.sharp || self.f.sharp_v {
            s.push('#');
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
            't' | 'v' => self.pad(if v { "true" } else { "false" }),
            _ => self.bad_verb(arg, verb),
        }
    }

    // go-std: fmt.pp.fmtInteger L1
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
                // fmt.fmtC (utf8.AppendRune writes RuneError for an invalid rune)
                let c = if v > 0x10ffff {
                    '\u{fffd}'
                } else {
                    char::from_u32(v as u32).unwrap_or('\u{fffd}')
                };
                let mut b = [0u8; 4];
                let s = c.encode_utf8(&mut b).to_owned();
                self.pad(&s);
            }
            'U' => self.fmt_unicode(v),
            _ => self.bad_verb(arg, verb),
        }
    }

    /// `fmtUnicode` formats a uint64 as "U+0078" or with f.sharp set as "U+0078 'x'".
    // go-std: fmt.fmt.fmtUnicode L1
    fn fmt_unicode(&mut self, mut u: u64) {
        let mut prec: isize = 4;
        // If the precision is set explicitly use that.
        let mut width = 68usize;
        if self.f.prec_present && self.f.prec > 4 {
            prec = self.f.prec;
            // Compute space needed for "U+" , number, " '", character, "'".
            width = width.max((2 + prec + 2 + 4 + 1) as usize);
        }
        let mut buf = vec![0u8; width];
        // Format into buf, ending at buf[i]. Formatting numbers is easier right-to-left.
        let mut i = buf.len();
        // For %#U we want to add a space and a quoted character at the end of the buffer.
        if self.f.sharp && u <= 0x10ffff && char::from_u32(u as u32).is_some_and(gostd::is_print) {
            let c = char::from_u32(u as u32).unwrap_or('\u{fffd}');
            i -= 1;
            buf[i] = b'\'';
            let mut e = [0u8; 4];
            let enc = c.encode_utf8(&mut e).as_bytes();
            i -= enc.len();
            buf[i..i + enc.len()].copy_from_slice(enc);
            i -= 1;
            buf[i] = b'\'';
            i -= 1;
            buf[i] = b' ';
        }
        // Format the Unicode code point u as a hexadecimal number.
        while u >= 16 {
            i -= 1;
            buf[i] = UDIGITS[(u & 0xF) as usize];
            prec -= 1;
            u >>= 4;
        }
        i -= 1;
        buf[i] = UDIGITS[u as usize];
        prec -= 1;
        // Add zeros in front of the number until requested precision is reached.
        while prec > 0 {
            i -= 1;
            buf[i] = b'0';
            prec -= 1;
        }
        // Add a leading "U+".
        i -= 1;
        buf[i] = b'+';
        i -= 1;
        buf[i] = b'U';
        let old_zero = self.f.zero;
        self.f.zero = false;
        let b = buf[i..].to_vec();
        self.pad(&b);
        self.f.zero = old_zero;
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
        let b = buf[i..].to_vec();
        self.pad(&b);
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
            self.pad(&num);
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
                self.buf.push_bytes(&num[1..]);
                return;
            }
            self.pad(&num);
            return;
        }
        // No sign to show and the number is positive; just print the unsigned number.
        self.pad(&num[1..]);
    }

    // go-std: fmt.pp.fmtString L1
    fn fmt_string(&mut self, arg: &Arg, v: &[u8], verb: char) {
        match verb {
            'v' => {
                if self.f.sharp_v {
                    self.fmt_q(v);
                } else {
                    self.fmt_s(v);
                }
            }
            's' => self.fmt_s(v),
            'x' => self.fmt_sbx(v, LDIGITS),
            'X' => self.fmt_sbx(v, UDIGITS),
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
                self.fmt_s(v);
            }
            'x' => self.fmt_sbx(v, LDIGITS),
            'X' => self.fmt_sbx(v, UDIGITS),
            'q' => {
                self.fmt_q(v);
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
    fn pad(&mut self, b: impl AsRef<[u8]>) {
        let b = b.as_ref();
        if !self.f.wid_present || self.f.wid == 0 {
            self.buf.push_bytes(b);
            return;
        }
        let width = self.f.wid - gostd::rune_count(b) as isize;
        if !self.f.minus {
            // left padding
            self.write_padding(width);
            self.buf.push_bytes(b);
        } else {
            // right padding
            self.buf.push_bytes(b);
            self.write_padding(width);
        }
    }

    // go-std: fmt.fmt.truncateString L1
    fn truncate_string<'a>(&self, s: &'a [u8]) -> &'a [u8] {
        if self.f.prec_present {
            let mut n = self.f.prec;
            let mut i = 0;
            // for i := range s (rune starts; an invalid byte is one rune)
            while i < s.len() {
                n -= 1;
                if n < 0 {
                    return &s[..i];
                }
                i += gostd::decode_rune(&s[i..]).1;
            }
        }
        s
    }

    // go-std: fmt.fmt.fmtS L1
    fn fmt_s(&mut self, s: &[u8]) {
        let s = self.truncate_string(s).to_vec();
        self.pad(&s);
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
    fn fmt_q(&mut self, s: &[u8]) {
        let s = self.truncate_string(s).to_vec();
        if self.f.sharp && can_backquote(&s) {
            let mut q = Vec::with_capacity(s.len() + 2);
            q.push(b'`');
            q.extend_from_slice(&s);
            q.push(b'`');
            self.pad(&q);
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
// go-std: strconv.appendQuotedRuneWith L1
fn quote_rune(r: char, ascii: bool) -> Vec<u8> {
    // utf8.ValidRune is guaranteed by char.
    let mut buf = vec![b'\''];
    append_escaped_rune(&mut buf, r, b'\'', ascii);
    buf.push(b'\'');
    buf
}

/// `strconv.CanBackquote`.
// go-std: strconv.CanBackquote L1
fn can_backquote(mut s: &[u8]) -> bool {
    while !s.is_empty() {
        let (r, wid) = gostd::decode_rune(s);
        s = &s[wid..];
        if wid > 1 {
            if r == '\u{feff}' {
                return false; // BOMs are invisible and should not be quoted.
            }
            continue; // All other multibyte runes are correctly encoded and assumed printable.
        }
        if r == '\u{fffd}' {
            return false;
        }
        if (r < ' ' && r != '\t') || r == '`' || r == '\u{7f}' {
            return false;
        }
    }
    true
}

/// Go `strconv.Quote(string(s))`.
// go-std: strconv.Quote L1
pub fn strconv_quote(s: &[u8]) -> String {
    // quote escapes everything that is not printable UTF-8: the output is ASCII or valid UTF-8.
    String::from_utf8(quote(s, false)).expect("strconv.Quote output is valid UTF-8")
}

/// `strconv.Quote` / `QuoteToASCII` (`ascii`).
// go-std: strconv.appendQuotedWith L1
fn quote(mut s: &[u8], ascii: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(3 * s.len() / 2 + 2);
    buf.push(b'"');
    while !s.is_empty() {
        let (r, width) = if s[0] < 0x80 {
            (char::from(s[0]), 1)
        } else {
            gostd::decode_rune(s)
        };
        if width == 1 && r == '\u{fffd}' {
            buf.extend_from_slice(b"\\x");
            buf.push(LDIGITS[usize::from(s[0] >> 4)]);
            buf.push(LDIGITS[usize::from(s[0] & 0xF)]);
            s = &s[width..];
            continue;
        }
        append_escaped_rune(&mut buf, r, b'"', ascii);
        s = &s[width..];
    }
    buf.push(b'"');
    buf
}

// go-std: strconv.appendEscapedRune L1 (graphicOnly false)
fn append_escaped_rune(buf: &mut Vec<u8>, r: char, quote: u8, ascii: bool) {
    let mut e = [0u8; 4];
    if r == char::from(quote) || r == '\\' {
        // always backslashed
        buf.push(b'\\');
        buf.extend_from_slice(r.encode_utf8(&mut e).as_bytes());
        return;
    }
    if ascii {
        if (r as u32) < 0x80 && gostd::is_print(r) {
            buf.push(r as u8);
            return;
        }
    } else if gostd::is_print(r) {
        buf.extend_from_slice(r.encode_utf8(&mut e).as_bytes());
        return;
    }
    match r {
        '\u{7}' => buf.extend_from_slice(b"\\a"),
        '\u{8}' => buf.extend_from_slice(b"\\b"),
        '\u{c}' => buf.extend_from_slice(b"\\f"),
        '\n' => buf.extend_from_slice(b"\\n"),
        '\r' => buf.extend_from_slice(b"\\r"),
        '\t' => buf.extend_from_slice(b"\\t"),
        '\u{b}' => buf.extend_from_slice(b"\\v"),
        _ => {
            let c = r as u32;
            if c < u32::from(b' ') || c == 0x7f {
                buf.extend_from_slice(b"\\x");
                buf.push(LDIGITS[(c >> 4) as usize & 0xF]);
                buf.push(LDIGITS[c as usize & 0xF]);
            } else if c < 0x10000 {
                buf.extend_from_slice(b"\\u");
                for sh in (0..16).step_by(4).rev() {
                    buf.push(LDIGITS[(c >> sh) as usize & 0xF]);
                }
            } else {
                buf.extend_from_slice(b"\\U");
                for sh in (0..32).step_by(4).rev() {
                    buf.push(LDIGITS[(c >> sh) as usize & 0xF]);
                }
            }
        }
    }
}

/// `n` correctly rounded significant decimal digits of `|v|` (Go: `ryuFtoaFixed*` /
/// `bigFtoa`) without trailing zeros, and the decimal point position `dp`
/// (`v = 0.d1d2... × 10^dp`, Go's `decimalSlice`). Rust's `{:.*e}` prints the exactly
/// rounded value (ties to even, as Go's `decimal.Round`); a double has at most 767
/// significant digits, so more than 800 requested digits need no more than 800 computed
/// (Rust's formatting also limits precision to 65535).
// glue: fixed-precision digits from Rust's float formatting (Go's are exact too)
fn fixed_digits(v: f64, n: usize) -> (String, isize) {
    if v == 0.0 {
        return (String::new(), 0);
    }
    let n = n.clamp(1, 800);
    let s = format!("{:.*e}", n - 1, v.abs());
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0').to_owned();
    (digits, exp.parse::<isize>().unwrap_or(0) + 1)
}

/// `strconv.AppendFloat` / `FormatFloat(v, fmt, prec, size)` for fmt `b e E f g G x X`.
// go-std: strconv.genericFtoa L1 (fixed-precision digits: see fixed_digits)
pub(crate) fn append_float(v: f64, fmt: u8, prec: isize, size: u8) -> String {
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
        // A double has at most 1074 fractional digits; beyond them the digits are zeros
        // (and Rust's formatting limits precision to 65535).
        let p = (prec as usize).min(1100);
        let mut s = format!("{:.*}", p, v.abs());
        s.extend(std::iter::repeat_n('0', prec as usize - p));
        return if neg { format!("-{s}") } else { s };
    }

    let shortest = prec < 0;
    let mut prec = prec;
    let (digs, dp) = if shortest {
        // Use Ryu algorithm.
        let mut dd = gostd::ftoaryu::DecimalSlice::new();
        let flt = if size == 32 {
            &gostd::ftoaryu::FLOAT32INFO
        } else {
            &gostd::ftoaryu::FLOAT64INFO
        };
        gostd::ftoaryu::ryu_ftoa_shortest(&mut dd, mant, exp - mantbits as i32, flt);
        let d = String::from_utf8_lossy(dd.digits()).into_owned();
        let dp = dd.dp;
        let nd = dd.nd;
        let dpp = dp;
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
        fixed_digits(v, digits.max(1) as usize)
    };
    format_digits(neg, &digs, dp, shortest, prec, fmt)
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

    fn sf(f: &str, a: &[Arg]) -> String {
        String::from_utf8(sprintf(f, a)).unwrap()
    }
    fn sp(a: &[Arg]) -> String {
        String::from_utf8(sprint(a)).unwrap()
    }

    #[test]
    #[allow(clippy::excessive_precision)] // exact binary values written out
    fn basics() {
        assert_eq!(sf("a %s b %d", &go_args!["x", 5i64]), "a x b 5");
        assert_eq!(sf("%-.3s|", &go_args!["abcdef"]), "abc|");
        assert_eq!(sf("%d", &go_args!["?"]), "%!d(string=?)");
        assert_eq!(sf("%d %d", &go_args![1isize]), "1 %!d(MISSING)");
        assert_eq!(sf("%d", &go_args![1isize, "x"]), "1%!(EXTRA string=x)");
        assert_eq!(sf("%#v", &go_args!["a\"b"]), "\"a\\\"b\"");
        assert_eq!(sf("%T", &go_args![1u64]), "uint64");
        assert_eq!(sf("%v", &[Arg::Nil]), "<nil>");
        assert_eq!(sp(&go_args![1isize, 2isize, "a", 3isize]), "1 2a3");
        assert_eq!(sf("%.*s", &go_args![2isize, "abc"]), "ab");
        assert_eq!(sf("%v", &go_args![1.5f64]), "1.5");
        assert_eq!(sf("%v", &go_args![1e21f64]), "1e+21");
        assert_eq!(sf("%v", &go_args![123456789.0f64]), "1.23456789e+08");
        assert_eq!(sf("%v", &go_args![0.0001f64]), "0.0001");
        assert_eq!(sf("%v", &go_args![0.00001f64]), "1e-05");
        assert_eq!(sf("%v", &go_args![123456.0f64]), "123456");
        assert_eq!(
            sf("%-.4s", &[Arg::Redact(Box::new(Arg::from("ab‹cdef")))]),
            "‹ab‹‹c›"
        );
        // Go strings are bytes; %x of an invalid byte, %q escapes it, precision counts it.
        assert_eq!(
            sprintf(
                "%s|%q|%.2s",
                &[
                    Arg::go_string(&b"a\xffb"[..]),
                    Arg::go_string(&b"ab\xdd$"[..]),
                    Arg::go_string(&b"\xff\xfeZ"[..])
                ]
            ),
            b"a\xffb|\"ab\\xdd$\"|\xff\xfe".to_vec()
        );
        // Go's shortest digits round ties to even.
        assert_eq!(sf("%v", &[Arg::from(-367872.625f32)]), "-367872.62");
        assert_eq!(
            sf("%v", &[Arg::from(-1154740326697035.25f64)]),
            "-1.1547403266970352e+15"
        );
        assert_eq!(
            sf("%U|%#U", &go_args![0x13i64, 0x78i64]),
            "U+0013|U+0078 'x'"
        );
        assert_eq!(sf("%*d", &go_args![70000isize, 1isize]).len(), 70000);
    }
}
