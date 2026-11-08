//! go-file: pkg/sessionctx/variable/session.go (the non-scalar SessionVars state, D05.2 T6)
//!
//! The non-scalar fields of Go's `SessionVars` that the session hooks read or write, with the
//! small helpers of other packages they call (`mathutil.MysqlRng`, `tiflash.ReplicaRead`,
//! `kv.ToMppVersion`, `fixcontrol.ParseToMap`, ...). Fields Go keeps behind a pointer that
//! `NewSessionVars` always allocates are held by value.

// Go names for constants (M-GONAME).
#![allow(non_upper_case_globals)]

use std::collections::BTreeSet;
use std::collections::HashMap;

use tidb_base::Error;
use tidb_base::kv::StoreType;

/// `tikvstore.Variables` (client-go `kv.Variables`): the KV options of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KVVars {
    /// BackoffLockFast specifies the LockFast backoff base duration in milliseconds.
    pub backoff_lock_fast: i32,
    /// BackOffWeight specifies the weight of the max back off time duration.
    pub back_off_weight: i32,
}

/// client-go `kv.DefBackoffLockFast`.
pub const DefBackoffLockFast: i32 = 10;
/// client-go `kv.DefBackOffWeight`.
pub const DefBackOffWeight: i32 = 2;

impl Default for KVVars {
    // go-dep: github.com/tikv/client-go/v2/kv.NewVariables L1 (Killed: the session's SQLKiller, not read here)
    fn default() -> Self {
        KVVars {
            backoff_lock_fast: DefBackoffLockFast,
            back_off_weight: DefBackOffWeight,
        }
    }
}

/// `txnIsolationLevelOneShotState`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OneShotState {
    /// oneShotDef means default, that is tx_isolation_one_shot not set.
    #[default]
    OneShotDef,
    /// oneShotSet means it's set in current transaction.
    OneShotSet,
    /// onsShotUse means it should be used in current transaction.
    OneShotUse,
}

/// The `txnIsolationLevelOneShot` field of `SessionVars`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TxnIsolationLevelOneShot {
    /// state of the one-shot isolation level.
    pub state: OneShotState,
    /// value is the isolation level name.
    pub value: String,
}

/// `TransactionContext` (the fields the session hooks read).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransactionContext {
    /// StartTS is the start timestamp of the transaction.
    pub start_ts: u64,
    /// IsExplicit indicates whether the txn is an interactive txn.
    pub is_explicit: bool,
}

/// TxnReadTS is the staleness transaction read ts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TxnReadTS {
    read_ts: u64,
    used: bool,
}

impl TxnReadTS {
    /// NewTxnReadTS creates TxnReadTS
    // go: NewTxnReadTS L1
    pub fn new(ts: u64) -> TxnReadTS {
        TxnReadTS {
            read_ts: ts,
            used: false,
        }
    }

    /// PeakTxnReadTS returns readTS
    // go: TxnReadTS.PeakTxnReadTS L1
    pub fn peak_txn_read_ts(&self) -> u64 {
        self.read_ts
    }
}

/// `mathutil.MysqlRng`: the random generator of `RAND()` with MySQL's algorithm. The mutex of
/// Go is not needed: the session owns its generator (`&mut`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MysqlRng {
    seed1: u32,
    seed2: u32,
}

const maxRandValue: u32 = 0x3FFFFFFF;

impl MysqlRng {
    /// NewWithSeed create a rng with random seed.
    // go-dep: github.com/pingcap/tidb/pkg/util/mathutil.NewWithSeed L1
    pub fn new_with_seed(seed: i64) -> MysqlRng {
        let seed1 = (seed.wrapping_mul(0x10001).wrapping_add(55555555) as u32) % maxRandValue;
        let seed2 = (seed.wrapping_mul(0x10000001) as u32) % maxRandValue;
        MysqlRng { seed1, seed2 }
    }

