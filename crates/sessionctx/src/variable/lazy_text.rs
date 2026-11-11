//! go-file: pkg/sessionctx/variable/session.go
//!
//! `LazyStmtText`, the parse/plan/execute duration helpers, the parser configuration and the
//! session statement caches of `SessionVars` (block D05.2, PR F).

use tidb_base::gostd::time::{self as gotime, Duration, Time};

use super::names::CharacterSetClient;
use super::plan_cache_params::PlanCacheParamList;
use super::session::{SessionVars, StmtCacheValue};
use super::session_methods::PlanCacheStmtKey;
use crate::kvcache::new_simple_lru_cache;

/// The formatter of [`LazyStmtText`] (Go `func(string) string` on Go string bytes).
pub type LazyTextFormat = Box<dyn Fn(Vec<u8>) -> Vec<u8> + Send + Sync>;

/// LazyStmtText represents the sql text of a stmt that used in log. It's lazily evaluated to
/// reduce the mem allocs. Texts are Go string bytes.
#[derive(Default)]
pub struct LazyStmtText {
    text: Option<Vec<u8>>,
    /// SQL is the statement text.
    pub sql: Vec<u8>,
    /// Redact is the `tidb_redact_log` mode applied by `String`.
    pub redact: String,
    /// Params are the statement's parameters.
    pub params: PlanCacheParamList,
    /// Format post-processes the text.
    pub format: Option<LazyTextFormat>,
}

impl std::fmt::Debug for LazyStmtText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyStmtText")
            .field("text", &self.text)
            .field("sql", &self.sql)
            .field("redact", &self.redact)
            .finish_non_exhaustive()
    }
}

impl LazyStmtText {
    /// SetText sets the text directly.
    // go: LazyStmtText.SetText L1
    pub fn set_text(&mut self, text: &[u8]) {
        self.text = Some(text.to_vec());
    }

    /// Update resets the lazy text and leads to re-eval for next `s.String()`. It copies
    /// params so it's safe to use `SessionVars.PlanCacheParams` directly without worrying
    /// about the params get reset later.
    // go: LazyStmtText.Update L1
    pub fn update(&mut self, redact: &str, sql: &[u8], params: Option<&PlanCacheParamList>) {
        self.text = None;
        self.sql = sql.to_vec();
        self.redact = redact.to_owned();
        self.params.reset();
        if let Some(params) = params {
            self.params.set_for_non_prep_cache(params.for_non_prep_cache());
            self.params.append(params.all_param_values());
        }
    }

    /// String implements fmt.Stringer. `None` (Go's nil receiver) gives "".
    // go: LazyStmtText.String L1
    pub fn string(this: Option<&mut LazyStmtText>) -> Vec<u8> {
        let Some(s) = this else {
            return Vec::new();
        };
        if s.text.is_none() {
            let mut input = s.sql.clone();
            input.extend_from_slice(&s.params.string());
            let mut text = tidb_base::redact::string_bytes(&s.redact, &input);
            if let Some(format) = &s.format {
                text = format(text);
            }
            s.text = Some(text);
        }
        s.text.clone().unwrap_or_default()
    }
}

/// `parser.ParserConfig`, as built by [`SessionVars::build_parser_config`].
// glue: carrier of pkg/parser ParserConfig (the Rust parser takes `ParseOptions`)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParserConfig {
    /// EnableWindowFunction.
    pub enable_window_function: bool,
    /// EnableStrictDoubleTypeCheck.
    pub enable_strict_double_type_check: bool,
    /// SkipPositionRecording.
    pub skip_position_recording: bool,
}

/// `parser.ParseParam`: the charset parameters `GetParseParams` returns.
// glue: carrier of pkg/parser ParseParam (CharsetConnection, CollationConnection, CharsetClient)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseParam {
    /// `parser.CharsetConnection`.
    CharsetConnection(String),
    /// `parser.CollationConnection`.
    CollationConnection(String),
    /// `parser.CharsetClient`.
    CharsetClient(String),
}

impl SessionVars {
    /// BuildParserConfig generate parser.ParserConfig for initial parser
    // go: SessionVars.BuildParserConfig L1
    pub fn build_parser_config(&self) -> ParserConfig {
        ParserConfig {
            enable_window_function: self.enable_window_function,
            enable_strict_double_type_check: self.enable_strict_double_type_check,
            skip_position_recording: true,
        }
    }

