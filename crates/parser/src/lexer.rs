//! Hand-written lexer producing the token stream the LR driver consumes.
//!
//! Mirrors the *token classification* rules of TiDB `lexer.go`/`misc.go` that matter for
//! grammar acceptance (not literal value conversion):
//!   * keywords from `tokenMap` (case-insensitive), not when adjacent to '.'
//!   * `btFuncTokenMap` keywords only when immediately followed by '('
//!   * `windowFuncTokenMap` keywords only when window functions are enabled
//!   * multi-token fusion: AS OF, MEMBER OF, TO TIMESTAMP <str>, TO TSO <int>,
//!     OPTIONALLY ENCLOSED BY, FULL OUTER JOIN
//!   * `||` -> pipesAsOr (default sql_mode), `_charset'str'` / N'str' -> underscoreCS
//!   * `/*! ... */`, `/*T![..] ... */` are transparent; `/*+ ... */` -> hintComment after
//!     SELECT/INSERT/REPLACE/UPDATE/DELETE/CREATE/PARTITION, otherwise a comment
//!   * @var / @@[global.|session.|local.]var, numbers (int/dec/float/hex/bit), strings, `quoted`
//!
//! Not handled (deliberately out of scope): ANSI_QUOTES / HIGH_NOT_PRECEDENCE / IGNORE_SPACE
//! sql modes, literal value conversion, client charset decoding, `/*T!` feature gating.
use std::collections::{HashMap, HashSet};

use crate::keywords_gen::{BUILTIN_FUNCS, HINTED_TOKENS, KEYWORDS, WINDOW_FUNCS};
use crate::token_map::TOKEN_IDS;

#[derive(Debug, Clone)]
pub struct LexError {
    pub pos: usize,
    pub msg: String,
}

pub struct TokenTable {
    pub by_name: HashMap<&'static str, u32>,
    pub names: HashMap<u32, &'static str>,
    keywords: HashMap<&'static str, u32>,
    builtins: HashMap<&'static str, u32>,
    windows: HashMap<&'static str, u32>,
    hinted: HashSet<u32>,
    t: Ids,
}

/// Frequently used token ids.
#[derive(Clone, Copy)]
struct Ids {
    identifier: u32,
    string_lit: u32,
    int_lit: u32,
    dec_lit: u32,
    float_lit: u32,
    hex_lit: u32,
    bit_lit: u32,
    single_at: u32,
    double_at: u32,
    underscore_cs: u32,
    hint_comment: u32,
    invalid: u32,
    null: u32,
    pipes_as_or: u32,
    andand: u32,
    andnot: u32,
    assignment_eq: u32,
    eq: u32,
    ge: u32,
    le: u32,
    jss: u32,
    juss: u32,
    lsh: u32,
    rsh: u32,
    neq: u32,
    neq_synonym: u32,
    nulleq: u32,
    param_marker: u32,
    // fusion
    as_: u32,
    of: u32,
    member: u32,
    to: u32,
    timestamp_type: u32,
    tso_type: u32,
    optionally: u32,
    enclosed: u32,
    by: u32,
    full: u32,
    outer: u32,
    join: u32,
    asof: u32,
    memberof: u32,
    to_timestamp: u32,
    to_tso: u32,
    optionally_enclosed_by: u32,
    full_join_type: u32,
}

