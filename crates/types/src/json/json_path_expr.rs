//! go-file: pkg/types/json_path_expr.go
//!
//! JSON path expressions (exception A4): the parser, the leg operations and `String`.
//!
//!    From MySQL 5.7, JSON path expression grammar:
//!        pathExpression ::= scope (jsonPathLeg)*
//!        scope ::= [ columnReference ] '$'
//!        columnReference ::= // omit...
//!        jsonPathLeg ::= member | arrayLocation | '**'
//!        member ::= '.' (keyName | '*')
//!        arrayLocation ::= '[' (non-negative-integer | '*') ']'
//!        keyName ::= ECMAScript-identifier | ECMAScript-string-literal
//!
//!    And some implementation limits in MySQL 5.7:
//!        1) columnReference in scope must be empty now;
//!        2) double asterisk(**) could not be last leg;
//!
//! Go keeps parsed expressions in a global LRU cache (`peCache`, 1000 entries, under a mutex);
//! the cache is invisible to users and is not kept (A4 spec 5.6): [`parse_json_path_expr`]
//! parses every time, and callers with constant paths cache the result themselves.
//!
//! Go reads the path with `s.pathExpr[s.pos]` in places where `pos` may be at the end (and
//! `readWhile` can step past it); those reads panic in Go and panic here too.

#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::gofmt::Arg;

use super::json_binary::BinaryJSON;
use super::json_binary_functions::{quote_json_string, unquote_json_string};
use super::json_constants::ErrInvalidJSONPath;
use crate::gostd::atoi;

/// `jsonPathArrayIndex`: a non-negative index counts from the start, a negative one from the
/// end (`-1` is `last`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathArrayIndex(pub isize);

impl PathArrayIndex {
    // go: jsonPathArrayIndex.getIndexFromStart L1
    pub(crate) fn get_index_from_start(self, obj: &BinaryJSON<'_>) -> isize {
        if self.0 < 0 {
            return obj.get_elem_count() as isize + self.0;
        }

        self.0
    }

    // go: jsonPathArrayIndex.String L1
    fn string(self) -> String {
        if self.0 < 0 {
            let index_str = ((self.0 + 1) as f64).abs() as isize;
            return format!("last-{index_str}");
        }

        self.0.to_string()
    }
}

// go: validateIndexRange L1
fn validate_index_range(a: PathArrayIndex, b: PathArrayIndex) -> bool {
    if (a.0 >= 0 && b.0 >= 0) || (a.0 < 0 && b.0 < 0) {
        return a.0 <= b.0;
    }

    true
}

// go: jsonPathArrayIndexFromStart L1
pub(crate) fn json_path_array_index_from_start(index: isize) -> PathArrayIndex {
    PathArrayIndex(index)
}

// go: jsonPathArrayIndexFromLast L1
fn json_path_array_index_from_last(index: isize) -> PathArrayIndex {
    PathArrayIndex(-1 - index)
}

/// `jsonPathArraySelection`: the Go interface with its three implementations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArraySelection {
    /// `jsonPathArraySelectionAsterisk`: `[*]`.
    Asterisk,
    /// `jsonPathArraySelectionIndex`: `[n]`, `[last-n]`.
    Index(PathArrayIndex),
    /// `jsonPathArraySelectionRange`: `[m to n]`.
    Range(PathArrayIndex, PathArrayIndex),
}

impl ArraySelection {
    // go: jsonPathArraySelectionAsterisk.getIndexRange L1
    // go: jsonPathArraySelectionIndex.getIndexRange L1
    // go: jsonPathArraySelectionRange.getIndexRange L1
    pub(crate) fn get_index_range(&self, bj: &BinaryJSON<'_>) -> (isize, isize) {
        match self {
            ArraySelection::Asterisk => (0, bj.get_elem_count() as isize - 1),
            ArraySelection::Index(index) => {
                let mut end = index.get_index_from_start(bj);

                let elem_count = bj.get_elem_count() as isize;
                if end >= elem_count {
                    end = elem_count - 1;
                }
                (index.get_index_from_start(bj), end)
            }
            ArraySelection::Range(start, end) => {
                let start = start.get_index_from_start(bj);
                let mut end = end.get_index_from_start(bj);

                let elem_count = bj.get_elem_count() as isize;
                if end >= elem_count {
                    end = elem_count - 1;
                }
                (start, end)
            }
        }
    }
}

/// jsonPathLegType is only used by JSONPathExpression.
pub type PathLegType = u8;

