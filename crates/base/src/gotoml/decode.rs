//! go-file: github.com/BurntSushi/toml@v1.5.0/decode.go
//! go-file: github.com/BurntSushi/toml@v1.5.0/meta.go
//! go-file: github.com/BurntSushi/toml@v1.5.0/type_fields.go
//!
//! `Decode` into the config structs: Go's `reflect` walk is the walk over
//! [`GoStruct`](crate::config::reflect::GoStruct). Go iterates the TOML tables (maps) in random
//! order; the port iterates them in key order, so with several errors in one file the port
//! reports the first in key order where Go reports any one of them.

use std::collections::{HashMap, HashSet};

use super::error::{DecodeError, LexErr, ParseError, Position};
use super::parse::{self, Key, KeyInfo, Table, Value, fmt_type, key_string};
use crate::config::reflect::{GoStruct, ValMut, ValRef};
use crate::gofmt::{self, Arg};

/// `MetaData` allows access to meta information about TOML data that's not accessible
/// otherwise.
#[derive(Debug, Default)]
pub struct MetaData {
    /// Used only during decoding.
    context: Key,
    key_info: HashMap<String, KeyInfo>,
    mapping: Table,
    keys: Vec<Key>,
    decoded: HashSet<String>,
    /// Input file; for errors.
    data: Vec<u8>,
}

type DResult = Result<(), DecodeError>;

/// `Decode` the TOML data into `v`.
// go-dep: github.com/BurntSushi/toml@v1.5.0.Decoder.Decode L1
pub fn decode(data: &[u8], v: &mut dyn GoStruct) -> (MetaData, DResult) {
    let p = match parse::parse(data) {
        Ok(p) => p,
        Err(e) => return (MetaData::default(), Err(DecodeError::Parse(e))),
    };
    let mut md = MetaData {
        decoded: HashSet::with_capacity(p.ordered.len()),
        mapping: p.mapping,
        key_info: p.key_info,
        keys: p.ordered,
        context: Vec::new(),
        data: data.to_vec(),
    };
    let mapping = std::mem::take(&mut md.mapping);
    let r = md.unify_struct(&Value::Table(mapping.clone()), v);
    md.mapping = mapping;
    (md, r)
}

/// `DecodeFile` reads the contents of a file and decodes it with [`decode`].
// go-dep: github.com/BurntSushi/toml@v1.5.0.DecodeFile L1
pub fn decode_file(path: &str, v: &mut dyn GoStruct) -> (MetaData, DResult) {
    match std::fs::read(path) {
        Ok(data) => decode(&data, v),
        Err(e) => (
            MetaData::default(),
            Err(DecodeError::Io(path_error("open", path, &e))),
        ),
    }
}

/// `*fs.PathError` text: `op path: errno string`.
// glue: Go os.PathError.Error() for the errors of os.Open
fn path_error(op: &str, path: &str, e: &std::io::Error) -> String {
    let msg = match e.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        std::io::ErrorKind::IsADirectory => "is a directory".to_owned(),
        _ => {
            let s = e.to_string();
            let s = s.split(" (os error").next().unwrap_or(&s).to_owned();
            crate::gostd::to_lower(&s[..1]) + &s[1..]
        }
    };
    format!("{op} {path}: {msg}")
}

/// A field found by `typeFields`.
#[derive(Clone, Debug)]
struct Field {
    name: &'static str,
    tag: bool,
    index: Vec<usize>,
}