    /// NewWithTime create a rng with time stamp.
    // go-dep: github.com/pingcap/tidb/pkg/util/mathutil.NewWithTime L1
    pub fn new_with_time() -> MysqlRng {
        let now = tidb_base::gostd::time::now();
        MysqlRng::new_with_seed(
            now.unix()
                .wrapping_mul(1_000_000_000)
                .wrapping_add(now.nanosecond()),
        )
    }

    /// Gen will generate random number.
    // go-dep: github.com/pingcap/tidb/pkg/util/mathutil.MysqlRng.Gen L1
    pub fn r#gen(&mut self) -> f64 {
        self.seed1 = (self.seed1.wrapping_mul(3).wrapping_add(self.seed2)) % maxRandValue;
        self.seed2 = (self.seed1.wrapping_add(self.seed2).wrapping_add(33)) % maxRandValue;
        f64::from(self.seed1) / f64::from(maxRandValue)
    }

    /// SetSeed1 is a interface to set seed1
    // go-dep: github.com/pingcap/tidb/pkg/util/mathutil.MysqlRng.SetSeed1 L1
    pub fn set_seed1(&mut self, seed: u32) {
        self.seed1 = seed;
    }

    /// SetSeed2 is a interface to set seed2
    // go-dep: github.com/pingcap/tidb/pkg/util/mathutil.MysqlRng.SetSeed2 L1
    pub fn set_seed2(&mut self, seed: u32) {
        self.seed2 = seed;
    }

    /// GetSeed1 is an interface to get seed1. It's only used for getting session states.
    // go-dep: github.com/pingcap/tidb/pkg/util/mathutil.MysqlRng.GetSeed1 L1
    pub fn get_seed1(&self) -> u32 {
        self.seed1
    }

    /// GetSeed2 is an interface to get seed2. It's only used for getting session states.
    // go-dep: github.com/pingcap/tidb/pkg/util/mathutil.MysqlRng.GetSeed2 L1
    pub fn get_seed2(&self) -> u32 {
        self.seed2
    }
}

/// RuntimeFilterType type of runtime filter "IN"
pub type RuntimeFilterType = i64;
/// In type of runtime filter, like "t.k1 in (?)"
pub const In: RuntimeFilterType = 0;
/// MinMax type of runtime filter, like "t.k1 < ? and t.k1 > ?"
pub const MinMax: RuntimeFilterType = 1;

/// RuntimeFilterTypeStringToType convert RuntimeFilterTypeNameString to RuntimeFilterType
/// If name is legal, it will return Runtime Filter Type and true
/// Else, it will return -1 and false
/// The second param means the convert is ok or not. True is ok, false means it is illegal name
/// At present, we only support two names: "IN" and "MIN_MAX"
// go: RuntimeFilterTypeStringToType L1
pub fn runtime_filter_type_string_to_type(name: &str) -> (RuntimeFilterType, bool) {
    match name {
        "IN" => (In, true),
        "MIN_MAX" => (MinMax, true),
        _ => (-1, false),
    }
}

/// ToRuntimeFilterType convert session var value to RuntimeFilterType list
/// If sessionVarValue is legal, it will return RuntimeFilterType list and true
/// The second param means the convert is ok or not. True is ok, false means it is illegal value
/// The legal value should be comma-separated, eg: "IN,MIN_MAX"
///
/// Go builds the list by ranging over a map (random order); the list here is sorted
/// (M-MAPORDER).
// go: ToRuntimeFilterType L2 (M-MAPORDER: sorted; Go ranges over a map)
pub fn to_runtime_filter_type(session_var_value: &str) -> (Vec<RuntimeFilterType>, bool) {
    let mut rf_type_map = BTreeSet::new();
    for type_name in session_var_value.split(',') {
        let (rf_type, ok) =
            runtime_filter_type_string_to_type(&tidb_base::gostd::to_upper(type_name));
        if !ok {
            return (Vec::new(), ok);
        }
        rf_type_map.insert(rf_type);
    }
    (rf_type_map.into_iter().collect(), true)
}