/// jsonPathLegKey indicates the path leg with '.key'.
pub const jsonPathLegKey: PathLegType = 0x01;
/// jsonPathLegArraySelection indicates the path leg with form '[index]', '[index to index]'.
pub const jsonPathLegArraySelection: PathLegType = 0x02;
/// jsonPathLegDoubleAsterisk indicates the path leg with form '**'.
pub const jsonPathLegDoubleAsterisk: PathLegType = 0x03;

/// jsonPathLeg is only used by JSONPathExpression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathLeg {
    pub(crate) typ: PathLegType,
    /// if typ is jsonPathLegArraySelection, the value should be parsed into here.
    pub(crate) array_selection: Option<ArraySelection>,
    /// if typ is jsonPathLegKey, the key should be parsed into here.
    pub(crate) dot_key: Vec<u8>,
}

/// jsonPathExpressionFlag holds attributes of JSONPathExpression
pub type PathExpressionFlag = u8;

pub(crate) const jsonPathExpressionContainsAsterisk: PathExpressionFlag = 0x01;
pub(crate) const jsonPathExpressionContainsDoubleAsterisk: PathExpressionFlag = 0x02;
pub(crate) const jsonPathExpressionContainsRange: PathExpressionFlag = 0x04;

/// The methods of `jsonPathExpressionFlag`.
pub(crate) trait PathExpressionFlagExt {
    // glue: method declaration (implemented below)
    fn contains_any_asterisk(self) -> bool;
    // glue: method declaration (implemented below)
    fn contains_any_range(self) -> bool;
}

impl PathExpressionFlagExt for PathExpressionFlag {
    /// containsAnyAsterisk returns true if pef contains any asterisk.
    // go: jsonPathExpressionFlag.containsAnyAsterisk L1
    fn contains_any_asterisk(self) -> bool {
        self & (jsonPathExpressionContainsAsterisk | jsonPathExpressionContainsDoubleAsterisk) != 0
    }

    /// containsAnyRange returns true if pef contains any range.
    // go: jsonPathExpressionFlag.containsAnyRange L1
    fn contains_any_range(self) -> bool {
        self & jsonPathExpressionContainsRange != 0
    }
}

/// JSONPathExpression is for JSON path expression.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct JSONPathExpression {
    pub(crate) legs: Vec<PathLeg>,
    pub(crate) flags: PathExpressionFlag,
}

impl Clone for JSONPathExpression {
    // go: JSONPathExpression.clone L1
    fn clone(&self) -> JSONPathExpression {
        JSONPathExpression {
            legs: self.legs.clone(),
            flags: self.flags,
        }
    }
}

impl JSONPathExpression {
    /// popOneLeg returns a jsonPathLeg, and a child JSONPathExpression without that leg.
    // go: JSONPathExpression.popOneLeg L1
    pub(crate) fn pop_one_leg(&self) -> (PathLeg, JSONPathExpression) {
        let mut new_pe = JSONPathExpression {
            legs: self.legs[1..].to_vec(),
            flags: 0,
        };
        for leg in &new_pe.legs {
            if leg.typ == jsonPathLegArraySelection {
                match leg.array_selection {
                    Some(ArraySelection::Asterisk) => {
                        new_pe.flags |= jsonPathExpressionContainsAsterisk
                    }
                    Some(ArraySelection::Range(..)) => {
                        new_pe.flags |= jsonPathExpressionContainsRange
                    }
                    _ => {}
                }
            } else if leg.typ == jsonPathLegKey && leg.dot_key == b"*" {
                new_pe.flags |= jsonPathExpressionContainsAsterisk;
            } else if leg.typ == jsonPathLegDoubleAsterisk {
                new_pe.flags |= jsonPathExpressionContainsDoubleAsterisk;
            }
        }
        (self.legs[0].clone(), new_pe)
    }

    /// popOneLastLeg returns the parent JSONPathExpression and the last jsonPathLeg
    // go: JSONPathExpression.popOneLastLeg L1
    pub(crate) fn pop_one_last_leg(&self) -> (JSONPathExpression, PathLeg) {
        let last_leg_idx = self.legs.len() - 1;
        let last_leg = self.legs[last_leg_idx].clone();
        // It is used only in modification, it has been checked that there is no asterisks.
        (
            JSONPathExpression {
                legs: self.legs[..last_leg_idx].to_vec(),
                flags: 0,
            },
            last_leg,
        )
    }

