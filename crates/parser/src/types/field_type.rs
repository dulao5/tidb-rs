//! go-file: pkg/parser/types/field_type.go
//!
//! `types.FieldType`: the column / expression type every layer uses, and its JSON form (part
//! of the schema stored in TiKV by Go TiDB, so it must be byte-identical).
//!
//! Go strings: `charset` and `collate` are charset / collation names (ASCII) and are held as
//! `String`; enum / set elements may be binary literals (`elemsIsBinaryLit`), so they are
//! bytes. A Go nil slice is `None` (`json.Marshal` writes `null` for it and `[]` for an empty
//! one).

#![allow(non_upper_case_globals)]

use std::sync::atomic::{AtomicBool, Ordering};

use tidb_base::Error;
use tidb_base::gostd::json::{self, Decoder};
use tidb_base::mysql;

use super::etc::{dig2bytes, digitsPerWord, is_type_blob, is_type_char, type_to_str, wordSize};
use super::eval_type::*;

/// UnspecifiedLength is unspecified length.
pub const UnspecifiedLength: isize = -1;

/// TiDBStrictIntegerDisplayWidth represent whether return warnings when integerType with
/// (length) was parsed. The default is `false`. A Go package variable set by the config.
pub static TiDBStrictIntegerDisplayWidth: AtomicBool = AtomicBool::new(false);

/// IHasher is internal usage represent cascades/base.Hasher.
pub trait IHasher {
    /// HashBool.
    // glue: method of the Go IHasher interface
    fn hash_bool(&mut self, val: bool);
    /// HashInt.
    // glue: method of the Go IHasher interface
    fn hash_int(&mut self, val: isize);
    /// HashInt64.
    // glue: method of the Go IHasher interface
    fn hash_int64(&mut self, val: i64);
    /// HashUint64.
    // glue: method of the Go IHasher interface
    fn hash_uint64(&mut self, val: u64);
    /// HashFloat64.
    // glue: method of the Go IHasher interface
    fn hash_float64(&mut self, val: f64);
    /// HashRune.
    // glue: method of the Go IHasher interface
    fn hash_rune(&mut self, val: char);
    /// HashString (Go string bytes).
    // glue: method of the Go IHasher interface
    fn hash_string(&mut self, val: &[u8]);
    /// HashByte.
    // glue: method of the Go IHasher interface
    fn hash_byte(&mut self, val: u8);
    /// HashBytes.
    // glue: method of the Go IHasher interface
    fn hash_bytes(&mut self, val: &[u8]);
    /// Reset.
    // glue: method of the Go IHasher interface
    fn reset(&mut self);
    /// Sum64.
    // glue: method of the Go IHasher interface
    fn sum64(&self) -> u64;
}

/// FieldType records field type information.
#[derive(Debug, Default, PartialEq, Eq, Hash)]
pub struct FieldType {
    /// tp is type of the field
    tp: u8,
    /// flag represent NotNull, Unsigned, PriKey flags etc.
    flag: usize,
    /// flen represent size of bytes of the field
    flen: isize,
    /// decimal represent decimal length of the field
    decimal: isize,
    /// charset represent character set
    charset: String,
    /// collate represent collate rules of the charset
    collate: String,
    /// elems is the element list for enum and set type.
    elems: Option<Vec<Vec<u8>>>,
    elems_is_binary_lit: Option<Vec<bool>>,
    array: bool,
}

/// `len(s)` of a Go slice held as `Option<Vec<_>>`.
// glue: len of a possibly nil Go slice
fn slice_len<T>(s: &Option<Vec<T>>) -> usize {
    s.as_ref().map_or(0, Vec::len)
}

/// `slices.Equal` of two possibly nil Go slices (nil and empty are equal).
// glue: slices.Equal on possibly nil Go slices
fn slices_equal<T: PartialEq>(a: &Option<Vec<T>>, b: &Option<Vec<T>>) -> bool {
    a.as_deref().unwrap_or(&[]) == b.as_deref().unwrap_or(&[])
}

/// `ctx.WriteKeyWord` with `format.DefaultRestoreFlags` (upper case).
// go: pkg/parser/format/format.go RestoreCtx.WriteKeyWord L2 (DefaultRestoreFlags only)
fn write_keyword(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(tidb_base::gostd::to_upper(s).as_bytes());
}

