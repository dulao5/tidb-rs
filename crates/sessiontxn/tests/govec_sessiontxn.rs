//! Replays the govec `sessiontxn` vectors (tidb-rs-tracker `govec/domains/sessiontxn.go`):
//! the pure functions of block D05.4 part 1 (`ErrorAction`, `RetryReady`, `NoIdea`,
//! `ConstantFuture.Wait`), and the read-ts sources of random statement sequences (`read_ts`,
//! N11 §4.4): per read, the provider's `ReadTsSource` and the TSO requests of the statement
//! must be Go's, replayed on the mock session of `providers.rs`.

mod common;

use std::io::Read;

use futures::executor::block_on;
use serde_json::{Value, json};
use tidb_sessiontxn::isolation::{
    BaseTxnContextProvider, new_pessimistic_rc_txn_context_provider,
    new_pessimistic_rr_txn_context_provider,
};
use tidb_sessiontxn::sctx::ReadTsSource;
use tidb_sessiontxn::{
    ConstantFuture, EnterNewTxnType, TxnContextProvider, error_action, no_idea, retry_ready,
};

use common::{MockSession, PointGet, ReadOnlyStmt};

/// The govec `read_ts` op: the session's steps over the isolation providers.
fn read_ts(cfg: &Value, steps: &[Value]) -> Value {
    let mut s = MockSession::new();
    s.vars.set_status_flag(
        tidb_base::mysql::ServerStatusAutocommit,
        cfg["autocommit"].as_bool().unwrap(),
    );
    s.vars.low_resolution_tso = cfg["low_res"].as_bool().unwrap();
    if cfg["snapshot"].as_bool().unwrap() {
        s.vars.snapshot_ts = 400_000;
    }
    let mode = cfg["mode"].as_str().unwrap().to_owned();
    let new_provider = |s: &MockSession, explicit: bool| -> BaseTxnContextProvider {
        let mode = if !explicit && s.vars.is_autocommit() {
            "optimistic"
        } else {
            mode.as_str()
        };
        match mode {
            "rr" => new_pessimistic_rr_txn_context_provider(false),
            "rc" => new_pessimistic_rc_txn_context_provider(false),
            _ => BaseTxnContextProvider::reset_for_new_txn(false),
        }
    };
    let mut p: Option<BaseTxnContextProvider> = None;
    let mut out = vec![];
    for step in steps {
        match step.as_str().unwrap() {
            "begin" => {
                let mut np = new_provider(&s, true);
                block_on(np.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnWithBeginStmt))
                    .unwrap();
                p = Some(np);
            }
            "commit" => {
                s.end_txn();
                p = None;
            }
            step => {
                if p.is_none() {
                    let mut np = new_provider(&s, false);
                    block_on(np.on_initialize(&mut s, EnterNewTxnType::EnterNewTxnBeforeStmt))
                        .unwrap();
                    p = Some(np);
                }
                let pr = p.as_mut().unwrap();
                pr.on_stmt_start(&mut s, Some(&ReadOnlyStmt)).unwrap();
                if step == "point" {
                    pr.advise_optimize_with_plan(&mut s, Some(&PointGet))
                        .unwrap();
                }
                pr.advise_warmup(&mut s).unwrap();
                block_on(pr.get_stmt_read_ts(&mut s)).unwrap();
                let src = match pr.read_ts_source(&s) {
                    ReadTsSource::Tso => "Tso",
                    ReadTsSource::LowResolution => "LowResolution",
                    ReadTsSource::MaxTs => "MaxTs",
                    ReadTsSource::TxnStart(_) => "TxnStart",
                    ReadTsSource::Stale(_) => "Stale",
                    ReadTsSource::Snapshot(_) => "Snapshot",
                };
                out.push(json!([src, pr.stmt_tso_requests()]));
                if !s.vars.in_txn() {
                    s.end_txn();
                    p = None;
                }
            }
        }
    }
    if out.is_empty() {
        Value::Null
    } else {
        Value::Array(out)
    }
}

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
        "read_ts" => read_ts(&a[0], a[1].as_array().unwrap()),
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
