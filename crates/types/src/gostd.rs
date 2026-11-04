//! Go standard-library behaviour that `mydecimal.go` depends on and that Rust's std does not
//! reproduce exactly (ON-DEMAND tool code, Go 1.25.12 as recorded in the govec vectors).
//!
//! go-file: $GOROOT/src/strings/strings.go
//! go-file: $GOROOT/src/unicode/utf8/utf8.go
//! go-file: $GOROOT/src/strconv/ftoa.go
//! go-file: $GOROOT/src/encoding/json/decode.go
//!
//! Only the logic is ported; the shortest round-trip digits of a float come from Rust's
//! formatter (both produce the shortest digit string that parses back to the same float and,
//! among those, the closest one), and JSON syntax comes from serde_json.

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;
use std::fmt;

/// `utf8.DecodeRune`: the rune at the start of `s` and its width. An empty or invalid
/// sequence is `(None, 1)` (Go: `RuneError, 1`; `RuneError, 0` for empty input).
// go-std: unicode/utf8.DecodeRune L2 (std::str::from_utf8 does the validation)
pub(crate) fn decode_rune(s: &[u8]) -> (Option<char>, usize) {
    if s.is_empty() {
        return (None, 0);
    }
    let want = match s[0] {
        0x00..=0x7f => return (Some(char::from(s[0])), 1),
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return (None, 1),
    };
    if s.len() >= want
        && let Ok(t) = std::str::from_utf8(&s[..want])
    {
        return (t.chars().next(), want);
    }
    (None, 1)
}

/// `utf8.DecodeLastRune`: the rune at the end of `s` and its width; `(None, 1)` if invalid.
// go-std: unicode/utf8.DecodeLastRune L2 (std::str::from_utf8 does the validation)
fn decode_last_rune(s: &[u8]) -> (Option<char>, usize) {
    let end = s.len();
    if end == 0 {
        return (None, 0);
    }
    for w in 1..=4.min(end) {
        if let Ok(t) = std::str::from_utf8(&s[end - w..]) {
            let mut it = t.chars();
            if let (Some(c), None) = (it.next(), it.next()) {
                return (Some(c), w);
            }
        }
    }
    (None, 1)
}

/// `unicode.IsSpace`. Go's set ('\t', '\n', '\v', '\f', '\r', ' ', U+0085, U+00A0 and the
/// Unicode White_Space property above Latin-1) is exactly Rust's `char::is_whitespace`.
// go-std: unicode.IsSpace L1
fn unicode_is_space(r: Option<char>) -> bool {
    r.is_some_and(char::is_whitespace)
}

/// `strings.TrimSpace` on bytes (Go converts `[]byte` to `string` without validation).
// go-std: strings.TrimSpace L1
pub(crate) fn trim_space(s: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < s.len() {
        let (r, w) = decode_rune(&s[start..]);
        if !unicode_is_space(r) {
            break;
        }
        start += w;
    }
    let mut stop = s.len();
    while stop > start {
        let (r, w) = decode_last_rune(&s[start..stop]);
        if !unicode_is_space(r) {
            break;
        }
        stop -= w;
    }
    &s[start..stop]
}

/// `strconv.FormatFloat(f, 'g', -1, 64)`.
// go-std: strconv.FormatFloat L2 (only fmt 'g', prec -1, bitSize 64; shortest digits from Rust's {:e})
pub fn format_float_g(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f.is_sign_negative() { "-Inf" } else { "+Inf" }.to_string();
    }
    let neg = f.is_sign_negative();
    // Shortest round-trip digits: "d.ddde±x".
    let e = format!("{:e}", f.abs());
    let (mant, exp) = e.split_once('e').expect("LowerExp output has an exponent");
    let exp: isize = exp.parse().expect("LowerExp exponent is an integer");
    let mut d: Vec<u8> = mant.bytes().filter(|b| *b != b'.').collect();
    let mut dp = exp + 1;
    if f == 0.0 {
        // Go's ryuFtoaShortest: zero has nd = 0, dp = 0.
        d.clear();
        dp = 0;
    }
    ryu_tie_to_even(f.abs(), &mut d, exp);
    let digs = DecimalSlice {
        nd: d.len() as isize,
        d,
        dp,
    };
    let mut dst = Vec::with_capacity(24);
    // FormatFloat(.., 'g', -1, ..): shortest, prec = digs.nd.
    let prec = digs.nd;
    format_digits(&mut dst, true, neg, &digs, prec, b'g');
    String::from_utf8(dst).expect("formatted float is ASCII")
}

