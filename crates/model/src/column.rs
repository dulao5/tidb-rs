//! go-file: pkg/meta/model/column.go

use std::sync::Arc;
use tidb_base::gofmt::Arg;
use tidb_base::{Error, mysql};

use crate::any::AnyValue;
use crate::{
    ColumnInfo, ExtraHandleID, ExtraHandleName, ExtraPhysTblID, ExtraPhysTblIDName, TableInfo,
    changingColumnPrefix, removingObjPrefix,
};

/// The elements of a possibly nil Go slice.
// glue: range over a possibly nil Go slice
pub(crate) fn sl<T>(s: &Option<Vec<T>>) -> &[T] {
    s.as_deref().unwrap_or(&[])
}

/// A default value as `GetOriginDefaultValue` / `GetDefaultValue` return it: the stored
/// value, or for a BIT column the raw bytes of `*DefaultValueBit` (`string(...)`, a Go string
/// that may not be UTF-8).
#[derive(Clone, Debug, PartialEq)]
pub enum DefaultValueRef<'a> {
    /// The `interface{}` value.
    Value(&'a AnyValue),
    /// `string(c.DefaultValueBit)`.
    Bytes(&'a [u8]),
}

/// GenUniqueChangingColumnName generates a unique changing column name.
// go: GenUniqueChangingColumnName L1
pub fn gen_unique_changing_column_name(tbl_info: &TableInfo, old_col: &ColumnInfo) -> String {
    // Check whether the new column name is used.
    let column_name_map: std::collections::HashSet<&str> = sl(&tbl_info.columns)
        .iter()
        .map(|c| c.name.l.as_str())
        .collect();
    let mut suffix = 0;
    let mut new_column_name = format!("{changingColumnPrefix}{}_{suffix}", old_col.name.o);
    while column_name_map.contains(tidb_base::gostd::to_lower(&new_column_name).as_str()) {
        suffix += 1;
        new_column_name = format!("{changingColumnPrefix}{}_{suffix}", old_col.name.o);
    }
    new_column_name
}

/// GenRemovingObjName generates a removing object name.
// go: GenRemovingObjName L1
pub fn gen_removing_obj_name(name: &str) -> String {
    if name.starts_with(removingObjPrefix) {
        return name.to_owned();
    }
    format!("{removingObjPrefix}{name}")
}

impl Clone for ColumnInfo {
    /// Clone clones ColumnInfo (a struct copy).
    // go: ColumnInfo.Clone L1
    fn clone(&self) -> ColumnInfo {
        self.struct_copy()
    }
}

impl ColumnInfo {
    /// GetType returns the type of ColumnInfo.
    // go: ColumnInfo.GetType L1
    pub fn get_type(&self) -> u8 {
        self.field_type.get_type()
    }

    /// GetFlag returns the flag of ColumnInfo.
    // go: ColumnInfo.GetFlag L1
    pub fn get_flag(&self) -> usize {
        self.field_type.get_flag()
    }

    /// GetFlen returns the flen of ColumnInfo.
    // go: ColumnInfo.GetFlen L1
    pub fn get_flen(&self) -> isize {
        self.field_type.get_flen()
    }

    /// GetDecimal returns the decimal of ColumnInfo.
    // go: ColumnInfo.GetDecimal L1
    pub fn get_decimal(&self) -> isize {
        self.field_type.get_decimal()
    }

    /// GetCharset returns the charset of ColumnInfo.
    // go: ColumnInfo.GetCharset L1
    pub fn get_charset(&self) -> &str {
        self.field_type.get_charset()
    }

    /// GetCollate returns the collation of ColumnInfo.
    // go: ColumnInfo.GetCollate L1
    pub fn get_collate(&self) -> &str {
        self.field_type.get_collate()
    }

    /// GetElems returns the elems of ColumnInfo.
    // go: ColumnInfo.GetElems L1
    pub fn get_elems(&self) -> Option<&[Vec<u8>]> {
        self.field_type.get_elems()
    }

    /// SetType set the type of ColumnInfo.
    // go: ColumnInfo.SetType L1
    pub fn set_type(&mut self, tp: u8) {
        self.field_type.set_type(tp);
    }

    /// SetFlag set the flag of ColumnInfo.
    // go: ColumnInfo.SetFlag L1
    pub fn set_flag(&mut self, flag: usize) {
        self.field_type.set_flag(flag);
    }

    /// AddFlag adds the flag of ColumnInfo.
    // go: ColumnInfo.AddFlag L1
    pub fn add_flag(&mut self, flag: usize) {
        self.field_type.add_flag(flag);
    }

    /// AndFlag adds a flag to the column.
    // go: ColumnInfo.AndFlag L1
    pub fn and_flag(&mut self, flag: usize) {
        self.field_type.and_flag(flag);
    }

    /// ToggleFlag flips the flag according to the value.
    // go: ColumnInfo.ToggleFlag L1
    pub fn toggle_flag(&mut self, flag: usize) {
        self.field_type.toggle_flag(flag);
    }

    /// DelFlag removes the flag from the column's flag.
    // go: ColumnInfo.DelFlag L1
    pub fn del_flag(&mut self, flag: usize) {
        self.field_type.del_flag(flag);
    }

    /// SetFlen sets the flen of ColumnInfo.
    // go: ColumnInfo.SetFlen L1
    pub fn set_flen(&mut self, flen: isize) {
        self.field_type.set_flen(flen);
    }

    /// SetDecimal sets the decimal of ColumnInfo.
    // go: ColumnInfo.SetDecimal L1
    pub fn set_decimal(&mut self, decimal: isize) {
        self.field_type.set_decimal(decimal);
    }

    /// SetCharset sets charset of the ColumnInfo.
    // go: ColumnInfo.SetCharset L1
    pub fn set_charset(&mut self, charset: &str) {
        self.field_type.set_charset(charset);
    }

    /// SetCollate sets the collation of the column.
    // go: ColumnInfo.SetCollate L1
    pub fn set_collate(&mut self, collate: &str) {
        self.field_type.set_collate(collate);
    }

    /// SetElems set the elements of enum column.
    // go: ColumnInfo.SetElems L1
    pub fn set_elems(&mut self, elems: Option<Vec<Vec<u8>>>) {
        self.field_type.set_elems(elems);
    }

    /// IsGenerated returns true if the column is generated column.
    // go: ColumnInfo.IsGenerated L1
    pub fn is_generated(&self) -> bool {
        !self.generated_expr_string.is_empty()
    }

    /// IsVirtualGenerated checks the column if it is virtual.
    // go: ColumnInfo.IsVirtualGenerated L1
    pub fn is_virtual_generated(&self) -> bool {
        self.is_generated() && !self.generated_stored
    }

    /// IsChanging checks the column if it is changing.
    // go: ColumnInfo.IsChanging L1
    pub fn is_changing(&self) -> bool {
        self.name.o.starts_with(changingColumnPrefix)
    }

    /// IsRemoving checks the column if it is removing.
    // go: ColumnInfo.IsRemoving L1
    pub fn is_removing(&self) -> bool {
        self.name.o.starts_with(removingObjPrefix)
    }

    /// GetRemovingOriginName returns the name of the column without the removing prefix.
    // go: ColumnInfo.GetRemovingOriginName L1
    pub fn get_removing_origin_name(&self) -> &str {
        self.name
            .o
            .strip_prefix(removingObjPrefix)
            .unwrap_or(&self.name.o)
    }

    /// GetChangingOriginName returns the name of the column without the changing prefix and
    /// the suffix number.
    // go: ColumnInfo.GetChangingOriginName L1
    pub fn get_changing_origin_name(&self) -> &str {
        let column_name = self
            .name
            .o
            .strip_prefix(changingColumnPrefix)
            .unwrap_or(&self.name.o);
        match column_name.rfind('_') {
            None => column_name,
            Some(pos) => &column_name[..pos],
        }
    }

    /// SetOriginDefaultValue sets the origin default value. For mysql.TypeBit type, the
    /// default value storage format must be a string; the bytes are kept in
    /// `OriginDefaultValueBit`.
    // go: ColumnInfo.SetOriginDefaultValue L1 [M-ERR]
    pub fn set_origin_default_value(&mut self, value: AnyValue) -> Result<(), Error> {
        self.origin_default_value = value;
        if self.get_type() == mysql::TypeBit {
            if self.origin_default_value.is_nil() {
                return Ok(());
            }
            if let Some(v) = self.origin_default_value.as_go_string() {
                self.origin_default_value_bit = Some(v.into());
                return Ok(());
            }
            return Err(
                tidb_parser::types::ErrInvalidDefault.gen_with_stack_by_args(vec![Arg::Stringer(
                    self.name.o.clone().into_bytes(),
                    "model.CIStr",
                )]),
            );
        }
        Ok(())
    }

    /// GetOriginDefaultValue gets the origin default value. For a BIT column with
    /// `OriginDefaultValueBit`, both default values were corrupted by the JSON round trip, so
    /// the bit bytes are returned.
    // go: ColumnInfo.GetOriginDefaultValue L1
    pub fn get_origin_default_value(&self) -> DefaultValueRef<'_> {
        if self.get_type() == mysql::TypeBit
            && let Some(b) = &self.origin_default_value_bit
        {
            return DefaultValueRef::Bytes(b);
        }
        DefaultValueRef::Value(&self.origin_default_value)
    }

    /// SetDefaultValue sets the default value.
    // go: ColumnInfo.SetDefaultValue L1 [M-ERR]
    pub fn set_default_value(&mut self, value: AnyValue) -> Result<(), Error> {
        self.default_value = value;
        if self.get_type() == mysql::TypeBit {
            if self.default_value.is_nil() {
                return Ok(());
            }
            if let Some(v) = self.default_value.as_go_string() {
                self.default_value_bit = Some(v.into());
                return Ok(());
            }
            return Err(
                tidb_parser::types::ErrInvalidDefault.gen_with_stack_by_args(vec![Arg::Stringer(
                    self.name.o.clone().into_bytes(),
                    "model.CIStr",
                )]),
            );
        }
        Ok(())
    }

    /// GetDefaultValue gets the default value of the column. Default value use to stored in
    /// DefaultValue field, but now, bit type default value will store in DefaultValueBit for
    /// fix bit default value decode/encode bug.
    // go: ColumnInfo.GetDefaultValue L1
    pub fn get_default_value(&self) -> DefaultValueRef<'_> {
        if self.get_type() == mysql::TypeBit
            && let Some(b) = &self.default_value_bit
        {
            return DefaultValueRef::Bytes(b);
        }
        DefaultValueRef::Value(&self.default_value)
    }

    /// GetTypeDesc gets the description for column type.
    // go: ColumnInfo.GetTypeDesc L1
    pub fn get_type_desc(&self) -> String {
        let mut desc = self.field_type.compact_str();
        if mysql::has_unsigned_flag(self.get_flag())
            && self.get_type() != mysql::TypeBit
            && self.get_type() != mysql::TypeYear
        {
            desc.push_str(" unsigned");
        }
        if mysql::has_zerofill_flag(self.get_flag()) && self.get_type() != mysql::TypeYear {
            desc.push_str(" zerofill");
        }
        desc
    }
}

