//! go-file: pkg/types/field_type_builder.go
//!
//! Mapping: Go's builder methods return `*FieldTypeBuilder` for chaining; here they take and
//! return the builder by value (`Build` and `BuildP` both return the FieldType).

use crate::etc::FieldType;

/// FieldTypeBuilder constructor
#[derive(Debug, Default)]
pub struct FieldTypeBuilder {
    ft: FieldType,
}

/// NewFieldTypeBuilder will allocate the builder on the heap.
// go: NewFieldTypeBuilder L1
pub fn new_field_type_builder() -> FieldTypeBuilder {
    FieldTypeBuilder::default()
}

impl FieldTypeBuilder {
    /// GetType returns type of the ft
    // go: FieldTypeBuilder.GetType L1
    pub fn get_type(&self) -> u8 {
        self.ft.get_type()
    }

    /// GetFlag returns flag of the ft
    // go: FieldTypeBuilder.GetFlag L1
    pub fn get_flag(&self) -> usize {
        self.ft.get_flag()
    }

    /// GetFlen returns length of the ft
    // go: FieldTypeBuilder.GetFlen L1
    pub fn get_flen(&self) -> isize {
        self.ft.get_flen()
    }

    /// GetDecimal returns decimals of the ft
    // go: FieldTypeBuilder.GetDecimal L1
    pub fn get_decimal(&self) -> isize {
        self.ft.get_decimal()
    }

    /// GetCharset returns charset of the ft
    // go: FieldTypeBuilder.GetCharset L1
    pub fn get_charset(&self) -> &str {
        self.ft.get_charset()
    }

    /// GetCollate returns collation of the ft
    // go: FieldTypeBuilder.GetCollate L1
    pub fn get_collate(&self) -> &str {
        self.ft.get_collate()
    }

    /// SetType sets type of the ft
    // go: FieldTypeBuilder.SetType L1
    pub fn set_type(mut self, tp: u8) -> FieldTypeBuilder {
        self.ft.set_type(tp);
        self
    }

    /// SetFlag sets flag of the ft
    // go: FieldTypeBuilder.SetFlag L1
    pub fn set_flag(mut self, flag: usize) -> FieldTypeBuilder {
        self.ft.set_flag(flag);
        self
    }

    /// AddFlag adds flag to the ft
    // go: FieldTypeBuilder.AddFlag L1
    pub fn add_flag(mut self, flag: usize) -> FieldTypeBuilder {
        self.ft.add_flag(flag);
        self
    }

    /// ToggleFlag toggles flag of the ft
    // go: FieldTypeBuilder.ToggleFlag L1
    pub fn toggle_flag(mut self, flag: usize) -> FieldTypeBuilder {
        self.ft.toggle_flag(flag);
        self
    }

    /// DelFlag deletes flag of the ft
    // go: FieldTypeBuilder.DelFlag L1
    pub fn del_flag(mut self, flag: usize) -> FieldTypeBuilder {
        self.ft.del_flag(flag);
        self
    }

    /// SetFlen sets length of the ft
    // go: FieldTypeBuilder.SetFlen L1
    pub fn set_flen(mut self, flen: isize) -> FieldTypeBuilder {
        self.ft.set_flen(flen);
        self
    }

    /// SetDecimal sets decimal of the ft
    // go: FieldTypeBuilder.SetDecimal L1
    pub fn set_decimal(mut self, decimal: isize) -> FieldTypeBuilder {
        self.ft.set_decimal(decimal);
        self
    }

    /// SetCharset sets charset of the ft
    // go: FieldTypeBuilder.SetCharset L1
    pub fn set_charset(mut self, charset: &str) -> FieldTypeBuilder {
        self.ft.set_charset(charset);
        self
    }

    /// SetCollate sets collation of the ft
    // go: FieldTypeBuilder.SetCollate L1
    pub fn set_collate(mut self, collate: &str) -> FieldTypeBuilder {
        self.ft.set_collate(collate);
        self
    }

    /// SetElems sets elements of the ft
    // go: FieldTypeBuilder.SetElems L1
    pub fn set_elems(mut self, elems: Option<Vec<Vec<u8>>>) -> FieldTypeBuilder {
        self.ft.set_elems(elems);
        self
    }

    /// SetArray sets array of the ft
    // go: FieldTypeBuilder.SetArray L1
    pub fn set_array(mut self, x: bool) -> FieldTypeBuilder {
        self.ft.set_array(x);
        self
    }

    /// Build returns the ft
    // go: FieldTypeBuilder.Build L1
    pub fn build(self) -> FieldType {
        self.ft
    }

    /// BuildP returns pointer of the ft
    // go: FieldTypeBuilder.BuildP L1
    pub fn build_p(self) -> FieldType {
        self.ft
    }
}
