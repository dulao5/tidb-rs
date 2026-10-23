//! TiDB schema objects as stored in the meta (JSON written by Go's `encoding/json`).
//!
//! Only the fields the read-only node needs are modelled; unknown fields are ignored and
//! `null` is accepted wherever Go may write a nil slice/pointer. Field names follow the Go
//! JSON tags in `pkg/meta/model` and `pkg/parser/types/field_type.go`.

use serde::Deserialize;
use serde::Deserializer;

/// Go `nil` slices and maps marshal as `null`.
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A case-insensitive identifier: original and lower-cased forms (Go: `ast.CIStr`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct CIStr {
    #[serde(rename = "O", default)]
    pub o: String,
    #[serde(rename = "L", default)]
    pub l: String,
}

/// State of a schema element during online DDL (Go: `model.SchemaState`). Only `Public`
/// elements are visible to reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "u8")]
pub enum SchemaState {
    #[default]
    None,
    DeleteOnly,
    WriteOnly,
    WriteReorganization,
    DeleteReorganization,
    Public,
    ReplicaOnly,
    GlobalTxnOnly,
    Unknown(u8),
}

impl From<u8> for SchemaState {
    fn from(v: u8) -> SchemaState {
        match v {
            0 => SchemaState::None,
            1 => SchemaState::DeleteOnly,
            2 => SchemaState::WriteOnly,
            3 => SchemaState::WriteReorganization,
            4 => SchemaState::DeleteReorganization,
            5 => SchemaState::Public,
            6 => SchemaState::ReplicaOnly,
            7 => SchemaState::GlobalTxnOnly,
            v => SchemaState::Unknown(v),
        }
    }
}

/// MySQL column type codes (`mysql.Type*`; the same values as tipb / TiKV's `FieldTypeTp`).
pub mod tp {
    pub const DECIMAL: u8 = 0;
    pub const TINY: u8 = 1;
    pub const SHORT: u8 = 2;
    pub const LONG: u8 = 3;
    pub const FLOAT: u8 = 4;
    pub const DOUBLE: u8 = 5;
    pub const NULL: u8 = 6;
    pub const TIMESTAMP: u8 = 7;
    pub const LONGLONG: u8 = 8;
    pub const INT24: u8 = 9;
    pub const DATE: u8 = 10;
    pub const DURATION: u8 = 11;
    pub const DATETIME: u8 = 12;
    pub const YEAR: u8 = 13;
    pub const NEWDATE: u8 = 14;
    pub const VARCHAR: u8 = 15;
    pub const BIT: u8 = 16;
    pub const JSON: u8 = 245;
    pub const NEWDECIMAL: u8 = 246;
    pub const ENUM: u8 = 247;
    pub const SET: u8 = 248;
    pub const TINY_BLOB: u8 = 249;
    pub const MEDIUM_BLOB: u8 = 250;
    pub const LONG_BLOB: u8 = 251;
    pub const BLOB: u8 = 252;
    pub const VAR_STRING: u8 = 253;
    pub const STRING: u8 = 254;
    pub const GEOMETRY: u8 = 255;
}

/// Column flag bits (`mysql.*Flag`).
pub mod flag {
    pub const NOT_NULL: u32 = 1;
    pub const PRI_KEY: u32 = 1 << 1;
    pub const UNIQUE_KEY: u32 = 1 << 2;
    pub const MULTIPLE_KEY: u32 = 1 << 3;
    pub const BLOB: u32 = 1 << 4;
    pub const UNSIGNED: u32 = 1 << 5;
    pub const ZEROFILL: u32 = 1 << 6;
    pub const BINARY: u32 = 1 << 7;
    pub const ENUM: u32 = 1 << 8;
    pub const AUTO_INCREMENT: u32 = 1 << 9;
    pub const TIMESTAMP: u32 = 1 << 10;
    pub const SET: u32 = 1 << 11;
    pub const NO_DEFAULT_VALUE: u32 = 1 << 12;
    pub const ON_UPDATE_NOW: u32 = 1 << 13;
}

/// A column type (Go: `types.FieldType`, JSON with capitalised field names).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct FieldType {
    #[serde(rename = "Tp")]
    pub tp: u8,
    #[serde(rename = "Flag")]
    pub flag: u32,
    /// Go `int` (64-bit): LONGBLOB/LONGTEXT have 4294967295.
    #[serde(rename = "Flen")]
    pub flen: i64,
    #[serde(rename = "Decimal")]
    pub decimal: i64,
    #[serde(rename = "Charset")]
    pub charset: String,
    #[serde(rename = "Collate")]
    pub collate: String,
    #[serde(rename = "Elems", deserialize_with = "null_as_default")]
    pub elems: Vec<String>,
    #[serde(rename = "Array")]
    pub array: bool,
}