/// Go's `ryuDigits32` rounds the last shortest digit half-to-even when the float lies exactly
/// halfway between the two shortest candidates (`cNextDigit == 5 && c0 && central&1 == 1`);
/// Rust's shortest formatter rounds such a tie up (e.g. 979980073090831.25: Go
/// "9.799800730908312e+14", Rust "9.799800730908313e+14").
///
/// A tie needs the exact binary value to end in a 5 right after the shortest digits, which
/// is only possible when those digits are about as many as the 53-bit mantissa holds (the
/// two neighbours 10^k apart must both be within half an ulp), so only results with at
/// least 15 digits are checked, against the exact expansion of the float.
// go-std: strconv.ryuDigits32 L2 (only its round-half-even rule, applied to Rust's shortest digits)
fn ryu_tie_to_even(f: f64, d: &mut Vec<u8>, exp: isize) {
    let nd = d.len();
    if nd < 15 {
        return;
    }
    // An f64 has at most 767 significant decimal digits; 800 makes the expansion exact.
    let exact = format!("{f:.800e}");
    let (mant, e) = exact
        .split_once('e')
        .expect("LowerExp output has an exponent");
    if e.parse::<isize>().expect("LowerExp exponent is an integer") != exp {
        return;
    }
    let ex: Vec<u8> = mant.bytes().filter(|b| *b != b'.').collect();
    if ex[nd] != b'5' || ex[nd + 1..].iter().any(|b| *b != b'0') {
        return;
    }
    // Exact tie between ex[..nd] (round down) and ex[..nd] + 1 (round up): keep the even one.
    let mut down = ex[..nd].to_vec();
    if (down[nd - 1] - b'0').is_multiple_of(2) {
        *d = down;
        return;
    }
    // Round up: increment the decimal digit string. All nines would carry into a new
    // leading digit; Rust then rounds up too and its exponent differs (returned above).
    if down.iter().all(|b| *b == b'9') {
        return;
    }
    let mut i = nd - 1;
    loop {
        if down[i] == b'9' {
            down[i] = b'0';
            i -= 1;
        } else {
            down[i] += 1;
            break;
        }
    }
    while down.last() == Some(&b'0') {
        down.pop();
    }
    *d = down;
}

struct DecimalSlice {
    d: Vec<u8>,
    nd: isize,
    dp: isize,
}

// go-std: strconv.formatDigits L1
fn format_digits(
    dst: &mut Vec<u8>,
    shortest: bool,
    neg: bool,
    digs: &DecimalSlice,
    mut prec: isize,
    fmt: u8,
) {
    match fmt {
        b'e' | b'E' => fmt_e(dst, neg, digs, prec, fmt),
        b'f' => fmt_f(dst, neg, digs, prec),
        b'g' | b'G' => {
            // trailing fractional zeros in 'e' form will be trimmed.
            let mut eprec = prec;
            if eprec > digs.nd && digs.nd >= digs.dp {
                eprec = digs.nd;
            }
            // %e is used if the exponent from the conversion
            // is less than -4 or greater than or equal to the precision.
            // if precision was the shortest possible, use precision 6 for this decision.
            if shortest {
                eprec = 6;
            }
            let exp = digs.dp - 1;
            if exp < -4 || exp >= eprec {
                if prec > digs.nd {
                    prec = digs.nd;
                }
                return fmt_e(dst, neg, digs, prec - 1, fmt + b'e' - b'g');
            }
            if prec > digs.dp {
                prec = digs.nd;
            }
            fmt_f(dst, neg, digs, (prec - digs.dp).max(0))
        }
        _ => {
            // unknown format
            dst.push(b'%');
            dst.push(fmt);
        }
    }
}