/// `typeFields` returns a list of fields that TOML should recognize for the given type.
/// The algorithm is breadth-first search over the set of structs to include - the top
/// struct and then any reachable anonymous structs.
// go-dep: github.com/BurntSushi/toml@v1.5.0.typeFields L1
fn type_fields(t: &dyn GoStruct) -> Vec<Field> {
    let mut next: Vec<(Vec<usize>, &dyn GoStruct)> = vec![(Vec::new(), t)];
    let mut count: HashMap<&'static str, usize>;
    let mut next_count: HashMap<&'static str, usize> = HashMap::new();
    let mut visited: HashSet<&'static str> = HashSet::new();
    let mut fields: Vec<Field> = Vec::new();
    while !next.is_empty() {
        let current = std::mem::take(&mut next);
        count = std::mem::take(&mut next_count);
        for (findex, ftyp) in current {
            if !visited.insert(ftyp.go_type()) {
                continue;
            }
            for (i, sf) in ftyp.fields().iter().enumerate() {
                let Some(name) = sf.toml else { continue }; // toml:"-"
                let mut index = findex.clone();
                index.push(i);
                let sub = match ftyp.field(i) {
                    ValRef::Struct(s) => Some(s),
                    _ => None,
                };
                if sf.toml_tagged || !sf.embedded || sub.is_none() {
                    fields.push(Field {
                        name,
                        tag: sf.toml_tagged,
                        index,
                    });
                    if count.get(ftyp.go_type()).copied().unwrap_or(0) > 1 {
                        // If there were multiple instances, add a second, so that the
                        // annihilation code will see a duplicate.
                        let last = fields[fields.len() - 1].clone();
                        fields.push(last);
                    }
                    continue;
                }
                let ft = sub.unwrap_or(ftyp);
                let n = next_count.entry(ft.go_type()).or_insert(0);
                *n += 1;
                if *n == 1 {
                    next.push((index, ft));
                }
            }
        }
    }
    // byName
    fields.sort_by(|a, b| {
        a.name
            .cmp(b.name)
            .then(a.index.len().cmp(&b.index.len()))
            .then(b.tag.cmp(&a.tag))
            .then(a.index.cmp(&b.index))
    });
    // Delete all fields that are hidden by the Go rules for embedded fields, except that
    // fields with TOML tags are promoted.
    let mut out = Vec::with_capacity(fields.len());
    let mut i = 0;
    while i < fields.len() {
        let name = fields[i].name;
        let mut advance = 1;
        while i + advance < fields.len() && fields[i + advance].name == name {
            advance += 1;
        }
        if advance == 1 {
            out.push(fields[i].clone());
        } else if let Some(d) = dominant_field(&fields[i..i + advance]) {
            out.push(d);
        }
        i += advance;
    }
    // byIndex
    out.sort_by(|a, b| a.index.cmp(&b.index));
    out
}

// go-dep: github.com/BurntSushi/toml@v1.5.0.dominantField L1
fn dominant_field(fields: &[Field]) -> Option<Field> {
    let length = fields[0].index.len();
    let mut tagged: Option<usize> = None;
    let mut fields = fields;
    for (i, f) in fields.iter().enumerate() {
        if f.index.len() > length {
            fields = &fields[..i];
            break;
        }
        if f.tag {
            if tagged.is_some() {
                return None;
            }
            tagged = Some(i);
        }
    }
    if let Some(t) = tagged {
        return Some(fields[t].clone());
    }
    if fields.len() > 1 {
        return None;
    }
    Some(fields[0].clone())
}

/// `subv.Field(i)` along an index path.
// glue: reflect.Value.Field along field.index
fn field_by_index<'a>(s: &'a mut dyn GoStruct, index: &[usize]) -> ValMut<'a> {
    let v = s.field_mut(index[0]);
    if index.len() == 1 {
        return v;
    }
    match v {
        ValMut::Struct(sub) => field_by_index(sub, &index[1..]),
        _ => unreachable!("embedded field is a struct"),
    }
}

/// `Key.add`.
// go-dep: github.com/BurntSushi/toml@v1.5.0.Key.add L1
fn key_add(k: &[String], piece: &str) -> Key {
    let mut n = k.to_vec();
    n.push(piece.to_owned());
    n
}

/// Go integer kinds of the config structs (for range checks and messages).
#[derive(Clone, Copy)]
enum IntKind {
    Int,
    I32,
    I64,
    Uint,
    U32,
    U64,
}

impl MetaData {
    /// `IsDefined` reports if the key exists in the TOML data.
    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.IsDefined L1
    pub fn is_defined(&self, key: &[&str]) -> bool {
        if key.is_empty() {
            return false;
        }
        let mut hash = &self.mapping;
        for (i, k) in key.iter().enumerate() {
            match hash.get(*k) {
                None => return false,
                Some(v) if i == key.len() - 1 => {
                    let _ = v;
                    return true;
                }
                Some(Value::Table(t)) => hash = t,
                Some(_) => return false,
            }
        }
        true
    }

    /// `Keys` returns a slice of every key in the TOML data, including key groups.
    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.Keys L1
    pub fn keys(&self) -> &[Key] {
        &self.keys
    }

