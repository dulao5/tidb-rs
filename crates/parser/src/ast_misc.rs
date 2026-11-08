//! go-file: pkg/parser/ast/misc.go
//!
//! `RedactURL` (block D02.1), used by the `tidb_cloud_storage_uri` GetGlobal hook (D05.2).
//! The rest of misc.go is in the hand-written `ast` module.

use tidb_base::gostd::to_lower;
use tidb_base::gostd::url;

/// RedactURL redacts the secret tokens in the URL. only S3 is supported by now.
/// User should use this function to redact the URL before logging it.
// go: RedactURL L1 (failpoint "forceRedactURL" not ported)
pub fn redact_url(str: &str) -> String {
    // FIXME: this solution is not scalable, and duplicates some logic from BR.
    let Ok(mut u) = url::parse(str) else {
        return str.to_owned();
    };
    let scheme = String::from_utf8_lossy(&u.scheme).into_owned();
    let redact_keys: &[&str] = match to_lower(&scheme).as_str() {
        "s3" | "ks3" => &["access-key", "secret-access-key", "session-token"],
        "azure" | "azblob" => &["sas-token"],
        _ => &[],
    };
    if !redact_keys.is_empty() {
        let mut values = u.query();
        for (k, v) in values.iter_mut() {
            // see below on why we normalize key
            // https://github.com/pingcap/tidb/blob/a7c0d95f16ea2582bb569278c3f829403e6c3a7e/br/pkg/storage/parse.go#L163
            let normalized_key = String::from_utf8_lossy(k).replace('_', "-");
            if redact_keys.contains(&to_lower(&normalized_key).as_str()) {
                *v = vec![b"xxxxxx".to_vec()];
            }
        }
        // In go1.25.5, url.Values.Encode() will sort the keys.
        u.raw_query = url::encode(&values);
    }
    u.string()
}
