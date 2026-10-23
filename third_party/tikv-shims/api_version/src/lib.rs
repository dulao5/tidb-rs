//! Shim of TiKV `api_version`: only what tidb_query_datatype uses.
pub type KeyspaceId = u32;

#[derive(Debug)]
pub struct ParseKeyspaceError;
impl std::fmt::Display for ParseKeyspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("parse keyspace error") }
}
impl std::error::Error for ParseKeyspaceError {}

pub trait KvFormat: Clone + Copy + 'static + Send + Sync {
    /// Strip the (optional) keyspace prefix from a key.
    fn parse_keyspace(key: &[u8]) -> Result<(Option<KeyspaceId>, &[u8]), ParseKeyspaceError>;
}

/// API V1: no keyspace prefix.
#[derive(Clone, Copy, Debug)]
pub struct ApiV1;
impl KvFormat for ApiV1 {
    fn parse_keyspace(key: &[u8]) -> Result<(Option<KeyspaceId>, &[u8]), ParseKeyspaceError> {
        Ok((None, key))
    }
}
