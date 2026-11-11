//! go-file: pkg/util/stringutil/string_util.go
//!
//! D00.3 utility file, ported whole when first needed (constitution 4.4) and placed in the
//! lowest crate that uses it (N0 §3): `pkg/util/collate` matches `LIKE` patterns with it.
//! Go strings are byte strings here (`&[u8]`, mapping rule M-ERR); `[]rune(s)` decodes
//! invalid bytes as U+FFFD, like Go.

#![allow(non_upper_case_globals)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;

use tidb_base::gostd::{decode_rune, rune_count, runes};
use tidb_base::mysql::{ModeANSIQuotes, SQLMode};
use tidb_base::{Error, errors};

/// ErrSyntax indicates that a value does not have the right syntax for the target type.
pub static ErrSyntax: LazyLock<Error> = LazyLock::new(|| errors::new("invalid syntax"));

/// `[]rune(s)`: like Go, one allocation of the exact rune count (an ASCII fast path
/// counts nothing).
// glue: Go []rune(string) conversion
fn to_runes(s: &[u8]) -> Vec<char> {
    if s.is_ascii() {
        return s.iter().map(|&b| char::from(b)).collect();
    }
    let mut v = Vec::with_capacity(rune_count(s));
    v.extend(runes(s));
    v
}

/// UnquoteChar decodes the first character or byte in the escaped string
/// or character literal represented by the string s.
/// It returns four values:
///
/// 1) value, the decoded Unicode code point or byte value;
/// 2) multibyte, a boolean indicating whether the decoded character requires a multibyte UTF-8 representation;
/// 3) tail, the remainder of the string after the character; and
/// 4) an error that will be nil if the character is syntactically valid.
///
/// The second argument, quote, specifies the type of literal being parsed
/// and therefore which escaped quote character is permitted.
/// If set to a single quote, it permits the sequence \' and disallows unescaped '.
/// If set to a double quote, it permits \" and disallows unescaped ".
/// If set to zero, it does not permit either escape and allows both quote characters to appear unescaped.
/// Different with strconv.UnquoteChar, it permits unnecessary backslash.
///
/// Like Go, `s` must not be empty (Go indexes `s[0]`).
// go: UnquoteChar L1 [M-ERR]
pub fn unquote_char(s: &[u8], quote: u8) -> Result<(Vec<u8>, &[u8]), Error> {
    let mut value = Vec::new();
    // easy cases
    let c = s[0];
    if c == quote {
        return Err(ErrSyntax.clone());
    } else if c >= 0x80 {
        let (r, size) = decode_rune(s);
        if r == char::REPLACEMENT_CHARACTER {
            value.push(c);
            return Ok((value, &s[1..]));
        }
        let mut b = [0u8; 4];
        value.extend_from_slice(r.encode_utf8(&mut b).as_bytes());
        return Ok((value, &s[size..]));
    } else if c != b'\\' {
        value.push(c);
        return Ok((value, &s[1..]));
    }
    // hard case: c is backslash
    if s.len() <= 1 {
        return Err(ErrSyntax.clone());
    }
    let c = s[1];
    let s = &s[2..];
    match c {
        b'b' => value.push(b'\x08'),
        b'n' => value.push(b'\n'),
        b'r' => value.push(b'\r'),
        b't' => value.push(b'\t'),
        b'Z' => value.push(b'\x1a'),
        b'0' => value.push(b'\0'),
        b'_' | b'%' => {
            value.push(b'\\');
            value.push(c);
        }
        b'\\' => value.push(b'\\'),
        b'\'' | b'"' => value.push(c),
        _ => value.push(c),
    }
    Ok((value, s))
}

/// Unquote interprets s as a single-quoted, double-quoted,
/// or backquoted Go string literal, returning the string value
/// that s quotes. For example: test=`"\"\n"` (hex: 22 5c 22 5c 6e 22)
/// should be converted to `"\n` (hex: 22 0a).
// go: Unquote L1 [M-ERR]
pub fn unquote(s: &[u8]) -> Result<Vec<u8>, Error> {
    let n = s.len();
    if n < 2 {
        return Err(ErrSyntax.clone());
    }
    let quote = s[0];
    if quote != s[n - 1] {
        return Err(ErrSyntax.clone());
    }
    let mut s = &s[1..n - 1];
    if quote != b'"' && quote != b'\'' {
        return Err(ErrSyntax.clone());
    }
    // Avoid allocation. No need to convert if there is no '\'
    if !s.contains(&b'\\') && !s.contains(&quote) {
        return Ok(s.to_vec());
    }
    let mut buf = Vec::with_capacity(3 * s.len() / 2); // Try to avoid more allocations.
    while !s.is_empty() {
        let (mb, ss) = unquote_char(s, quote)?;
        s = ss;
        buf.extend_from_slice(&mb);
    }
    Ok(buf)
}