/// FindColumnInfo finds ColumnInfo in cols by name.
// go: FindColumnInfo L1
pub fn find_column_info<'a>(
    cols: &'a [Arc<ColumnInfo>],
    name: &str,
) -> Option<&'a Arc<ColumnInfo>> {
    let name = tidb_base::gostd::to_lower(name);
    cols.iter().find(|col| col.name.l == name)
}

/// FindColumnInfoByID finds ColumnInfo in cols by id.
// go: FindColumnInfoByID L1
pub fn find_column_info_by_id(cols: &[Arc<ColumnInfo>], id: i64) -> Option<&Arc<ColumnInfo>> {
    cols.iter().find(|col| col.id == id)
}

/// NewExtraHandleColInfo mocks a column info for extra handle column.
// go: NewExtraHandleColInfo L1
pub fn new_extra_handle_col_info() -> ColumnInfo {
    let mut col_info = ColumnInfo {
        id: ExtraHandleID as i64,
        name: ExtraHandleName.clone(),
        ..ColumnInfo::default()
    };
    col_info.set_flag(mysql::PriKeyFlag | mysql::NotNullFlag);
    col_info.set_type(mysql::TypeLonglong);
    let (flen, decimal) = mysql::get_default_field_length_and_decimal(mysql::TypeLonglong);
    col_info.set_flen(flen);
    col_info.set_decimal(decimal);
    col_info.set_charset("binary");
    col_info.set_collate("binary");
    col_info
}

/// NewExtraPhysTblIDColInfo mocks a column info for extra partition id column.
// go: NewExtraPhysTblIDColInfo L1
pub fn new_extra_phys_tbl_id_col_info() -> ColumnInfo {
    let mut col_info = ColumnInfo {
        id: ExtraPhysTblID as i64,
        name: ExtraPhysTblIDName.clone(),
        ..ColumnInfo::default()
    };
    col_info.set_type(mysql::TypeLonglong);
    col_info.set_flag(mysql::NotNullFlag);
    let (flen, decimal) = mysql::get_default_field_length_and_decimal(mysql::TypeLonglong);
    col_info.set_flen(flen);
    col_info.set_decimal(decimal);
    col_info.set_charset("binary");
    col_info.set_collate("binary");
    col_info
}
