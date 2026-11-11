//! go-file: pkg/meta/metadef/db.go
//!
//! The names of the memory and system databases.

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::mysql;
use tidb_parser::model::CIStr;

/// InformationSchemaName is the `INFORMATION_SCHEMA` database name.
pub static InformationSchemaName: LazyLock<CIStr> =
    LazyLock::new(|| CIStr::new("INFORMATION_SCHEMA"));
/// PerformanceSchemaName is the `PERFORMANCE_SCHEMA` database name.
pub static PerformanceSchemaName: LazyLock<CIStr> =
    LazyLock::new(|| CIStr::new("PERFORMANCE_SCHEMA"));
/// MetricSchemaName is the `METRICS_SCHEMA` database name.
pub static MetricSchemaName: LazyLock<CIStr> = LazyLock::new(|| CIStr::new("METRICS_SCHEMA"));
/// ClusterTableInstanceColumnName is the `INSTANCE` column name of the cluster tables.
pub const ClusterTableInstanceColumnName: &str = "INSTANCE";

/// temporaryDBNamePrefix is the prefix name of system db, e.g. mysql system db will be
/// rename to `__TiDB_BR_Temporary_mysql`
const temporaryDBNamePrefix: &str = "__TiDB_BR_Temporary_";

/// IsMemOrSysDB uses to check whether dbLowerName is memory database or system database.
// go: IsMemOrSysDB L1
pub fn is_mem_or_sys_db(db_lower_name: &str) -> bool {
    is_mem_db(db_lower_name) || is_system_related_db(db_lower_name)
}

/// IsMemDB checks whether dbLowerName is memory database.
// go: IsMemDB L1
pub fn is_mem_db(db_lower_name: &str) -> bool {
    db_lower_name == InformationSchemaName.l
        || db_lower_name == PerformanceSchemaName.l
        || db_lower_name == MetricSchemaName.l
}

/// IsSystemRelatedDB checks whether dbLowerName is a system related database.
// go: IsSystemRelatedDB L1
pub fn is_system_related_db(db_lower_name: &str) -> bool {
    is_system_db(db_lower_name) || db_lower_name == mysql::SysDB
}

/// IsSystemDB checks whether dbLowerName is the system database (`mysql`).
// go: IsSystemDB L1
pub fn is_system_db(db_lower_name: &str) -> bool {
    db_lower_name == mysql::SystemDB
}

/// IsBRRelatedDB checks whether dbOriginName is a temporary database during BR restore.
// go: IsBRRelatedDB L1
pub fn is_br_related_db(db_origin_name: &str) -> bool {
    db_origin_name.starts_with(temporaryDBNamePrefix)
}