/// PatMatch is the enumeration value for per-character match.
pub const PatMatch: u8 = 1;
/// PatOne is the enumeration value for '_' match.
pub const PatOne: u8 = 2;
/// PatAny is the enumeration value for '%' match.
pub const PatAny: u8 = 3;

/// CompilePatternBinary is used for binary strings.
// go: CompilePatternBinary L1
pub fn compile_pattern_binary(pattern: &[u8], escape: u8) -> (Vec<u8>, Vec<u8>) {
    compile_pattern_inner_binary(pattern, escape)
}

/// CompilePattern is an adapter for `CompilePatternInner`, `pattern` can be any unicode string.
// go: CompilePattern L1
pub fn compile_pattern(pattern: &[u8], escape: u8) -> (Vec<char>, Vec<u8>) {
    compile_pattern_inner(pattern, escape)
}

/// CompilePatternInner handles escapes and wild cards convert pattern characters and
/// pattern types.
/// Note: if anything changes in this method, please double-check CompilePatternInnerBytes
// go: CompilePatternInner L1
pub fn compile_pattern_inner(pattern: &[u8], escape: u8) -> (Vec<char>, Vec<u8>) {
    let (mut pat_weights, mut pat_types) = (Vec::new(), Vec::new());
    compile_pattern_inner_into(pattern, escape, &mut pat_weights, &mut pat_types);
    (pat_weights, pat_types)
}

/// [`compile_pattern_inner`] into existing buffers (a pattern compiled again for each row
/// reuses its allocation; Go allocates new slices each time).
// glue: buffer-reusing form of CompilePatternInner (L2, allocation only)
pub fn compile_pattern_inner_into(
    pattern: &[u8],
    escape: u8,
    pat_weights: &mut Vec<char>,
    pat_types: &mut Vec<u8>,
) {
    let runes = to_runes(pattern);
    let escape_rune = char::from(escape);
    let len_runes = runes.len();
    pat_weights.clear();
    pat_weights.resize(len_runes, '\0');
    pat_types.clear();
    pat_types.resize(len_runes, 0u8);
    let mut pat_len = 0;
    let mut i = 0;
    while i < len_runes {
        let tp;
        let mut r = runes[i];
        if r == escape_rune {
            tp = PatMatch;
            if i < len_runes - 1 {
                i += 1;
                r = runes[i];
            }
        } else if r == '_' {
            // %_ => _%
            if pat_len > 0 && pat_types[pat_len - 1] == PatAny {
                tp = PatAny;
                r = '%';
                pat_weights[pat_len - 1] = '_';
                pat_types[pat_len - 1] = PatOne;
            } else {
                tp = PatOne;
            }
        } else if r == '%' {
            // %% => %
            if pat_len > 0 && pat_types[pat_len - 1] == PatAny {
                i += 1;
                continue;
            }
            tp = PatAny;
        } else {
            tp = PatMatch;
        }
        pat_weights[pat_len] = r;
        pat_types[pat_len] = tp;
        pat_len += 1;
        i += 1;
    }
    pat_weights.truncate(pat_len);
    pat_types.truncate(pat_len);
}

/// CompilePatternInnerBinary handles escapes and wild cards convert pattern characters and
/// pattern types in bytes.
/// The main algorithm is the same as CompilePatternInner. However, it's not easy to use interface/lambda to hide the different details here.
/// Note: if anything changes in this method, please double-check CompilePatternInner
// go: CompilePatternInnerBinary L1
pub fn compile_pattern_inner_binary(pattern: &[u8], escape: u8) -> (Vec<u8>, Vec<u8>) {
    let bytes = pattern;
    let len_bytes = bytes.len();
    let mut pat_weights = vec![0u8; len_bytes];
    let mut pat_types = vec![0u8; len_bytes];
    let mut pat_len = 0;
    let mut i = 0;
    while i < len_bytes {
        let tp;
        let mut b = bytes[i];
        if b == escape {
            tp = PatMatch;
            if i < len_bytes - 1 {
                i += 1;
                b = bytes[i];
            }
        } else if b == b'_' {
            // %_ => _%
            if pat_len > 0 && pat_types[pat_len - 1] == PatAny {
                tp = PatAny;
                b = b'%';
                pat_weights[pat_len - 1] = b'_';
                pat_types[pat_len - 1] = PatOne;
            } else {
                tp = PatOne;
            }
        } else if b == b'%' {
            // %% => %
            if pat_len > 0 && pat_types[pat_len - 1] == PatAny {
                i += 1;
                continue;
            }
            tp = PatAny;
        } else {
            tp = PatMatch;
        }
        pat_weights[pat_len] = b;
        pat_types[pat_len] = tp;
        pat_len += 1;
        i += 1;
    }
    pat_weights.truncate(pat_len);
    pat_types.truncate(pat_len);
    (pat_weights, pat_types)
}

