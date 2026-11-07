//! go-file: github.com/BurntSushi/toml@v1.5.0/lex.go
//!
//! The TOML lexer of BurntSushi/toml (the decoder Go's `pkg/config` uses), ported for its
//! exact error messages and positions. A state function returns the next state; items are
//! queued (Go: a buffered channel read by `nextItem`).

use std::collections::VecDeque;

use super::error::{LexErr, Position};
use crate::gofmt::{self, Arg};
use crate::gostd;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemType {
    Error,
    // itemNil is declared but never produced by Go's lexer; not ported.
    Eof,
    Text,
    String,
    StringEsc,
    RawString,
    MultilineString,
    RawMultilineString,
    Bool,
    Integer,
    Float,
    Datetime,
    Array,
    ArrayEnd,
    TableStart,
    TableEnd,
    ArrayTableStart,
    ArrayTableEnd,
    KeyStart,
    KeyEnd,
    CommentStart,
    InlineTableStart,
    InlineTableEnd,
}

/// `eof` rune sentinel.
const EOF: char = '\0';

/// `stateFn`.
#[derive(Clone, Copy)]
pub(crate) struct StateFn(fn(&mut Lexer) -> Option<StateFn>);

pub(crate) struct Lexer {
    pub(crate) input: Vec<u8>,
    start: usize,
    pos: usize,
    line: isize,
    state: Option<StateFn>,
    items: VecDeque<Item>,
    toml_next: bool,
    esc: bool,
    // Allow for backing up up to 4 runes. This is necessary because TOML
    // contains 3-rune tokens (""" and ''').
    prev_widths: [usize; 4],
    nprev: usize,
    at_eof: bool,
    // A stack of state functions used to maintain context.
    stack: Vec<StateFn>,
}

#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub(crate) typ: ItemType,
    pub(crate) val: String,
    pub(crate) err: Option<LexErr>,
    pub(crate) pos: Position,
}