// go-std: strconv.fmtE L1
fn fmt_e(dst: &mut Vec<u8>, neg: bool, d: &DecimalSlice, prec: isize, fmt: u8) {
    // sign
    if neg {
        dst.push(b'-');
    }

    // first digit
    let mut ch = b'0';
    if d.nd != 0 {
        ch = d.d[0];
    }
    dst.push(ch);

    // .moredigits
    if prec > 0 {
        dst.push(b'.');
        let mut i = 1;
        let m = d.nd.min(prec + 1);
        if i < m {
            dst.extend_from_slice(&d.d[i as usize..m as usize]);
            i = m;
        }
        while i <= prec {
            dst.push(b'0');
            i += 1;
        }
    }

    // e±
    dst.push(fmt);
    let mut exp = d.dp - 1;
    if d.nd == 0 {
        // special case: 0 has exponent 0
        exp = 0;
    }
    if exp < 0 {
        ch = b'-';
        exp = -exp;
    } else {
        ch = b'+';
    }
    dst.push(ch);

    // dd or ddd
    if exp < 10 {
        dst.extend_from_slice(&[b'0', exp as u8 + b'0']);
    } else if exp < 100 {
        dst.extend_from_slice(&[(exp / 10) as u8 + b'0', (exp % 10) as u8 + b'0']);
    } else {
        dst.extend_from_slice(&[
            (exp / 100) as u8 + b'0',
            ((exp / 10) as u8) % 10 + b'0',
            (exp % 10) as u8 + b'0',
        ]);
    }
}

// go-std: strconv.fmtF L1
fn fmt_f(dst: &mut Vec<u8>, neg: bool, d: &DecimalSlice, prec: isize) {
    // sign
    if neg {
        dst.push(b'-');
    }

    // integer, padded with zeros as needed.
    if d.dp > 0 {
        let mut m = d.nd.min(d.dp);
        dst.extend_from_slice(&d.d[..m as usize]);
        while m < d.dp {
            dst.push(b'0');
            m += 1;
        }
    } else {
        dst.push(b'0');
    }

    // fraction
    if prec > 0 {
        dst.push(b'.');
        for i in 0..prec {
            let mut ch = b'0';
            let j = d.dp + i;
            if 0 <= j && j < d.nd {
                ch = d.d[j as usize];
            }
            dst.push(ch);
        }
    }
}

/// `strconv.ParseFloat(s, 64)` for the decimal strings `MyDecimal.String` produces. Rust's
/// parser is correctly rounded like Go's; the error carries no detail since the only caller
/// maps every error to ErrOverflow.
// go-std: strconv.ParseFloat L2 (Rust's correctly rounded str::parse::<f64>; decimal syntax only)
pub(crate) fn parse_float(s: &str) -> Result<f64, ()> {
    s.parse::<f64>().map_err(|_| ())
}

/// The fields of Go's `jsonMyDecimal` as `encoding/json.Unmarshal` fills them.
#[derive(Default)]
pub(crate) struct JsonMyDecimal {
    pub(crate) digits_int: i8,
    pub(crate) digits_frac: i8,
    pub(crate) result_frac: i8,
    pub(crate) negative: bool,
    pub(crate) word_buf: [i32; 9],
}

/// `json.Unmarshal(data, &r)` with `r` a zero `jsonMyDecimal`.
///
/// Go semantics kept: a syntax error wins over type errors; `null` leaves a value unchanged;
/// keys match field names exactly or case-insensitively, last one wins, unknown keys are
/// ignored; integers must be integer literals that fit the field type; a short `WordBuf`
/// array zeroes the remaining elements and extra elements are ignored; a type mismatch is
/// remembered and reported after the whole input is decoded.
/// Not kept (documented in pilot-notes): Unicode case folding of keys beyond ASCII, the
/// literal `-0` (serde_json reports it as a float), invalid UTF-8 inside strings, and the
/// wording of Go's error messages.
// go-std: encoding/json.Unmarshal L2 (serde_json for syntax, Go's struct decoding rules for jsonMyDecimal)
pub(crate) fn json_unmarshal_my_decimal(data: &[u8]) -> Result<JsonMyDecimal, String> {
    let mut de = serde_json::Deserializer::from_slice(data);
    let decoded = TopLevel
        .deserialize(&mut de)
        .and_then(|r| de.end().map(|()| r))
        .map_err(|e| e.to_string())?;
    match decoded {
        Decoded::Ok(r) => Ok(r),
        Decoded::TypeError(msg) => Err(msg),
    }
}