// go: matchRune L1
fn match_rune(a: char, b: char) -> bool {
    a == b
    // We may reuse below code block when like function go back to case insensitive.
    /*
        if a == b {
            return true
        }
        if a >= 'a' && a <= 'z' && a-caseDiff == b {
            return true
        }
        return a >= 'A' && a <= 'Z' && a+caseDiff == b
    */
}

/// CompileLike2Regexp convert a like `lhs` to a regular expression
// go: CompileLike2Regexp L1
pub fn compile_like2_regexp(str: &[u8]) -> String {
    let (pat_chars, pat_types) = compile_pattern(str, b'\\');
    let mut result = String::new();
    for i in 0..pat_chars.len() {
        match pat_types[i] {
            PatMatch => result.push(pat_chars[i]),
            PatOne => result.push('.'),
            PatAny => result.push_str(".*"),
            _ => {}
        }
    }
    format!("^{result}$")
}

/// DoMatchBinary is an adapter for `DoMatchInner`, `str` is binary strings or ascii string.
// go: DoMatchBinary L1
pub fn do_match_binary(str: &[u8], pat_chars: &[u8], pat_types: &[u8]) -> bool {
    let bytes = str;
    let len_bytes = bytes.len();
    let len_pat_weights = pat_chars.len();
    do_match_inner(len_pat_weights, len_bytes, pat_types, |a, b| {
        bytes[a] == pat_chars[b]
    })
}

/// DoMatch is an adapter for `DoMatchCustomized`, `str` can be any unicode string.
// go: DoMatch L1
pub fn do_match(str: &[u8], pat_chars: &[char], pat_types: &[u8]) -> bool {
    do_match_customized(str, pat_chars, pat_types, match_rune)
}

/// DoMatchCustomized is an adapter for `DoMatchInner`, `str` can be any unicode string.
// go: DoMatchCustomized L1
pub fn do_match_customized(
    str: &[u8],
    pat_weights: &[char],
    pat_types: &[u8],
    matcher: impl Fn(char, char) -> bool,
) -> bool {
    // TODO(bb7133): it is possible to get the rune one by one to avoid the cost of get them as a whole.
    let runes = to_runes(str);
    let len_runes = runes.len();
    let len_pat_weights = pat_weights.len();
    do_match_inner(len_pat_weights, len_runes, pat_types, |a, b| {
        matcher(runes[a], pat_weights[b])
    })
}

/// doMatchInner matches the string with patChars and patTypes.
/// The algorithm has linear time complexity.
/// https://research.swtch.com/glob
// go: doMatchInner L1
fn do_match_inner(
    len_pat_weights: usize,
    len_chars: usize,
    pat_types: &[u8],
    matcher: impl Fn(usize, usize) -> bool,
) -> bool {
    let (mut c_idx, mut p_idx, mut next_c_idx, mut next_p_idx) = (0usize, 0usize, 0usize, 0usize);
    while p_idx < len_pat_weights || c_idx < len_chars {
        if p_idx < len_pat_weights {
            match pat_types[p_idx] {
                PatMatch => {
                    if c_idx < len_chars && matcher(c_idx, p_idx) {
                        p_idx += 1;
                        c_idx += 1;
                        continue;
                    }
                }
                PatOne => {
                    if c_idx < len_chars {
                        p_idx += 1;
                        c_idx += 1;
                        continue;
                    }
                }
                PatAny => {
                    // Try to match at sIdx.
                    // If that doesn't work out,
                    // restart at sIdx+1 next.
                    next_p_idx = p_idx;
                    next_c_idx = c_idx + 1;
                    p_idx += 1;
                    continue;
                }
                _ => {}
            }
        }
        // Mismatch. Maybe restart.
        if 0 < next_c_idx && next_c_idx <= len_chars {
            p_idx = next_p_idx;
            c_idx = next_c_idx;
            continue;
        }
        return false;
    }
    // Matched all of pattern to all of name. Success.
    true
}

/// IsExactMatch return true if no wildcard character
// go: IsExactMatch L1
pub fn is_exact_match(pat_types: &[u8]) -> bool {
    for &pt in pat_types {
        if pt != PatMatch {
            return false;
        }
    }
    true
}