impl Lexer {
    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.nextItem L1
    pub(crate) fn next_item(&mut self) -> Item {
        loop {
            if let Some(item) = self.items.pop_front() {
                return item;
            }
            let st = self.state.expect("BUG in lexer: no state");
            self.state = (st.0)(self);
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lex L1
    pub(crate) fn new(input: Vec<u8>, toml_next: bool) -> Lexer {
        Lexer {
            input,
            start: 0,
            pos: 0,
            line: 1,
            state: Some(StateFn(lex_top)),
            items: VecDeque::new(),
            toml_next,
            esc: false,
            prev_widths: [0; 4],
            nprev: 0,
            at_eof: false,
            stack: Vec::with_capacity(10),
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.push L1
    fn push(&mut self, state: StateFn) {
        self.stack.push(state);
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.pop L1
    fn pop(&mut self) -> Option<StateFn> {
        match self.stack.pop() {
            Some(s) => Some(s),
            None => self.errorf("BUG in lexer: no states to pop", &[]),
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.current L1
    fn current(&self) -> String {
        String::from_utf8_lossy(&self.input[self.start..self.pos]).into_owned()
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.getPos L1
    fn get_pos(&self) -> Position {
        let mut p = Position {
            line: self.line,
            col: 0,
            start: self.start as isize,
            len: self.pos as isize - self.start as isize,
        };
        if p.len <= 0 {
            p.len = 1;
        }
        p
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.emit L1
    fn emit(&mut self, typ: ItemType) {
        // Needed for multiline strings ending with an incomplete UTF-8 sequence.
        if self.start > self.pos {
            let b = self.input[self.pos];
            self.error(LexErr::Utf8(b));
            return;
        }
        let item = Item {
            typ,
            pos: self.get_pos(),
            val: self.current(),
            err: None,
        };
        self.items.push_back(item);
        self.start = self.pos;
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.emitTrim L1
    fn emit_trim(&mut self, typ: ItemType) {
        let item = Item {
            typ,
            pos: self.get_pos(),
            val: self.current().trim().to_owned(),
            err: None,
        };
        self.items.push_back(item);
        self.start = self.pos;
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.next L1
    fn next(&mut self) -> char {
        if self.at_eof {
            panic!("BUG in lexer: next called after EOF");
        }
        if self.pos >= self.input.len() {
            self.at_eof = true;
            return EOF;
        }
        if self.input[self.pos] == b'\n' {
            self.line += 1;
        }
        self.prev_widths[3] = self.prev_widths[2];
        self.prev_widths[2] = self.prev_widths[1];
        self.prev_widths[1] = self.prev_widths[0];
        if self.nprev < 4 {
            self.nprev += 1;
        }
        let (r, w) = gostd::decode_rune(&self.input[self.pos..]);
        if r == '\u{fffd}' && w == 1 {
            let b = self.input[self.pos];
            self.error(LexErr::Utf8(b));
            return '\u{fffd}';
        }
        // Note: don't use peek() here, as this calls next().
        if is_control(r)
            || (r == '\r'
                && (self.input.len() - 1 == self.pos || self.input[self.pos + 1] != b'\n'))
        {
            self.error_control_char(r);
            return '\u{fffd}';
        }
        self.prev_widths[0] = w;
        self.pos += w;
        r
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.ignore L1
    fn ignore(&mut self) {
        self.start = self.pos;
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.backup L1
    fn backup(&mut self) {
        if self.at_eof {
            self.at_eof = false;
            return;
        }
        if self.nprev < 1 {
            panic!("BUG in lexer: backed up too far");
        }
        let w = self.prev_widths[0];
        self.prev_widths[0] = self.prev_widths[1];
        self.prev_widths[1] = self.prev_widths[2];
        self.prev_widths[2] = self.prev_widths[3];
        self.nprev -= 1;
        self.pos -= w;
        if self.pos < self.input.len() && self.input[self.pos] == b'\n' {
            self.line -= 1;
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.accept L1
    fn accept(&mut self, valid: char) -> bool {
        if self.next() == valid {
            return true;
        }
        self.backup();
        false
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.peek L1
    fn peek(&mut self) -> char {
        let r = self.next();
        self.backup();
        r
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.skip L1
    fn skip(&mut self, pred: fn(char) -> bool) {
        loop {
            let r = self.next();
            if pred(r) {
                continue;
            }
            self.backup();
            self.ignore();
            return;
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.error L1
    fn error(&mut self, err: LexErr) -> Option<StateFn> {
        if self.at_eof {
            return self.error_prev_line(err);
        }
        let pos = self.get_pos();
        self.items.push_back(Item {
            typ: ItemType::Error,
            pos,
            err: Some(err),
            val: String::new(),
        });
        None
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.errorPrevLine L1
    fn error_prev_line(&mut self, err: LexErr) -> Option<StateFn> {
        let mut pos = self.get_pos();
        pos.line -= 1;
        pos.len = 1;
        pos.start = self.pos as isize - 1;
        self.items.push_back(Item {
            typ: ItemType::Error,
            pos,
            err: Some(err),
            val: String::new(),
        });
        None
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.errorPos L1
    fn error_pos(&mut self, start: isize, length: isize, err: LexErr) -> Option<StateFn> {
        let mut pos = self.get_pos();
        pos.start = start;
        pos.len = length;
        self.items.push_back(Item {
            typ: ItemType::Error,
            pos,
            err: Some(err),
            val: String::new(),
        });
        None
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.errorf L1
    fn errorf(&mut self, format: &str, values: &[Arg]) -> Option<StateFn> {
        let msg = String::from_utf8_lossy(&gofmt::sprintf(format, values)).into_owned();
        if self.at_eof {
            let mut pos = self.get_pos();
            if self.pos >= 1 && self.input[self.pos - 1] == b'\n' {
                pos.line -= 1;
            }
            pos.len = 1;
            pos.start = self.pos as isize - 1;
            self.items.push_back(Item {
                typ: ItemType::Error,
                pos,
                err: Some(LexErr::Msg(msg)),
                val: String::new(),
            });
            return None;
        }
        let pos = self.get_pos();
        self.items.push_back(Item {
            typ: ItemType::Error,
            pos,
            err: Some(LexErr::Msg(msg)),
            val: String::new(),
        });
        None
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.lexer.errorControlChar L1
    fn error_control_char(&mut self, cc: char) -> Option<StateFn> {
        self.error_pos(self.pos as isize - 1, 1, LexErr::Control(cc))
    }
}

/// A rune as a Go `%q` argument.
// glue: Go rune argument
fn rq(r: char) -> Arg {
    Arg::Int(i64::from(r as u32), "int32")
}

/// A Go string argument.
// glue: Go string argument
fn sq(s: String) -> Arg {
    Arg::from(s)
}

macro_rules! st {
    ($f:ident) => {
        Some(StateFn($f))
    };
}

/// lexTop consumes elements at the top level of TOML data.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexTop L1
fn lex_top(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_whitespace(r) || is_nl(r) {
        return lex_skip(lx, StateFn(lex_top));
    }
    match r {
        '#' => {
            lx.push(StateFn(lex_top));
            return st!(lex_comment_start);
        }
        '[' => return st!(lex_table_start),
        EOF => {
            if lx.pos > lx.start {
                return lx.errorf("unexpected EOF", &[]);
            }
            lx.emit(ItemType::Eof);
            return None;
        }
        _ => {}
    }
    // At this point, the only valid item can be a key, so we back up
    // and let the key lexer do the rest.
    lx.backup();
    lx.push(StateFn(lex_top_end));
    st!(lex_key_start)
}

/// lexTopEnd is entered whenever a top-level item has been consumed. (A value
/// or a table.) It must see only whitespace, and will turn back to lexTop
/// upon a newline. If it sees EOF, it will quit the lexer successfully.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexTopEnd L1
fn lex_top_end(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if r == '#' {
        // a comment will read to a newline for us.
        lx.push(StateFn(lex_top));
        return st!(lex_comment_start);
    } else if is_whitespace(r) {
        return st!(lex_top_end);
    } else if is_nl(r) {
        lx.ignore();
        return st!(lex_top);
    } else if r == EOF {
        lx.emit(ItemType::Eof);
        return None;
    }
    lx.errorf(
        "expected a top-level item to end with a newline, comment, or EOF, but got %q instead",
        &[rq(r)],
    )
}

/// lexTable lexes the beginning of a table. Namely, it makes sure that
/// it starts with a character other than '.' and ']'.
/// It assumes that '[' has already been consumed.
/// It also handles the case that this is an item in an array of tables.
/// e.g., '[[name]]'.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexTableStart L1
fn lex_table_start(lx: &mut Lexer) -> Option<StateFn> {
    if lx.peek() == '[' {
        lx.next();
        lx.emit(ItemType::ArrayTableStart);
        lx.push(StateFn(lex_array_table_end));
    } else {
        lx.emit(ItemType::TableStart);
        lx.push(StateFn(lex_table_end));
    }
    st!(lex_table_name_start)
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.lexTableEnd L1
fn lex_table_end(lx: &mut Lexer) -> Option<StateFn> {
    lx.emit(ItemType::TableEnd);
    st!(lex_top_end)
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.lexArrayTableEnd L1
fn lex_array_table_end(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if r != ']' {
        return lx.errorf(
            "expected end of table array name delimiter ']', but got %q instead",
            &[rq(r)],
        );
    }
    lx.emit(ItemType::ArrayTableEnd);
    st!(lex_top_end)
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.lexTableNameStart L1
fn lex_table_name_start(lx: &mut Lexer) -> Option<StateFn> {
    lx.skip(is_whitespace);
    let r = lx.peek();
    if r == ']' || r == EOF {
        lx.errorf(
            "unexpected end of table name (table names cannot be empty)",
            &[],
        )
    } else if r == '.' {
        lx.errorf(
            "unexpected table separator (table names cannot be empty)",
            &[],
        )
    } else if r == '"' || r == '\'' {
        lx.ignore();
        lx.push(StateFn(lex_table_name_end));
        st!(lex_quoted_name)
    } else {
        lx.push(StateFn(lex_table_name_end));
        st!(lex_bare_name)
    }
}

/// lexTableNameEnd reads the end of a piece of a table name, optionally
/// consuming whitespace.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexTableNameEnd L1
fn lex_table_name_end(lx: &mut Lexer) -> Option<StateFn> {
    lx.skip(is_whitespace);
    let r = lx.next();
    if is_whitespace(r) {
        st!(lex_table_name_end)
    } else if r == '.' {
        lx.ignore();
        st!(lex_table_name_start)
    } else if r == ']' {
        lx.pop()
    } else {
        lx.errorf(
            "expected '.' or ']' to end table name, but got %q instead",
            &[rq(r)],
        )
    }
}

/// lexBareName lexes one part of a key or table.
///
/// It assumes that at least one valid character for the table has already been
/// read.
///
/// Lexes only one part, e.g. only 'a' inside 'a.b'.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexBareName L1
fn lex_bare_name(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_bare_key_char(r, lx.toml_next) {
        return st!(lex_bare_name);
    }
    lx.backup();
    lx.emit(ItemType::Text);
    lx.pop()
}

/// lexQuotedName lexes one part of a quoted key or table name.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexQuotedName L1
fn lex_quoted_name(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_whitespace(r) {
        lex_skip(lx, StateFn(lex_value))
    } else if r == '"' {
        lx.ignore(); // ignore the '"'
        st!(lex_string)
    } else if r == '\'' {
        lx.ignore(); // ignore the "'"
        st!(lex_raw_string)
    } else if r == EOF {
        lx.errorf("unexpected EOF; expected value", &[])
    } else {
        lx.errorf("expected value but found %q instead", &[rq(r)])
    }
}

/// lexKeyStart consumes all key parts until a '='.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexKeyStart L1
fn lex_key_start(lx: &mut Lexer) -> Option<StateFn> {
    lx.skip(is_whitespace);
    let r = lx.peek();
    if r == '=' || r == EOF {
        return lx.errorf("unexpected '=': key name appears blank", &[]);
    } else if r == '.' {
        return lx.errorf("unexpected '.': keys cannot start with a '.'", &[]);
    } else if r == '"' || r == '\'' {
        lx.ignore();
    }
    // Bare key
    lx.emit(ItemType::KeyStart);
    st!(lex_key_name_start)
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.lexKeyNameStart L1
fn lex_key_name_start(lx: &mut Lexer) -> Option<StateFn> {
    lx.skip(is_whitespace);
    let r = lx.peek();
    if r == '=' || r == EOF {
        lx.errorf("unexpected '='", &[])
    } else if r == '.' {
        lx.errorf("unexpected '.'", &[])
    } else if r == '"' || r == '\'' {
        lx.ignore();
        lx.push(StateFn(lex_key_end));
        st!(lex_quoted_name)
    } else {
        lx.push(StateFn(lex_key_end));
        st!(lex_bare_name)
    }
}

/// lexKeyEnd consumes the end of a key and trims whitespace (up to the key
/// separator).
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexKeyEnd L1
fn lex_key_end(lx: &mut Lexer) -> Option<StateFn> {
    lx.skip(is_whitespace);
    let r = lx.next();
    if is_whitespace(r) {
        lex_skip(lx, StateFn(lex_key_end))
    } else if r == EOF {
        lx.errorf("unexpected EOF; expected key separator '='", &[])
    } else if r == '.' {
        lx.ignore();
        st!(lex_key_name_start)
    } else if r == '=' {
        lx.emit(ItemType::KeyEnd);
        lex_skip(lx, StateFn(lex_value))
    } else {
        if r == '\n' {
            let msg = String::from_utf8_lossy(&gofmt::sprintf(
                "expected '.' or '=', but got %q instead",
                &[rq(r)],
            ))
            .into_owned();
            return lx.error_prev_line(LexErr::Msg(msg));
        }
        lx.errorf("expected '.' or '=', but got %q instead", &[rq(r)])
    }
}

/// lexValue starts the consumption of a value anywhere a value is expected.
/// lexValue will ignore whitespace.
/// After a value is lexed, the last state on the next is popped and returned.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexValue L1
fn lex_value(lx: &mut Lexer) -> Option<StateFn> {
    // We allow whitespace to precede a value, but NOT newlines.
    // In array syntax, the array states are responsible for ignoring newlines.
    let r = lx.next();
    if is_whitespace(r) {
        return lex_skip(lx, StateFn(lex_value));
    } else if is_digit(r) {
        lx.backup(); // avoid an extra state and use the same as above
        return st!(lex_number_or_date_start);
    }
    match r {
        '[' => {
            lx.ignore();
            lx.emit(ItemType::Array);
            return st!(lex_array_value);
        }
        '{' => {
            lx.ignore();
            lx.emit(ItemType::InlineTableStart);
            return st!(lex_inline_table_value);
        }
        '"' => {
            if lx.accept('"') {
                if lx.accept('"') {
                    lx.ignore(); // Ignore """
                    return st!(lex_multiline_string);
                }
                lx.backup();
            }
            lx.ignore(); // ignore the '"'
            return st!(lex_string);
        }
        '\'' => {
            if lx.accept('\'') {
                if lx.accept('\'') {
                    lx.ignore(); // Ignore """
                    return st!(lex_multiline_raw_string);
                }
                lx.backup();
            }
            lx.ignore(); // ignore the "'"
            return st!(lex_raw_string);
        }
        '.' => {
            // special error case, be kind to users
            return lx.errorf("floats must start with a digit, not '.'", &[]);
        }
        'i' | 'n' => {
            if (lx.accept('n') && lx.accept('f')) || (lx.accept('a') && lx.accept('n')) {
                lx.emit(ItemType::Float);
                return lx.pop();
            }
        }
        '-' | '+' => return st!(lex_decimal_number_start),
        _ => {}
    }
    if gostd::is_letter(r) {
        // Be permissive here; lexBool will give a nice error if the
        // user wrote something like
        //   x = foo
        // (i.e. not 'true' or 'false' but is something else word-like.)
        lx.backup();
        return st!(lex_bool);
    }
    if r == EOF {
        return lx.errorf("unexpected EOF; expected value", &[]);
    }
    if r == '\n' {
        let msg = String::from_utf8_lossy(&gofmt::sprintf(
            "expected value but found %q instead",
            &[rq(r)],
        ))
        .into_owned();
        return lx.error_prev_line(LexErr::Msg(msg));
    }
    lx.errorf("expected value but found %q instead", &[rq(r)])
}

/// lexArrayValue consumes one value in an array. It assumes that '[' or ','
/// have already been consumed. All whitespace and newlines are ignored.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexArrayValue L1
fn lex_array_value(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_whitespace(r) || is_nl(r) {
        return lex_skip(lx, StateFn(lex_array_value));
    } else if r == '#' {
        lx.push(StateFn(lex_array_value));
        return st!(lex_comment_start);
    } else if r == ',' {
        return lx.errorf("unexpected comma", &[]);
    } else if r == ']' {
        return st!(lex_array_end);
    }
    lx.backup();
    lx.push(StateFn(lex_array_value_end));
    st!(lex_value)
}

/// lexArrayValueEnd consumes everything between the end of an array value and
/// the next value (or the end of the array): it ignores whitespace and newlines
/// and expects either a ',' or a ']'.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexArrayValueEnd L1
fn lex_array_value_end(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_whitespace(r) || is_nl(r) {
        lex_skip(lx, StateFn(lex_array_value_end))
    } else if r == '#' {
        lx.push(StateFn(lex_array_value_end));
        st!(lex_comment_start)
    } else if r == ',' {
        lx.ignore();
        st!(lex_array_value) // move on to the next value
    } else if r == ']' {
        st!(lex_array_end)
    } else {
        lx.errorf(
            "expected a comma (',') or array terminator (']'), but got %s",
            &[sq(rune_or_eof(r))],
        )
    }
}

/// lexArrayEnd finishes the lexing of an array.
/// It assumes that a ']' has just been consumed.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexArrayEnd L1
fn lex_array_end(lx: &mut Lexer) -> Option<StateFn> {
    lx.ignore();
    lx.emit(ItemType::ArrayEnd);
    lx.pop()
}

/// lexInlineTableValue consumes one key/value pair in an inline table.
/// It assumes that '{' or ',' have already been consumed. Whitespace is ignored.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexInlineTableValue L1
fn lex_inline_table_value(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_whitespace(r) {
        return lex_skip(lx, StateFn(lex_inline_table_value));
    } else if is_nl(r) {
        if lx.toml_next {
            return lex_skip(lx, StateFn(lex_inline_table_value));
        }
        return lx.error_prev_line(LexErr::InlineTableNL);
    } else if r == '#' {
        lx.push(StateFn(lex_inline_table_value));
        return st!(lex_comment_start);
    } else if r == ',' {
        return lx.errorf("unexpected comma", &[]);
    } else if r == '}' {
        return st!(lex_inline_table_end);
    }
    lx.backup();
    lx.push(StateFn(lex_inline_table_value_end));
    st!(lex_key_start)
}

/// lexInlineTableValueEnd consumes everything between the end of an inline table
/// key/value pair and the next pair (or the end of the table):
/// it ignores whitespace and expects either a ',' or a '}'.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexInlineTableValueEnd L1
fn lex_inline_table_value_end(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_whitespace(r) {
        lex_skip(lx, StateFn(lex_inline_table_value_end))
    } else if is_nl(r) {
        if lx.toml_next {
            return lex_skip(lx, StateFn(lex_inline_table_value_end));
        }
        lx.error_prev_line(LexErr::InlineTableNL)
    } else if r == '#' {
        lx.push(StateFn(lex_inline_table_value_end));
        st!(lex_comment_start)
    } else if r == ',' {
        lx.ignore();
        lx.skip(is_whitespace);
        if lx.peek() == '}' {
            if lx.toml_next {
                return st!(lex_inline_table_value_end);
            }
            return lx.errorf("trailing comma not allowed in inline tables", &[]);
        }
        st!(lex_inline_table_value)
    } else if r == '}' {
        st!(lex_inline_table_end)
    } else {
        lx.errorf(
            "expected a comma or an inline table terminator '}', but got %s instead",
            &[sq(rune_or_eof(r))],
        )
    }
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.runeOrEOF L1
fn rune_or_eof(r: char) -> String {
    if r == EOF {
        return "end of file".to_owned();
    }
    format!("'{r}'")
}

/// lexInlineTableEnd finishes the lexing of an inline table.
/// It assumes that a '}' has just been consumed.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexInlineTableEnd L1
fn lex_inline_table_end(lx: &mut Lexer) -> Option<StateFn> {
    lx.ignore();
    lx.emit(ItemType::InlineTableEnd);
    lx.pop()
}

/// lexString consumes the inner contents of a string. It assumes that the
/// beginning '"' has already been consumed and ignored.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexString L1
fn lex_string(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if r == EOF {
        lx.errorf("unexpected EOF; expected '\"'", &[])
    } else if is_nl(r) {
        lx.error_prev_line(LexErr::StringNL)
    } else if r == '\\' {
        lx.push(StateFn(lex_string));
        st!(lex_string_escape)
    } else if r == '"' {
        lx.backup();
        if lx.esc {
            lx.esc = false;
            lx.emit(ItemType::StringEsc);
        } else {
            lx.emit(ItemType::String);
        }
        lx.next();
        lx.ignore();
        lx.pop()
    } else {
        st!(lex_string)
    }
}

/// lexMultilineString consumes the inner contents of a string. It assumes that
/// the beginning '"""' has already been consumed and ignored.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexMultilineString L1
fn lex_multiline_string(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    match r {
        EOF => lx.errorf("unexpected EOF; expected '\"\"\"'", &[]),
        '\\' => st!(lex_multiline_string_escape),
        '"' => {
            // Check if the next two runes are "
            if lx.accept('"') {
                if lx.accept('"') {
                    // Check if the next rune is " (four or five quotes)
                    if lx.peek() == '"' {
                        // Check for six or more quotes
                        let cur = lx.current();
                        if cur.ends_with("\"\"\"\"\"") && !cur.ends_with("\\\"\"\"\"\"") {
                            return lx.errorf("unexpected '\"\"\"\"\"\"'", &[]);
                        }
                        lx.backup();
                        lx.backup();
                        return st!(lex_multiline_string);
                    }
                    lx.backup(); // backup: don't include the """ in the item.
                    lx.backup();
                    lx.backup();
                    lx.esc = false;
                    lx.emit(ItemType::MultilineString);
                    lx.next(); // Read over ''' again and discard it.
                    lx.next();
                    lx.next();
                    lx.ignore();
                    return lx.pop();
                }
                lx.backup();
            }
            st!(lex_multiline_string)
        }
        _ => st!(lex_multiline_string),
    }
}

/// lexRawString consumes a raw string. Nothing can be escaped in such a string.
/// It assumes that the beginning "'" has already been consumed and ignored.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexRawString L1
fn lex_raw_string(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if r == EOF {
        lx.errorf("unexpected EOF; expected \"'\"", &[])
    } else if is_nl(r) {
        lx.error_prev_line(LexErr::StringNL)
    } else if r == '\'' {
        lx.backup();
        lx.emit(ItemType::RawString);
        lx.next();
        lx.ignore();
        lx.pop()
    } else {
        st!(lex_raw_string)
    }
}

/// lexMultilineRawString consumes a raw string. Nothing can be escaped in such a
/// string. It assumes that the beginning triple-' has already been consumed and
/// ignored.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexMultilineRawString L1
fn lex_multiline_raw_string(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    match r {
        EOF => lx.errorf("unexpected EOF; expected \"'''\"", &[]),
        '\'' => {
            // Check if the next two runes are '
            if lx.accept('\'') {
                if lx.accept('\'') {
                    // Check if the next rune is ' (four or five quotes)
                    if lx.peek() == '\'' {
                        // Check for six or more quotes
                        if lx.current().ends_with("'''''") {
                            return lx.errorf("unexpected \"''''''\"", &[]);
                        }
                        lx.backup();
                        lx.backup();
                        return st!(lex_multiline_raw_string);
                    }
                    lx.backup(); // backup: don't include the ''' in the item.
                    lx.backup();
                    lx.backup();
                    lx.emit(ItemType::RawMultilineString);
                    lx.next(); // Read over ''' again and discard it.
                    lx.next();
                    lx.next();
                    lx.ignore();
                    return lx.pop();
                }
                lx.backup();
            }
            st!(lex_multiline_raw_string)
        }
        _ => st!(lex_multiline_raw_string),
    }
}

/// lexMultilineStringEscape consumes an escaped character. It assumes that the
/// preceding '\\' has already been consumed.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexMultilineStringEscape L1
fn lex_multiline_string_escape(lx: &mut Lexer) -> Option<StateFn> {
    if is_nl(lx.next()) {
        // \ escaping newline.
        return st!(lex_multiline_string);
    }
    lx.backup();
    lx.push(StateFn(lex_multiline_string));
    lex_string_escape(lx)
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.lexStringEscape L1
fn lex_string_escape(lx: &mut Lexer) -> Option<StateFn> {
    lx.esc = true;
    let r = lx.next();
    match r {
        'e' => {
            if !lx.toml_next {
                return lx.error(LexErr::Escape(r));
            }
            lx.pop()
        }
        'b' | 't' | 'n' | 'f' | 'r' | '"' | ' ' | '\t' | '\\' => lx.pop(),
        'x' => {
            if !lx.toml_next {
                return lx.error(LexErr::Escape(r));
            }
            st!(lex_hex_escape)
        }
        'u' => st!(lex_short_unicode_escape),
        'U' => st!(lex_long_unicode_escape),
        _ => lx.error(LexErr::Escape(r)),
    }
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.lexHexEscape L1
fn lex_hex_escape(lx: &mut Lexer) -> Option<StateFn> {
    for _ in 0..2 {
        let r = lx.next();
        if !is_hex(r) {
            let cur = lx.current();
            return lx.errorf(
                "expected two hexadecimal digits after '\\x', but got %q instead",
                &[sq(cur)],
            );
        }
    }
    lx.pop()
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.lexShortUnicodeEscape L1
fn lex_short_unicode_escape(lx: &mut Lexer) -> Option<StateFn> {
    for _ in 0..4 {
        let r = lx.next();
        if !is_hex(r) {
            let cur = lx.current();
            return lx.errorf(
                "expected four hexadecimal digits after '\\u', but got %q instead",
                &[sq(cur)],
            );
        }
    }
    lx.pop()
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.lexLongUnicodeEscape L1
fn lex_long_unicode_escape(lx: &mut Lexer) -> Option<StateFn> {
    for _ in 0..8 {
        let r = lx.next();
        if !is_hex(r) {
            let cur = lx.current();
            return lx.errorf(
                "expected eight hexadecimal digits after '\\U', but got %q instead",
                &[sq(cur)],
            );
        }
    }
    lx.pop()
}

/// lexNumberOrDateStart processes the first character of a value which begins
/// with a digit. It exists to catch values starting with '0', so that
/// lexBaseNumberOrDate can differentiate base prefixed integers from other
/// types.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexNumberOrDateStart L1
fn lex_number_or_date_start(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if r == '0' {
        return st!(lex_base_number_or_date);
    }
    if !is_digit(r) {
        // The only way to reach this state is if the value starts
        // with a digit, so specifically treat anything else as an
        // error.
        return lx.errorf("expected a digit but got %q", &[rq(r)]);
    }
    st!(lex_number_or_date)
}

/// lexNumberOrDate consumes either an integer, float or datetime.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexNumberOrDate L1
fn lex_number_or_date(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_digit(r) {
        return st!(lex_number_or_date);
    }
    match r {
        '-' | ':' => return st!(lex_datetime),
        '_' => return st!(lex_decimal_number),
        '.' | 'e' | 'E' => return st!(lex_float),
        _ => {}
    }
    lx.backup();
    lx.emit(ItemType::Integer);
    lx.pop()
}

/// lexDatetime consumes a Datetime, to a first approximation.
/// The parser validates that it matches one of the accepted formats.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexDatetime L1
fn lex_datetime(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_digit(r) {
        return st!(lex_datetime);
    }
    if matches!(r, '-' | ':' | 'T' | 't' | ' ' | '.' | 'Z' | 'z' | '+') {
        return st!(lex_datetime);
    }
    lx.backup();
    lx.emit_trim(ItemType::Datetime); // TODO: remove trim?
    lx.pop()
}

/// lexHexInteger consumes a hexadecimal integer after seeing the '0x' prefix.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexHexInteger L1
fn lex_hex_integer(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_hex(r) || r == '_' {
        return st!(lex_hex_integer);
    }
    lx.backup();
    lx.emit(ItemType::Integer);
    lx.pop()
}

/// lexOctalInteger consumes an octal integer after seeing the '0o' prefix.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexOctalInteger L1
fn lex_octal_integer(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_octal(r) || r == '_' {
        return st!(lex_octal_integer);
    }
    lx.backup();
    lx.emit(ItemType::Integer);
    lx.pop()
}

/// lexBinaryInteger consumes a binary integer after seeing the '0b' prefix.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexBinaryInteger L1
fn lex_binary_integer(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_binary(r) || r == '_' {
        return st!(lex_binary_integer);
    }
    lx.backup();
    lx.emit(ItemType::Integer);
    lx.pop()
}

/// lexDecimalNumber consumes a decimal float or integer.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexDecimalNumber L1
fn lex_decimal_number(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_digit(r) {
        return st!(lex_decimal_number);
    }
    match r {
        '.' | 'e' | 'E' => return st!(lex_float),
        '_' => return st!(lex_decimal_number),
        _ => {}
    }
    lx.backup();
    lx.emit(ItemType::Integer);
    lx.pop()
}

/// lexDecimalNumber consumes the first digit of a number beginning with a sign.
/// It assumes the sign has already been consumed. Values which start with a sign
/// are only allowed to be decimal integers or floats.
///
/// The special "nan" and "inf" values are also recognized.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexDecimalNumberStart L1
fn lex_decimal_number_start(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    // Special error cases to give users better error messages
    match r {
        'i' => {
            if !lx.accept('n') || !lx.accept('f') {
                let cur = lx.current();
                return lx.errorf("invalid float: '%s'", &[sq(cur)]);
            }
            lx.emit(ItemType::Float);
            return lx.pop();
        }
        'n' => {
            if !lx.accept('a') || !lx.accept('n') {
                let cur = lx.current();
                return lx.errorf("invalid float: '%s'", &[sq(cur)]);
            }
            lx.emit(ItemType::Float);
            return lx.pop();
        }
        '0' => {
            let p = lx.peek();
            if matches!(p, 'b' | 'o' | 'x') {
                let cur = lx.current();
                return lx.errorf(
                    "cannot use sign with non-decimal numbers: '%s%c'",
                    &[sq(cur), rq(p)],
                );
            }
        }
        '.' => return lx.errorf("floats must start with a digit, not '.'", &[]),
        _ => {}
    }
    if is_digit(r) {
        return st!(lex_decimal_number);
    }
    lx.errorf("expected a digit but got %q", &[rq(r)])
}

/// lexBaseNumberOrDate differentiates between the possible values which
/// start with '0'. It assumes that before reaching this state, the initial '0'
/// has been consumed.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexBaseNumberOrDate L1
fn lex_base_number_or_date(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    // Note: All datetimes start with at least two digits, so we don't
    // handle date characters (':', '-', etc.) here.
    if is_digit(r) {
        return st!(lex_number_or_date);
    }
    match r {
        '_' => return st!(lex_decimal_number),
        '.' | 'e' | 'E' => return st!(lex_float),
        'b' => {
            let r = lx.peek();
            if !is_binary(r) {
                let cur = lx.current();
                lx.errorf("not a binary number: '%s%c'", &[sq(cur), rq(r)]);
            }
            return st!(lex_binary_integer);
        }
        'o' => {
            let r = lx.peek();
            if !is_octal(r) {
                let cur = lx.current();
                lx.errorf("not an octal number: '%s%c'", &[sq(cur), rq(r)]);
            }
            return st!(lex_octal_integer);
        }
        'x' => {
            let r = lx.peek();
            if !is_hex(r) {
                let cur = lx.current();
                lx.errorf("not a hexadecimal number: '%s%c'", &[sq(cur), rq(r)]);
            }
            return st!(lex_hex_integer);
        }
        _ => {}
    }
    lx.backup();
    lx.emit(ItemType::Integer);
    lx.pop()
}

/// lexFloat consumes the elements of a float. It allows any sequence of
/// float-like characters, so floats emitted by the lexer are only a first
/// approximation and must be validated by the parser.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexFloat L1
fn lex_float(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_digit(r) {
        return st!(lex_float);
    }
    if matches!(r, '_' | '.' | '-' | '+' | 'e' | 'E') {
        return st!(lex_float);
    }
    lx.backup();
    lx.emit(ItemType::Float);
    lx.pop()
}

/// lexBool consumes a bool string: 'true' or 'false.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexBool L1
fn lex_bool(lx: &mut Lexer) -> Option<StateFn> {
    let mut rs = String::new();
    loop {
        let r = lx.next();
        if !gostd::is_letter(r) {
            lx.backup();
            break;
        }
        rs.push(r);
    }
    match rs.as_str() {
        "true" | "false" => {
            lx.emit(ItemType::Bool);
            lx.pop()
        }
        _ => lx.errorf("expected value but found %q instead", &[sq(rs)]),
    }
}

/// lexCommentStart begins the lexing of a comment. It will emit
/// itemCommentStart and consume no characters, passing control to lexComment.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexCommentStart L1
fn lex_comment_start(lx: &mut Lexer) -> Option<StateFn> {
    lx.ignore();
    lx.emit(ItemType::CommentStart);
    st!(lex_comment)
}

/// lexComment lexes an entire comment. It assumes that '#' has been consumed.
/// It will consume *up to* the first newline character, and pass control
/// back to the last state on the stack.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexComment L1
fn lex_comment(lx: &mut Lexer) -> Option<StateFn> {
    let r = lx.next();
    if is_nl(r) || r == EOF {
        lx.backup();
        lx.emit(ItemType::Text);
        return lx.pop();
    }
    st!(lex_comment)
}

/// lexSkip ignores all slurped input and moves on to the next state.
// go-dep: github.com/BurntSushi/toml@v1.5.0.lexSkip L1
fn lex_skip(lx: &mut Lexer, next_state: StateFn) -> Option<StateFn> {
    lx.ignore();
    Some(next_state)
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.isWhitespace L1
fn is_whitespace(r: char) -> bool {
    r == '\t' || r == ' '
}
// go-dep: github.com/BurntSushi/toml@v1.5.0.isNL L1
fn is_nl(r: char) -> bool {
    r == '\n' || r == '\r'
}
/// Control characters except \t, \r, \n
// go-dep: github.com/BurntSushi/toml@v1.5.0.isControl L1
fn is_control(r: char) -> bool {
    match r {
        '\t' | '\r' | '\n' => false,
        _ => (r as u32) <= 0x1f || r as u32 == 0x7f,
    }
}
// go-dep: github.com/BurntSushi/toml@v1.5.0.isDigit L1
fn is_digit(r: char) -> bool {
    r.is_ascii_digit()
}
// go-dep: github.com/BurntSushi/toml@v1.5.0.isBinary L1
fn is_binary(r: char) -> bool {
    r == '0' || r == '1'
}
// go-dep: github.com/BurntSushi/toml@v1.5.0.isOctal L1
fn is_octal(r: char) -> bool {
    ('0'..='7').contains(&r)
}
// go-dep: github.com/BurntSushi/toml@v1.5.0.isHex L1
pub(crate) fn is_hex(r: char) -> bool {
    let l = r as u32 | 0x20;
    r.is_ascii_digit() || ((u32::from(b'a')..=u32::from(b'f')).contains(&l))
}
// go-dep: github.com/BurntSushi/toml@v1.5.0.isBareKeyChar L1
fn is_bare_key_char(r: char, _toml_next: bool) -> bool {
    r.is_ascii_uppercase() || r.is_ascii_lowercase() || r.is_ascii_digit() || r == '_' || r == '-'
}