/// RuntimeFilterMode the mode of runtime filter "OFF", "LOCAL"
pub type RuntimeFilterMode = i64;
/// RFOff disable runtime filter
pub const RFOff: RuntimeFilterMode = 1;
/// RFLocal enable local runtime filter
pub const RFLocal: RuntimeFilterMode = 2;
/// RFGlobal enable local and global runtime filter
pub const RFGlobal: RuntimeFilterMode = 3;

/// RuntimeFilterModeStringToMode convert RuntimeFilterModeString to RuntimeFilterMode
/// If name is legal, it will return Runtime Filter Mode and true
/// Else, it will return -1 and false
/// The second param means the convert is ok or not. True is ok, false means it is illegal name
/// At present, we only support one name: "OFF", "LOCAL"
// go: RuntimeFilterModeStringToMode L1
pub fn runtime_filter_mode_string_to_mode(name: &str) -> (RuntimeFilterMode, bool) {
    match name {
        "OFF" => (RFOff, true),
        "LOCAL" => (RFLocal, true),
        _ => (-1, false),
    }
}

/// `tiflash.ReplicaRead`: the policy of TiFlash replica read.
pub type TiFlashReplicaRead = i64;
/// AllReplicas means using all the available nodes to do analytic computing.
pub const AllReplicas: TiFlashReplicaRead = 0;
/// ClosestAdaptive means using the nodes in the same zone as the entry TiDB.
pub const ClosestAdaptive: TiFlashReplicaRead = 1;
/// ClosestReplicas means using only the nodes in the same zone as the entry TiDB.
pub const ClosestReplicas: TiFlashReplicaRead = 2;
/// AllReplicaStr is the string value of AllReplicas.
pub const AllReplicaStr: &str = "all_replicas";
/// ClosestAdaptiveStr is the string value of ClosestAdaptive.
pub const ClosestAdaptiveStr: &str = "closest_adaptive";
/// ClosestReplicasStr is the string value of ClosestReplicas.
pub const ClosestReplicasStr: &str = "closest_replicas";

/// GetTiFlashReplicaRead return the string of the policy.
// go-dep: github.com/pingcap/tidb/pkg/util/tiflash.GetTiFlashReplicaRead L1
pub fn get_tiflash_replica_read(policy: TiFlashReplicaRead) -> &'static str {
    match policy {
        AllReplicas => AllReplicaStr,
        ClosestAdaptive => ClosestAdaptiveStr,
        ClosestReplicas => ClosestReplicasStr,
        _ => AllReplicaStr,
    }
}

/// GetTiFlashReplicaReadByStr return corresponding policy in string.
// go-dep: github.com/pingcap/tidb/pkg/util/tiflash.GetTiFlashReplicaReadByStr L1
pub fn get_tiflash_replica_read_by_str(str: &str) -> TiFlashReplicaRead {
    match str {
        AllReplicaStr => AllReplicas,
        ClosestAdaptiveStr => ClosestAdaptive,
        ClosestReplicasStr => ClosestReplicas,
        _ => AllReplicas,
    }
}

/// ToMppVersion converts the name to the mpp version (`kv.ToMppVersion`, block DEF.MPP; its
/// only read-path caller is the mpp_version SetSession hook).
// go-dep: github.com/pingcap/tidb/pkg/kv.ToMppVersion L1
pub fn to_mpp_version(name: &str) -> (i64, bool) {
    let name = name.to_uppercase();
    if name == tidb_kv::mpp::MppVersionUnspecifiedName {
        return (tidb_kv::mpp::MppVersionUnspecified.0, true);
    }
    let (v, err) = tidb_types::gostd::parse_int(name.as_bytes(), 64);
    if err.is_err() {
        return (tidb_kv::mpp::MppVersionUnspecified.0, false);
    }
    if v >= tidb_kv::mpp::MppVersionUnspecified.0 && v <= tidb_kv::mpp::get_newest_mpp_version().0 {
        return (v, true);
    }
    (tidb_kv::mpp::MppVersionUnspecified.0, false)
}

