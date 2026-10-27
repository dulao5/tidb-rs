//! The server-info record a TiDB node publishes at `/tidb/server/info/<ddl_id>` (Go:
//! `pkg/domain/infosync/info.go`, `ServerInfo`).
//!
//! Every Go TiDB reads the whole prefix and aborts if *any* value fails to unmarshal, and the
//! DDL owner keys its wait set by the JSON `ddl_id` (not the etcd key), de-duplicated by
//! `ip:listening_port` with the newest `start_timestamp` winning. So the JSON must have Go's
//! field names and types, `ddl_id` must equal the key suffix, and `ip:listening_port` must be
//! unique and real. See `docs/adr/0005-schema-sync.md`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

/// MySQL version prefix TiDB advertises (Go: `mysql.ServerVersion`).
pub const MYSQL_VERSION: &str = "8.0.11";
/// The Go TiDB release whose cluster protocol this node implements. Go's DDL uses job
/// version 2 only if every server reports a TiDB version >= 8.4.0 after `-TiDB-`.
pub const TIDB_RELEASE: &str = "v8.5.8";
/// Commit this binary was built from (`git rev-parse HEAD`, or `None`).
pub const GIT_HASH: &str = env!("TIDB_RS_GIT_HASH");

/// Go `ServerInfo` JSON. Field order follows Go's `encoding/json` output (embedded
/// `StaticServerInfo{ServerVersionInfo, ...}`, then `DynamicServerInfo`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub git_hash: String,
    pub ddl_id: String,
    pub ip: String,
    pub listening_port: u32,
    pub status_port: u32,
    /// Display only (Go: the schema lease, e.g. `"45s"`).
    pub lease: String,
    /// Unix seconds.
    pub start_timestamp: i64,
    /// 0: no `/tidb/server_id` registration, so global KILL never routes here.
    pub server_id: u64,
    pub labels: BTreeMap<String, String>,
    /// Not in v8.5 (ignored there); newer TiDB skips nodes with it for TiDB-to-TiDB RPC.
    #[serde(default)]
    pub disable_rpc: bool,
}

/// `8.0.11-TiDB-v8.5.8-tidb-rs-<short hash>`: Go parses the part after `-TiDB-` as a
/// semantic version (pre-release ignored) and `FormatTiDBVersion` splits it at the first `-`.
pub fn version_string() -> String {
    let short = GIT_HASH.get(..7).unwrap_or(GIT_HASH);
    format!("{MYSQL_VERSION}-TiDB-{TIDB_RELEASE}-tidb-rs-{short}")
}

/// A fresh DDL id: a random UUIDv4, as Go's `ownerManager` uses.
pub fn new_ddl_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

impl ServerInfo {
    /// The record of this node. `status_port` is advertised but nothing listens there (see the
    /// ADR): Go's cluster-table fan-out then fails fast with a warning.
    pub fn new(ip: String, listening_port: u32, status_port: u32) -> ServerInfo {
        let start_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        ServerInfo {
            version: version_string(),
            git_hash: GIT_HASH.to_owned(),
            ddl_id: new_ddl_id(),
            ip,
            listening_port,
            status_port,
            lease: "45s".to_owned(),
            start_timestamp,
            server_id: 0,
            labels: BTreeMap::new(),
            disable_rpc: true,
        }
    }