    /// pushBackOneArraySelectionLeg pushback one leg of INDEX type
    // go: JSONPathExpression.pushBackOneArraySelectionLeg L1
    pub(crate) fn push_back_one_array_selection_leg(
        &self,
        array_selection: ArraySelection,
    ) -> JSONPathExpression {
        let mut legs = self.legs.clone();
        let mut flags = self.flags;
        match array_selection {
            ArraySelection::Asterisk => flags |= jsonPathExpressionContainsAsterisk,
            ArraySelection::Range(..) => flags |= jsonPathExpressionContainsRange,
            ArraySelection::Index(_) => {}
        }
        legs.push(PathLeg {
            typ: jsonPathLegArraySelection,
            array_selection: Some(array_selection),
            dot_key: Vec::new(),
        });
        JSONPathExpression { legs, flags }
    }

    /// pushBackOneKeyLeg pushback one leg of KEY type
    // go: JSONPathExpression.pushBackOneKeyLeg L1
    pub(crate) fn push_back_one_key_leg(&self, key: Vec<u8>) -> JSONPathExpression {
        let mut flags = self.flags;
        if key == b"*" {
            flags |= jsonPathExpressionContainsAsterisk;
        }
        let mut legs = self.legs.clone();
        legs.push(PathLeg {
            typ: jsonPathLegKey,
            array_selection: None,
            dot_key: key,
        });
        JSONPathExpression { legs, flags }
    }

    /// CouldMatchMultipleValues returns true if pe contains any asterisk or range selection.
    // go: JSONPathExpression.CouldMatchMultipleValues L1
    pub fn could_match_multiple_values(&self) -> bool {
        self.flags.contains_any_asterisk() || self.flags.contains_any_range()
    }

    /// String implements fmt.Stringer interface.
    // go: JSONPathExpression.String L1
    pub fn string(&self) -> Vec<u8> {
        let mut s: Vec<u8> = b"$".to_vec();
        for leg in &self.legs {
            match leg.typ {
                jsonPathLegArraySelection => match &leg.array_selection {
                    Some(ArraySelection::Asterisk) => s.extend_from_slice(b"[*]"),
                    Some(ArraySelection::Index(index)) => {
                        s.push(b'[');
                        s.extend_from_slice(index.string().as_bytes());
                        s.push(b']');
                    }
                    Some(ArraySelection::Range(start, end)) => {
                        s.push(b'[');
                        s.extend_from_slice(start.string().as_bytes());
                        s.extend_from_slice(b" to ");
                        s.extend_from_slice(end.string().as_bytes());
                        s.push(b']');
                    }
                    None => {}
                },
                jsonPathLegKey => {
                    s.push(b'.');
                    if leg.dot_key == b"*" {
                        s.extend_from_slice(&leg.dot_key);
                    } else {
                        s.extend_from_slice(&quote_json_string(&leg.dot_key));
                    }
                }
                jsonPathLegDoubleAsterisk => s.extend_from_slice(b"**"),
                _ => {}
            }
        }
        s
    }
}

/// `unicode.IsSpace(rune(b))` for one byte: the ASCII spaces, U+0085 and U+00A0.
// go-std: unicode.IsSpace L1 (for runes below 0x100)
fn is_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ' | 0x85 | 0xa0)
}

struct PathStream<'a> {
    path_expr: &'a [u8],
    pos: usize,
}

impl<'a> PathStream<'a> {
    // go: jsonPathStream.skipWhiteSpace L1
    fn skip_white_space(&mut self) {
        while self.pos < self.path_expr.len() {
            if !is_space(self.path_expr[self.pos]) {
                break;
            }
            self.pos += 1;
        }
    }

    // go: jsonPathStream.read L1
    fn read(&mut self) -> u8 {
        let b = self.path_expr[self.pos];
        self.pos += 1;
        b
    }

    // go: jsonPathStream.peek L1
    fn peek(&self) -> u8 {
        self.path_expr[self.pos]
    }

    // go: jsonPathStream.skip L1
    fn skip(&mut self, i: usize) {
        self.pos += i;
    }

    // go: jsonPathStream.exhausted L1
    fn exhausted(&self) -> bool {
        self.pos >= self.path_expr.len()
    }