/// Copy deep copies a string.
// go: Copy L1
pub fn copy(src: &[u8]) -> Vec<u8> {
    src.to_vec()
}

/// StringerFunc defines string func implement fmt.Stringer.
pub struct StringerFunc(Box<dyn Fn() -> String>);

impl StringerFunc {
    // glue: Go conversion StringerFunc(f)
    pub fn new(f: impl Fn() -> String + 'static) -> StringerFunc {
        StringerFunc(Box::new(f))
    }
}

impl fmt::Display for StringerFunc {
    /// String implements fmt.Stringer
    // go: StringerFunc.String L1
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&(self.0)())
    }
}

/// MemoizeStr returns memoized version of stringFunc. When the result of l is not
/// "", it will be cached and returned directly next time.
///
/// MemoizeStr is not concurrency safe.
// go: MemoizeStr L1
pub fn memoize_str(l: impl Fn() -> String + 'static) -> StringerFunc {
    let result = RefCell::new(String::new());
    StringerFunc::new(move || {
        if !result.borrow().is_empty() {
            return result.borrow().clone();
        }
        *result.borrow_mut() = l();
        result.borrow().clone()
    })
}

/// StringerStr defines a alias to normal string.
/// implement fmt.Stringer
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringerStr(pub String);

impl fmt::Display for StringerStr {
    /// String implements fmt.Stringer
    // go: StringerStr.String L1
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Escape the identifier for pretty-printing.
/// For instance, the identifier
///
/// ```text
/// "foo `bar`" will become "`foo ``bar```".
/// ```
///
/// The sqlMode controls whether to escape with backquotes (`) or double quotes
/// (`"`) depending on whether mysql.ModeANSIQuotes is enabled.
// go: Escape L1
pub fn escape(str: &[u8], sql_mode: SQLMode) -> Vec<u8> {
    let quote: u8 = if sql_mode & ModeANSIQuotes != SQLMode(0) {
        b'"'
    } else {
        b'`'
    };
    let mut out = Vec::with_capacity(str.len() + 2);
    out.push(quote);
    for &b in str {
        if b == quote {
            out.push(quote);
        }
        out.push(b);
    }
    out.push(quote);
    out
}

/// BuildStringFromLabels construct config labels into string by following format:
/// "keyA=valueA,keyB=valueB"
// go: BuildStringFromLabels L1
pub fn build_string_from_labels(labels: &HashMap<String, String>) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let mut s: Vec<&String> = labels.keys().collect();
    s.sort();
    let mut r = String::new();
    // visit labels by sorted key in order to make sure that result should be consistency
    for key in s {
        r.push_str(&format!("{}={},", key, labels[key]));
    }
    r.pop();
    r
}

/// GetTailSpaceCount returns the number of tailed spaces.
// go: GetTailSpaceCount L1
pub fn get_tail_space_count(str: &[u8]) -> i64 {
    let mut length = str.len();
    while length > 0 && str[length - 1] == b' ' {
        length -= 1;
    }
    (str.len() - length) as i64
}

/// Utf8Len calculates how many bytes the utf8 character takes.
/// This b parameter should be the first byte of utf8 character
// go: Utf8Len L1
pub fn utf8_len(b: u8) -> usize {
    let mut flag: u8 = 128;
    if (flag & b) == 0 {
        return 1;
    }

    let mut length = 0;

    while (flag & b) != 0 {
        length += 1;
        flag >>= 1;
    }

    length
}

/// TrimUtf8String needs the string input should always be valid which means
/// that it should always return true in utf8.ValidString(str)
// go: TrimUtf8String L1
pub fn trim_utf8_string(str: &mut &[u8], mut trimmed_num: i64) -> i64 {
    let mut total_len_trimmed = 0i64;
    while trimmed_num > 0 {
        let length = utf8_len(str[0]); // character length
        *str = &str[length..];
        total_len_trimmed += length as i64;
        trimmed_num -= 1;
    }
    total_len_trimmed
}

/// ConvertPosInUtf8 converts a binary index to the position which shows the occurrence location in the utf8 string
/// Take "你好" as example:
///
/// ```text
/// binary index for "好" is 3, ConvertPosInUtf8("你好", 3) should return 2
/// ```
// go: ConvertPosInUtf8 L1
pub fn convert_pos_in_utf8(str: &[u8], pos: i64) -> i64 {
    let pre_str = &str[..pos as usize];
    let pre_str_num = rune_count(pre_str);
    (pre_str_num + 1) as i64
}

// go: toLowerIfAlphaASCII L1
fn to_lower_if_alpha_ascii(c: u8) -> u8 {
    c | 0x20
}

