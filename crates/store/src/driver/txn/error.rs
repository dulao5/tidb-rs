//! go-file: pkg/store/driver/txn/error.go
//!
//! Errors of the transaction driver: the key-error conversion of the read path
//! (`extractKeyErr`, write conflicts, retryable errors) and the duplicate-key errors of the
//! write path, which need `pkg/table/tables` (D06.4) and `kv.GenKeyExistsErr` (X).

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_base::gofmt::{Arg, sprintf};
use tidb_kv::Key;
use tidb_kv::error::{ErrKeyExists, ErrTxnRetryable, ErrWriteConflict};
use tidb_model::TableInfo;
use tidb_tablecodec::tablecodec;
use tikv_client::proto::kvrpcpb;

use crate::driver::error::{ClientGoError, TiKVError, client_go_error, to_tidb_err};

/// genKeyExistsError is the fallback path when can't extract key columns for
/// kv.GenKeyExistsErr.
// go: genKeyExistsError L1
pub fn gen_key_exists_error(name: &str, value: &[u8], err: Option<&Error>) -> Error {
    if let Some(err) = err {
        log::info!("extractKeyExistsErr meets error: {err}");
    }
    ErrKeyExists.fast_gen_by_args(vec![
        Arg::go_string(value.to_vec()),
        Arg::from(name.to_owned()),
    ])
}

/// ExtractKeyExistsErrFromHandle returns a ErrKeyExists error from a handle key.
///
/// The write path (a duplicate primary key on insert): needs `tables.FindPrimaryIndex` and
/// `kv.GenKeyExistsErr`.
// go: ExtractKeyExistsErrFromHandle L2 (boundary stub, blocked-by:D06.4)
pub fn extract_key_exists_err_from_handle(key: &Key, value: &[u8], tbl_info: &TableInfo) -> Error {
    let _ = (key, value, tbl_info);
    blocked("D06.4", "duplicate-key errors of the write path")
}

/// ExtractKeyExistsErrFromIndex returns a ErrKeyExists error from a index key.
///
/// The write path (a duplicate unique key on insert): needs
/// `tables.BuildRowcodecColInfoForIndexColumns`.
// go: ExtractKeyExistsErrFromIndex L2 (boundary stub, blocked-by:D06.4)
pub fn extract_key_exists_err_from_index(
    key: &Key,
    value: &[u8],
    tbl_info: &TableInfo,
    index_id: i64,
) -> Error {
    let _ = (key, value, tbl_info, index_id);
    blocked("D06.4", "duplicate-key errors of the write path")
}

/// `extractKeyErr`: the write conflict and retryable errors of a key error, then
/// `derr.ToTiDBErr`.
// go: extractKeyErr L1
pub(crate) fn extract_key_err(err: TiKVError) -> Error {
    match client_go_error(&err) {
        ClientGoError::WriteConflict(conflict) => return new_write_conflict_error(Some(conflict)),
        ClientGoError::Retryable(retryable) => {
            let not_found_detail = pretty_lock_not_found_key(retryable);
            let mut msg = retryable.as_bytes().to_vec();
            msg.push(b' ');
            msg.extend_from_slice(&not_found_detail);
            return ErrTxnRetryable.gen_with_stack_by_args(vec![Arg::go_string(msg)]);
        }
        _ => {}
    }
    to_tidb_err(err)
}