impl TokenTable {
    pub fn new() -> Self {
        let by_name: HashMap<&'static str, u32> = TOKEN_IDS.iter().copied().collect();
        let names = by_name.iter().map(|(k, v)| (*v, *k)).collect();
        let id = |n: &str| -> u32 {
            *by_name
                .get(n)
                .unwrap_or_else(|| by_name.get("invalid").expect("invalid token"))
        };
        let map = |tbl: &[(&'static str, &'static str)]| -> HashMap<&'static str, u32> {
            tbl.iter()
                .filter_map(|(k, v)| by_name.get(v).map(|id| (*k, *id)))
                .collect()
        };
        let t = Ids {
            identifier: id("identifier"),
            string_lit: id("stringLit"),
            int_lit: id("intLit"),
            dec_lit: id("decLit"),
            float_lit: id("floatLit"),
            hex_lit: id("hexLit"),
            bit_lit: id("bitLit"),
            single_at: id("singleAtIdentifier"),
            double_at: id("doubleAtIdentifier"),
            underscore_cs: id("underscoreCS"),
            hint_comment: id("hintComment"),
            invalid: id("invalid"),
            null: id("null"),
            pipes_as_or: id("pipesAsOr"),
            andand: id("andand"),
            andnot: id("andnot"),
            assignment_eq: id("assignmentEq"),
            eq: id("eq"),
            ge: id("ge"),
            le: id("le"),
            jss: id("jss"),
            juss: id("juss"),
            lsh: id("lsh"),
            rsh: id("rsh"),
            neq: id("neq"),
            neq_synonym: id("neqSynonym"),
            nulleq: id("nulleq"),
            param_marker: id("paramMarker"),
            as_: id("as"),
            of: id("of"),
            member: id("member"),
            to: id("to"),
            timestamp_type: id("timestampType"),
            tso_type: id("tsoType"),
            optionally: id("optionally"),
            enclosed: id("enclosed"),
            by: id("by"),
            full: id("full"),
            outer: id("outer"),
            join: id("join"),
            asof: id("asof"),
            memberof: id("memberof"),
            to_timestamp: id("toTimestamp"),
            to_tso: id("toTSO"),
            optionally_enclosed_by: id("optionallyEnclosedBy"),
            full_join_type: id("fullJoinType"),
        };
        let hinted = HINTED_TOKENS
            .iter()
            .filter_map(|n| by_name.get(n).copied())
            .collect();
        TokenTable {
            keywords: map(KEYWORDS),
            builtins: map(BUILTIN_FUNCS),
            windows: map(WINDOW_FUNCS),
            hinted,
            by_name,
            names,
            t,
        }
    }

    pub fn name(&self, id: u32) -> &'static str {
        self.names.get(&id).copied().unwrap_or("?")
    }
}

impl Default for TokenTable {
    fn default() -> Self {
        Self::new()
    }
}

/// One token: grammar terminal id plus its byte range in the source.
#[derive(Clone, Copy, Debug)]
pub struct Tok {
    pub id: u32,
    pub start: usize,
    pub end: usize,
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn is_user_var_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'.' || c >= 0x80
}

pub struct Lexer<'a> {
    tt: &'a TokenTable,
    s: &'a [u8],
    src: &'a str,
    p: usize,
    in_bang: bool,
    pub window_funcs: bool,
    out: Vec<Tok>,
    upper: String,
}

impl<'a> Lexer<'a> {
    pub fn new(tt: &'a TokenTable, src: &'a str, window_funcs: bool) -> Self {
        Lexer {
            tt,
            s: src.as_bytes(),
            src,
            p: 0,
            in_bang: false,
            window_funcs,
            out: Vec::with_capacity(src.len() / 3),
            upper: String::new(),
        }
    }