enum Decoded {
    Ok(JsonMyDecimal),
    TypeError(String),
}

struct TopLevel;

impl TopLevel {
    // go-part: encoding/json.Unmarshal
    fn deserialize<'de, D: Deserializer<'de>>(self, d: D) -> Result<Decoded, D::Error> {
        d.deserialize_any(self)
    }
}

const TYPE_NAME: &str = "types.jsonMyDecimal";

impl<'de> Visitor<'de> for TopLevel {
    type Value = Decoded;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Decoded, E> {
        Ok(Decoded::Ok(JsonMyDecimal::default()))
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Decoded, E> {
        Ok(type_error("bool"))
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Decoded, E> {
        Ok(type_error("number"))
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Decoded, E> {
        Ok(type_error("number"))
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Decoded, E> {
        Ok(type_error("number"))
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Decoded, E> {
        Ok(type_error("string"))
    }
    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Decoded, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(type_error("array"))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Decoded, A::Error> {
        let mut r = JsonMyDecimal::default();
        let mut first_err: Option<String> = None;
        while let Some(key) = map.next_key::<String>()? {
            let v: Value = map.next_value()?;
            let field = [
                "DigitsInt",
                "DigitsFrac",
                "ResultFrac",
                "Negative",
                "WordBuf",
            ]
            .iter()
            .position(|f| *f == key)
            .or_else(|| {
                [
                    "DigitsInt",
                    "DigitsFrac",
                    "ResultFrac",
                    "Negative",
                    "WordBuf",
                ]
                .iter()
                .position(|f| f.eq_ignore_ascii_case(&key))
            });
            let res = match field {
                Some(0) => store_int8(&v, &mut r.digits_int),
                Some(1) => store_int8(&v, &mut r.digits_frac),
                Some(2) => store_int8(&v, &mut r.result_frac),
                Some(3) => match v {
                    Value::Null => Ok(()),
                    Value::Bool(b) => {
                        r.negative = b;
                        Ok(())
                    }
                    other => Err(kind(&other)),
                },
                Some(4) => match v {
                    Value::Null => Ok(()),
                    Value::Array(items) => {
                        let mut res = Ok(());
                        for (i, item) in items.iter().enumerate().take(9) {
                            if let Err(k) = store_int32(item, &mut r.word_buf[i]) {
                                res = res.and(Err(k));
                            }
                        }
                        for w in r.word_buf.iter_mut().skip(items.len()) {
                            *w = 0;
                        }
                        res
                    }
                    other => Err(kind(&other)),
                },
                _ => Ok(()),
            };
            if let Err(k) = res
                && first_err.is_none()
            {
                first_err = Some(format!(
                    "json: cannot unmarshal {k} into Go struct field {TYPE_NAME}.{key}"
                ));
            }
        }
        Ok(match first_err {
            Some(msg) => Decoded::TypeError(msg),
            None => Decoded::Ok(r),
        })
    }
}

fn type_error(kind: &str) -> Decoded {
    Decoded::TypeError(format!(
        "json: cannot unmarshal {kind} into Go value of type {TYPE_NAME}"
    ))
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn store_int8(v: &Value, dst: &mut i8) -> Result<(), &'static str> {
    match v {
        Value::Null => Ok(()),
        Value::Number(n) => match n.as_i64().and_then(|x| i8::try_from(x).ok()) {
            Some(x) => {
                *dst = x;
                Ok(())
            }
            None => Err("number"),
        },
        other => Err(kind(other)),
    }
}

fn store_int32(v: &Value, dst: &mut i32) -> Result<(), &'static str> {
    match v {
        Value::Null => Ok(()),
        Value::Number(n) => match n.as_i64().and_then(|x| i32::try_from(x).ok()) {
            Some(x) => {
                *dst = x;
                Ok(())
            }
            None => Err("number"),
        },
        other => Err(kind(other)),
    }
}