    /// `readWhile(f)`; `f` gets the stream, so that it can skip a byte as Go's closures do.
    // go: jsonPathStream.readWhile L1
    fn read_while(
        &mut self,
        mut f: impl FnMut(&mut PathStream<'a>, u8) -> bool,
    ) -> (&'a [u8], bool) {
        let start = self.pos;
        while !self.exhausted() {
            let b = self.peek();
            if !f(self, b) {
                return (&self.path_expr[start..self.pos], false);
            }
            self.skip(1);
        }
        (&self.path_expr[start..self.pos], true)
    }

    // go: jsonPathStream.tryReadString L1
    fn try_read_string(&mut self, expected: &[u8]) -> bool {
        let record_pos = self.pos;

        let mut i = 0;
        let (str, meet_end) = self.read_while(|_, _| {
            i += 1;
            i <= expected.len()
        });

        if meet_end || str != expected {
            self.pos = record_pos;
            return false;
        }
        true
    }

    // go: jsonPathStream.tryReadIndexNumber L1
    fn try_read_index_number(&mut self) -> (isize, bool) {
        let record_pos = self.pos;

        let (str, meet_end) = self.read_while(|_, b| b.is_ascii_digit());
        if meet_end {
            self.pos = record_pos;
            return (0, false);
        }

        let (index, err) = atoi(str);
        if err.is_err() || index > u32::MAX as isize {
            self.pos = record_pos;
            return (0, false);
        }

        (index, true)
    }

    // go: jsonPathStream.tryParseArrayIndex L1
    fn try_parse_array_index(&mut self) -> (PathArrayIndex, bool) {
        let record_pos = self.pos;

        self.skip_white_space();
        if self.exhausted() {
            return (PathArrayIndex(0), false);
        }
        let c = self.peek();
        if c.is_ascii_digit() {
            let (index, ok) = self.try_read_index_number();
            if !ok {
                self.pos = record_pos;
                return (PathArrayIndex(0), false);
            }
            return (json_path_array_index_from_start(index), true);
        } else if c == b'l' {
            if !self.try_read_string(b"last") {
                self.pos = record_pos;
                return (PathArrayIndex(0), false);
            }
            self.skip_white_space();
            if self.exhausted() {
                return (json_path_array_index_from_last(0), true);
            }

            if self.peek() != b'-' {
                return (json_path_array_index_from_last(0), true);
            }
            self.skip(1);
            self.skip_white_space();

            let (index, ok) = self.try_read_index_number();
            if !ok {
                self.pos = record_pos;
                return (PathArrayIndex(0), false);
            }
            return (json_path_array_index_from_last(index), true);
        }
        (PathArrayIndex(0), false)
    }
}

/// ParseJSONPathExpr parses a JSON path expression. Returns a JSONPathExpression
/// object which can be used in JSON_EXTRACT, JSON_SET and so on.
// go: ParseJSONPathExpr L2 (A4 5.6: no global LRU cache; every call parses)
// go: init L2 (A4 5.6: the peCache LRU it creates is not kept)
// go: jsonPathExpressionKey.Hash L2 (A4 5.6: the cache key of peCache, not kept)
pub fn parse_json_path_expr(path_expr: &[u8]) -> Result<JSONPathExpression, Error> {
    parse_json_path_expr_inner(path_expr)
}

/// parseJSONPathExpr parses a JSON path expression. Returns a JSONPathExpression
/// object which can be used in JSON_EXTRACT, JSON_SET and so on.
// go: parseJSONPathExpr L1 [M-ERR]
fn parse_json_path_expr_inner(path_expr: &[u8]) -> Result<JSONPathExpression, Error> {
    let mut s = PathStream { path_expr, pos: 0 };
    s.skip_white_space();
    if s.exhausted() || s.read() != b'$' {
        return Err(ErrInvalidJSONPath.gen_with_stack_by_args(vec![Arg::from(1isize)]));
    }
    s.skip_white_space();

    let mut pe = JSONPathExpression {
        legs: Vec::with_capacity(16),
        flags: 0,
    };

    while !s.exhausted() {
        let ok = match s.peek() {
            b'.' => parse_json_path_member(&mut s, &mut pe),
            b'[' => parse_json_path_array(&mut s, &mut pe),
            b'*' => parse_json_path_wildcard(&mut s, &mut pe),
            _ => false,
        };

        if !ok {
            return Err(ErrInvalidJSONPath.gen_with_stack_by_args(vec![Arg::from(s.pos as isize)]));
        }

        s.skip_white_space();
    }

    if pe
        .legs
        .last()
        .is_some_and(|l| l.typ == jsonPathLegDoubleAsterisk)
    {
        return Err(ErrInvalidJSONPath.gen_with_stack_by_args(vec![Arg::from(s.pos as isize)]));
    }

    Ok(pe)
}

// go: parseJSONPathWildcard L1
fn parse_json_path_wildcard(s: &mut PathStream<'_>, p: &mut JSONPathExpression) -> bool {
    s.skip(1);
    if s.exhausted() || s.read() != b'*' {
        return false;
    }
    if s.exhausted() || s.peek() == b'*' {
        return false;
    }

    p.flags |= jsonPathExpressionContainsDoubleAsterisk;
    p.legs.push(PathLeg {
        typ: jsonPathLegDoubleAsterisk,
        array_selection: None,
        dot_key: Vec::new(),
    });
    true
}

// go: parseJSONPathArray L1
fn parse_json_path_array(s: &mut PathStream<'_>, p: &mut JSONPathExpression) -> bool {
    s.skip(1);
    s.skip_white_space();
    if s.exhausted() {
        return false;
    }

    if s.peek() == b'*' {
        s.skip(1);
        p.flags |= jsonPathExpressionContainsAsterisk;
        p.legs.push(PathLeg {
            typ: jsonPathLegArraySelection,
            array_selection: Some(ArraySelection::Asterisk),
            dot_key: Vec::new(),
        });
    } else {
        let (start, ok) = s.try_parse_array_index();
        if !ok {
            return false;
        }

        let mut selection = ArraySelection::Index(start);
        if is_space(s.peek()) {
            s.skip_white_space();
            if s.try_read_string(b"to") && is_space(s.peek()) {
                s.skip_white_space();
                if s.exhausted() {
                    return false;
                }
                let (end, ok) = s.try_parse_array_index();
                if !ok {
                    return false;
                }

                if !validate_index_range(start, end) {
                    return false;
                }
                p.flags |= jsonPathExpressionContainsRange;
                selection = ArraySelection::Range(start, end);
            }
        }

        p.legs.push(PathLeg {
            typ: jsonPathLegArraySelection,
            array_selection: Some(selection),
            dot_key: Vec::new(),
        });
    }

    s.skip_white_space();
    if s.exhausted() || s.read() != b']' {
        return false;
    }

    true
}

// go: parseJSONPathMember L1
fn parse_json_path_member(s: &mut PathStream<'_>, p: &mut JSONPathExpression) -> bool {
    s.skip(1);
    s.skip_white_space();
    if s.exhausted() {
        return false;
    }

    if s.peek() == b'*' {
        s.skip(1);
        p.flags |= jsonPathExpressionContainsAsterisk;
        p.legs.push(PathLeg {
            typ: jsonPathLegKey,
            array_selection: None,
            dot_key: b"*".to_vec(),
        });
    } else {
        let dot_key: &[u8];
        let mut was_quoted = false;
        if s.peek() == b'"' {
            s.skip(1);
            let (str, meet_end) = s.read_while(|s, b| {
                if b == b'\\' {
                    s.skip(1);
                    return true;
                }
                b != b'"'
            });
            if meet_end {
                return false;
            }
            s.skip(1);
            dot_key = str;
            was_quoted = true;
        } else {
            (dot_key, _) =
                s.read_while(|_, b| !(is_space(b) || b == b'.' || b == b'[' || b == b'*'));
        }
        let mut quoted = Vec::with_capacity(dot_key.len() + 2);
        quoted.push(b'"');
        quoted.extend_from_slice(dot_key);
        quoted.push(b'"');

        if !valid_json_string(&quoted) {
            return false;
        }
        let dot_key = match unquote_json_string(&quoted[1..quoted.len() - 1]) {
            Ok(k) => k,
            Err(_) => return false,
        };
        if !was_quoted && !is_ecmascript_identifier(&dot_key) {
            return false;
        }

        p.legs.push(PathLeg {
            typ: jsonPathLegKey,
            array_selection: None,
            dot_key,
        });
    }
    true
}

// go: isEcmascriptIdentifier L1
pub(crate) fn is_ecmascript_identifier(s: &[u8]) -> bool {
    if s.is_empty() {
        return false;
    }

    for (i, &c) in s.iter().enumerate() {
        // accept unicode characters, see https://ecma-international.org/ecma-262/5.1/#sec-7.6
        if (i != 0 && c.is_ascii_digit())
            || c.is_ascii_lowercase()
            || c.is_ascii_uppercase()
            || c == b'_'
            || c == b'$'
            || c >= 0x80
        {
            continue;
        }
        return false;
    }
    true
}

/// `json.Valid(b)` where `b` starts with `"`: one string literal, then only whitespace
/// (the scanner's string states, without building the value).
// go-std: encoding/json.Valid L2 (the scanner's string states only; the input starts with a quote)
fn valid_json_string(b: &[u8]) -> bool {
    let mut i = 1;
    loop {
        let Some(&c) = b.get(i) else {
            return false;
        };
        i += 1;
        match c {
            b'"' => break,
            b'\\' => match b.get(i) {
                Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => i += 1,
                Some(b'u') => {
                    if b.len() < i + 5 || !b[i + 1..i + 5].iter().all(u8::is_ascii_hexdigit) {
                        return false;
                    }
                    i += 5;
                }
                _ => return false,
            },
            c if c < 0x20 => return false,
            _ => {}
        }
    }
    b[i..]
        .iter()
        .all(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
}