/// Go's `hex.EncodeToString`.
// go-std: encoding/hex.EncodeToString L1
fn hex_encode_to_string(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// go: newWriteConflictError L1
fn new_write_conflict_error(conflict: Option<&kvrpcpb::WriteConflict>) -> Error {
    let Some(conflict) = conflict else {
        return ErrWriteConflict.clone();
    };
    // table id part of conflict key, which is used to be parsed by upper level to provide
    // more information about the table
    let mut buf_conflict_key_table_id = Vec::new();
    let mut buf_conflict_key_rest = Vec::new(); // the rest part of conflict key
    let mut buf_primary_key_table_id = Vec::new(); // table id part of primary key
    let mut buf_primary_key_rest = Vec::new(); // the rest part of primary key
    pretty_write_key(
        &mut buf_conflict_key_table_id,
        &mut buf_conflict_key_rest,
        &conflict.key,
    );
    buf_conflict_key_rest.extend_from_slice(
        format!(", originalKey={}", hex_encode_to_string(&conflict.key)).as_bytes(),
    );
    buf_conflict_key_rest.extend_from_slice(b", primary=");
    pretty_write_key(
        &mut buf_primary_key_table_id,
        &mut buf_primary_key_rest,
        &conflict.primary,
    );
    buf_primary_key_rest.extend_from_slice(
        format!(
            ", originalPrimaryKey={}",
            hex_encode_to_string(&conflict.primary)
        )
        .as_bytes(),
    );
    // kvrpcpb.WriteConflict_Reason.String(): the enum value's name.
    let reason = kvrpcpb::write_conflict::Reason::try_from(conflict.reason)
        .map(|r| r.as_str_name().to_owned())
        .unwrap_or_else(|_| conflict.reason.to_string());
    ErrWriteConflict.fast_gen_by_args(vec![
        Arg::from(conflict.start_ts),
        Arg::from(conflict.conflict_ts),
        Arg::from(conflict.conflict_commit_ts),
        Arg::go_string(buf_conflict_key_table_id),
        Arg::go_string(buf_conflict_key_rest),
        Arg::go_string(buf_primary_key_table_id),
        Arg::go_string(buf_primary_key_rest),
        Arg::from(reason),
    ])
}

// go: prettyWriteKey L1 [M-BYTES]
fn pretty_write_key(buf_table_id: &mut Vec<u8>, buf_rest: &mut Vec<u8>, key: &[u8]) {
    if let Ok((table_id, index_id, index_values)) = tablecodec::decode_index_key(key) {
        buf_table_id.extend_from_slice(format!("{{tableID={table_id}").as_bytes());
        buf_rest.extend_from_slice(format!(", indexID={index_id}, indexValues={{").as_bytes());
        for v in &index_values {
            buf_rest.extend_from_slice(v);
            buf_rest.extend_from_slice(b", ");
        }
        buf_rest.extend_from_slice(b"}}");
        return;
    }

    if let Ok((table_id, handle)) = tablecodec::decode_record_key(key) {
        buf_table_id.extend_from_slice(format!("{{tableID={table_id}").as_bytes());
        buf_rest.extend_from_slice(format!(", handle={}}}", handle.string()).as_bytes());
        return;
    }

    if let Ok((m_key, m_field)) = tablecodec::decode_meta_key(key) {
        buf_rest.extend_from_slice(b"{metaKey=true, key=");
        buf_rest.extend_from_slice(&m_key);
        buf_rest.extend_from_slice(b", field=");
        buf_rest.extend_from_slice(&m_field);
        buf_rest.push(b'}');
        return;
    }

    buf_rest.extend_from_slice(&sprintf("%#v", &[Arg::Bytes(key.to_vec())]));
}

// go: prettyLockNotFoundKey L1
fn pretty_lock_not_found_key(raw_retry: &str) -> Vec<u8> {
    if !raw_retry.contains("TxnLockNotFound") {
        return Vec::new();
    }
    let Some(start) = raw_retry.find('[') else {
        return Vec::new();
    };
    let raw_retry = &raw_retry[start..];
    let Some(end) = raw_retry.find(']') else {
        return Vec::new();
    };
    let raw_retry = &raw_retry[..end + 1];
    // json.Unmarshal into []byte: a JSON array of numbers 0..=255.
    let key: Vec<u8> = match serde_json::from_str(raw_retry) {
        Ok(k) => k,
        Err(_) => return Vec::new(),
    };
    let mut buf1 = Vec::new();
    let mut buf2 = Vec::new();
    pretty_write_key(&mut buf1, &mut buf2, &key);
    buf1.extend_from_slice(&buf2);
    buf1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_key(table_id: i64, handle: i64) -> Vec<u8> {
        tablecodec::encode_row_key_with_handle(
            table_id,
            &tidb_kv::Handle::Int(tidb_kv::key::IntHandle(handle)),
        )
        .0
    }

    #[test]
    fn write_conflict_message() {
        let conflict = kvrpcpb::WriteConflict {
            start_ts: 1,
            conflict_ts: 2,
            conflict_commit_ts: 3,
            key: record_key(5, 7),
            primary: b"m\x00".to_vec(),
            reason: kvrpcpb::write_conflict::Reason::Optimistic as i32,
        };
        let e = extract_key_err(TiKVError::from(tikv_client::Error::KeyError(Box::new(
            kvrpcpb::KeyError {
                conflict: Some(conflict.clone()),
                ..Default::default()
            },
        ))));
        assert!(ErrWriteConflict.equal(&e));
        let msg = e.to_string();
        assert!(
            msg.contains("txnStartTS=1, conflictStartTS=2, conflictCommitTS=3"),
            "{msg}"
        );
        assert!(msg.contains("key={tableID=5, handle=7}"), "{msg}");
        assert!(
            msg.contains(&format!(
                "originalKey={}",
                hex_encode_to_string(&conflict.key)
            )),
            "{msg}"
        );
        assert!(msg.contains("reason=Optimistic"), "{msg}");
        assert!(ErrWriteConflict.equal(&new_write_conflict_error(None)));
    }

    #[test]
    fn retryable_with_lock_not_found_key() {
        let key = record_key(5, 7);
        let raw = format!(
            "TxnLockNotFound {{ key: {:?} }}",
            key.iter().map(|b| *b as u32).collect::<Vec<_>>()
        );
        let detail = pretty_lock_not_found_key(&raw);
        assert_eq!(detail, b"{tableID=5, handle=7}".to_vec());
        assert!(pretty_lock_not_found_key("other [1]").is_empty());
        assert!(pretty_lock_not_found_key("TxnLockNotFound [300]").is_empty());
        let e = extract_key_err(TiKVError::from(tikv_client::Error::KeyError(Box::new(
            kvrpcpb::KeyError {
                retryable: raw.clone(),
                ..Default::default()
            },
        ))));
        assert!(ErrTxnRetryable.equal(&e));
        assert!(e.to_string().contains("{tableID=5, handle=7}"), "{e}");
    }

    #[test]
    fn pretty_keys() {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        pretty_write_key(&mut a, &mut b, b"\x01\x02");
        assert!(a.is_empty());
        assert_eq!(String::from_utf8(b).unwrap(), "[]byte{0x1, 0x2}");
    }

    #[test]
    fn key_exists_fallback() {
        let e = gen_key_exists_error("t.PRIMARY", b"1", None);
        assert_eq!(
            e.to_string(),
            "[kv:1062]Duplicate entry '1' for key 't.PRIMARY'"
        );
    }
}
