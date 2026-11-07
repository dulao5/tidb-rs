//! The reflection that Go's `pkg/config` relies on (glue).
//!
//! Go decodes TOML (BurntSushi), encodes JSON (`GetJSONConfig`, `CloneConf`) and merges
//! configs (`MergeConfigItems`) by walking struct fields with `reflect`. The port keeps the
//! same walks over a small reflection interface: every config struct (generated in
//! `types_gen.rs` from the Go types) lists its fields with their Go name, TOML and JSON tags,
//! and gives typed access to each field.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use crate::errors::Error;
use crate::gofmt::{self, Arg};
use crate::gostd::json;

/// One struct field: Go name and tags (`None` = `toml:"-"` / `json:"-"`).
#[derive(Debug)]
pub struct FieldDesc {
    pub go_name: &'static str,
    pub toml: Option<&'static str>,
    /// The TOML name comes from a tag (BurntSushi `field.tag`).
    pub toml_tagged: bool,
    pub json: Option<&'static str>,
    /// Anonymous (embedded) field: its fields are promoted for TOML and JSON.
    pub embedded: bool,
}

/// A Go struct the config code can reflect on.
pub trait GoStruct: Send + Sync + std::fmt::Debug {
    /// `reflect.Type.String()`, e.g. `config.Log`.
    // glue: reflect.Type.String
    fn go_type(&self) -> &'static str;
    /// The fields, in Go order.
    // glue: reflect struct fields
    fn fields(&self) -> &'static [FieldDesc];
    /// Field `i` by reference.
    // glue: reflect.Value.Field
    fn field(&self, i: usize) -> ValRef<'_>;
    /// Field `i` by mutable reference.
    // glue: reflect.Value.Field, settable
    fn field_mut(&mut self, i: usize) -> ValMut<'_>;
}