    fn peek(&self) -> u8 {
        *self.s.get(self.p).unwrap_or(&0)
    }
    fn peek_at(&self, k: usize) -> u8 {
        *self.s.get(self.p + k).unwrap_or(&0)
    }
    fn push(&mut self, id: u32, start: usize) {
        self.out.push(Tok {
            id,
            start,
            end: self.p,
        });
    }
    fn err(&self, msg: &str) -> LexError {
        LexError {
            pos: self.p,
            msg: msg.to_string(),
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Tok>, LexError> {
        let t = self.tt.t;
        loop {
            // whitespace
            while self.p < self.s.len() && self.s[self.p].is_ascii_whitespace() {
                self.p += 1;
            }
            if self.p >= self.s.len() {
                break;
            }
            let start = self.p;
            let c = self.peek();
            match c {
                b'#' => self.skip_line(),
                b'-' if self.peek_at(1) == b'-'
                    && (self.p + 2 >= self.s.len() || self.peek_at(2).is_ascii_whitespace()) =>
                {
                    self.skip_line()
                }
                b'-' if self.peek_at(1) == b'>' => {
                    if self.peek_at(2) == b'>' {
                        self.p += 3;
                        self.push(t.juss, start)
                    } else {
                        self.p += 2;
                        self.push(t.jss, start)
                    }
                }
                b'/' if self.peek_at(1) == b'*' => self.comment()?,
                b'*' if self.in_bang && self.peek_at(1) == b'/' => {
                    self.in_bang = false;
                    self.p += 2;
                }
                b'@' => self.at()?,
                b'\'' | b'"' => {
                    self.string()?;
                    self.push(t.string_lit, start)
                }
                b'`' => {
                    self.quoted_ident()?;
                    self.push(t.identifier, start)
                }
                b'0'..=b'9' => self.number(),
                b'.' if self.peek_at(1).is_ascii_digit() && !self.ident_before_dot() => {
                    self.number()
                }
                b'X' | b'x' if self.peek_at(1) == b'\'' => {
                    self.p += 2;
                    self.digits_then_quote(|c| c.is_ascii_hexdigit())?;
                    // ParseHexStr: x'..' needs an even number of digits ("hex literal" error).
                    if !(self.p - start - 3).is_multiple_of(2) {
                        return Err(LexError {
                            pos: start,
                            msg: "invalid hexadecimal format, must even numbers".into(),
                        });
                    }
                    self.push(t.hex_lit, start)
                }
                b'B' | b'b' if self.peek_at(1) == b'\'' => {
                    self.p += 2;
                    self.digits_then_quote(|c| c == b'0' || c == b'1')?;
                    self.push(t.bit_lit, start)
                }
                b'N' | b'n' if self.peek_at(1) == b'\'' => {
                    self.p += 1;
                    self.push(t.underscore_cs, start)
                }
                b'\\' if self.peek_at(1) == b'N' => {
                    self.p += 2;
                    self.push(t.null, start)
                }
                c if is_ident_char(c) => self.ident(),
                _ => self.operator()?,
            }
        }
        Ok(self.fuse())
    }

    fn skip_line(&mut self) {
        while self.p < self.s.len() && self.s[self.p] != b'\n' {
            self.p += 1;
        }
    }

    fn comment(&mut self) -> Result<(), LexError> {
        let start = self.p;
        self.p += 2;
        match self.peek() {
            b'!' => {
                self.p += 1;
                let mut n = 0;
                while n < 5 && self.peek().is_ascii_digit() {
                    self.p += 1;
                    n += 1;
                }
                self.in_bang = true;
                return Ok(());
            }
            b'T' if self.peek_at(1) == b'!' => {
                self.p += 2;
                if self.peek() == b'[' {
                    while self.p < self.s.len() && self.s[self.p] != b']' {
                        self.p += 1;
                    }
                    self.p += 1;
                }
                self.in_bang = true;
                return Ok(());
            }
            _ => {}
        }
        let is_hint = self.peek() == b'+'
            && self
                .out
                .last()
                .is_some_and(|tk| self.tt.hinted.contains(&tk.id));
        match self.src[self.p..].find("*/") {
            Some(k) => self.p += k + 2,
            None => return Err(self.err("unterminated comment")),
        }
        if is_hint {
            self.push(self.tt.t.hint_comment, start);
        }
        Ok(())
    }

    fn string(&mut self) -> Result<(), LexError> {
        let q = self.peek();
        self.p += 1;
        while self.p < self.s.len() {
            let c = self.s[self.p];
            if c == b'\\' {
                self.p += 2;
                continue;
            }
            self.p += 1;
            if c == q {
                if self.peek() == q {
                    self.p += 1;
                    continue;
                }
                return Ok(());
            }
        }
        Err(self.err("unterminated string"))
    }

    /// Body of X'..' / B'..' literals: only valid digits, then the closing quote.
    fn digits_then_quote(&mut self, ok: impl Fn(u8) -> bool) -> Result<(), LexError> {
        while ok(self.peek()) {
            self.p += 1;
        }
        if self.peek() != b'\'' {
            return Err(self.err("invalid hex/bit literal"));
        }
        self.p += 1;
        Ok(())
    }

    fn quoted_ident(&mut self) -> Result<(), LexError> {
        self.p += 1;
        while self.p < self.s.len() {
            let c = self.s[self.p];
            self.p += 1;
            if c == b'`' {
                if self.peek() == b'`' {
                    self.p += 1;
                    continue;
                }
                return Ok(());
            }
        }
        Err(self.err("unterminated quoted identifier"))
    }

    fn at(&mut self) -> Result<(), LexError> {
        let t = self.tt.t;
        let start = self.p;
        self.p += 1;
        if self.peek() == b'@' {
            self.p += 1;
            for pre in ["global.", "session.", "local."] {
                if self.src.len() >= self.p + pre.len()
                    && self.src[self.p..self.p + pre.len()].eq_ignore_ascii_case(pre)
                {
                    self.p += pre.len();
                    break;
                }
            }
            match self.peek() {
                b'\'' | b'"' => self.string()?,
                b'`' => self.quoted_ident()?,
                c if is_user_var_char(c) => {
                    while is_user_var_char(self.peek()) {
                        self.p += 1
                    }
                }
                _ => return Err(self.err("bad @@ variable")),
            }
            self.push(t.double_at, start);
            return Ok(());
        }
        match self.peek() {
            b'\'' | b'"' => self.string()?,
            b'`' => self.quoted_ident()?,
            c if is_user_var_char(c) => {
                while is_user_var_char(self.peek()) {
                    self.p += 1
                }
            }
            // TiDB: a bare '@' (followed by a non-variable char) is still a singleAtIdentifier
            _ => {}
        }
        self.push(t.single_at, start);
        Ok(())
    }

    /// TiDB's `identifierDot`: the previous token is '.' directly after an identifier.
    fn after_ident_dot(&self) -> bool {
        let n = self.out.len();
        n >= 2
            && self.tt.name(self.out[n - 1].id) == "."
            && self.out[n - 2].id == self.tt.t.identifier
            && self.out[n - 2].end == self.out[n - 1].start
    }

    /// The '.' at the cursor directly follows an identifier (so it is a qualifier dot).
    fn ident_before_dot(&self) -> bool {
        self.out
            .last()
            .is_some_and(|t| t.id == self.tt.t.identifier && t.end == self.p)
    }

    fn number(&mut self) {
        let t = self.tt.t;
        let start = self.p;
        if self.after_ident_dot() {
            while is_ident_char(self.peek()) {
                self.p += 1
            }
            return self.push(t.identifier, start);
        }
        if self.peek() == b'0' && matches!(self.peek_at(1), b'x' | b'b') {
            let hex = self.peek_at(1) == b'x';
            self.p += 2;
            let d0 = self.p;
            while if hex {
                self.peek().is_ascii_hexdigit()
            } else {
                matches!(self.peek(), b'0' | b'1')
            } {
                self.p += 1
            }
            if self.p == d0 || is_ident_char(self.peek()) {
                while is_ident_char(self.peek()) {
                    self.p += 1
                }
                return self.push(t.identifier, start);
            }
            return self.push(if hex { t.hex_lit } else { t.bit_lit }, start);
        }
        while self.peek().is_ascii_digit() {
            self.p += 1
        }
        let mut tok = t.int_lit;
        if self.peek() == b'.' {
            self.p += 1;
            while self.peek().is_ascii_digit() {
                self.p += 1
            }
            tok = t.dec_lit;
        }
        if matches!(self.peek(), b'e' | b'E') {
            let save = self.p;
            self.p += 1;
            if matches!(self.peek(), b'+' | b'-') {
                self.p += 1
            }
            if self.peek().is_ascii_digit() {
                while self.peek().is_ascii_digit() {
                    self.p += 1
                }
                tok = t.float_lit;
            } else {
                self.p = save;
                while is_ident_char(self.peek()) {
                    self.p += 1
                }
                return self.push(t.identifier, start);
            }
        }
        if tok == t.int_lit && is_ident_char(self.peek()) {
            while is_ident_char(self.peek()) {
                self.p += 1
            }
            return self.push(t.identifier, start);
        }
        // Go's toInt: an integer beyond uint64 is a decimal literal.
        if tok == t.int_lit && self.src[start..self.p].parse::<u64>().is_err() {
            tok = t.dec_lit;
        }
        self.push(tok, start)
    }

    fn ident(&mut self) {
        let t = self.tt.t;
        let start = self.p;
        while is_ident_char(self.peek()) {
            self.p += 1
        }
        let lit = &self.src[start..self.p];
        // _charset introducer (handleIdent: any charset GetCharsetInfo knows, even unsupported)
        if let Some(cs) = lit.strip_prefix('_')
            && crate::charset::introducer_charset(cs).is_some()
        {
            return self.push(t.underscore_cs, start);
        }
        // qualified name parts are never keywords
        let next_dot = self.peek() == b'.';
        let prev_dot = self.src[..start].trim_end_matches(' ').ends_with('.');
        if next_dot || prev_dot {
            return self.push(t.identifier, start);
        }
        self.upper.clear();
        self.upper.push_str(lit);
        self.upper.make_ascii_uppercase();
        let up = self.upper.as_str();
        let mut id = None;
        if self.peek() == b'(' {
            id = self.tt.builtins.get(up).copied();
        }
        if id.is_none() {
            id = self.tt.keywords.get(up).copied();
        }
        if id.is_none() && self.window_funcs {
            id = self.tt.windows.get(up).copied();
        }
        self.push(id.unwrap_or(t.identifier), start)
    }

    fn char_tok(&self, c: u8) -> u32 {
        let s = [c];
        let s = std::str::from_utf8(&s).unwrap_or("?");
        self.tt.by_name.get(s).copied().unwrap_or(self.tt.t.invalid)
    }

    fn operator(&mut self) -> Result<(), LexError> {
        let t = self.tt.t;
        let start = self.p;
        let rest = &self.s[self.p..];
        let two: &[(&[u8], u32)] = &[
            (b"<=>", t.nulleq),
            (b"||", t.pipes_as_or),
            (b"&&", t.andand),
            (b"&^", t.andnot),
            (b":=", t.assignment_eq),
            (b">=", t.ge),
            (b"<=", t.le),
            (b"!=", t.neq),
            (b"<>", t.neq_synonym),
            (b"<<", t.lsh),
            (b">>", t.rsh),
        ];
        for (pat, id) in two {
            if rest.starts_with(pat) {
                self.p += pat.len();
                self.push(*id, start);
                return Ok(());
            }
        }
        let c = rest[0];
        let id = match c {
            b'=' => t.eq,
            b'?' => t.param_marker,
            b'(' | b')' | b',' | b'.' | b'*' | b'+' | b'-' | b'/' | b'<' | b'>' | b'|' | b'&'
            | b'%' | b'^' | b'~' | b'!' | b'{' | b'}' | b';' | b':' | b'@' | b'[' | b']'
            | b'\\' => self.char_tok(c),
            _ => return Err(self.err(&format!("unexpected byte {:?}", c as char))),
        };
        self.p += 1;
        self.push(id, start);
        Ok(())
    }

    /// Multi-token fusion done by TiDB's `Lex()` with lookahead.
    fn fuse(self) -> Vec<Tok> {
        let t = self.tt.t;
        let v = self.out;
        let mut o: Vec<Tok> = Vec::with_capacity(v.len());
        let mut i = 0;
        while i < v.len() {
            let a = v[i];
            let b = v.get(i + 1).map(|x| x.id);
            let c = v.get(i + 2).map(|x| x.id);
            let fused = if (a.id == t.as_ || a.id == t.member) && b == Some(t.of) {
                Some((if a.id == t.as_ { t.asof } else { t.memberof }, 2))
            } else if a.id == t.to && b == Some(t.timestamp_type) && c == Some(t.string_lit) {
                Some((t.to_timestamp, 2))
            } else if a.id == t.to && b == Some(t.tso_type) && c == Some(t.int_lit) {
                Some((t.to_tso, 2))
            } else if a.id == t.optionally && b == Some(t.enclosed) && c == Some(t.by) {
                Some((t.optionally_enclosed_by, 3))
            } else if a.id == t.full && b == Some(t.outer) && c == Some(t.join) {
                // TiDB only rewrites FULL itself; OUTER JOIN stay as separate tokens.
                Some((t.full_join_type, 1))
            } else {
                None
            };
            match fused {
                Some((id, n)) => {
                    o.push(Tok {
                        id,
                        start: a.start,
                        end: v[i + n - 1].end,
                    });
                    i += n;
                }
                None => {
                    o.push(a);
                    i += 1;
                }
            }
        }
        o
    }
}
