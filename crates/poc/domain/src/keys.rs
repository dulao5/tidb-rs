//! etcd keys of TiDB's schema sync (Go: `pkg/ddl/util/util.go`, `pkg/domain/infosync`).

/// `/tidb/server/info/<ddl_id>` → [`crate::ServerInfo`] JSON, lease-bound.
pub const SERVER_INFO_PREFIX: &str = "/tidb/server/info/";
/// `/tidb/server_id/<id>` → cluster-unique server ID lease key (Go: `serverIDEtcdPath`).
pub const SERVER_ID_PREFIX: &str = "/tidb/server_id/";
/// Put by the DDL owner after each schema version it commits (Go: `DDLGlobalSchemaVersion`).
pub const GLOBAL_SCHEMA_VERSION: &str = "/tidb/ddl/global_schema_version";
/// `/tidb/ddl/all_schema_versions/<ddl_id>` → loaded version, lease-bound (Go:
/// `DDLAllSchemaVersions`; waited on by the owner when MDL is off).
pub const ALL_SCHEMA_VERSIONS: &str = "/tidb/ddl/all_schema_versions";
/// `/tidb/ddl/all_schema_by_job_versions/<job_id>/<ddl_id>` → version, no lease (Go:
/// `DDLAllSchemaVersionsByJob`; waited on by the owner when MDL is on).
pub const ALL_SCHEMA_BY_JOB_VERSIONS: &str = "/tidb/ddl/all_schema_by_job_versions";
/// Put by TiDB on `CREATE/ALTER/DROP USER`, `GRANT`, `REVOKE`, `FLUSH PRIVILEGES`
/// (Go: `pkg/domain/domain.go` `privilegeKey`).
pub const PRIVILEGE_NOTIFY: &str = "/tidb/privilege";

pub fn self_schema_version(ddl_id: &str) -> String {
    format!("{ALL_SCHEMA_VERSIONS}/{ddl_id}")
}

pub fn job_schema_version(job_id: i64, ddl_id: &str) -> String {
    format!("{ALL_SCHEMA_BY_JOB_VERSIONS}/{job_id}/{ddl_id}")
}

pub fn server_id_key(server_id: u64) -> String {
    format!("{SERVER_ID_PREFIX}{server_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_go() {
        // As seen in a v8.5.8 playground's etcd.
        assert_eq!(
            self_schema_version("f989950c-7c23-42e3-84c0-42c797d4063c"),
            "/tidb/ddl/all_schema_versions/f989950c-7c23-42e3-84c0-42c797d4063c"
        );
        assert_eq!(
            job_schema_version(0, "f989950c-7c23-42e3-84c0-42c797d4063c"),
            "/tidb/ddl/all_schema_by_job_versions/0/f989950c-7c23-42e3-84c0-42c797d4063c"
        );
        assert_eq!(server_id_key(841), "/tidb/server_id/841");
    }
}