/// A field value (Go kinds used by the config structs; `M-INT` widths).
#[derive(Debug)]
pub enum ValRef<'a> {
    Bool(&'a bool),
    Int(&'a isize),
    I32(&'a i32),
    I64(&'a i64),
    Uint(&'a usize),
    U32(&'a u32),
    U64(&'a u64),
    F64(&'a f64),
    Str(&'a String),
    Duration(&'a Duration),
    /// `[]string`; `None` is Go's nil slice.
    StrSlice(&'a Option<Vec<String>>),
    /// `map[string]string`; `None` is Go's nil map.
    StrMap(&'a Option<BTreeMap<String, String>>),
    NullableBool(&'a NullableBool),
    AtomicBool(&'a AtomicBool),
    Struct(&'a dyn GoStruct),
}

/// A field value, mutable.
#[derive(Debug)]
pub enum ValMut<'a> {
    Bool(&'a mut bool),
    Int(&'a mut isize),
    I32(&'a mut i32),
    I64(&'a mut i64),
    Uint(&'a mut usize),
    U32(&'a mut u32),
    U64(&'a mut u64),
    F64(&'a mut f64),
    Str(&'a mut String),
    Duration(&'a mut Duration),
    StrSlice(&'a mut Option<Vec<String>>),
    StrMap(&'a mut Option<BTreeMap<String, String>>),
    NullableBool(&'a mut NullableBool),
    AtomicBool(&'a mut AtomicBool),
    Struct(&'a mut dyn GoStruct),
}

/// Go `time.Duration` (nanoseconds).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration(pub i64);

/// `nullableBool` defaults unset bool options to unset instead of false, which enables us to
/// know if the user has set 2 conflict options at the same time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NullableBool {
    pub is_valid: bool,
    pub is_true: bool,
}

pub const nbUnset: NullableBool = NullableBool {
    is_valid: false,
    is_true: false,
};
pub const nbFalse: NullableBool = NullableBool {
    is_valid: true,
    is_true: false,
};
pub const nbTrue: NullableBool = NullableBool {
    is_valid: true,
    is_true: true,
};

/// `AtomicBool` is a helper type for atomic operations on a boolean value (Go: embeds
/// `go.uber.org/atomic.Bool`; copied by value like the Go struct).
#[derive(Debug, Default)]
pub struct AtomicBool(std::sync::atomic::AtomicBool);

impl AtomicBool {
    /// `atomic.NewBool(v)` inside.
    // glue: constructor of the embedded atomic.Bool
    pub const fn new(v: bool) -> AtomicBool {
        AtomicBool(std::sync::atomic::AtomicBool::new(v))
    }

    /// `Load` (promoted from atomic.Bool).
    // glue: atomic.Bool.Load
    pub fn load(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Store` (promoted from atomic.Bool).
    // glue: atomic.Bool.Store
    pub fn store(&self, v: bool) {
        self.0.store(v, Ordering::SeqCst)
    }
}

impl Clone for AtomicBool {
    // glue: Go copies the struct (value semantics)
    fn clone(&self) -> Self {
        AtomicBool::new(self.load())
    }
}

impl PartialEq for AtomicBool {
    // glue: reflect.DeepEqual of two AtomicBool values (the inner uint32)
    fn eq(&self, other: &Self) -> bool {
        self.load() == other.load()
    }
}

/// `json.Marshal` of a config struct, as the generic value. With `as_float`, numbers are
/// what `json.Unmarshal` into `map[string]any` gives back (`float64`); otherwise integers
/// stay exact. Fields tagged `json:"-"` are skipped; untagged embedded structs are promoted
/// (the config structs have no name conflicts between promoted fields). `NaN` / `±Inf`
/// floats fail like Go's encoder (`*json.UnsupportedValueError`).
// glue: encoding/json reflection walk over GoStruct (encoder rules used by the config types)
pub fn to_json(s: &dyn GoStruct, as_float: bool) -> Result<json::Value, Error> {
    let mut m = BTreeMap::new();
    struct_to_json(s, as_float, &mut m)?;
    Ok(json::Value::Object(m))
}

// glue: see to_json
fn struct_to_json(
    s: &dyn GoStruct,
    as_float: bool,
    m: &mut BTreeMap<String, json::Value>,
) -> Result<(), Error> {
    for (i, f) in s.fields().iter().enumerate() {
        let Some(name) = f.json else { continue };
        if f.embedded
            && name == f.go_name
            && let ValRef::Struct(sub) = s.field(i)
        {
            struct_to_json(sub, as_float, m)?;
            continue;
        }
        m.insert(name.to_owned(), val_to_json(s.field(i), as_float)?);
    }
    Ok(())
}

// glue: see to_json
fn val_to_json(v: ValRef<'_>, as_float: bool) -> Result<json::Value, Error> {
    let int = |i: i128| {
        if as_float {
            json::Value::Number(i as f64)
        } else {
            json::Value::Int(i)
        }
    };
    Ok(match v {
        ValRef::Bool(b) => json::Value::Bool(*b),
        ValRef::Int(i) => int(*i as i128),
        ValRef::I32(i) => int(i128::from(*i)),
        ValRef::I64(i) => int(i128::from(*i)),
        ValRef::Uint(u) => int(*u as i128),
        ValRef::U32(u) => int(i128::from(*u)),
        ValRef::U64(u) => int(i128::from(*u)),
        ValRef::Duration(d) => int(i128::from(d.0)),
        ValRef::F64(f) => {
            if !f.is_finite() {
                let s = gofmt::sprintf("%v", &[Arg::from(*f)]);
                let mut msg = b"json: unsupported value: ".to_vec();
                msg.extend_from_slice(&s);
                return Err(Error::plain(msg));
            }
            json::Value::Number(*f)
        }
        ValRef::Str(s) => json::Value::String(s.clone()),
        ValRef::StrSlice(None) | ValRef::StrMap(None) => json::Value::Null,
        ValRef::StrSlice(Some(v)) => {
            json::Value::Array(v.iter().cloned().map(json::Value::String).collect())
        }
        ValRef::StrMap(Some(m)) => json::Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), json::Value::String(v.clone())))
                .collect(),
        ),
        ValRef::NullableBool(b) => match *b {
            nbTrue => json::Value::Bool(true),
            nbFalse => json::Value::Bool(false),
            _ => json::Value::Null,
        },
        // The MarshalJSON promoted from the embedded go.uber.org/atomic.Bool wins over
        // AtomicBool's MarshalText: a JSON bool.
        ValRef::AtomicBool(b) => json::Value::Bool(b.load()),
        ValRef::Struct(s) => to_json(s, as_float)?,
    })
}

/// `json.Unmarshal(json.Marshal(src))` into a zero value, for the config structs: every
/// field the encoder writes is restored exactly, `json:"-"` fields are zero, and a
/// `nullableBool` that is neither `nbTrue` nor `nbFalse` becomes `nbUnset`.
// glue: encoding/json round trip over GoStruct (CloneConf)
pub fn json_round_trip(dst: &mut dyn GoStruct) {
    for i in 0..dst.fields().len() {
        let skip = dst.fields()[i].json.is_none();
        match dst.field_mut(i) {
            ValMut::Struct(s) => {
                if skip {
                    set_zero(s);
                } else {
                    json_round_trip(s);
                }
            }
            ValMut::NullableBool(b) => {
                if skip || (*b != nbTrue && *b != nbFalse) {
                    *b = nbUnset;
                }
            }
            v if skip => set_zero_val(v),
            _ => {}
        }
    }
}

// glue: zero value of a struct (reflect.Zero)
fn set_zero(s: &mut dyn GoStruct) {
    for i in 0..s.fields().len() {
        set_zero_val(s.field_mut(i));
    }
}

// glue: zero value of a field (reflect.Zero)
fn set_zero_val(v: ValMut<'_>) {
    match v {
        ValMut::Bool(b) => *b = false,
        ValMut::Int(x) => *x = 0,
        ValMut::I32(x) => *x = 0,
        ValMut::I64(x) => *x = 0,
        ValMut::Uint(x) => *x = 0,
        ValMut::U32(x) => *x = 0,
        ValMut::U64(x) => *x = 0,
        ValMut::F64(x) => *x = 0.0,
        ValMut::Str(s) => s.clear(),
        ValMut::Duration(d) => *d = Duration(0),
        ValMut::StrSlice(s) => *s = None,
        ValMut::StrMap(m) => *m = None,
        ValMut::NullableBool(b) => *b = nbUnset,
        ValMut::AtomicBool(b) => *b = AtomicBool::new(false),
        ValMut::Struct(s) => set_zero(s),
    }
}

/// `reflect.DeepEqual` of two leaf values of the same Go type (`false` for different types).
// glue: reflect.DeepEqual on the leaf kinds of the config structs
pub fn leaf_equal(a: &ValRef<'_>, b: &ValRef<'_>) -> bool {
    match (a, b) {
        (ValRef::Bool(x), ValRef::Bool(y)) => x == y,
        (ValRef::Int(x), ValRef::Int(y)) => x == y,
        (ValRef::I32(x), ValRef::I32(y)) => x == y,
        (ValRef::I64(x), ValRef::I64(y)) => x == y,
        (ValRef::Uint(x), ValRef::Uint(y)) => x == y,
        (ValRef::U32(x), ValRef::U32(y)) => x == y,
        (ValRef::U64(x), ValRef::U64(y)) => x == y,
        (ValRef::F64(x), ValRef::F64(y)) => x == y,
        (ValRef::Str(x), ValRef::Str(y)) => x == y,
        (ValRef::Duration(x), ValRef::Duration(y)) => x == y,
        (ValRef::StrSlice(x), ValRef::StrSlice(y)) => x == y,
        (ValRef::StrMap(x), ValRef::StrMap(y)) => x == y,
        (ValRef::NullableBool(x), ValRef::NullableBool(y)) => x == y,
        (ValRef::AtomicBool(x), ValRef::AtomicBool(y)) => x == y,
        _ => false,
    }
}

/// `dst.Set(src)` for two leaf values of the same Go type.
// glue: reflect.Value.Set on the leaf kinds of the config structs
pub fn leaf_set(dst: ValMut<'_>, src: &ValRef<'_>) {
    match (dst, src) {
        (ValMut::Bool(d), ValRef::Bool(s)) => *d = **s,
        (ValMut::Int(d), ValRef::Int(s)) => *d = **s,
        (ValMut::I32(d), ValRef::I32(s)) => *d = **s,
        (ValMut::I64(d), ValRef::I64(s)) => *d = **s,
        (ValMut::Uint(d), ValRef::Uint(s)) => *d = **s,
        (ValMut::U32(d), ValRef::U32(s)) => *d = **s,
        (ValMut::U64(d), ValRef::U64(s)) => *d = **s,
        (ValMut::F64(d), ValRef::F64(s)) => *d = **s,
        (ValMut::Str(d), ValRef::Str(s)) => (*s).clone_into(d),
        (ValMut::Duration(d), ValRef::Duration(s)) => *d = **s,
        (ValMut::StrSlice(d), ValRef::StrSlice(s)) => *d = (*s).clone(),
        (ValMut::StrMap(d), ValRef::StrMap(s)) => *d = (*s).clone(),
        (ValMut::NullableBool(d), ValRef::NullableBool(s)) => *d = **s,
        (ValMut::AtomicBool(d), ValRef::AtomicBool(s)) => *d = (*s).clone(),
        _ => unreachable!("Set with values of different types"),
    }
}
