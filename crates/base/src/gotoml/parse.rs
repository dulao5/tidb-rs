//! go-file: github.com/BurntSushi/toml@v1.5.0/parse.go
//! go-file: github.com/BurntSushi/toml@v1.5.0/type_toml.go
//! go-file: github.com/BurntSushi/toml@v1.5.0/meta.go
//!
//! The parser of BurntSushi/toml: builds the value tree (`mapping`), the order of keys
//! (`ordered`, for `MetaData.Keys` / `Undecoded`) and their positions (`keyInfo`). Go's
//! `panic(ParseError)` + `recover` is a `Result`; Go's `p.bug` (internal errors) panics.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::datetime;
use super::error::{LexErr, ParseError, Position};
use super::lex::{Item, ItemType, Lexer};
use crate::gofmt::{self, Arg};

/// A TOML value (Go: the `any` in `map[string]any`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    /// `time.Time`, kept as its `MarshalText` (RFC 3339) text.
    Datetime(String),
    /// `[]any`.
    Array(Vec<Value>),
    /// `map[string]any`.
    Table(Table),
    /// `[]map[string]any` (array of tables).
    ArrayTables(Vec<Table>),
}

/// Go `map[string]any`; iteration in key order (Go's is random, see the block plan).
pub(crate) type Table = BTreeMap<String, Value>;

/// `fmtType`: `%T` of the Go value, with `interface {}` written `any`.
// go-dep: github.com/BurntSushi/toml@v1.5.0.fmtType L1
pub(crate) fn fmt_type(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "int64",
        Value::Float(_) => "float64",
        Value::Str(_) => "string",
        Value::Bool(_) => "bool",
        Value::Datetime(_) => "time.Time",
        Value::Array(_) => "[]any",
        Value::Table(_) => "map[string]any",
        Value::ArrayTables(_) => "[]map[string]any",
    }
}

/// `tomlType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TomlType {
    Integer,
    Float,
    Datetime,
    String,
    Bool,
    Array,
    Hash,
    ArrayHash,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct KeyInfo {
    pub(crate) pos: Position,
    pub(crate) toml_type: TomlType,
}

/// `Key` represents any TOML key, including key groups. Use [`key_string`] to get the
/// string representation.
pub type Key = Vec<String>;

/// `Key.String` returns a string representation of the key.
// go-dep: github.com/BurntSushi/toml@v1.5.0.Key.String L1
pub fn key_string(k: &[String]) -> String {
    let mut b = String::with_capacity(k.len() * 25);
    'outer: for (i, kk) in k.iter().enumerate() {
        if i > 0 {
            b.push('.');
        }
        if kk.is_empty() {
            b.push_str("\"\"");
        } else {
            for r in kk.chars() {
                // "Inline" isBareKeyChar
                if !(r.is_ascii_alphanumeric() || r == '_' || r == '-') {
                    b.push('"');
                    b.push_str(&dbl_quoted_replace(kk));
                    b.push('"');
                    continue 'outer;
                }
            }
            b.push_str(kk);
        }
    }
    b
}

/// `dblQuotedReplacer`: `"` → `\"`, `\` → `\\`, and control characters escaped.
// go-dep: github.com/BurntSushi/toml@v1.5.0.dblQuotedReplacer L1
fn dbl_quoted_replace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0}'..='\u{1f}' | '\u{7f}' => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            _ => out.push(c),
        }
    }
    out
}

pub(crate) struct Parser {
    lx: Lexer,
    /// Full key for the current hash in scope.
    context: Key,
    /// Base key name for everything except hashes.
    current_key: String,
    /// Current position in the TOML file.
    pos: Position,
    toml_next: bool,
    /// List of keys in the order that they appear in the TOML data.
    pub(crate) ordered: Vec<Key>,
    /// Map keyname → info about the TOML key.
    pub(crate) key_info: HashMap<String, KeyInfo>,
    /// Map keyname → key value.
    pub(crate) mapping: Table,
    /// Record implicit keys (e.g. "key.group.names").
    implicits: HashSet<String>,
}

type PResult<T> = Result<T, ParseError>;