    /// GetParseParams gets the parse parameters from session variables.
    // go: SessionVars.GetParseParams L1
    pub fn get_parse_params(&mut self) -> Vec<ParseParam> {
        let (chs, coll) = self.get_charset_info();
        let cli = self
            .get_session_or_global_system_var(CharacterSetClient)
            .unwrap_or_default();
        vec![
            ParseParam::CharsetConnection(chs),
            ParseParam::CollationConnection(coll),
            ParseParam::CharsetClient(cli),
        ]
    }

    /// GetTotalCostDuration returns the total cost duration of the last statement in the
    /// current session.
    // go: SessionVars.GetTotalCostDuration L1 (time.Since(s.StartTime) is `now - StartTime`;
    // `_at` takes `now` so tests do not read the clock)
    pub fn get_total_cost_duration(&self) -> Duration {
        self.get_total_cost_duration_at(&gotime::now())
    }

    /// [`Self::get_total_cost_duration`] at `now`.
    // glue: clock injection for GetTotalCostDuration
    pub fn get_total_cost_duration_at(&self, now: &Time) -> Duration {
        now.sub(&self.start_time).wrapping_add(self.duration_parse)
    }

    /// GetExecuteDuration returns the execute duration of the last statement in the current
    /// session.
    // go: SessionVars.GetExecuteDuration L1 (see GetTotalCostDuration)
    pub fn get_execute_duration(&self) -> Duration {
        self.get_execute_duration_at(&gotime::now())
    }

    /// [`Self::get_execute_duration`] at `now`.
    // glue: clock injection for GetExecuteDuration
    pub fn get_execute_duration_at(&self, now: &Time) -> Duration {
        now.sub(&self.start_time).wrapping_sub(self.duration_compile)
    }

    /// AddNonPreparedPlanCacheStmt adds this PlanCacheStmt into non-preapred plan-cache stmt
    /// cache
    // go: SessionVars.AddNonPreparedPlanCacheStmt L1 (panics like Go when
    // SessionPlanCacheSize is 0: NewSimpleLRUCache requires a positive capacity)
    pub fn add_non_prepared_plan_cache_stmt(&mut self, sql: &[u8], stmt: StmtCacheValue) {
        let size = self.session_plan_cache_size as usize;
        self.non_prepared_plan_cache_stmts
            .get_or_insert_with(|| new_simple_lru_cache(size, 0.0, 0))
            .put(PlanCacheStmtKey(sql.to_vec()), stmt);
    }

    /// GetNonPreparedPlanCacheStmt gets the PlanCacheStmt.
    // go: SessionVars.GetNonPreparedPlanCacheStmt L1
    pub fn get_non_prepared_plan_cache_stmt(&mut self, sql: &[u8]) -> Option<StmtCacheValue> {
        self.non_prepared_plan_cache_stmts
            .as_mut()?
            .get(&PlanCacheStmtKey(sql.to_vec()))
            .cloned()
    }

    /// GetPrepareStmtDedupCache returns the cached PrepareStmtCacheEntry for the given key,
    /// or nil when the cache is empty or the key is not found.
    // go: SessionVars.GetPrepareStmtDedupCache L1
    pub fn get_prepare_stmt_dedup_cache(&mut self, key: &[u8]) -> Option<StmtCacheValue> {
        self.prepare_stmt_dedup_cache
            .as_mut()?
            .get(&PlanCacheStmtKey(key.to_vec()))
            .cloned()
    }

    /// SetPrepareStmtDedupCache stores a PrepareStmtCacheEntry under the given key.
    /// The cache is lazily initialized and bounded by SessionPlanCacheSize (LRU eviction).
    // go: SessionVars.SetPrepareStmtDedupCache L1 (see AddNonPreparedPlanCacheStmt)
    pub fn set_prepare_stmt_dedup_cache(&mut self, key: &[u8], val: StmtCacheValue) {
        let size = self.session_plan_cache_size as usize;
        self.prepare_stmt_dedup_cache
            .get_or_insert_with(|| new_simple_lru_cache(size, 0.0, 0))
            .put(PlanCacheStmtKey(key.to_vec()), val);
    }
}