    /// `Undecoded` returns all keys that have not been decoded in the order in which they
    /// appear in the original TOML document.
    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.Undecoded L1
    pub fn undecoded(&self) -> Vec<Key> {
        self.keys
            .iter()
            .filter(|k| !self.decoded.contains(&key_string(k)))
            .cloned()
            .collect()
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.unify L1
    fn unify(&mut self, data: &Value, rv: ValMut<'_>) -> DResult {
        match rv {
            ValMut::NullableBool(b) => {
                let s = self.text_of(data)?;
                b.unmarshal_text(s.as_bytes()).map_err(|e| {
                    self.parse_err(LexErr::Msg(
                        String::from_utf8_lossy(&e.error()).into_owned(),
                    ))
                })
            }
            ValMut::AtomicBool(b) => {
                let s = self.text_of(data)?;
                b.unmarshal_text(s.as_bytes()).map_err(|e| {
                    self.parse_err(LexErr::Msg(
                        String::from_utf8_lossy(&e.error()).into_owned(),
                    ))
                })
            }
            ValMut::Int(v) => self.unify_int(data, IntKind::Int).map(|n| *v = n as isize),
            ValMut::I32(v) => self.unify_int(data, IntKind::I32).map(|n| *v = n as i32),
            ValMut::I64(v) => self.unify_int(data, IntKind::I64).map(|n| *v = n),
            ValMut::Uint(v) => self
                .unify_int(data, IntKind::Uint)
                .map(|n| *v = n as u64 as usize),
            ValMut::U32(v) => self.unify_int(data, IntKind::U32).map(|n| *v = n as u32),
            ValMut::U64(v) => self.unify_int(data, IntKind::U64).map(|n| *v = n as u64),
            ValMut::Duration(v) => {
                // Parse as string duration, and fall back to regular integer parsing (as
                // nanosecond) if this is not a string.
                if let Value::Str(s) = data {
                    let dur = crate::gostd::time::parse_duration(s)
                        .map_err(|_| self.parse_err(LexErr::ParseDuration(s.clone())))?;
                    v.0 = dur;
                    return Ok(());
                }
                self.unify_int(data, IntKind::I64).map(|n| v.0 = n)
            }
            ValMut::Struct(s) => self.unify_struct(data, s),
            ValMut::StrMap(m) => self.unify_map(data, m),
            ValMut::StrSlice(s) => self.unify_slice(data, s),
            ValMut::Str(s) => {
                let v = self.unify_string(data)?;
                *s = v;
                Ok(())
            }
            ValMut::Bool(b) => match data {
                Value::Bool(v) => {
                    *b = *v;
                    Ok(())
                }
                _ => Err(self.badtype("boolean", data)),
            },
            ValMut::F64(f) => self.unify_float64(data).map(|v| *f = v),
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.unifyStruct L1
    fn unify_struct(&mut self, mapping: &Value, rv: &mut dyn GoStruct) -> DResult {
        let Value::Table(tmap) = mapping else {
            return Err(self.e(
                "type mismatch for %s: expected table but found %s",
                &[Arg::from(rv.go_type()), Arg::from(fmt_type(mapping))],
            ));
        };
        let fields = type_fields(rv);
        for (key, datum) in tmap {
            let mut f: Option<&Field> = None;
            for ff in &fields {
                if ff.name == key {
                    f = Some(ff);
                    break;
                }
                if f.is_none() && crate::gostd::equal_fold(ff.name, key) {
                    f = Some(ff);
                }
            }
            if let Some(f) = f {
                let subv = field_by_index(rv, &f.index);
                self.decoded
                    .insert(key_string(&key_add(&self.context, key)));
                self.context.push(key.clone());
                self.unify(datum, subv)?;
                self.context.pop();
            }
        }
        Ok(())
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.unifyMap L1
    fn unify_map(
        &mut self,
        mapping: &Value,
        rv: &mut Option<std::collections::BTreeMap<String, String>>,
    ) -> DResult {
        let Value::Table(tmap) = mapping else {
            return Ok(()); // `if tmap == nil { return nil }` after the failed assertion
        };
        let m = rv.get_or_insert_with(Default::default);
        for (k, v) in tmap {
            self.decoded.insert(key_string(&key_add(&self.context, k)));
            self.context.push(k.clone());
            let s = self.unify_string(v)?;
            self.context.pop();
            m.insert(k.clone(), s);
        }
        Ok(())
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.unifySlice L1
    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.unifySliceArray L1
    fn unify_slice(&mut self, data: &Value, rv: &mut Option<Vec<String>>) -> DResult {
        let n = match data {
            Value::Array(a) => a.len(),
            Value::ArrayTables(a) => a.len(),
            _ => return Err(self.badtype("slice", data)),
        };
        // `if rv.IsNil() || rv.Cap() < n { MakeSlice(n, n) }; rv.SetLen(n)` (cap == len).
        match rv {
            Some(v) if v.len() >= n => v.truncate(n),
            _ => *rv = Some(vec![String::new(); n]),
        }
        let slice = rv.as_mut().expect("set above");
        for i in 0..n {
            let s = match data {
                Value::Array(a) => self.unify_string(&a[i])?,
                Value::ArrayTables(a) => self.unify_string(&Value::Table(a[i].clone()))?,
                _ => unreachable!(),
            };
            slice[i] = s;
        }
        Ok(())
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.unifyString L1
    fn unify_string(&self, data: &Value) -> Result<String, DecodeError> {
        match data {
            Value::Str(s) => Ok(s.clone()),
            _ => Err(self.badtype("string", data)),
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.unifyFloat64 L1
    fn unify_float64(&self, data: &Value) -> Result<f64, DecodeError> {
        const MAX_SAFE_FLOAT64_INT: i64 = 9007199254740991; // 2^53-1
        match data {
            Value::Float(f) => Ok(*f),
            Value::Int(num) => {
                if !(-MAX_SAFE_FLOAT64_INT..=MAX_SAFE_FLOAT64_INT).contains(num) {
                    return Err(self.parse_err(LexErr::UnsafeFloat(num.to_string(), "float64")));
                }
                Ok(*num as f64)
            }
            _ => Err(self.badtype("float", data)),
        }
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.unifyInt L1
    fn unify_int(&self, data: &Value, kind: IntKind) -> Result<i64, DecodeError> {
        let Value::Int(num) = data else {
            return Err(self.badtype("integer", data));
        };
        let num = *num;
        match kind {
            IntKind::I32 if num < i64::from(i32::MIN) || num > i64::from(i32::MAX) => {
                Err(self.parse_err(LexErr::ParseRange(num.to_string(), "int32")))
            }
            IntKind::U32 if num < 0 || num > i64::from(u32::MAX) => {
                Err(self.parse_err(LexErr::ParseRange(num.to_string(), "uint32")))
            }
            _ => Ok(num),
        }
    }

    /// The text `unifyText` gives to `UnmarshalText`.
    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.unifyText L1
    fn text_of(&self, data: &Value) -> Result<String, DecodeError> {
        Ok(match data {
            Value::Datetime(t) => t.clone(),
            Value::Str(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => {
                String::from_utf8_lossy(&gofmt::sprintf("%f", &[Arg::from(*f)])).into_owned()
            }
            _ => return Err(self.badtype("primitive (string-like)", data)),
        })
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.badtype L1
    fn badtype(&self, dst: &str, data: &Value) -> DecodeError {
        self.e(
            "incompatible types: TOML value has type %s; destination has type %s",
            &[Arg::from(fmt_type(data)), Arg::from(dst.to_owned())],
        )
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.parseErr L1
    fn parse_err(&self, err: LexErr) -> DecodeError {
        let k = key_string(&self.context);
        let pos = self.key_info.get(&k).map(|ki| ki.pos).unwrap_or_default();
        DecodeError::Parse(ParseError {
            message: err.error(),
            err: Some(Box::new(err)),
            last_key: k,
            position: pos.with_col(&self.data),
            line: pos.line,
        })
    }

    // go-dep: github.com/BurntSushi/toml@v1.5.0.MetaData.e L1
    fn e(&self, format: &str, args: &[Arg]) -> DecodeError {
        let mut f: Vec<u8> = b"toml: ".to_vec();
        if !self.context.is_empty() {
            let k = key_string(&self.context);
            f = gofmt::sprintf("toml: (last key %q): ", &[Arg::from(k.clone())]);
            let p: Position = self.key_info.get(&k).map(|ki| ki.pos).unwrap_or_default();
            if p.line > 0 {
                f = gofmt::sprintf(
                    "toml: line %d (last key %q): ",
                    &[Arg::from(p.line), Arg::from(k)],
                );
            }
        }
        f.extend_from_slice(format.as_bytes());
        DecodeError::Msg(String::from_utf8_lossy(&gofmt::sprintf(f, args)).into_owned())
    }
}
