//! Replays the govec `sessiontxn` vectors (tidb-rs-tracker `govec/domains/sessiontxn.go`):
//! the pure functions of block D05.4 part 1 (`ErrorAction`, `RetryReady`, `NoIdea`,
//! `ConstantFuture.Wait`). The providers are tested in `providers.rs`.

use std::io::Read;

use serde_json::{Value, json};
use tidb_sessiontxn::{ConstantFuture, error_action, no_idea, retry_ready};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn exec(op: &str, a: &[Value]) -> Value {
    match op {
        "action" => {
            let (action, err) = match a[0].as_str().unwrap() {
                "error" => error_action(tidb_base::errors::new(a[1].as_str().unwrap().to_owned())),
                "retry" => retry_ready(),
                _ => no_idea(),
            };
            let msg = match err {
                Ok(()) => Value::Null,
                Err(e) => json!(String::from_utf8_lossy(&e.error())),
            };
            json!([action as i64, msg])
        }
        "constant_future" => json!(ConstantFuture(a[0].as_u64().unwrap()).wait().unwrap()),
        _ => panic!("unknown op {op}"),
    }
}

#[test]
fn govec_sessiontxn() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/sessiontxn.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(&path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "sessiontxn");
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let got = exec(rec["op"].as_str().unwrap(), rec["args"].as_array().unwrap());
        assert_eq!(got, rec["out"], "{line}");
        assert!(rec["err"].is_null());
    }
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}
