//! go-file: pkg/parser/ast/misc.go
//!
//! `RedactURL` (block D02.1), used by the `tidb_cloud_storage_uri` GetGlobal hook (D05.2).
//! The rest of misc.go is in the hand-written `ast` module.
//!
//! `RedactURL` carries two upstream security fixes from release-8.5 after v8.5.8
//! (pingcap/tidb#71844 and #72214, issue #71858): v8.5.8 kept the userinfo, the fragment
//! and every query option it did not know, so credentials in them leaked into
//! `tidb_cloud_storage_uri` and redacted SQL.

#![allow(non_upper_case_globals)]

use tidb_base::gostd::json;
use tidb_base::gostd::to_lower;
use tidb_base::gostd::url;

/// RedactedValue replaces a secret in redacted output.
// go-fix: pingcap/tidb@4f61995df8 #71844
pub const RedactedValue: &str = "xxxxxx";

/// InvalidURLPlaceholder replaces a value that cannot be parsed as a URL in
/// redacted output, because such a value cannot be reliably masked. The value
/// is not necessarily a storage URL; it may also be an endpoint or a local path.
// go-fix: pingcap/tidb@84040e1f0a #72214
pub const InvalidURLPlaceholder: &str = "(invalid URL)";

/// RedactURL removes userinfo, and in supported storage URLs keeps only known query options,
/// redacts sensitive values, and removes fragments.
/// Queries for other schemes are removed entirely.
/// Plain local paths are unchanged; an invalid URL is replaced by InvalidURLPlaceholder.
// go: RedactURL L1 (failpoint "forceRedactURL" not ported)
// go-fix: pingcap/tidb@4f61995df8 #71844
// go-fix: pingcap/tidb@84040e1f0a #72214
pub fn redact_url(str: &str) -> String {
    // FIXME: this solution is not scalable, and duplicates some logic from BR.
    if !str.contains("://") && !str.contains('?') {
        return str.to_owned();
    }
    let Ok(mut u) = url::parse(str) else {
        return InvalidURLPlaceholder.to_owned();
    };
    u.user = None;
    u.fragment.clear();
    u.raw_fragment.clear();
    let scheme = String::from_utf8_lossy(&u.scheme).into_owned();

    // The allowlist maps known options to whether their values must be redacted.
    // Unknown keys may be new secrets, misspellings, or parts of split secret values.
    let query_keys: &[(&str, bool)] = match to_lower(&scheme).as_str() {
        "s3" | "ks3" | "oss" => &[
            // Endpoints can embed userinfo or signed query credentials.
            ("endpoint", true),
            ("region", false),
            ("storage-class", false),
            ("sse", false),
            ("sse-kms-key-id", false),
            ("acl", false),
            ("provider", false),
            ("force-path-style", false),
            ("use-accelerate-endpoint", false),
            ("role-arn", false),
            ("external-id", false),
            ("profile", false),
            ("object-lock-enabled", false),
            ("access-key", true),
            ("secret-access-key", true),
            ("session-token", true),
            ("sse-kms-context", true),
        ],
        "gs" | "gcs" => {
            u.force_query = false;
            &[
                ("endpoint", true),
                ("storage-class", false),
                ("predefined-acl", false),
                ("credentials-file", true),
            ]
        }
        "azure" | "azblob" => &[
            ("account-name", false),
            ("access-tier", false),
            ("encryption-scope", false),
            ("account-key", true),
            ("encryption-key", true),
            ("sas-token", true),
            // Azure endpoints can contain SAS tokens used directly by the storage
            // client, so masking only the separate sas-token parameter is insufficient.
            ("endpoint", true),
        ],
        _ => &[],
    };

    if !query_keys.is_empty() {
        // URL.Query skips undecodable pairs, which may hide a split secret.
        let (mut values, err) = url::parse_query_values(&u.raw_query);
        if err.is_err() {
            u.raw_query.clear();
            return u.string();
        }
        let keys: Vec<Vec<u8>> = values.keys().cloned().collect();
        for k in keys {
            // see below on why we normalize key
            // https://github.com/pingcap/tidb/blob/a7c0d95f16ea2582bb569278c3f829403e6c3a7e/br/pkg/storage/parse.go#L163
            let normalized_key = to_lower(&String::from_utf8_lossy(&k).replace('_', "-"));
            match query_keys.iter().find(|(key, _)| *key == normalized_key) {
                None => {
                    values.remove(&k);
                }
                Some((_, true)) => {
                    // An unescaped '&' can split the JSON context into an allowlisted
                    // non-secret key (e.g. region), exposing the rest of the context.
                    // Drop the whole query if any context value is invalid JSON.
                    if normalized_key == "sse-kms-context"
                        && values[&k].iter().any(|v| json::unmarshal_raw(v).is_err())
                    {
                        u.raw_query.clear();
                        return u.string();
                    }
                    values.insert(k, vec![RedactedValue.as_bytes().to_vec()]);
                }
                Some((_, false)) => {}
            }
        }
        // In go1.25.5, url.Values.Encode() will sort the keys.
        u.raw_query = url::encode(&values);
    } else {
        u.raw_query.clear();
        u.force_query = false;
    }
    u.string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Go: TestRedactURL in pkg/parser/ast/misc_test.go (release-8.5 after 84040e1f0a).
    #[test]
    fn test_redact_url_secrets_removed() {
        for raw_url in [
            "s3://FAKE_AST_USER:FAKE_AST_PASS@bucket/path?Access_Key=FAKE_AST_AK&secret_access_key=FAKE_AST_SK&other-key=benign",
            "azure://FAKE_AST_USER:FAKE_AST_PASS@bucket/path?SAS_Token=FAKE_AST_SAS&account-name=benign",
            "s3://FAKE_AST_USER:FAKE_AST_PASS@bucket/path%zz?access-key=FAKE_AST_AK&secret-access-key=FAKE_AST_SK",
            "azure://bucket/path?account-name=benign&sas-token=sv=2020-08-04&sig=FAKE_AST_SAS&FAKE_AST_SK",
            "s3://bucket/path?region=us-east-1&new-secret=FAKE_AST_SK&FAKE_AST_AK=unknown",
            "s3://bucket/path?endpoint=https%3A%2F%2FFAKE_AST_USER%3AFAKE_AST_PASS%40host%2F%3Fsig%3DFAKE_AST_SK",
            r#"s3://bucket/path?sse-kms-context={"a":"x&region=FAKE_AST_SK"}"#,
        ] {
            let output = redact_url(raw_url);
            for secret in [
                "FAKE_AST_USER",
                "FAKE_AST_PASS",
                "FAKE_AST_AK",
                "FAKE_AST_SK",
                "FAKE_AST_SAS",
            ] {
                assert!(!output.contains(secret), "{raw_url} -> {output}");
            }
        }
    }

    #[test]
    fn test_redact_url() {
        let tests: &[(&str, &str)] = &[
            ("", ""),
            (":", ":"),
            (
                "s3://FAKE_AST_USER:FAKE_AST_PASS@bucket/path%zz?access-key=FAKE_AST_AK",
                InvalidURLPlaceholder,
            ),
            (
                "s3://FAKE_AST_USER:FAKE_AST_PASS@bucket/path?Access_Key=FAKE_AST_AK&other-key=benign",
                "s3://bucket/path?Access_Key=xxxxxx",
            ),
            (
                "azure://FAKE_AST_USER:FAKE_AST_PASS@bucket/path?SAS_Token=FAKE_AST_SAS&account-name=benign",
                "azure://bucket/path?SAS_Token=xxxxxx&account-name=benign",
            ),
            ("~/file", "~/file"),
            ("gs://bucket/file", "gs://bucket/file"),
            (
                "gs://bucket/file?storage-class=NEARLINE&predefined-acl=private&unknown=secret",
                "gs://bucket/file?predefined-acl=private&storage-class=NEARLINE",
            ),
            (
                "gcs://bucket/file?storage-class=COLDLINE&predefined-acl=private&credentials-file=secret",
                "gcs://bucket/file?credentials-file=xxxxxx&predefined-acl=private&storage-class=COLDLINE",
            ),
            (
                "GS://user:pass@bucket/file?Credentials_File=first-secret&Credentials_File=second-secret&EndPoint=https%3A%2F%2Fuser%3Apass%40host%2F%3Fsig%3Dsecret#fragment",
                "gs://bucket/file?Credentials_File=xxxxxx&EndPoint=xxxxxx",
            ),
            (
                "gcs://bucket/file?endpoint=first-secret&endpoint=second-secret&storage-class=STANDARD",
                "gcs://bucket/file?endpoint=xxxxxx&storage-class=STANDARD",
            ),
            // Unknown options must not expose arbitrary values.
            ("gs://bucket/file?access-key=123", "gs://bucket/file"),
            ("gs://bucket/file?secret-access-key=123", "gs://bucket/file"),
            (
                "gcs://user:pass@bucket/file?credentials-file=secret#fragment",
                "gcs://bucket/file?credentials-file=xxxxxx",
            ),
            (
                "file:///tmp/backup?token=secret#fragment",
                "file:///tmp/backup",
            ),
            (
                "local:///tmp/backup?token=secret#fragment",
                "local:///tmp/backup",
            ),
            (
                "hdfs://user:pass@host/backup?token=secret#fragment",
                "hdfs://host/backup",
            ),
            ("noop:///?token=secret#fragment", "noop:///"),
            (
                "https://user:pass@host/backup?token=secret#fragment",
                "https://host/backup",
            ),
            ("/tmp/backup?token=secret#fragment", "/tmp/backup"),
            ("gcs://bucket/file?token=secret%zz", "gcs://bucket/file"),
            ("GCS://bucket/file#fragment", "gcs://bucket/file"),
            ("gcs://bucket/file?", "gcs://bucket/file"),
            ("local:///tmp/backup?", "local:///tmp/backup"),
            ("s3://bucket/file", "s3://bucket/file"),
            ("s3://bucket/file?other-key=123", "s3://bucket/file"),
            (
                "s3://bucket/file?acl=private&force-path-style=true&provider=aws&region=us-east-1&sse=AES256&storage-class=STANDARD",
                "s3://bucket/file?acl=private&force-path-style=true&provider=aws&region=us-east-1&sse=AES256&storage-class=STANDARD",
            ),
            (
                "ks3://bucket/file?region=us-east-1&unknown-key=secret",
                "ks3://bucket/file?region=us-east-1",
            ),
            (
                "oss://bucket/file?provider=alibaba&access-key-typo=secret",
                "oss://bucket/file?provider=alibaba",
            ),
            (
                "azure://container/file?account-name=a&unknown-key=secret",
                "azure://container/file?account-name=a",
            ),
            (
                "azblob://container/file?access-tier=Hot&secret-name&%73ig=secret",
                "azblob://container/file?access-tier=Hot",
            ),
            (
                "S3://bucket/file?Region=us-east-1&Force_Path_Style=true&Region=us-west-2&unknown=secret",
                "s3://bucket/file?Force_Path_Style=true&Region=us-east-1&Region=us-west-2",
            ),
            (
                "s3://bucket/file?region=us-east-1&session-token=secret&session-token=another#fragment-secret",
                "s3://bucket/file?region=us-east-1&session-token=xxxxxx",
            ),
            (
                "s3://bucket/file?sse-kms-context=%7B%22tenant%22%3A%22context-value%22%7D",
                "s3://bucket/file?sse-kms-context=xxxxxx",
            ),
            // Split JSON contexts drop the whole query, even if the tail matches an allowed key.
            (
                r##"s3://bucket/file?sse-kms-context={"tenant":"#context-value"}"##,
                "s3://bucket/file",
            ),
            (
                r#"s3://bucket/file?sse=aws:kms&sse-kms-context={"tenant":"a&private-context-value"}"#,
                "s3://bucket/file",
            ),
            (
                r#"s3://bucket/file?sse=aws:kms&sse-kms-context={"tenant":"x&y"}"#,
                "s3://bucket/file",
            ),
            (
                r#"s3://bucket/file?sse-kms-context={"a":"x&region=private-context-value"}"#,
                "s3://bucket/file",
            ),
            (
                r#"ks3://bucket/file?sse-kms-context={"a":"x&region=private-context-value"}"#,
                "ks3://bucket/file",
            ),
            (
                r#"oss://bucket/file?SSE_KMS_CONTEXT={"a":"x&provider=private-context-value"}"#,
                "oss://bucket/file",
            ),
            // Malformed query encoding still drops the whole query.
            (
                r#"s3://bucket/file?sse=aws:kms&sse-kms-context={"tenant":"a;x&private-context-value"}"#,
                "s3://bucket/file",
            ),
            (
                r#"s3://bucket/file?sse=aws:kms&sse-kms-context={"tenant":"a%zz&private-context-value"}"#,
                "s3://bucket/file",
            ),
            (
                "s3://bucket/file?SSE_KMS_CONTEXT=%7B%7D&SSE_KMS_CONTEXT=%7B%22a%22%3A%22b%22%7D",
                "s3://bucket/file?SSE_KMS_CONTEXT=xxxxxx",
            ),
            (
                r#"s3://bucket/file?sse-kms-context=%7B%7D&sse-kms-context={"a":"x&region=private-context-value"}"#,
                "s3://bucket/file",
            ),
            (
                r#"s3://bucket/file?sse-kms-context={"a":"x&region=private-context-value"}&sse-kms-context=%7B%7D"#,
                "s3://bucket/file",
            ),
            (
                r#"s3://bucket/file?sse-kms-context=%7B%7D&SSE_KMS_CONTEXT={"a":"x&region=private-context-value"}"#,
                "s3://bucket/file",
            ),
            (
                "s3://bucket/file?sse-kms-context=%7B%22a%22%3A%22x%26region%3Dprivate-context-value%22%7D&region=us-east-1",
                "s3://bucket/file?region=us-east-1&sse-kms-context=xxxxxx",
            ),
            (
                "s3://bucket/file?access-key=123",
                "s3://bucket/file?access-key=xxxxxx",
            ),
            (
                "s3://bucket/file?secret-access-key=123",
                "s3://bucket/file?secret-access-key=xxxxxx",
            ),
            (
                "s3://bucket/file?external-id=tenant-123&role-arn=role&profile=default",
                "s3://bucket/file?external-id=tenant-123&profile=default&role-arn=role",
            ),
            (
                "ks3://bucket/file?access-key=123",
                "ks3://bucket/file?access-key=xxxxxx",
            ),
            (
                "ks3://bucket/file?secret-access-key=123",
                "ks3://bucket/file?secret-access-key=xxxxxx",
            ),
            (
                "oss://bucket/file?access-key=123",
                "oss://bucket/file?access-key=xxxxxx",
            ),
            (
                "oss://bucket/file?secret-access-key=123",
                "oss://bucket/file?secret-access-key=xxxxxx",
            ),
            // underline
            (
                "s3://bucket/file?access_key=123",
                "s3://bucket/file?access_key=xxxxxx",
            ),
            (
                "s3://bucket/file?secret_access_key=123",
                "s3://bucket/file?secret_access_key=xxxxxx",
            ),
            (
                "oss://bucket/file?access-key=AKID&secret_access_key=SKEY&session-token=TOKEN",
                "oss://bucket/file?access-key=xxxxxx&secret_access_key=xxxxxx&session-token=xxxxxx",
            ),
            (
                "azure://bucket/file?account-key=AZKEY&encryption_key=ENC&endpoint=https%3A%2F%2Faccount.blob.core.windows.net%2F%3Fsig%3DSAS",
                "azure://bucket/file?account-key=xxxxxx&encryption_key=xxxxxx&endpoint=xxxxxx",
            ),
            (
                "azure://bucket/file?sas-token=123",
                "azure://bucket/file?sas-token=xxxxxx",
            ),
            (
                "azure://c/p?account-name=a&sas-token=sv=2020-08-04&ss=b&sig=SECRETSIG",
                "azure://c/p?account-name=a&sas-token=xxxxxx",
            ),
            (
                "azblob://c/p?account-name=a&sas-token=sv=2020-08-04&ss=b&sig=SECRETSIG",
                "azblob://c/p?account-name=a&sas-token=xxxxxx",
            ),
            (
                "azblob://container/file?sas-token=123",
                "azblob://container/file?sas-token=xxxxxx",
            ),
            (
                "azure://container/file?account-name=test&sas_token=123",
                "azure://container/file?account-name=test&sas_token=xxxxxx",
            ),
            (
                "azure://container/file?account-name=test&account-key=123",
                "azure://container/file?account-key=xxxxxx&account-name=test",
            ),
            (
                "azure://container/file?access-tier=Hot&account-name=test&encryption-scope=scope",
                "azure://container/file?access-tier=Hot&account-name=test&encryption-scope=scope",
            ),
            (
                "azblob://container/file?encryption-key=123",
                "azblob://container/file?encryption-key=xxxxxx",
            ),
            (
                "azure://container/file?account_key=123&encryption_key=456",
                "azure://container/file?account_key=xxxxxx&encryption_key=xxxxxx",
            ),
            (
                "azure://container/file?account-name=acct&endpoint=https%3A%2F%2Facct.blob.core.windows.net%2F%3Fsv%3D2023-11-03%26sig%3Dsecret&sas-token=token",
                "azure://container/file?account-name=acct&endpoint=xxxxxx&sas-token=xxxxxx",
            ),
            (
                "azblob://container/file?EndPoint=https%3A%2F%2Facct.blob.core.windows.net%2F%3Fsig%3Dsecret&access-tier=Hot",
                "azblob://container/file?EndPoint=xxxxxx&access-tier=Hot",
            ),
            (
                "azure://container/file?endpoint=https%3A%2F%2Facct.blob.core.windows.net",
                "azure://container/file?endpoint=xxxxxx",
            ),
            (
                "azure://container/file?endpoint=https%3A%2F%2Facct.blob.core.windows.net%2F%25zz%3Fsig%3Dsecret",
                "azure://container/file?endpoint=xxxxxx",
            ),
            (
                "azblob://container/file?endpoint=first-secret&endpoint=second-secret",
                "azblob://container/file?endpoint=xxxxxx",
            ),
            (
                "s3://bucket/file?endpoint=https%3A%2F%2Fs3.example.com",
                "s3://bucket/file?endpoint=xxxxxx",
            ),
            (
                "ks3://bucket/file?endpoint=https%3A%2F%2Fuser%3Apass%40host%2F%3Fsig%3Dsecret",
                "ks3://bucket/file?endpoint=xxxxxx",
            ),
            (
                "oss://bucket/file?EndPoint=first-secret&EndPoint=second-secret",
                "oss://bucket/file?EndPoint=xxxxxx",
            ),
            ("s3://user:pass@b/p?region=r", "s3://b/p?region=r"),
            ("gs://user:pass@bucket/file", "gs://bucket/file"),
        ];
        for (input, want) in tests {
            assert_eq!(redact_url(input), *want, "input {input:?}");
        }
    }
}