/// `ctx.WriteString` with `format.DefaultRestoreFlags` (single quotes doubled).
// go: pkg/parser/format/format.go RestoreCtx.WriteString L2 (DefaultRestoreFlags only)
fn write_string(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'\'');
    for &b in s {
        if b == b'\'' {
            out.extend_from_slice(b"''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
}

/// `format.OutputFormat`: escapes `\0`, `'`, `\n`, `\r`; iterates runes, so an invalid
/// UTF-8 byte becomes U+FFFD.
// go: pkg/parser/format/format.go OutputFormat L1
fn output_format(s: &[u8]) -> String {
    let mut buf = String::new();
    for r in tidb_base::gostd::runes(s) {
        match r {
            '\0' => buf.push_str("\\0"),
            '\'' => buf.push_str("''"),
            '\n' => buf.push_str("\\n"),
            '\r' => buf.push_str("\\r"),
            r => buf.push(r),
        }
    }
    buf
}

impl FieldType {
    /// DeepCopy returns a deep copy of the FieldType (empty slices become nil).
    // go: FieldType.DeepCopy L1
    pub fn deep_copy(&self) -> FieldType {
        FieldType {
            tp: self.tp,
            flag: self.flag,
            flen: self.flen,
            decimal: self.decimal,
            charset: self.charset.clone(),
            collate: self.collate.clone(),
            array: self.array,
            elems: self.elems.clone().filter(|e| !e.is_empty()),
            elems_is_binary_lit: self.elems_is_binary_lit.clone().filter(|e| !e.is_empty()),
        }
    }

    /// Hash64 implements the cascades/base.Hasher.<0th> interface.
    // go: FieldType.Hash64 L1
    pub fn hash64(&self, h: &mut dyn IHasher) {
        h.hash_byte(self.tp);
        h.hash_uint64(self.flag as u64);
        h.hash_int(self.flen);
        h.hash_int(self.decimal);
        h.hash_string(self.charset.as_bytes());
        h.hash_string(self.collate.as_bytes());
        h.hash_int(slice_len(&self.elems) as isize);
        for elem in self.elems.iter().flatten() {
            h.hash_string(elem);
        }
        h.hash_int(slice_len(&self.elems_is_binary_lit) as isize);
        for elem in self.elems_is_binary_lit.iter().flatten() {
            h.hash_bool(*elem);
        }
        h.hash_bool(self.array);
    }

    /// Equals implements the cascades/base.Hasher.<1th> interface. `other` is the Go `any`
    /// holding a `*FieldType` or `FieldType`; `None` is a nil interface.
    // go: FieldType.Equals L1
    pub fn equals(&self, other: Option<&FieldType>) -> bool {
        let Some(ft2) = other else {
            return false;
        };
        let ok = self.tp == ft2.tp
            && self.flag == ft2.flag
            && self.flen == ft2.flen
            && self.decimal == ft2.decimal
            && self.charset == ft2.charset
            && self.collate == ft2.collate
            && self.array == ft2.array;
        if !ok {
            return false;
        }
        slices_equal(&self.elems, &ft2.elems)
            && slices_equal(&self.elems_is_binary_lit, &ft2.elems_is_binary_lit)
    }

    /// NewFieldType returns a FieldType, with a type and other information about field type.
    // go: NewFieldType L1
    pub fn new(tp: u8) -> FieldType {
        FieldType {
            tp,
            flen: UnspecifiedLength,
            decimal: UnspecifiedLength,
            ..FieldType::default()
        }
    }

    /// IsDecimalValid checks whether the decimal is valid.
    // go: FieldType.IsDecimalValid L1
    pub fn is_decimal_valid(&self) -> bool {
        !(self.get_type() == mysql::TypeNewDecimal
            && (self.decimal < 0
                || self.decimal > mysql::MaxDecimalScale
                || self.flen <= 0
                || self.flen > mysql::MaxDecimalWidth
                || self.flen < self.decimal))
    }

    /// IsVarLengthType Determine whether the column type is a variable-length type.
    // go: FieldType.IsVarLengthType L1
    pub fn is_var_length_type(&self) -> bool {
        matches!(
            self.get_type(),
            mysql::TypeVarchar
                | mysql::TypeVarString
                | mysql::TypeJSON
                | mysql::TypeBlob
                | mysql::TypeTinyBlob
                | mysql::TypeMediumBlob
                | mysql::TypeLongBlob
                | mysql::TypeTiDBVectorFloat32
        )
    }

    /// GetType returns the type of the FieldType.
    // go: FieldType.GetType L1
    pub fn get_type(&self) -> u8 {
        if self.array {
            return mysql::TypeJSON;
        }
        self.tp
    }

    /// GetFlag returns the flag of the FieldType.
    // go: FieldType.GetFlag L1
    #[inline]
    pub fn get_flag(&self) -> usize {
        self.flag
    }

    /// GetFlen returns the length of the field.
    // go: FieldType.GetFlen L1
    pub fn get_flen(&self) -> isize {
        self.flen
    }

    /// GetDecimal returns the decimal of the FieldType.
    // go: FieldType.GetDecimal L1
    pub fn get_decimal(&self) -> isize {
        self.decimal
    }

    /// GetCharset returns the field's charset.
    // go: FieldType.GetCharset L1
    pub fn get_charset(&self) -> &str {
        &self.charset
    }

    /// GetCollate returns the collation of the field.
    // go: FieldType.GetCollate L1
    pub fn get_collate(&self) -> &str {
        &self.collate
    }

    /// GetElems returns the elements of the FieldType (`None`: nil).
    // go: FieldType.GetElems L1
    pub fn get_elems(&self) -> Option<&[Vec<u8>]> {
        self.elems.as_deref()
    }

    /// SetType sets the type of the FieldType.
    // go: FieldType.SetType L1
    pub fn set_type(&mut self, tp: u8) {
        self.tp = tp;
        self.array = false;
    }

    /// SetFlag sets the flag of the FieldType.
    // go: FieldType.SetFlag L1
    pub fn set_flag(&mut self, flag: usize) {
        self.flag = flag;
    }

    /// AddFlag adds a flag to the FieldType.
    // go: FieldType.AddFlag L1
    pub fn add_flag(&mut self, flag: usize) {
        self.flag |= flag;
    }

    /// AndFlag and the flag of the FieldType.
    // go: FieldType.AndFlag L1
    pub fn and_flag(&mut self, flag: usize) {
        self.flag &= flag;
    }

    /// ToggleFlag toggle the flag of the FieldType.
    // go: FieldType.ToggleFlag L1
    pub fn toggle_flag(&mut self, flag: usize) {
        self.flag ^= flag;
    }

    /// DelFlag delete the flag of the FieldType.
    // go: FieldType.DelFlag L1
    pub fn del_flag(&mut self, flag: usize) {
        self.flag &= !flag;
    }

    /// SetFlen sets the length of the field.
    // go: FieldType.SetFlen L1
    pub fn set_flen(&mut self, flen: isize) {
        self.flen = flen;
    }

    /// SetFlenUnderLimit sets the length of the field to the value of the argument.
    // go: FieldType.SetFlenUnderLimit L1
    pub fn set_flen_under_limit(&mut self, flen: isize) {
        if self.get_type() == mysql::TypeNewDecimal {
            self.flen = flen.min(mysql::MaxDecimalWidth);
        } else {
            self.flen = flen;
        }
    }

    /// SetDecimal sets the decimal of the FieldType.
    // go: FieldType.SetDecimal L1
    pub fn set_decimal(&mut self, decimal: isize) {
        self.decimal = decimal;
    }

    /// SetDecimalUnderLimit sets the decimal of the field to the value of the argument.
    // go: FieldType.SetDecimalUnderLimit L1
    pub fn set_decimal_under_limit(&mut self, decimal: isize) {
        if self.get_type() == mysql::TypeNewDecimal {
            self.decimal = decimal.min(mysql::MaxDecimalScale);
        } else {
            self.decimal = decimal;
        }
    }

    /// UpdateFlenAndDecimalUnderLimit updates the length and decimal to the value of the
    /// argument.
    // go: FieldType.UpdateFlenAndDecimalUnderLimit L1 [M-INT]
    pub fn update_flen_and_decimal_under_limit(
        &mut self,
        old: &FieldType,
        delta_decimal: isize,
        mut delta_flen: isize,
    ) {
        if self.get_type() != mysql::TypeNewDecimal {
            return;
        }
        if old.decimal < 0 {
            delta_flen = delta_flen.wrapping_add(mysql::MaxDecimalScale);
            self.decimal = mysql::MaxDecimalScale;
        } else {
            self.set_decimal(old.decimal.wrapping_add(delta_decimal));
        }
        if old.flen < 0 {
            self.flen = mysql::MaxDecimalWidth;
        } else {
            self.set_flen_under_limit(old.flen.wrapping_add(delta_flen));
        }
    }

    /// SetCharset sets the charset of the FieldType.
    // go: FieldType.SetCharset L1
    pub fn set_charset(&mut self, charset: &str) {
        charset.clone_into(&mut self.charset);
    }

    /// SetCollate sets the collation of the FieldType.
    // go: FieldType.SetCollate L1
    pub fn set_collate(&mut self, collate: &str) {
        collate.clone_into(&mut self.collate);
    }

    /// SetElems sets the elements of the FieldType (`None`: nil).
    // go: FieldType.SetElems L1
    pub fn set_elems(&mut self, elems: Option<Vec<Vec<u8>>>) {
        self.elems = elems;
    }

    /// SetElem sets the element of the FieldType. Panics if `idx` is out of range, as Go.
    // go: FieldType.SetElem L1
    pub fn set_elem(&mut self, idx: usize, element: Vec<u8>) {
        self.elems.as_mut().expect("index out of range")[idx] = element;
    }

    /// SetArray sets the array field of the FieldType.
    // go: FieldType.SetArray L1
    pub fn set_array(&mut self, array: bool) {
        self.array = array;
    }

    /// IsArray return true if the filed type is array.
    // go: FieldType.IsArray L1
    pub fn is_array(&self) -> bool {
        self.array
    }

    /// ArrayType return the type of the array.
    // go: FieldType.ArrayType L1
    pub fn array_type(&self) -> FieldType {
        if !self.array {
            return self.clone();
        }
        let mut clone = self.clone();
        clone.set_array(false);
        clone
    }

    /// SetElemWithIsBinaryLit sets the element of the FieldType.
    // go: FieldType.SetElemWithIsBinaryLit L1
    pub fn set_elem_with_is_binary_lit(
        &mut self,
        idx: usize,
        element: Vec<u8>,
        is_binary_lit: bool,
    ) {
        let n = slice_len(&self.elems);
        self.elems.as_mut().expect("index out of range")[idx] = element;
        if is_binary_lit {
            // Create the binary literal flags lazily.
            self.elems_is_binary_lit
                .get_or_insert_with(|| vec![false; n])[idx] = true;
        }
    }

    /// GetElem returns the element of the FieldType.
    // go: FieldType.GetElem L1
    pub fn get_elem(&self, idx: usize) -> &[u8] {
        &self.elems.as_ref().expect("index out of range")[idx]
    }

    /// GetElemIsBinaryLit returns the binary literal flag of the element at index idx.
    // go: FieldType.GetElemIsBinaryLit L1
    pub fn get_elem_is_binary_lit(&self, idx: usize) -> bool {
        match &self.elems_is_binary_lit {
            Some(v) if !v.is_empty() => v[idx],
            _ => false,
        }
    }

    /// CleanElemIsBinaryLit cleans the binary literal flags.
    // go: FieldType.CleanElemIsBinaryLit L1
    pub fn clean_elem_is_binary_lit(&mut self) {
        self.elems_is_binary_lit = None;
    }

    /// Equal checks whether two FieldType objects are equal.
    // go: FieldType.Equal L1
    pub fn equal(&self, other: &FieldType) -> bool {
        // We do not need to compare whole `ft.flag == other.flag` when wrapping cast upon an
        // Expression, but need compare unsigned_flag of ft.flag. When tp is float or double
        // with decimal unspecified, do not check whether flen is equal, because flen for them
        // is useless. The decimal field can be ignored if the type is int or string.
        let tp_equal = (self.get_type() == other.get_type())
            || (self.get_type() == mysql::TypeVarchar && other.get_type() == mysql::TypeVarString)
            || (self.get_type() == mysql::TypeVarString && other.get_type() == mysql::TypeVarchar);
        let flen_equal = self.flen == other.flen
            || (self.eval_type() == ETReal && self.decimal == UnspecifiedLength)
            || self.eval_type() == ETJson;
        let ignore_decimal = self.eval_type() == ETInt || self.eval_type() == ETString;
        let partial_equal = tp_equal
            && (ignore_decimal || self.decimal == other.decimal)
            && self.charset == other.charset
            && self.collate == other.collate
            && flen_equal
            && mysql::has_unsigned_flag(self.flag) == mysql::has_unsigned_flag(other.flag);
        if !partial_equal {
            return false;
        }
        slices_equal(&self.elems, &other.elems)
    }

    /// PartialEqual checks whether two FieldType objects are equal. If unsafe is true and
    /// the objects is string type, PartialEqual will ignore flen.
    // go: FieldType.PartialEqual L1
    pub fn partial_equal(&self, other: &FieldType, unsafe_: bool) -> bool {
        if !unsafe_ || self.eval_type() != ETString || other.eval_type() != ETString {
            return self.equal(other);
        }
        let partial_equal = self.charset == other.charset
            && self.collate == other.collate
            && mysql::has_unsigned_flag(self.flag) == mysql::has_unsigned_flag(other.flag);
        if !partial_equal {
            return false;
        }
        slices_equal(&self.elems, &other.elems)
    }

    /// EvalType gets the type in evaluation.
    // go: FieldType.EvalType L1
    pub fn eval_type(&self) -> EvalType {
        match self.get_type() {
            mysql::TypeTiny
            | mysql::TypeShort
            | mysql::TypeInt24
            | mysql::TypeLong
            | mysql::TypeLonglong
            | mysql::TypeBit
            | mysql::TypeYear => ETInt,
            mysql::TypeFloat | mysql::TypeDouble => ETReal,
            mysql::TypeNewDecimal => ETDecimal,
            mysql::TypeDate | mysql::TypeDatetime => ETDatetime,
            mysql::TypeTimestamp => ETTimestamp,
            mysql::TypeDuration => ETDuration,
            mysql::TypeJSON => ETJson,
            mysql::TypeTiDBVectorFloat32 => ETVectorFloat32,
            mysql::TypeEnum | mysql::TypeSet if self.flag & mysql::EnumSetAsIntFlag > 0 => ETInt,
            _ => ETString,
        }
    }

    /// Hybrid checks whether a type is a hybrid type, which can represent different types of
    /// value in specific context.
    // go: FieldType.Hybrid L1
    pub fn hybrid(&self) -> bool {
        self.get_type() == mysql::TypeEnum
            || self.get_type() == mysql::TypeBit
            || self.get_type() == mysql::TypeSet
    }

    /// Init initializes the FieldType data.
    // go: FieldType.Init L1
    pub fn init(&mut self, tp: u8) {
        self.tp = tp;
        self.flen = UnspecifiedLength;
        self.decimal = UnspecifiedLength;
    }

    /// CompactStr only considers tp/CharsetBin/flen/Deimal. This is used for showing column
    /// type in infoschema.
    // go: FieldType.CompactStr L1
    pub fn compact_str(&self) -> String {
        let ts = type_to_str(self.get_type(), &self.charset);
        let mut suffix = String::new();

        let (default_flen, default_decimal) =
            mysql::get_default_field_length_and_decimal(self.get_type());
        let is_decimal_not_default = self.decimal != default_decimal
            && self.decimal != 0
            && self.decimal != UnspecifiedLength;

        // displayFlen and displayDecimal are flen and decimal values with `-1` substituted
        // with default value.
        let (mut display_flen, mut display_decimal) = (self.flen, self.decimal);
        if display_flen == UnspecifiedLength {
            display_flen = default_flen;
        }
        if display_decimal == UnspecifiedLength {
            display_decimal = default_decimal;
        }

        let strict = TiDBStrictIntegerDisplayWidth.load(Ordering::Relaxed);
        match self.get_type() {
            mysql::TypeEnum | mysql::TypeSet => {
                // Format is ENUM ('e1', 'e2') or SET ('e1', 'e2')
                let es: Vec<String> = self
                    .elems
                    .iter()
                    .flatten()
                    .map(|e| output_format(e))
                    .collect();
                suffix = format!("('{}')", es.join("','"));
            }
            mysql::TypeTimestamp | mysql::TypeDatetime | mysql::TypeDuration => {
                if is_decimal_not_default {
                    suffix = format!("({display_decimal})");
                }
            }
            mysql::TypeDouble | mysql::TypeFloat => {
                if is_decimal_not_default {
                    suffix = format!("({display_flen},{display_decimal})");
                }
            }
            mysql::TypeNewDecimal => {
                suffix = format!("({display_flen},{display_decimal})");
            }
            mysql::TypeBit | mysql::TypeVarchar | mysql::TypeString | mysql::TypeVarString => {
                suffix = format!("({display_flen})");
            }
            mysql::TypeTiny => {
                // With display length deprecation active tinyint(1) still has a display
                // length to indicate this might have been a BOOL. Connectors expect this.
                if !strict || (mysql::has_zerofill_flag(self.flag) || display_flen == 1) {
                    suffix = format!("({display_flen})");
                }
            }
            mysql::TypeShort | mysql::TypeInt24 | mysql::TypeLong | mysql::TypeLonglong => {
                // The integer max display length is deprecated in MySQL 8.0 (issue #6688).
                if !strict || mysql::has_zerofill_flag(self.flag) {
                    suffix = format!("({display_flen})");
                }
            }
            mysql::TypeYear => {
                suffix = format!("({})", self.flen);
            }
            mysql::TypeTiDBVectorFloat32 => {
                if self.flen != UnspecifiedLength {
                    suffix = format!("({})", self.flen);
                }
            }
            mysql::TypeNull => {
                suffix = "(0)".to_owned();
            }
            _ => {}
        }
        ts + &suffix
    }

    /// InfoSchemaStr joins the CompactStr with unsigned flag and returns a string.
    // go: FieldType.InfoSchemaStr L1
    pub fn info_schema_str(&self) -> String {
        let mut suffix = "";
        if mysql::has_unsigned_flag(self.flag)
            && self.get_type() != mysql::TypeBit
            && self.get_type() != mysql::TypeYear
        {
            suffix = " unsigned";
        }
        self.compact_str() + suffix
    }

    /// String joins the information of FieldType and returns a string. When flen or decimal
    /// is unspecified, this function will use the default value instead of -1.
    // go: FieldType.String L1 [M-NAME]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut strs = vec![self.compact_str()];
        if mysql::has_unsigned_flag(self.flag) {
            strs.push("UNSIGNED".to_owned());
        }
        if mysql::has_zerofill_flag(self.flag) {
            strs.push("ZEROFILL".to_owned());
        }
        if mysql::has_binary_flag(self.flag) && self.get_type() != mysql::TypeString {
            strs.push("BINARY".to_owned());
        }

        if is_type_char(self.get_type()) || is_type_blob(self.get_type()) {
            if !self.charset.is_empty() && self.charset != crate::charset::CHARSET_BIN {
                strs.push(format!("CHARACTER SET {}", self.charset));
            }
            if !self.collate.is_empty() && self.collate != crate::charset::CHARSET_BIN {
                strs.push(format!("COLLATE {}", self.collate));
            }
        }

        strs.join(" ")
    }

    /// Restore implements Node interface (`format.DefaultRestoreFlags`). The output is bytes:
    /// enum / set elements are written as they are.
    // go: FieldType.Restore L1
    pub fn restore(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        write_keyword(out, &type_to_str(self.get_type(), &self.charset));

        let mut precision = UnspecifiedLength;
        let mut scale = UnspecifiedLength;

        match self.get_type() {
            mysql::TypeEnum | mysql::TypeSet => {
                out.push(b'(');
                for (i, e) in self.elems.iter().flatten().enumerate() {
                    if i != 0 {
                        out.push(b',');
                    }
                    write_string(out, e);
                }
                out.push(b')');
            }
            mysql::TypeTimestamp | mysql::TypeDatetime | mysql::TypeDuration => {
                precision = self.decimal;
            }
            mysql::TypeUnspecified
            | mysql::TypeFloat
            | mysql::TypeDouble
            | mysql::TypeNewDecimal => {
                precision = self.flen;
                scale = self.decimal;
            }
            _ => {
                precision = self.flen;
            }
        }

        if precision != UnspecifiedLength {
            out.extend_from_slice(format!("({precision}").as_bytes());
            if scale != UnspecifiedLength {
                out.extend_from_slice(format!(",{scale}").as_bytes());
            }
            out.push(b')');
        }

        if mysql::has_unsigned_flag(self.flag) {
            write_keyword(out, " UNSIGNED");
        }
        if mysql::has_zerofill_flag(self.flag) {
            write_keyword(out, " ZEROFILL");
        }
        if mysql::has_binary_flag(self.flag) && self.charset != crate::charset::CHARSET_BIN {
            write_keyword(out, " BINARY");
        }

        if is_type_char(self.get_type()) || is_type_blob(self.get_type()) {
            if !self.charset.is_empty() && self.charset != crate::charset::CHARSET_BIN {
                write_keyword(out, &format!(" CHARACTER SET {}", self.charset));
            }
            if !self.collate.is_empty() && self.collate != crate::charset::CHARSET_BIN {
                write_keyword(out, " COLLATE ");
                out.extend_from_slice(self.collate.as_bytes());
            }
        }

        Ok(())
    }

    /// RestoreAsCastType is used for write AST back to string.
    // go: FieldType.RestoreAsCastType L1
    pub fn restore_as_cast_type(&self, out: &mut Vec<u8>, explicit_charset: bool) {
        use crate::charset::{CHARSET_BIN, COLLATION_BIN};
        match self.tp {
            mysql::TypeVarString | mysql::TypeString => {
                let mut skip_write_binary = false;
                if self.charset == CHARSET_BIN && self.collate == COLLATION_BIN {
                    write_keyword(out, "BINARY");
                    skip_write_binary = true;
                } else {
                    write_keyword(out, "CHAR");
                }
                if self.flen != UnspecifiedLength {
                    out.extend_from_slice(format!("({})", self.flen).as_bytes());
                }
                if explicit_charset {
                    if !skip_write_binary && self.flag & mysql::BinaryFlag != 0 {
                        write_keyword(out, " BINARY");
                    }
                    if self.charset != CHARSET_BIN && self.charset != mysql::DefaultCharset {
                        write_keyword(out, " CHARSET ");
                        write_keyword(out, &self.charset);
                    }
                }
            }
            mysql::TypeDate => write_keyword(out, "DATE"),
            mysql::TypeDatetime => {
                write_keyword(out, "DATETIME");
                if self.decimal > 0 {
                    out.extend_from_slice(format!("({})", self.decimal).as_bytes());
                }
            }
            mysql::TypeNewDecimal => {
                write_keyword(out, "DECIMAL");
                if self.flen > 0 && self.decimal > 0 {
                    out.extend_from_slice(format!("({}, {})", self.flen, self.decimal).as_bytes());
                } else if self.flen > 0 {
                    out.extend_from_slice(format!("({})", self.flen).as_bytes());
                }
            }
            mysql::TypeDuration => {
                write_keyword(out, "TIME");
                if self.decimal > 0 {
                    out.extend_from_slice(format!("({})", self.decimal).as_bytes());
                }
            }
            mysql::TypeLonglong => {
                if self.flag & mysql::UnsignedFlag != 0 {
                    write_keyword(out, "UNSIGNED");
                } else {
                    write_keyword(out, "SIGNED");
                }
            }
            mysql::TypeJSON => write_keyword(out, "JSON"),
            mysql::TypeDouble => write_keyword(out, "DOUBLE"),
            mysql::TypeFloat => write_keyword(out, "FLOAT"),
            mysql::TypeYear => write_keyword(out, "YEAR"),
            mysql::TypeTiDBVectorFloat32 => write_keyword(out, "VECTOR"),
            _ => {}
        }
        if self.array {
            out.push(b' ');
            write_keyword(out, "ARRAY");
        }
    }

    /// FormatAsCastType is used for write AST back to string.
    // go: FieldType.FormatAsCastType L1
    pub fn format_as_cast_type(&self, w: &mut Vec<u8>, explicit_charset: bool) {
        self.restore_as_cast_type(w, explicit_charset);
    }

    /// StorageLength is the length of stored value for the type. Panics on a decimal with
    /// `flen < decimal` (negative index), as Go does.
    // go: FieldType.StorageLength L1 [M-INT]
    pub fn storage_length(&self) -> isize {
        match self.get_type() {
            mysql::TypeTiny
            | mysql::TypeShort
            | mysql::TypeInt24
            | mysql::TypeLong
            | mysql::TypeLonglong
            | mysql::TypeDouble
            | mysql::TypeFloat
            | mysql::TypeYear
            | mysql::TypeDuration
            | mysql::TypeDate
            | mysql::TypeDatetime
            | mysql::TypeTimestamp
            | mysql::TypeEnum
            | mysql::TypeSet
            | mysql::TypeBit => {
                // This may not be the accurate length, because we may encode them as varint.
                8
            }
            mysql::TypeNewDecimal => {
                let (precision, frac) = (self.flen.wrapping_sub(self.decimal), self.decimal);
                let idx = |n: isize| -> usize {
                    usize::try_from(n % digitsPerWord)
                        .unwrap_or_else(|_| panic!("index out of range [{}]", n % digitsPerWord))
                };
                precision / digitsPerWord * wordSize
                    + dig2bytes[idx(precision)]
                    + frac / digitsPerWord * wordSize
                    + dig2bytes[idx(frac)]
            }
            _ => VarStorageLen,
        }
    }

    /// The JSON members of `jsonFieldType`, in declaration order.
    const JSON_NAMES: [&'static str; 9] = [
        "Tp",
        "Flag",
        "Flen",
        "Decimal",
        "Charset",
        "Collate",
        "Elems",
        "ElemsIsBinaryLit",
        "Array",
    ];

    /// UnmarshalJSON implements the json.Unmarshaler interface: `json.Unmarshal` into a
    /// `jsonFieldType`, copied into `ft` only if there was no error.
    // go: FieldType.UnmarshalJSON L1 [M-ERR]
    pub fn unmarshal_json(&mut self, data: &[u8]) -> Result<(), Error> {
        let v = json::unmarshal_raw(data).map_err(Error::plain)?;
        self.decode_json(&v)
            .map_err(|e| Error::plain(e.to_string()))
    }

    /// `UnmarshalJSON` on an already valid JSON value, as `encoding/json` calls it for a
    /// `FieldType` field: the error is Go's `*json.UnmarshalTypeError` (the caller applies
    /// its error context).
    // go-part: FieldType.UnmarshalJSON
    pub fn decode_json(&mut self, v: &json::Raw) -> Result<(), json::DecodeError> {
        let mut r = FieldType::default();
        let mut d = Decoder::new();
        let (mut tp, mut flag, mut flen, mut decimal) = (0u64, 0u64, 0i64, 0i64);
        d.object(
            v,
            "jsonFieldType",
            "types.jsonFieldType",
            &Self::JSON_NAMES,
            |d, i, v| match i {
                0 => d.uint(v, &mut tp, u64::from(u8::MAX), "uint8"),
                1 => d.uint(v, &mut flag, usize::MAX as u64, "uint"),
                2 => d.int(v, &mut flen, isize::MIN as i64, isize::MAX as i64, "int"),
                3 => d.int(v, &mut decimal, isize::MIN as i64, isize::MAX as i64, "int"),
                4 => d.string(v, &mut r.charset),
                5 => d.string(v, &mut r.collate),
                6 => d.slice(v, &mut r.elems, "[]string", |d, v, e: &mut Vec<u8>| {
                    let mut s = String::from_utf8(std::mem::take(e)).unwrap_or_default();
                    d.string(v, &mut s);
                    *e = s.into_bytes();
                }),
                7 => d.slice(v, &mut r.elems_is_binary_lit, "[]bool", |d, v, b| {
                    d.bool(v, b)
                }),
                _ => d.bool(v, &mut r.array),
            },
        );
        d.finish()?;
        r.tp = tp as u8;
        r.flag = flag as usize;
        r.flen = flen as isize;
        r.decimal = decimal as isize;
        *self = r;
        Ok(())
    }

    /// MarshalJSON marshals the FieldType to JSON (the `jsonFieldType` struct encoding; nil
    /// slices are `null`).
    // go: FieldType.MarshalJSON L1
    pub fn marshal_json(&self) -> String {
        let mut out = format!(
            "{{\"Tp\":{},\"Flag\":{},\"Flen\":{},\"Decimal\":{},\"Charset\":",
            self.tp, self.flag, self.flen, self.decimal
        );
        json::append_string(&mut out, &self.charset);
        out.push_str(",\"Collate\":");
        json::append_string(&mut out, &self.collate);
        out.push_str(",\"Elems\":");
        match &self.elems {
            None => out.push_str("null"),
            Some(es) => {
                out.push('[');
                for (i, e) in es.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    json::append_go_string(&mut out, e);
                }
                out.push(']');
            }
        }
        out.push_str(",\"ElemsIsBinaryLit\":");
        match &self.elems_is_binary_lit {
            None => out.push_str("null"),
            Some(bs) => {
                out.push('[');
                for (i, b) in bs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(if *b { "true" } else { "false" });
                }
                out.push(']');
            }
        }
        out.push_str(",\"Array\":");
        out.push_str(if self.array { "true" } else { "false" });
        out.push('}');
        out
    }

    /// MemoryUsage return the memory usage of FieldType. Go counts `cap()` of the slices;
    /// this counts the Rust capacity, which follows a different growth policy.
    // go: FieldType.MemoryUsage L2 (Vec capacity instead of Go slice cap)
    pub fn memory_usage(&self) -> i64 {
        let mut sum = emptyFieldTypeSize
            + (self.charset.len() + self.collate.len()) as i64
            + self.elems.as_ref().map_or(0, Vec::capacity) as i64 * 16
            + self.elems_is_binary_lit.as_ref().map_or(0, Vec::capacity) as i64;
        for s in self.elems.iter().flatten() {
            sum += s.len() as i64;
        }
        sum
    }
}