/// `kv.ExchangeCompressionModeNONE`.
pub const ExchangeCompressionModeNONE: i64 = 0;
/// `kv.ExchangeCompressionModeUnspecified`.
pub const ExchangeCompressionModeUnspecified: i64 = 3;

/// ToExchangeCompressionMode returns the ExchangeCompressionMode from name (`tipb.CompressionMode_value`:
/// NONE, FAST, HIGH_COMPRESSION).
// go-dep: github.com/pingcap/tidb/pkg/kv.ToExchangeCompressionMode L1
pub fn to_exchange_compression_mode(name: &str) -> (i64, bool) {
    let name = name.to_uppercase();
    if name == "UNSPECIFIED" {
        return (ExchangeCompressionModeUnspecified, true);
    }
    match name.as_str() {
        "NONE" => (0, true),
        "FAST" => (1, true),
        "HIGH_COMPRESSION" => (2, true),
        _ => (ExchangeCompressionModeNONE, false),
    }
}

/// ParseToMap parses the user input to an optimizer fix control map.
///
/// v8.5.8 indexes `s[0]` after the value's colon without a length check, so `"1:"` panics
/// (index out of range). Upstream fixed it on master (pingcap/tidb#65884, not backported to
/// release-8.5): an empty value after the colon is the value `""`, no error, no warning. The
/// fix is ported (constitution R1, as #67149); govec generates these records with the fix
/// applied (tracker `scripts/govec-fixes.sh`).
// go-dep: github.com/pingcap/tidb/pkg/planner/util/fixcontrol.ParseToMap L1
// go-fix: pingcap/tidb@811a10e115 #65884
pub fn parse_to_map(s: &str) -> Result<(HashMap<u64, String>, Vec<String>), Error> {
    let mut m: HashMap<u64, String> = HashMap::new();
    let mut warn_msgs = Vec::new();
    let mut s = s;
    while !s.is_empty() {
        // find the colon
        let Some(colon_idx) = s.find(':') else {
            return Err(tidb_base::errors::new(
                "invalid fix control: expected colon not found",
            ));
        };
        // 1. the part before the colon is the fix control number and also the key in the map
        let key_str = s[..colon_idx].trim();
        let key = super::session_hooks::go_parse_uint(key_str)?;
        // 2. the part after the colon is the value set for this fix
        s = s[colon_idx + 1..].trim();
        let mut value = "";
        // if it starts with quote, we need to find the closing quote, and the content between is the value for this fix
        // NOTE: s can be empty here for inputs like "123:", so guard the index access.
        // (go-fix pingcap/tidb#65884: v8.5.8 indexes s[0] unguarded and panics.)
        let first = s.as_bytes().first().copied();
        if let Some(first @ (b'\'' | b'"')) = first {
            let quote = first as char;
            let Some(end_idx) = s[1..].find(quote) else {
                return Err(tidb_base::errors::new(
                    "invalid fix control: expected quote not found",
                ));
            };
            let end_idx = end_idx + 1;
            value = &s[1..end_idx];
            s = &s[end_idx + 1..];
        }
        // if there's no comma, the remaining content belong to this fix
        let mut end_idx = s.len();
        let mut next_start_idx = s.len();
        if let Some(comma_idx) = s.find(',') {
            end_idx = comma_idx;
            next_start_idx = comma_idx + 1;
        }
        // if there's no quote, get the value for this fix now.
        if value.is_empty() {
            value = s[..end_idx].trim();
        }
        // 3. key and value are found, now handling repeated key and set into map
        if let Some(original_value) = m.get(&key)
            && original_value != value
        {
            warn_msgs.push(format!(
                "repeated assignment for fix control: {key}. existing value: {}. new value: {}.",
                go_quote(original_value),
                go_quote(value)
            ));
        }
        m.insert(key, value.to_owned());
        // 4. prepare to parse remaining content
        s = s[next_start_idx..].trim();
    }
    Ok((m, warn_msgs))
}

