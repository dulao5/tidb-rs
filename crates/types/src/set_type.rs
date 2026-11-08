//! go-file: pkg/types/set.go
//!
//! Mapping: Go `string` names may hold any byte (`Vec<u8>`); `%v` of the `[]string`
//! element list in the error messages is Go's `[a b c]`.

use std::collections::HashSet;

use tidb_base::errors::errorf;
use tidb_base::gofmt::Arg;

use crate::collate;
use crate::errors::Error;
use crate::gostd::parse_uint_base0;
use crate::stringutil;

/// Set is for MySQL Set type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Set {
    pub name: Vec<u8>,
    pub value: u64,
}

/// `zeroSet`.
// glue: Go package-level var zeroSet
fn zero_set() -> Set {
    Set::default()
}

/// `%v` of a Go `[]string`: `[a b c]`.
// glue: fmt %v of []string in the error messages
fn elems_arg(elems: &[Vec<u8>]) -> Arg {
    let mut v = vec![b'['];
    for (i, e) in elems.iter().enumerate() {
        if i > 0 {
            v.push(b' ');
        }
        v.extend_from_slice(e);
    }
    v.push(b']');
    Arg::go_string(v)
}

impl Set {
    /// String implements fmt.Stringer interface.
    // go: Set.String L1
    pub fn string(&self) -> Vec<u8> {
        self.name.clone()
    }

    /// ToNumber changes Set to float64 for numeric operation.
    // go: Set.ToNumber L1
    pub fn to_number(&self) -> f64 {
        self.value as f64
    }

    /// Copy deep copy a Set.
    // go: Set.Copy L1
    pub fn copy(&self) -> Set {
        Set {
            name: stringutil::copy(&self.name),
            value: self.value,
        }
    }
}

/// ParseSet creates a Set with name or value.
// go: ParseSet L1
pub fn parse_set(elems: &[Vec<u8>], name: &[u8], collation: &str) -> Result<Set, Error> {
    if let Ok(set_name) = parse_set_name(elems, name, collation) {
        return Ok(set_name);
    }
    // name doesn't exist, maybe an integer?
    if let Some(num) = parse_uint_base0(name) {
        return parse_set_value(elems, num);
    }
    Err(errorf(
        "item %s is not in Set %v",
        &[Arg::go_string(name.to_vec()), elems_arg(elems)],
    ))
}

/// ParseSetName creates a Set with name.
// go: ParseSetName L1
pub fn parse_set_name(elems: &[Vec<u8>], name: &[u8], collation: &str) -> Result<Set, Error> {
    if name.is_empty() {
        return Ok(zero_set());
    }
    let ctor = collate::get_collator(collation);
    let seps = name.split(|&c| c == b',');
    let mut marked: HashSet<Vec<u8>> = HashSet::new();
    for s in seps {
        marked.insert(ctor.key(s));
    }
    let mut items: Vec<&[u8]> = Vec::new();
    let mut value = 0u64;
    for (i, n) in elems.iter().enumerate() {
        let key = ctor.key(n);
        if marked.contains(&key) {
            // Go: `1 << uint64(i)` is 0 for i >= 64.
            if i < 64 {
                value |= 1 << i;
            }
            marked.remove(&key);
            items.push(n);
        }
    }
    if marked.is_empty() {
        return Ok(Set {
            name: items.join(&b","[..]),
            value,
        });
    }
    Err(errorf(
        "item %s is not in Set %v",
        &[Arg::go_string(name.to_vec()), elems_arg(elems)],
    ))
}

/// ParseSetValue creates a Set with special number.
///
/// Go indexes its 64-entry `setIndexValue` table with every element position, so more than
/// 64 elements and a number with a bit beyond them panics (index out of range); so does this.
// go: ParseSetValue L1
pub fn parse_set_value(elems: &[Vec<u8>], number: u64) -> Result<Set, Error> {
    if number == 0 {
        return Ok(zero_set());
    }
    let value = number;
    let mut number = number;
    let mut items: Vec<&[u8]> = Vec::new();
    for (i, e) in elems.iter().enumerate() {
        let bit = SET_INDEX_VALUE[i];
        if number & bit > 0 {
            items.push(e);
            number &= !bit;
        }
    }
    if number != 0 {
        return Err(errorf(
            "invalid number %d for Set %v",
            &[Arg::from(number), elems_arg(elems)],
        ));
    }
    Ok(Set {
        name: items.join(&b","[..]),
        value,
    })
}

/// `setIndexValue` (Go builds it in `init`; `setIndexInvertValue` is its complement).
// go: init L2 (setIndexValue as a const table; the inverse is computed with `!`)
const SET_INDEX_VALUE: [u64; 64] = {
    let mut t = [0u64; 64];
    let mut i = 0;
    while i < 64 {
        t[i] = 1 << i;
        i += 1;
    }
    t
};
