//! go-file: pkg/expression/collation.go
//!
//! Block D08.2 owns this file; part 1 of D08.1 ports the `collationInfo` embedded in
//! `Column` and `Constant`, the `Coercibility`/`Repertoire` types and the derivations for
//! columns and constants. The collation derivation of functions comes with D08.2.
//!
//! `coer`/`coerInit` are atomics in Go because `Coercibility()` initialises them lazily on
//! nodes that are shared between plans; they are atomics here too, so `&self` suffices.

#![allow(non_upper_case_globals)]

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use tidb_base::mysql;
use tidb_parser::types::{ETJson, ETString};
use tidb_types::etc::{FieldType, is_type_bit};

use crate::column::Column;
use crate::constant::Constant;

/// Coercibility values are used to check whether the collation of one item can be coerced
/// to the collation of other. See https://dev.mysql.com/doc/refman/8.0/en/charset-collation-coercibility.html
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Coercibility(pub i32);

/// CoercibilityExplicit is derived from an explicit COLLATE clause.
pub const CoercibilityExplicit: Coercibility = Coercibility(0);
/// CoercibilityNone is derived from the concatenation of two strings with different collations.
pub const CoercibilityNone: Coercibility = Coercibility(1);
/// CoercibilityImplicit is derived from a column or a stored routine parameter or local variable or cast() function.
pub const CoercibilityImplicit: Coercibility = Coercibility(2);
/// CoercibilitySysconst is derived from a "system constant" (the string returned by functions such as USER() or VERSION()).
pub const CoercibilitySysconst: Coercibility = Coercibility(3);
/// CoercibilityCoercible is derived from a literal.
pub const CoercibilityCoercible: Coercibility = Coercibility(4);
/// CoercibilityNumeric is derived from a numeric or temporal value.
pub const CoercibilityNumeric: Coercibility = Coercibility(5);
/// CoercibilityIgnorable is derived from NULL or an expression that is derived from NULL.
pub const CoercibilityIgnorable: Coercibility = Coercibility(6);

/// Repertoire is the set of characters a string can contain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Repertoire(pub i32);

/// ASCII is pure ASCII U+0000..U+007F.
pub const ASCII: Repertoire = Repertoire(0x01);
/// EXTENDED is extended characters: U+0080..U+FFFF
pub const EXTENDED: Repertoire = Repertoire(0x01 << 1);
/// UNICODE is ASCII | EXTENDED
pub const UNICODE: Repertoire = Repertoire(0x01 | (0x01 << 1));

/// The `collationInfo` embedded in expression nodes.
#[derive(Debug, Default)]
pub struct CollationInfo {
    coer: AtomicI32,
    coer_init: AtomicBool,
    pub(crate) repertoire: Repertoire,

    pub(crate) charset: String,
    pub(crate) collation: String,

    is_explicit_charset: bool,
}

impl Clone for CollationInfo {
    // glue: Go copies the struct by value (the atomics are copied with it)
    fn clone(&self) -> Self {
        CollationInfo {
            coer: AtomicI32::new(self.coer.load(Ordering::Relaxed)),
            coer_init: AtomicBool::new(self.coer_init.load(Ordering::Relaxed)),
            repertoire: self.repertoire,
            charset: self.charset.clone(),
            collation: self.collation.clone(),
            is_explicit_charset: self.is_explicit_charset,
        }
    }
}

impl CollationInfo {
    // go: collationInfo.HasCoercibility L1
    pub fn has_coercibility(&self) -> bool {
        self.coer_init.load(Ordering::SeqCst)
    }

    // go: collationInfo.Coercibility L1
    pub fn coercibility(&self) -> Coercibility {
        Coercibility(self.coer.load(Ordering::SeqCst))
    }

    // go: collationInfo.SetCoercibility L1
    pub fn set_coercibility(&self, val: Coercibility) {
        self.coer.store(val.0, Ordering::SeqCst);
        self.coer_init.store(true, Ordering::SeqCst);
    }

    // go: collationInfo.Repertoire L1
    pub fn repertoire(&self) -> Repertoire {
        self.repertoire
    }

    // go: collationInfo.SetRepertoire L1
    pub fn set_repertoire(&mut self, r: Repertoire) {
        self.repertoire = r;
    }

    // go: collationInfo.SetCharsetAndCollation L1
    pub fn set_charset_and_collation(&mut self, chs: &str, coll: &str) {
        self.charset = chs.to_string();
        self.collation = coll.to_string();
    }

    // go: collationInfo.CharsetAndCollation L1
    pub fn charset_and_collation(&self) -> (&str, &str) {
        (&self.charset, &self.collation)
    }

    // go: collationInfo.IsExplicitCharset L1
    pub fn is_explicit_charset(&self) -> bool {
        self.is_explicit_charset
    }

    // go: collationInfo.SetExplicitCharset L1
    pub fn set_explicit_charset(&mut self, explicit: bool) {
        self.is_explicit_charset = explicit;
    }

    /// Byte size of the two strings (used by `MemoryUsage`).
    // glue: len(col.charset)+len(col.collation) in Column.MemoryUsage
    pub(crate) fn strings_len(&self) -> i64 {
        (self.charset.len() + self.collation.len()) as i64
    }
}

// go: deriveCoercibilityForConstant L1
pub(crate) fn derive_coercibility_for_constant(c: &Constant) -> Coercibility {
    if c.value.is_null() {
        return CoercibilityIgnorable;
    } else if c.ret_type.eval_type() != ETString {
        return CoercibilityNumeric;
    }
    CoercibilityCoercible
}

// go: deriveCoercibilityForColumn L1
pub(crate) fn derive_coercibility_for_column(c: &Column) -> Coercibility {
    let rt: &FieldType = &c.ret_type;
    // For specified type null, it should return CoercibilityIgnorable, which means it got the lowest priority in DeriveCollationFromExprs.
    if rt.get_type() == mysql::TypeNull {
        return CoercibilityIgnorable;
    } else if is_type_bit(rt) {
        return CoercibilityImplicit;
    }

    let et = rt.eval_type();
    if et == ETJson {
    } else if et == ETString {
        return CoercibilityImplicit;
    } else {
        return CoercibilityNumeric;
    }
    CoercibilityImplicit
}