/// `parse`; `data` is the file content (Go: `string(data)`, bytes).
// go-dep: github.com/BurntSushi/toml@v1.5.0.parse L1
pub(crate) fn parse(mut data: &[u8]) -> PResult<Parser> {
    // BURNTSUSHI_TOML_110 (TOML 1.1 draft) is not set in TiDB.
    let toml_next = std::env::var_os("BURNTSUSHI_TOML_110").is_some();
    // Read over BOM; do this here as the lexer calls utf8.DecodeRuneInString()
    // which mangles stuff. UTF-16 BOM isn't strictly valid, but some tools add
    // it anyway.
    if data.starts_with(b"\xff\xfe") || data.starts_with(b"\xfe\xff") {
        // UTF-16
        data = &data[2..];
    } else if data.starts_with(b"\xef\xbb\xbf") {
        // UTF-8
        data = &data[3..];
    }
    // Examine first few bytes for NULL bytes; this probably means it's a UTF-16
    // file (second byte will be NULL for ASCII). This is a good signal for a
    // UTF-16 file.
    let ex = data.len().min(6);
    if let Some(i) = data[..ex].iter().position(|&b| b == 0) {
        return Err(ParseError {
            message:
                "files cannot contain NULL bytes; probably using UTF-16; TOML files must be UTF-8"
                    .to_owned(),
            position: Position {
                line: 1,
                col: 1,
                start: i as isize,
                len: 1,
            },
            line: 1,
            last_key: String::new(),
            err: None,
        });
    }
    let mut p = Parser {
        lx: Lexer::new(data.to_vec(), toml_next),
        context: Vec::new(),
        current_key: String::new(),
        pos: Position::default(),
        toml_next,
        ordered: Vec::new(),
        key_info: HashMap::new(),
        mapping: Table::new(),
        implicits: HashSet::new(),
    };
    loop {
        let item = p.next()?;
        if item.typ == ItemType::Eof {
            break;
        }
        p.top_level(item)?;
    }
    Ok(p)
}

/// `fmt.Sprintf` to a Rust string (messages are UTF-8 here).
// glue: fmt.Sprintf for ASCII/UTF-8 messages
fn sf(format: &str, a: &[Arg]) -> String {
    String::from_utf8_lossy(&gofmt::sprintf(format, a)).into_owned()
}

