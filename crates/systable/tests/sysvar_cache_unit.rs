//! Unit tests of the sysvar cache rebuild (`rebuildSysVarCache`) and of the decoding of the
//! scanned `mysql.global_variables` records (`fetchTableValues`).

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::atomic::Ordering::SeqCst;

use serde_json::Value;
use tidb_base::Error;
use tidb_model::TableInfo;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_sessionctx::variable::tidb_vars::MaxPreparedStmtCountValue;
use tidb_sessionctx::variable::variable::get_sys_var;
use tidb_systable::memdb::MemBuffer;
use tidb_systable::replace::{RowEncodingConfig, replace_row};
use tidb_systable::sysvar_cache::{SysVarCache, decode_global_variables};

struct Acc;

impl GlobalVarAccessor for Acc {
    fn get_global_sys_var(&self, name: &str) -> Result<String, Error> {
        Ok(get_sys_var(name)
            .map(|s| s.value.clone())
            .unwrap_or_default())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Err(tidb_base::errors::new("empty"))
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// The callbacks the domain registers (Go's nil func otherwise): no-ops. A rebuild runs every
/// SetGlobal hook, so all of them are reached.
/// A rebuild runs every SetGlobal hook, which write process-wide state: the rebuild tests
/// run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn register_callbacks() {
    use std::sync::Arc;
    use tidb_sessionctx::variable::hook_state as hs;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        *hs::SetMemQuotaAnalyze.write() = Some(Arc::new(|_| {}));
        *hs::GetMemQuotaAnalyze.write() = Some(Arc::new(|| -1));
        *hs::SetStatsCacheCapacity.write() = Some(Arc::new(|_| {}));
        *hs::SetPDClientDynamicOption.write() = Some(Arc::new(|_, _| Ok(())));
        *hs::SwitchMDL.write() = Some(Arc::new(|_| Ok(())));
        *hs::SetExternalTimestamp.write() = Some(Arc::new(|_| Ok(())));
        *hs::GetExternalTimestamp.write() = Some(Arc::new(|| Ok(0)));
        *hs::ValidateCloudStorageURI.write() = Some(Arc::new(|_| Ok(())));
        *hs::SetGlobalResourceControl.write() = Some(Arc::new(|_| {}));
        *hs::SetLowResolutionTSOUpdateInterval.write() = Some(Arc::new(|_| Ok(())));
        *hs::ChangeSchemaCacheSize.write() = Some(Arc::new(|_| Ok(())));
        *hs::ChangePDMetadataCircuitBreakerErrorRateThresholdRatio.write() = Some(Arc::new(|_| {}));
        assert!(hs::unregistered_callbacks().is_empty());
    });
}

#[test]
fn rebuild_takes_table_values_and_runs_set_global_hooks() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    register_callbacks();
    let cache = SysVarCache::new();
    assert!(cache.needs_rebuild());
    let mut vars = SessionVars::new(Box::new(Acc));
    let table: HashMap<String, String> = [
        ("sql_mode", "ANSI_QUOTES"),
        // Out of range: the relaxed validation clips it to the maximum before the hook runs.
        ("max_prepared_stmt_count", "99999999"),
        ("no_such_variable", "x"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let errs = cache.rebuild(&mut vars, &mut || Ok(table.clone())).unwrap();
    assert!(errs.is_empty(), "{errs:?}");
    assert!(!cache.needs_rebuild());
    assert_eq!(cache.global_var("sql_mode").unwrap(), "ANSI_QUOTES");
    // The cache keeps the table value; the hook received the normalized one.
    assert_eq!(
        cache.global_var("max_prepared_stmt_count").unwrap(),
        "99999999"
    );
    assert_eq!(MaxPreparedStmtCountValue.load(SeqCst), 1048576);
    // A variable that is not in the table has its default.
    let sv = get_sys_var("tidb_mem_oom_action").unwrap();
    assert_eq!(cache.global_var("tidb_mem_oom_action").unwrap(), sv.value);
    // Session cache: every variable that is not skipInit, with the same values.
    let sess = cache.session_cache();
    assert_eq!(
        sess.get("sql_mode").map(String::as_str),
        Some("ANSI_QUOTES")
    );
    assert!(cache.global_var("no_such_variable").is_err());
    MaxPreparedStmtCountValue.store(-1, SeqCst);
}

#[test]
fn rebuild_keeps_the_old_cache_when_the_fetch_fails() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    register_callbacks();
    let cache = SysVarCache::new();
    let mut vars = SessionVars::new(Box::new(Acc));
    let t: HashMap<String, String> = [("sql_mode".to_owned(), "".to_owned())].into();
    cache.rebuild(&mut vars, &mut || Ok(t.clone())).unwrap();
    assert!(
        cache
            .rebuild(&mut vars, &mut || Err(tidb_base::errors::new(
                "scan failed"
            )))
            .is_err()
    );
    assert_eq!(cache.global_var("sql_mode").unwrap(), "");
}

/// The rows a REPLACE writes decode back to the name and value (fetchTableValues over the
/// record keys of the buffer), with the TableInfo of the govec records.
#[test]
fn decode_reads_back_written_rows() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/arm64/write_systable.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap());
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let rec: Value = text
        .lines()
        .skip(1)
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|r| r["op"] == "tableinfo" && r["args"][0] == "global_variables")
        .unwrap();
    let mut tbl = TableInfo::default();
    tbl.unmarshal_json(rec["out"]["info"].to_string().as_bytes())
        .unwrap();
    let snap: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut mb = MemBuffer::new();
    let mut next = 41;
    for (n, v) in [("a", "1"), ("b", ""), ("ü", "ä")] {
        replace_row(
            &tbl,
            &[Some(n), Some(v)],
            &snap,
            &mut mb,
            &mut || {
                next += 1;
                Ok(next)
            },
            &RowEncodingConfig::default(),
        )
        .unwrap();
    }
    let recs: Vec<(Vec<u8>, Vec<u8>)> = mb
        .entries()
        .iter()
        .map(|(k, e)| (k.clone(), e.value.clone()))
        .collect();
    let got = decode_global_variables(&tbl, &recs).unwrap();
    let want: HashMap<String, String> = [("a", "1"), ("b", ""), ("ü", "ä")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    assert_eq!(got, want);
    // Over-long values are refused.
    let mut mb = MemBuffer::new();
    let long = "x".repeat(16384);
    let err = replace_row(
        &tbl,
        &[Some("x"), Some(&long)],
        &snap,
        &mut mb,
        &mut || Ok(1),
        &RowEncodingConfig::default(),
    )
    .unwrap_err();
    assert_eq!(
        String::from_utf8_lossy(&err.error()),
        "[types:1406]Data too long for column 'VARIABLE_VALUE' at row 1"
    );
    assert!(mb.is_empty());
}