/// VarStorageLen indicates this column is a variable length column.
pub const VarStorageLen: isize = -1;

/// `int64(unsafe.Sizeof(FieldType{}))` on a 64-bit target.
const emptyFieldTypeSize: i64 = 120;

/// HasCharset indicates if a COLUMN has an associated charset. Returning false here prevents
/// some information statements(like `SHOW CREATE TABLE`) from attaching a CHARACTER SET
/// clause to the column.
// go: HasCharset L1
pub fn has_charset(ft: &FieldType) -> bool {
    match ft.get_type() {
        mysql::TypeVarchar
        | mysql::TypeString
        | mysql::TypeVarString
        | mysql::TypeBlob
        | mysql::TypeTinyBlob
        | mysql::TypeMediumBlob
        | mysql::TypeLongBlob => !mysql::has_binary_flag(ft.flag),
        mysql::TypeEnum | mysql::TypeSet => true,
        _ => false,
    }
}

impl Clone for FieldType {
    /// Clone returns a copy of itself. Go copies the struct and shares the element slices;
    /// here they are copied (no aliasing between the two copies).
    // go: FieldType.Clone L2 (slices copied instead of shared)
    fn clone(&self) -> FieldType {
        FieldType {
            tp: self.tp,
            flag: self.flag,
            flen: self.flen,
            decimal: self.decimal,
            charset: self.charset.clone(),
            collate: self.collate.clone(),
            elems: self.elems.clone(),
            elems_is_binary_lit: self.elems_is_binary_lit.clone(),
            array: self.array,
        }
    }
}