// go: toUpperIfAlphaASCII L1
fn to_upper_if_alpha_ascii(c: u8) -> u8 {
    c ^ 0x20
}

/// IsUpperASCII judges if this is capital alphabet
// go: IsUpperASCII L1
pub fn is_upper_ascii(c: u8) -> bool {
    c.is_ascii_uppercase()
}

/// IsLowerASCII judges if this is lower alphabet
// go: IsLowerASCII L1
pub fn is_lower_ascii(c: u8) -> bool {
    c.is_ascii_lowercase()
}

/// LowerOneString lowers the ascii characters in a string
// go: LowerOneString L1
pub fn lower_one_string(str: &mut [u8]) {
    for c in str.iter_mut() {
        if is_upper_ascii(*c) {
            *c = to_lower_if_alpha_ascii(*c);
        }
    }
}

/// IsNumericASCII judges if a byte is numeric
// go: IsNumericASCII L1
pub fn is_numeric_ascii(c: u8) -> bool {
    c.is_ascii_digit()
}

/// LowerOneStringExcludeEscapeChar lowers strings and exclude an escape char
///
/// When escape_char is a capital char, we shouldn't lower the escape char.
/// For example, 'aaaa' ilike 'AAAA' escape 'A', we should convert 'AAAA' to 'AaAa'.
/// If we do not exclude the escape char, 'AAAA' will be lowered to 'aaaa', and we
/// can not get the correct result.
///
/// When escape_char is a lower char, we need to convert it to the capital char
/// Because: when lowering "ABC" with escape 'a', after lower, "ABC" -> "abc",
/// then 'a' will be an escape char and it is not expected.
/// Morever, when escape char is uppered we need to tell it to the caller.
// go: LowerOneStringExcludeEscapeChar L1
pub fn lower_one_string_exclude_escape_char(str: &mut [u8], escape_char: u8) -> u8 {
    let mut actual_escape_char = escape_char;
    if is_lower_ascii(escape_char) {
        actual_escape_char = to_upper_if_alpha_ascii(escape_char);
    }
    let mut escaped = false;
    let str_len = str.len();

    let mut i = 0;
    while i < str_len {
        if is_upper_ascii(str[i]) {
            // Do not lower the escape char, however when a char is equal to
            // an escape char and it's after an escape char, we still lower it
            // For example: "AA" (escape 'A'), -> "Aa"
            if !(str[i] != escape_char || escaped) {
                escaped = true;
                i += 1;
                continue;
            }
            str[i] = to_lower_if_alpha_ascii(str[i]);
        } else {
            if str[i] == escape_char && !escaped {
                escaped = true;

                // It should be `str[i] = toUpperIfAlphaASCII(str[i])`,
                // but 'actual_escape_char' is always equal to 'toUpperIfAlphaASCII(str[i])'
                str[i] = actual_escape_char;
                i += 1;
                continue;
            }
            i += utf8_len(str[i]) - 1;
        }
        escaped = false;
        i += 1;
    }

    actual_escape_char
}

/// EscapeGlobQuestionMark escapes '?' for a glob path pattern.
// go: EscapeGlobQuestionMark L1
pub fn escape_glob_question_mark(s: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(s.len());
    for c in runes(s) {
        if c == '?' {
            buf.push(b'\\');
        }
        let mut b = [0u8; 4];
        buf.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    // Go: TestMemoizeStr (the govec domain exempts MemoizeStr and StringerFunc.String:
    // they take a function value).
    #[test]
    fn memoize_str_calls_once() {
        let cnt = Rc::new(Cell::new(0));
        let c = cnt.clone();
        let stringer = memoize_str(move || {
            c.set(c.get() + 1);
            "slow".to_owned()
        });
        assert_eq!(stringer.to_string(), "slow");
        assert_eq!(stringer.to_string(), "slow");
        assert_eq!(cnt.get(), 1);
    }

    // An empty result is not cached (Go: `if result != ""`).
    #[test]
    fn memoize_str_does_not_cache_empty() {
        let cnt = Rc::new(Cell::new(0));
        let c = cnt.clone();
        let stringer = memoize_str(move || {
            c.set(c.get() + 1);
            String::new()
        });
        assert_eq!(stringer.to_string(), "");
        assert_eq!(stringer.to_string(), "");
        assert_eq!(cnt.get(), 2);
    }

    #[test]
    fn stringer_func_calls_the_function() {
        let s = StringerFunc::new(|| "abc".to_owned());
        assert_eq!(format!("{s}"), "abc");
        assert_eq!(StringerStr("x y".to_owned()).to_string(), "x y");
    }
}