/// Go's `%q` of a string.
// glue: fmt %q
fn go_quote(s: &str) -> String {
    let out = tidb_base::gofmt::sprintf(
        "%q",
        &[tidb_base::gofmt::Arg::go_string(s.as_bytes().to_vec())],
    );
    String::from_utf8_lossy(&out).into_owned()
}

/// `sessionstates.QueryInfo`: the information of the last executed query.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueryInfo {
    /// TxnScope of the query's transaction.
    pub txn_scope: String,
    /// StartTS of the query's transaction.
    pub start_ts: u64,
    /// ForUpdateTS of the query.
    pub for_update_ts: u64,
    /// RUConsumption of the query.
    pub ru_consumption: f64,
    /// ErrMsg is the error message (`omitempty`).
    pub err_msg: String,
}

impl QueryInfo {
    /// `json.Marshal(QueryInfo)`.
    // glue: encoding/json of sessionstates.QueryInfo (field order and tags of the Go struct)
    pub fn marshal_json(&self) -> String {
        use tidb_base::gostd::json::{Value, encode_value};
        let mut out = String::from("{\"txn_scope\":");
        let _ = encode_value(&mut out, &Value::String(self.txn_scope.clone()));
        out.push_str(&format!(
            ",\"start_ts\":{},\"for_update_ts\":{},\"ru_consumption\":",
            self.start_ts, self.for_update_ts
        ));
        let _ = encode_value(&mut out, &Value::Number(self.ru_consumption));
        if !self.err_msg.is_empty() {
            out.push_str(",\"error\":");
            let _ = encode_value(&mut out, &Value::String(self.err_msg.clone()));
        }
        out.push('}');
        out
    }
}

/// `sessionstates.LastDDLInfo`: the information of the last DDL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LastDDLInfo {
    /// Query of the DDL.
    pub query: String,
    /// SeqNum of the DDL.
    pub seq_num: u64,
}

impl LastDDLInfo {
    /// `json.Marshal(LastDDLInfo)`.
    // glue: encoding/json of sessionstates.LastDDLInfo (field order and tags of the Go struct)
    pub fn marshal_json(&self) -> String {
        use tidb_base::gostd::json::{Value, encode_value};
        let mut out = String::from("{\"query\":");
        let _ = encode_value(&mut out, &Value::String(self.query.clone()));
        out.push_str(&format!(",\"seq_num\":{}}}", self.seq_num));
        out
    }
}

pub use super::slow_log_rules::SlowLogRules;

/// `slowlogrule.SessionSlowLogRules`.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSlowLogRules {
    /// The session rules (`*SlowLogRules`, nil until tidb_slow_log_rules is set).
    pub slow_log_rules: Option<SlowLogRules>,
    /// NeedUpdateEffectiveFields indicates whether EffectiveFields needs to be updated.
    pub need_update_effective_fields: bool,
}

impl Default for SessionSlowLogRules {
    // go-dep: github.com/pingcap/tidb/pkg/sessionctx/slowlogrule.NewSessionSlowLogRules L1 (nil rules)
    fn default() -> Self {
        SessionSlowLogRules {
            slow_log_rules: None,
            need_update_effective_fields: true,
        }
    }
}

/// The store types of a `map[kv.StoreType]struct{}` as their names, sorted (for dumps).
// glue: debug form of a store-type set
pub fn store_type_names(set: &BTreeSet<StoreType>) -> String {
    let mut v: Vec<&str> = set.iter().map(|t| t.name()).collect();
    v.sort_unstable();
    v.join(",")
}
