//! go-file: pkg/types/enum.go
//!
//! Mapping: Go `string` names may hold any byte (`Vec<u8>`). `errors.Wrap(ErrTruncated,
//! msg)` is `ErrTruncated` itself (M-ERR: wrappers are not ported; Go callers only look at
//! the cause), so the message text is not kept.

use crate::collate;
use crate::errors::{ErrTruncated, Error};
use crate::gostd::parse_uint_base0;
use crate::stringutil;

/// Enum is for MySQL enum type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Enum {
    pub name: Vec<u8>,
    pub value: u64,
}

impl Enum {
    /// Copy deep copy an Enum.
    // go: Enum.Copy L1
    pub fn copy(&self) -> Enum {
        Enum {
            name: stringutil::copy(&self.name),
            value: self.value,
        }
    }

    /// String implements fmt.Stringer interface.
    // go: Enum.String L1
    pub fn string(&self) -> Vec<u8> {
        self.name.clone()
    }

    /// ToNumber changes enum index to float64 for numeric operation.
    // go: Enum.ToNumber L1
    pub fn to_number(&self) -> f64 {
        self.value as f64
    }
}

/// ParseEnum creates a Enum with item name or value.
// go: ParseEnum L1 [M-ERR]
pub fn parse_enum(elems: &[Vec<u8>], name: &[u8], collation: &str) -> Result<Enum, Error> {
    if let Ok(enum_name) = parse_enum_name(elems, name, collation) {
        return Ok(enum_name);
    }
    // name doesn't exist, maybe an integer?
    if let Some(num) = parse_uint_base0(name) {
        return parse_enum_value(elems, num);
    }
    Err(ErrTruncated.share())
}

/// ParseEnumName creates a Enum with item name.
// go: ParseEnumName L1 [M-ERR]
pub fn parse_enum_name(elems: &[Vec<u8>], name: &[u8], collation: &str) -> Result<Enum, Error> {
    let ctor = collate::get_collator(collation);
    for (i, n) in elems.iter().enumerate() {
        if ctor.compare(n, name) == 0 {
            return Ok(Enum {
                name: n.clone(),
                value: i as u64 + 1,
            });
        }
    }
    Err(ErrTruncated.share())
}

/// ParseEnumValue creates a Enum with special number.
// go: ParseEnumValue L1 [M-ERR]
pub fn parse_enum_value(elems: &[Vec<u8>], number: u64) -> Result<Enum, Error> {
    if number == 0 || number > elems.len() as u64 {
        return Err(ErrTruncated.share());
    }
    Ok(Enum {
        name: elems[(number - 1) as usize].clone(),
        value: number,
    })
}