impl FieldType {
    pub fn has_flag(&self, f: u32) -> bool {
        self.flag & f != 0
    }

    pub fn is_unsigned(&self) -> bool {
        self.has_flag(flag::UNSIGNED)
    }

    pub fn is_not_null(&self) -> bool {
        self.has_flag(flag::NOT_NULL)
    }
}

/// A column (Go: `model.ColumnInfo`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ColumnInfo {
    pub id: i64,
    pub name: CIStr,
    /// Position in the table's public column list (`TableInfo.Cols()`).
    pub offset: usize,
    /// Value for rows written before the column was added (Go: `OriginDefaultValue`,
    /// usually a string). TiKV fills it in when the row has no value for the column.
    pub origin_default: serde_json::Value,
    /// Base64 of the origin default of a BIT column (Go: `OriginDefaultValueBit`).
    pub origin_default_bit: Option<String>,
    /// Current default for new rows (not needed for reads).
    #[serde(rename = "default")]
    pub default_value: serde_json::Value,
    pub default_is_expr: bool,
    pub generated_expr_string: String,
    pub generated_stored: bool,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    pub state: SchemaState,
    pub hidden: bool,
    pub version: u64,
}

impl ColumnInfo {
    pub fn is_public(&self) -> bool {
        self.state == SchemaState::Public
    }

    /// A virtual (not stored) generated column: TiKV has no data for it.
    pub fn is_virtual_generated(&self) -> bool {
        !self.generated_expr_string.is_empty() && !self.generated_stored
    }
}

/// One column of an index (Go: `model.IndexColumn`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct IndexColumn {
    pub name: CIStr,
    pub offset: usize,
    /// Prefix length, or -1 (`types.UnspecifiedLength`) for the whole column.
    pub length: i64,
}

/// An index (Go: `model.IndexInfo`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct IndexInfo {
    pub id: i64,
    #[serde(rename = "idx_name")]
    pub name: CIStr,
    #[serde(rename = "idx_cols", deserialize_with = "null_as_default")]
    pub columns: Vec<IndexColumn>,
    pub state: SchemaState,
    #[serde(rename = "is_unique")]
    pub unique: bool,
    #[serde(rename = "is_primary")]
    pub primary: bool,
    #[serde(rename = "is_invisible")]
    pub invisible: bool,
    #[serde(rename = "is_global")]
    pub global: bool,
    pub mv_index: bool,
}