impl Parser {
    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.panicErr L1
    fn panic_err<T>(&self, it: &Item, err: LexErr) -> PResult<T> {
        Err(ParseError {
            message: err.error(),
            err: Some(Box::new(err)),
            position: it.pos.with_col(&self.lx.input),
            line: it.pos.len,
            last_key: self.current(),
        })
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.panicItemf L1
    fn panic_itemf<T>(&self, it: &Item, msg: String) -> PResult<T> {
        Err(ParseError {
            message: msg,
            err: None,
            position: it.pos.with_col(&self.lx.input),
            line: it.pos.len,
            last_key: self.current(),
        })
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.panicf L1
    fn panicf<T>(&self, msg: String) -> PResult<T> {
        Err(ParseError {
            message: msg,
            err: None,
            position: self.pos.with_col(&self.lx.input),
            line: self.pos.line,
            last_key: self.current(),
        })
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.next L1
    fn next(&mut self) -> PResult<Item> {
        let it = self.lx.next_item();
        if it.typ == ItemType::Error {
            if let Some(err) = it.err.clone() {
                return Err(ParseError {
                    message: err.error(),
                    err: Some(Box::new(err)),
                    position: it.pos.with_col(&self.lx.input),
                    line: it.pos.line,
                    last_key: self.current(),
                });
            }
            return self.panic_itemf(&it, it.val.clone());
        }
        Ok(it)
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.nextPos L1
    fn next_pos(&mut self) -> PResult<Item> {
        let it = self.next()?;
        self.pos = it.pos;
        Ok(it)
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.bug L1
    fn bug(&self, msg: String) -> ! {
        panic!("BUG: {msg}\n\n");
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.expect L1
    fn expect(&mut self, typ: ItemType) -> PResult<Item> {
        let it = self.next()?;
        self.assert_equal(typ, it.typ);
        Ok(it)
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.assertEqual L1
    fn assert_equal(&self, expected: ItemType, got: ItemType) {
        if expected != got {
            self.bug(format!("Expected '{expected:?}' but got '{got:?}'."));
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.topLevel L1
    fn top_level(&mut self, item: Item) -> PResult<()> {
        match item.typ {
            ItemType::CommentStart => {
                // # ..
                self.expect(ItemType::Text)?;
            }
            ItemType::TableStart => {
                // [ .. ]
                let mut name = self.next_pos()?;
                let mut key: Key = Vec::new();
                while name.typ != ItemType::TableEnd && name.typ != ItemType::Eof {
                    key.push(self.key_string(&name)?);
                    name = self.next()?;
                }
                self.assert_equal(ItemType::TableEnd, name.typ);
                self.add_context(&key, false)?;
                self.set_type("", TomlType::Hash, item.pos);
                self.ordered.push(key);
            }
            ItemType::ArrayTableStart => {
                // [[ .. ]]
                let mut name = self.next_pos()?;
                let mut key: Key = Vec::new();
                while name.typ != ItemType::ArrayTableEnd && name.typ != ItemType::Eof {
                    key.push(self.key_string(&name)?);
                    name = self.next()?;
                }
                self.assert_equal(ItemType::ArrayTableEnd, name.typ);
                self.add_context(&key, true)?;
                self.set_type("", TomlType::ArrayHash, item.pos);
                self.ordered.push(key);
            }
            ItemType::KeyStart => {
                // key = ..
                let outer_context = self.context.clone();
                // Read all the key parts (e.g. 'a' and 'b' in 'a.b')
                let mut k = self.next_pos()?;
                let mut key: Key = Vec::new();
                while k.typ != ItemType::KeyEnd && k.typ != ItemType::Eof {
                    key.push(self.key_string(&k)?);
                    k = self.next()?;
                }
                self.assert_equal(ItemType::KeyEnd, k.typ);
                // The current key is the last part.
                self.current_key = key.last().cloned().unwrap_or_default();
                // All the other parts (if any) are the context; need to set each part
                // as implicit.
                let context = key[..key.len() - 1].to_vec();
                for part in &context {
                    let mut c = self.context.clone();
                    c.push(part.clone());
                    self.add_implicit_context(&c)?;
                }
                let mut ord = self.context.clone();
                ord.push(self.current_key.clone());
                self.ordered.push(ord);
                // Set value.
                let v_item = self.next()?;
                let (val, typ) = self.value(&v_item, false)?;
                let ck = self.current_key.clone();
                self.set_value(&ck, val)?;
                self.set_type(&ck, typ, v_item.pos);
                // Remove the context we added (preserving any context from [tbl] lines).
                self.context = outer_context;
                self.current_key = String::new();
            }
            _ => self.bug(format!("Unexpected type at top level: {:?}", item.typ)),
        }
        Ok(())
    }

    /// Gets a string for a key (or part of a key in a table name).
    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.keyString L1
    fn key_string(&mut self, it: &Item) -> PResult<String> {
        match it.typ {
            ItemType::Text => Ok(it.val.clone()),
            ItemType::String
            | ItemType::StringEsc
            | ItemType::MultilineString
            | ItemType::RawString
            | ItemType::RawMultilineString => match self.value(it, false)?.0 {
                Value::Str(s) => Ok(s),
                _ => unreachable!(),
            },
            _ => self.bug(format!("Unexpected key type: {:?}", it.typ)),
        }
    }

    /// value translates an expected value from the lexer into a Go value wrapped
    /// as an empty interface.
    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.value L1
    fn value(&mut self, it: &Item, parent_is_array: bool) -> PResult<(Value, TomlType)> {
        match it.typ {
            ItemType::String => Ok((Value::Str(it.val.clone()), self.type_of_primitive(it))),
            ItemType::StringEsc => Ok((
                Value::Str(self.replace_escapes(it, &it.val)?),
                self.type_of_primitive(it),
            )),
            ItemType::MultilineString => {
                let s = strip_escaped_newlines(strip_first_newline(&it.val));
                Ok((
                    Value::Str(self.replace_escapes(it, &s)?),
                    self.type_of_primitive(it),
                ))
            }
            ItemType::RawString => Ok((Value::Str(it.val.clone()), self.type_of_primitive(it))),
            ItemType::RawMultilineString => Ok((
                Value::Str(strip_first_newline(&it.val).to_owned()),
                self.type_of_primitive(it),
            )),
            ItemType::Integer => self.value_integer(it),
            ItemType::Float => self.value_float(it),
            ItemType::Bool => match it.val.as_str() {
                "true" => Ok((Value::Bool(true), self.type_of_primitive(it))),
                "false" => Ok((Value::Bool(false), self.type_of_primitive(it))),
                _ => self.bug(format!("Expected boolean value, but got '{}'.", it.val)),
            },
            ItemType::Datetime => self.value_datetime(it),
            ItemType::Array => self.value_array(it),
            ItemType::InlineTableStart => self.value_inline_table(it, parent_is_array),
            _ => self.bug(format!("Unexpected value type: {:?}", it.typ)),
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.valueInteger L1
    fn value_integer(&mut self, it: &Item) -> PResult<(Value, TomlType)> {
        if !num_underscores_ok(&it.val) {
            return self.panic_itemf(
                it,
                sf(
                    "Invalid integer %q: underscores must be surrounded by digits",
                    &[Arg::from(it.val.clone())],
                ),
            );
        }
        if num_has_leading_zero(&it.val) {
            return self.panic_itemf(
                it,
                sf(
                    "Invalid integer %q: cannot have leading zeroes",
                    &[Arg::from(it.val.clone())],
                ),
            );
        }
        match crate::gostd::strconv::parse_int_base0(&it.val) {
            Ok(num) => Ok((Value::Int(num), self.type_of_primitive(it))),
            Err(true) => self.panic_err(it, LexErr::ParseRange(it.val.clone(), "int64")),
            Err(false) => self.bug(format!("Expected integer value, but got '{}'.", it.val)),
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.valueFloat L1
    fn value_float(&mut self, it: &Item) -> PResult<(Value, TomlType)> {
        let parts: Vec<&str> = it
            .val
            .split(['.', 'e', 'E'])
            .filter(|p| !p.is_empty())
            .collect();
        for part in &parts {
            if !num_underscores_ok(part) {
                return self.panic_itemf(
                    it,
                    sf(
                        "Invalid float %q: underscores must be surrounded by digits",
                        &[Arg::from(it.val.clone())],
                    ),
                );
            }
        }
        if !parts.is_empty() && num_has_leading_zero(parts[0]) {
            return self.panic_itemf(
                it,
                sf(
                    "Invalid float %q: cannot have leading zeroes",
                    &[Arg::from(it.val.clone())],
                ),
            );
        }
        if !num_periods_ok(&it.val) {
            // As a special case, numbers like '123.' or '1.e2',
            // which are valid as far as Go/strconv are concerned,
            // must be rejected because TOML says that a fractional
            // part consists of '.' followed by 1+ digits.
            return self.panic_itemf(
                it,
                sf(
                    "Invalid float %q: '.' must be followed by one or more digits",
                    &[Arg::from(it.val.clone())],
                ),
            );
        }
        let mut val = it.val.replace('_', "");
        let mut signbit = false;
        if val == "+nan" || val == "-nan" {
            signbit = val == "-nan";
            val = "nan".to_owned();
        }
        let mut num = match crate::gostd::strconv::parse_float(&val) {
            Ok(n) => n,
            Err(true) => return self.panic_err(it, LexErr::ParseRange(it.val.clone(), "float64")),
            Err(false) => {
                return self.panic_itemf(
                    it,
                    sf("Invalid float value: %q", &[Arg::from(it.val.clone())]),
                );
            }
        };
        if signbit {
            num = -num.abs();
        }
        Ok((Value::Float(num), self.type_of_primitive(it)))
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.valueDatetime L2 (datetime layouts parsed by the port's datetime module)
    fn value_datetime(&mut self, it: &Item) -> PResult<(Value, TomlType)> {
        let v: String = it.val.replace('z', "Z").replace(['t', ' '], "T");
        match datetime::parse_toml_datetime(&v, self.toml_next) {
            Some(text) => Ok((Value::Datetime(text), self.type_of_primitive(it))),
            None => self.panic_err(it, LexErr::ParseDate(v)),
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.valueArray L1
    fn value_array(&mut self, it: &Item) -> PResult<(Value, TomlType)> {
        let ck = self.current_key.clone();
        self.set_type(&ck, TomlType::Array, it.pos);
        let mut array = Vec::with_capacity(2);
        let mut it = self.next()?;
        while it.typ != ItemType::ArrayEnd {
            if it.typ == ItemType::CommentStart {
                self.expect(ItemType::Text)?;
                it = self.next()?;
                continue;
            }
            let (val, _typ) = self.value(&it, true)?;
            array.push(val);
            // XXX: type isn't used here, we need it to record the accurate type
            // information.
            //
            // Not entirely sure how to best store this; could use "key[0]",
            // "key[1]" notation, or maybe store it on the Array type?
            it = self.next()?;
        }
        Ok((Value::Array(array), TomlType::Array))
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.valueInlineTable L1
    fn value_inline_table(
        &mut self,
        _it: &Item,
        parent_is_array: bool,
    ) -> PResult<(Value, TomlType)> {
        let mut top_hash = Table::new();
        let outer_context = self.context.clone();
        let outer_key = self.current_key.clone();

        self.context.push(self.current_key.clone());
        let prev_context = self.context.clone();
        self.current_key = String::new();

        let c = self.context.clone();
        self.add_implicit(&c);
        self.add_context(&c, parent_is_array)?;

        // Loop over all table key/value pairs.
        let mut it = self.next()?;
        while it.typ != ItemType::InlineTableEnd {
            if it.typ == ItemType::CommentStart {
                self.expect(ItemType::Text)?;
                it = self.next()?;
                continue;
            }
            // Read all key parts.
            let mut k = self.next_pos()?;
            let mut key: Key = Vec::new();
            while k.typ != ItemType::KeyEnd && k.typ != ItemType::Eof {
                key.push(self.key_string(&k)?);
                k = self.next()?;
            }
            self.assert_equal(ItemType::KeyEnd, k.typ);

            // The current key is the last part.
            self.current_key = key.last().cloned().unwrap_or_default();

            // All the other parts (if any) are the context; need to set each part
            // as implicit.
            let context = key[..key.len() - 1].to_vec();
            for part in &context {
                let mut c = self.context.clone();
                c.push(part.clone());
                self.add_implicit_context(&c)?;
            }
            let mut ord = self.context.clone();
            ord.push(self.current_key.clone());
            self.ordered.push(ord);

            // Set the value.
            let v_item = self.next()?;
            let (val, typ) = self.value(&v_item, false)?;
            let ck = self.current_key.clone();
            self.set_value(&ck, val.clone())?;
            self.set_type(&ck, typ, it.pos);

            let mut hash = &mut top_hash;
            for c in &context {
                let h = hash
                    .entry(c.clone())
                    .or_insert_with(|| Value::Table(Table::new()));
                match h {
                    Value::Table(t) => hash = t,
                    _ => {
                        return self.panicf(sf(
                            "%q is not a table",
                            &[Arg::from(key_string(&self.context))],
                        ));
                    }
                }
            }
            hash.insert(ck, val);

            self.context = prev_context.clone(); // Restore context.
            it = self.next()?;
        }
        self.context = outer_context;
        self.current_key = outer_key;
        Ok((Value::Table(top_hash), TomlType::Hash))
    }

    /// The hash (table) at `key` in the mapping, descending into the last table of arrays of
    /// tables (Go keeps a pointer to the current map while walking).
    // glue: navigation of the nested Go maps
    fn hash_at(&mut self, key: &[String]) -> Option<&mut Table> {
        let mut h = &mut self.mapping;
        for k in key {
            h = match h.get_mut(k)? {
                Value::Table(t) => t,
                Value::ArrayTables(ts) => ts.last_mut()?,
                _ => return None,
            };
        }
        Some(h)
    }

    /// addContext sets the current context of the parser, where the context is
    /// either a hash or an array of hashes, depending on the value of the `array`
    /// parameter.
    ///
    /// Establishing the context also makes sure that the key isn't a duplicate, and
    /// will create implicit hashes automatically.
    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.addContext L1
    fn add_context(&mut self, key: &[String], array: bool) -> PResult<()> {
        // Always start at the top level and drill down for our context.
        let mut key_context: Key = Vec::new();
        // We only need implicit hashes for the parents.
        for k in &key[..key.len() - 1] {
            key_context.push(k.clone());
            let parent = key_context[..key_context.len() - 1].to_vec();
            let exists = self.hash_at(&parent).is_some_and(|h| h.contains_key(k));
            if !exists {
                self.add_implicit(&key_context);
                if let Some(h) = self.hash_at(&parent) {
                    h.insert(k.clone(), Value::Table(Table::new()));
                }
            }
            let ok = matches!(
                self.hash_at(&parent).and_then(|h| h.get(k)),
                Some(Value::Table(_)) | Some(Value::ArrayTables(_))
            );
            if !ok {
                return self.panicf(sf(
                    "Key '%s' was already created as a hash.",
                    &[Arg::from(key_string(&key_context))],
                ));
            }
        }
        self.context = key_context.clone();
        if array {
            // If this is the first element for this array, then allocate a new
            // list of tables for it.
            let k = key.last().unwrap().clone();
            let hash = self.hash_at(&key_context).expect("context");
            if !hash.contains_key(&k) {
                hash.insert(k.clone(), Value::ArrayTables(Vec::with_capacity(4)));
            }
            // Add a new table. But make sure the key hasn't already been used
            // for something else.
            if let Some(Value::ArrayTables(ts)) = hash.get_mut(&k) {
                ts.push(Table::new());
            } else {
                return self.panicf(sf(
                    "Key '%s' was already created and cannot be used as an array.",
                    &[Arg::from(key_string(key))],
                ));
            }
        } else {
            self.set_value(key.last().unwrap(), Value::Table(Table::new()))?;
        }
        self.context.push(key.last().unwrap().clone());
        Ok(())
    }

    /// setValue sets the given key to the given value in the current context.
    /// It will make sure that the key hasn't already been defined, account for
    /// implicit key groups.
    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.setValue L1
    fn set_value(&mut self, key: &str, value: Value) -> PResult<()> {
        let mut key_context: Key = Vec::with_capacity(self.context.len() + 1);
        {
            let mut h = &self.mapping;
            for k in &self.context {
                key_context.push(k.clone());
                h = match h.get(k) {
                    None => self.bug(format!(
                        "Context for key '{}' has not been established.",
                        key_string(&key_context)
                    )),
                    Some(Value::ArrayTables(ts)) => ts.last().expect("array of tables"),
                    Some(Value::Table(t)) => t,
                    Some(_) => {
                        return self.panicf(sf(
                            "Key '%s' has already been defined.",
                            &[Arg::from(key_string(&key_context))],
                        ));
                    }
                };
            }
        }
        key_context.push(key.to_owned());
        let ctx = self.context.clone();
        let exists = self.hash_at(&ctx).expect("context").contains_key(key);
        if exists {
            // Normally redefining keys isn't allowed, but the key could have been
            // defined implicitly and it's allowed to be redefined concretely. (See
            // the `valid/implicit-and-explicit-after.toml` in toml-test)
            //
            // But we have to make sure to stop marking it as an implicit. (So that
            // another redefinition provokes an error.)
            //
            // Note that since it has already been defined (as a hash), we don't
            // want to overwrite it. So our business is done.
            if self.is_array(&key_context) {
                self.remove_implicit(&key_context);
                self.hash_at(&ctx)
                    .expect("context")
                    .insert(key.to_owned(), value);
                return Ok(());
            }
            if self.is_implicit(&key_context) {
                self.remove_implicit(&key_context);
                return Ok(());
            }
            // Otherwise, we have a concrete key trying to override a previous key,
            // which is *always* wrong.
            return self.panicf(sf(
                "Key '%s' has already been defined.",
                &[Arg::from(key_string(&key_context))],
            ));
        }
        self.hash_at(&ctx)
            .expect("context")
            .insert(key.to_owned(), value);
        Ok(())
    }

    /// setType sets the type of a particular value at a given key. It should be
    /// called immediately AFTER setValue.
    ///
    /// Note that if `key` is empty, then the type given will be applied to the
    /// current context (which is either a table or an array of tables).
    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.setType L1
    fn set_type(&mut self, key: &str, typ: TomlType, pos: Position) {
        let mut key_context: Key = self.context.clone();
        if !key.is_empty() {
            // allow type setting for hashes
            key_context.push(key.to_owned());
        }
        if key_context.is_empty() {
            key_context = vec![String::new()];
        }
        self.key_info.insert(
            key_string(&key_context),
            KeyInfo {
                toml_type: typ,
                pos,
            },
        );
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.addImplicit L1
    fn add_implicit(&mut self, key: &[String]) {
        self.implicits.insert(key_string(key));
    }
    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.removeImplicit L1
    fn remove_implicit(&mut self, key: &[String]) {
        self.implicits.remove(&key_string(key));
    }
    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.isImplicit L1
    fn is_implicit(&self, key: &[String]) -> bool {
        self.implicits.contains(&key_string(key))
    }
    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.isArray L1
    fn is_array(&self, key: &[String]) -> bool {
        self.key_info
            .get(&key_string(key))
            .is_some_and(|k| k.toml_type == TomlType::Array)
    }
    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.addImplicitContext L1
    fn add_implicit_context(&mut self, key: &[String]) -> PResult<()> {
        self.add_implicit(key);
        self.add_context(key, false)
    }

    /// current returns the full key name of the current context.
    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.current L1
    fn current(&self) -> String {
        if self.current_key.is_empty() {
            return key_string(&self.context);
        }
        if self.context.is_empty() {
            return self.current_key.clone();
        }
        format!("{}.{}", key_string(&self.context), self.current_key)
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.replaceEscapes L1
    fn replace_escapes(&self, it: &Item, s: &str) -> PResult<String> {
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut skip = 0;
        for (i, c) in s.char_indices() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            if c != '\\' {
                out.push(c);
                continue;
            }
            if i >= s.len() {
                self.bug("Escape sequence at end of string.".to_owned());
            }
            match b[i + 1] {
                b' ' | b'\t' => {
                    return self.panic_itemf(
                        it,
                        sf(
                            "invalid escape: '\\%c'",
                            &[Arg::Int(i64::from(b[i + 1]), "uint8")],
                        ),
                    );
                }
                b'b' => {
                    out.push('\u{8}');
                    skip = 1;
                }
                b't' => {
                    out.push('\t');
                    skip = 1;
                }
                b'n' => {
                    out.push('\n');
                    skip = 1;
                }
                b'f' => {
                    out.push('\u{c}');
                    skip = 1;
                }
                b'r' => {
                    out.push('\r');
                    skip = 1;
                }
                b'e' => {
                    if self.toml_next {
                        out.push('\u{1b}');
                        skip = 1;
                    }
                }
                b'"' => {
                    out.push('"');
                    skip = 1;
                }
                b'\\' => {
                    out.push('\\');
                    skip = 1;
                }
                // The lexer guarantees the correct number of characters are present;
                // don't need to check here.
                b'x' => {
                    if self.toml_next {
                        out.push(self.ascii_escape_to_unicode(it, &s[i + 2..i + 4])?);
                        skip = 3;
                    }
                }
                b'u' => {
                    out.push(self.ascii_escape_to_unicode(it, &s[i + 2..i + 6])?);
                    skip = 5;
                }
                b'U' => {
                    out.push(self.ascii_escape_to_unicode(it, &s[i + 2..i + 10])?);
                    skip = 9;
                }
                x => self.bug(format!(
                    "Expected valid escape code after \\, but got {:?}.",
                    x as char
                )),
            }
        }
        Ok(out)
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.asciiEscapeToUnicode L1
    fn ascii_escape_to_unicode(&self, it: &Item, s: &str) -> PResult<char> {
        let hex = u32::from_str_radix(&s.to_lowercase(), 16).unwrap_or_else(|e| {
            self.bug(format!(
                "Could not parse '{s}' as a hexadecimal number, but the lexer claims it's OK: {e}"
            ))
        });
        match char::from_u32(hex) {
            Some(c) => Ok(c),
            None => self.panic_itemf(
                it,
                sf(
                    "Escaped character '\\u%s' is not valid UTF-8.",
                    &[Arg::from(s.to_owned())],
                ),
            ),
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.parser.typeOfPrimitive L1
    fn type_of_primitive(&self, it: &Item) -> TomlType {
        match it.typ {
            ItemType::Integer => TomlType::Integer,
            ItemType::Float => TomlType::Float,
            ItemType::Datetime => TomlType::Datetime,
            ItemType::String
            | ItemType::StringEsc
            | ItemType::MultilineString
            | ItemType::RawString
            | ItemType::RawMultilineString => TomlType::String,
            ItemType::Bool => TomlType::Bool,
            _ => self.bug(format!(
                "Cannot infer primitive type of lex item '{:?}'.",
                it.typ
            )),
        }
    }
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.numHasLeadingZero L1
fn num_has_leading_zero(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() > 1 && b[0] == b'0' && !(b[1] == b'b' || b[1] == b'o' || b[1] == b'x') {
        // Allow 0b, 0o, 0x
        return true;
    }
    if b.len() > 2 && (b[0] == b'-' || b[0] == b'+') && b[1] == b'0' {
        return true;
    }
    false
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.numUnderscoresOK L1
fn num_underscores_ok(s: &str) -> bool {
    match s {
        "nan" | "+nan" | "-nan" | "inf" | "-inf" | "+inf" => return true,
        _ => {}
    }
    let mut accept = false;
    for r in s.chars() {
        if r == '_' && !accept {
            return false;
        }
        // isHexis superset of all numeric chars (decimal, octal, binary,
        // hex)
        accept = super::lex::is_hex(r);
    }
    accept
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.numPeriodsOK L1
fn num_periods_ok(s: &str) -> bool {
    let mut period = false;
    for r in s.chars() {
        if period && !r.is_ascii_digit() {
            return false;
        }
        period = r == '.';
    }
    !period
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.stripFirstNewline L1
fn strip_first_newline(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix('\n') {
        return rest;
    }
    if let Some(rest) = s.strip_prefix("\r\n") {
        return rest;
    }
    s
}

/// stripEscapedNewlines removes whitespace after line-ending backslashes in
/// multiline strings.
///
/// A line-ending backslash is an unescaped \ followed only by whitespace until
/// the next newline. After a line-ending backslash, all whitespace is removed
/// until the next non-whitespace character.
// go-dep: github.com/BurntSushi/toml@v1.5.0.parser.stripEscapedNewlines L1
fn strip_escaped_newlines(s: &str) -> String {
    let mut b = String::with_capacity(s.len());
    let mut s = s;
    let mut i = 0;
    loop {
        let Some(ix) = s[i..].find('\\') else {
            b.push_str(s);
            return b;
        };
        i += ix;
        let sb = s.as_bytes();
        if sb.len() > i + 1 && sb[i + 1] == b'\\' {
            // Escaped backslash.
            i += 2;
            continue;
        }
        // Scan until the next non-whitespace.
        let mut j = i + 1;
        while j < sb.len() && matches!(sb[j], b' ' | b'\t' | b'\r' | b'\n') {
            j += 1;
        }
        if j == i + 1 {
            // Not a whitespace escape.
            i += 1;
            continue;
        }
        if !s[i..j].contains('\n') {
            // This is not a line-ending backslash. (It's a bad escape sequence,
            // but we can let replaceEscapes catch it.)
            i += 1;
            continue;
        }
        b.push_str(&s[..i]);
        s = &s[j..];
        i = 0;
    }
}