    /// The etcd key (`/tidb/server/info/<ddl_id>`).
    pub fn key(&self) -> String {
        format!("{}{}", crate::keys::SERVER_INFO_PREFIX, self.ddl_id)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ServerInfo serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Value written by Go TiDB v8.5.8 (tiup playground), read back from PD's etcd.
    const GO_JSON: &str = r#"{"version":"8.0.11-TiDB-v8.5.8","git_hash":"8b857efa20363d50a8fa2ea7dd9809a85a61b115","ddl_id":"f989950c-7c23-42e3-84c0-42c797d4063c","ip":"127.0.0.1","listening_port":29000,"status_port":35080,"lease":"45s","start_timestamp":1793090405,"server_id":841,"labels":{}}"#;

    /// Go's struct as a JSON schema: every field name and JSON type of `ServerInfo`
    /// (`string`, `uint`, `int64`, `uint64`, `map[string]string`).
    fn go_shape(v: &serde_json::Value) -> Vec<(String, &'static str)> {
        let obj = v.as_object().expect("object");
        obj.iter()
            .map(|(k, v)| {
                let t = match v {
                    serde_json::Value::String(_) => "string",
                    serde_json::Value::Number(n) if n.is_u64() => "unsigned",
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::Object(_) => "object",
                    serde_json::Value::Bool(_) => "bool",
                    _ => "other",
                };
                (k.clone(), t)
            })
            .collect()
    }

    #[test]
    fn go_json_round_trips() {
        let info: ServerInfo = serde_json::from_str(GO_JSON).unwrap();
        assert_eq!(info.ddl_id, "f989950c-7c23-42e3-84c0-42c797d4063c");
        assert_eq!(info.listening_port, 29000);
        assert_eq!(info.server_id, 841);
        assert!(!info.disable_rpc);
        // Re-encoding gives Go's bytes plus the trailing `disable_rpc` field.
        let mut ours: serde_json::Value = serde_json::from_str(&info.to_json()).unwrap();
        assert_eq!(
            ours.as_object_mut().unwrap().remove("disable_rpc"),
            Some(false.into())
        );
        let go: serde_json::Value = serde_json::from_str(GO_JSON).unwrap();
        assert_eq!(ours, go);
        let mut encoded = info.to_json();
        encoded = encoded.replace(r#","disable_rpc":false"#, "");
        assert_eq!(encoded, GO_JSON, "field order and formatting match Go");
    }

    #[test]
    fn our_record_has_go_field_names_and_types() {
        let info = ServerInfo::new("10.0.0.5".to_owned(), 4001, 0);
        let ours: serde_json::Value = serde_json::from_str(&info.to_json()).unwrap();
        let go: serde_json::Value = serde_json::from_str(GO_JSON).unwrap();
        let mut expected = go_shape(&go);
        expected.push(("disable_rpc".to_owned(), "bool"));
        let mut got = go_shape(&ours);
        expected.sort();
        got.sort();
        assert_eq!(got, expected);
        assert_eq!(ours["server_id"], 0);
        assert_eq!(ours["disable_rpc"], true);
        assert_eq!(ours["labels"], serde_json::json!({}));
        assert!(info.key().ends_with(&info.ddl_id));
        assert!(info.key().starts_with("/tidb/server/info/"));
        // A UUIDv4 like Go's.
        assert_eq!(
            uuid::Uuid::parse_str(&info.ddl_id)
                .unwrap()
                .get_version_num(),
            4
        );
    }

    /// The version passes Go's checks: `-TiDB-` present, semver >= 8.4.0 after it (pre-release
    /// cleared), and `FormatTiDBVersion` gives `<semver>-<rest>`.
    #[test]
    fn version_string_satisfies_go_parsers() {
        let v = version_string();
        let rest = &v[v.find("-TiDB-").unwrap() + "-TiDB-".len()..];
        let rest = rest.strip_prefix('v').unwrap();
        let (core, pre) = rest.split_once('-').unwrap();
        let nums: Vec<u32> = core.split('.').map(|p| p.parse().unwrap()).collect();
        assert!(nums >= vec![8, 4, 0], "{v}");
        assert!(pre.starts_with("tidb-rs-"), "{v}");
        // FormatTiDBVersion: everything after "TiDB-", minus the leading 'v'.
        let formatted = &v[v.find("TiDB-").unwrap() + 5..][1..];
        assert!(formatted.starts_with("8.5.8-tidb-rs-"), "{formatted}");
    }
}
