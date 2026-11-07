//! A Go `interface{}` field of the model structs (`ColumnInfo.DefaultValue`, ...).

use tidb_base::Error;
use tidb_base::gostd::json::{self, Decoder, Raw, Value};

/// A Go `any` as the model structs hold it. Decoded from JSON it is a generic value (numbers
/// are `float64`, as `json.Unmarshal` without `UseNumber` gives); set by Go code it may also
/// be a Go `string` that is not UTF-8 (the raw bytes of a BIT or BINARY default), which
/// `json.Marshal` writes with each invalid byte as `�`.
#[derive(Clone, Debug, PartialEq)]
pub enum AnyValue {
    /// A JSON-shaped value (`nil`, `bool`, `float64`, an integer, a UTF-8 `string`, `[]any`,
    /// `map[string]any`).
    Value(Value),
    /// A Go `string` holding arbitrary bytes.
    GoString(Vec<u8>),
}

impl Default for AnyValue {
    // glue: the nil interface
    fn default() -> AnyValue {
        AnyValue::Value(Value::Null)
    }
}

impl AnyValue {
    /// Whether the interface is nil (`isEmptyValue` for `omitempty`).
    // glue: v.IsNil() of an interface
    pub fn is_nil(&self) -> bool {
        matches!(self, AnyValue::Value(Value::Null))
    }

    /// `json.Marshal` of the dynamic value (NaN / ±Inf are an error, like Go).
    // go-std: encoding/json.interfaceEncoder L2 (the dynamic value's encoder)
    pub fn encode_json(&self, out: &mut String) -> Result<(), Error> {
        match self {
            AnyValue::Value(v) => json::encode_value(out, v).map_err(Error::plain),
            AnyValue::GoString(b) => {
                json::append_go_string(out, b);
                Ok(())
            }
        }
    }

    /// `json.Unmarshal` into an `interface{}` field: replaced by the generic value.
    // go-std: encoding/json.decodeState.valueInterface L2 (into an empty interface field)
    pub fn decode_json(&mut self, d: &mut Decoder, v: &Raw) {
        let mut x = Value::Null;
        d.any(v, &mut x);
        *self = AnyValue::Value(x);
    }

    /// The Go `string` the interface holds, if it holds one (`value.(string)`).
    // glue: type assertion value.(string)
    pub fn as_go_string(&self) -> Option<&[u8]> {
        match self {
            AnyValue::Value(Value::String(s)) => Some(s.as_bytes()),
            AnyValue::GoString(b) => Some(b),
            _ => None,
        }
    }
}