/// A table, view or sequence (Go: `model.TableInfo`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct TableInfo {
    pub id: i64,
    pub name: CIStr,
    pub charset: String,
    pub collate: String,
    #[serde(rename = "cols", deserialize_with = "null_as_default")]
    pub columns: Vec<ColumnInfo>,
    #[serde(rename = "index_info", deserialize_with = "null_as_default")]
    pub indices: Vec<IndexInfo>,
    pub state: SchemaState,
    /// The single integer primary key column is the row handle (`t{id}_r{pk}`).
    pub pk_is_handle: bool,
    /// Clustered non-integer primary key: the handle is the encoded key columns.
    pub is_common_handle: bool,
    pub common_handle_version: u16,
    pub max_col_id: i64,
    pub max_idx_id: i64,
    pub update_timestamp: u64,
    /// Partition definition; only its presence matters here.
    pub partition: Option<serde_json::Value>,
    pub view: Option<serde_json::Value>,
    pub sequence: Option<serde_json::Value>,
    pub temp_table_type: u8,
    pub cache_table_status: u8,
    /// Table info version (`model.CurrLatestTableInfoVersion`), not the schema version.
    pub version: u16,
    pub revision: u64,
    /// Id of the database the table belongs to. Not part of the JSON (Go: `json:"-"`); set
    /// by the loader.
    #[serde(skip)]
    pub db_id: i64,
}

/// Why a table cannot be read by this node.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    #[error("table `{0}` is partitioned; partitioned tables are not supported")]
    Partitioned(String),
    #[error("`{0}` is a view")]
    View(String),
    #[error("`{0}` is a sequence")]
    Sequence(String),
    #[error("table `{0}` is a temporary table")]
    Temporary(String),
    #[error("table `{0}` has a clustered non-integer primary key (common handle)")]
    CommonHandle(String),
    #[error("table `{0}` is not public (state {1:?})")]
    NotPublic(String, SchemaState),
}

impl TableInfo {
    /// Public columns ordered by offset (Go: `TableInfo.Cols()`).
    pub fn public_columns(&self) -> Vec<&ColumnInfo> {
        let mut cols: Vec<&ColumnInfo> = self.columns.iter().filter(|c| c.is_public()).collect();
        cols.sort_by_key(|c| c.offset);
        cols
    }

    /// Public column by (case-insensitive) name.
    pub fn find_column(&self, name: &str) -> Option<&ColumnInfo> {
        let lower = name.to_lowercase();
        self.columns
            .iter()
            .find(|c| c.is_public() && c.name.l == lower)
    }

    /// The integer primary key column used as the handle, if `pk_is_handle`
    /// (Go: `GetPkColInfo`).
    pub fn pk_handle_column(&self) -> Option<&ColumnInfo> {
        if !self.pk_is_handle {
            return None;
        }
        self.columns
            .iter()
            .find(|c| c.field_type.has_flag(flag::PRI_KEY))
    }

    pub fn is_partitioned(&self) -> bool {
        self.partition.is_some()
    }

    pub fn is_view(&self) -> bool {
        self.view.is_some()
    }

    pub fn is_sequence(&self) -> bool {
        self.sequence.is_some()
    }

    /// Public index by (case-insensitive) name.
    pub fn find_index(&self, name: &str) -> Option<&IndexInfo> {
        let lower = name.to_lowercase();
        self.indices
            .iter()
            .find(|i| i.state == SchemaState::Public && i.name.l == lower)
    }

    /// Check that rows of this table can be read with an integer-handle table scan: a
    /// public, non-partitioned base table whose handle is an integer (`pk_is_handle`
    /// or the hidden `_tidb_rowid`).
    pub fn check_int_handle_readable(&self) -> Result<(), Unsupported> {
        let name = || self.name.o.clone();
        if self.is_view() {
            return Err(Unsupported::View(name()));
        }
        if self.is_sequence() {
            return Err(Unsupported::Sequence(name()));
        }
        if self.is_partitioned() {
            return Err(Unsupported::Partitioned(name()));
        }
        if self.temp_table_type != 0 {
            return Err(Unsupported::Temporary(name()));
        }
        if self.is_common_handle {
            return Err(Unsupported::CommonHandle(name()));
        }
        if self.state != SchemaState::Public {
            return Err(Unsupported::NotPublic(name(), self.state));
        }
        Ok(())
    }
}

/// A database (Go: `model.DBInfo`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct DbInfo {
    pub id: i64,
    #[serde(rename = "db_name")]
    pub name: CIStr,
    pub charset: String,
    pub collate: String,
    pub state: SchemaState,
}
