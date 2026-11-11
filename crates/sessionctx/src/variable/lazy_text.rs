//! go-file: pkg/sessionctx/variable/session.go
//!
//! `LazyStmtText`, the parse/plan/execute duration helpers, the parser configuration, the
//! session statement caches and the session-state encoding of `SessionVars` (block D05.2,
//! PR F).

use tidb_base::gostd::time::{self as gotime, Duration, Time};

use super::names::CharacterSetClient;
use super::plan_cache_params::PlanCacheParamList;
use super::session::{SessionVars, StmtCacheValue};
use super::session_methods::PlanCacheStmtKey;
use crate::kvcache::new_simple_lru_cache;
use crate::sessionstates::SessionStates;
use tidb_base::Error;

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
    // glue: Debug
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
            self.params
                .set_for_non_prep_cache(params.for_non_prep_cache());
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
    // Note: time.Since(s.StartTime) is `now - StartTime`; `_at` takes `now` so tests do not
    // read the clock.
    // go: SessionVars.GetTotalCostDuration L1
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
    // go: SessionVars.GetExecuteDuration L1
    pub fn get_execute_duration(&self) -> Duration {
        self.get_execute_duration_at(&gotime::now())
    }

    /// [`Self::get_execute_duration`] at `now`.
    // glue: clock injection for GetExecuteDuration
    pub fn get_execute_duration_at(&self, now: &Time) -> Duration {
        now.sub(&self.start_time)
            .wrapping_sub(self.duration_compile)
    }

    /// AddNonPreparedPlanCacheStmt adds this PlanCacheStmt into non-preapred plan-cache stmt
    /// cache
    // Note: panics like Go when SessionPlanCacheSize is 0 (NewSimpleLRUCache requires a
    // positive capacity).
    // go: SessionVars.AddNonPreparedPlanCacheStmt L1
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
    // go: SessionVars.SetPrepareStmtDedupCache L1
    pub fn set_prepare_stmt_dedup_cache(&mut self, key: &[u8], val: StmtCacheValue) {
        let size = self.session_plan_cache_size as usize;
        self.prepare_stmt_dedup_cache
            .get_or_insert_with(|| new_simple_lru_cache(size, 0.0, 0))
            .put(PlanCacheStmtKey(key.to_vec()), val);
    }
}

impl SessionVars {
    /// EncodeSessionStates saves session states into SessionStates.
    // Note: no context argument (unused in Go).
    // go: SessionVars.EncodeSessionStates L1
    pub fn encode_session_states(&self, session_states: &mut SessionStates) -> Result<(), Error> {
        // Encode user-defined variables.
        let (values, types) = self.state.user_vars.snapshot();
        session_states.user_vars = Some(values);
        session_states.user_var_types = Some(types);

        // Encode other session contexts.
        session_states.prepared_stmt_id = self.prepared_stmt_id;
        session_states.status = self.state.status;
        session_states.current_db = self.current_db.clone();
        session_states.last_txn_info = self.last_txn_info.clone();
        if self.last_query_info.start_ts != 0 {
            session_states.last_query_info = Some(self.last_query_info.clone());
        }
        if self.last_ddl_info.seq_num != 0 {
            session_states.last_ddl_info = Some(self.last_ddl_info.clone());
        }
        session_states.last_found_rows = self.last_found_rows;
        session_states.sequence_latest_values = Some(self.sequence_state.get_all_states());
        session_states.found_in_plan_cache = self.prev_found_in_plan_cache;
        session_states.found_in_binding = self.prev_found_in_binding;
        session_states.resource_group_name = self.resource_group_name.clone();
        session_states.hypo_indexes = self.hypo_indexes.clone();
        session_states.hypo_tiflash_replicas = self.hypo_tiflash_replicas.clone();

        // Encode StatementContext. We encode it here to avoid circle dependency.
        let sc = self.stmt_ctx();
        session_states.last_affected_rows = sc.prev_affected_rows;
        session_states.last_insert_id = sc.prev_last_insert_id;
        session_states.warnings = sc.get_warnings();
        Ok(())
    }

    /// DecodeSessionStates restores session states from SessionStates.
    // Note: no context argument (unused in Go). Go ranges over the maps; the order is not
    // observable (each entry is set independently).
    // go: SessionVars.DecodeSessionStates L1
    pub fn decode_session_states(&mut self, session_states: &SessionStates) -> Result<(), Error> {
        // Decode user-defined variables.
        for (name, user_var) in session_states.user_vars.iter().flatten() {
            self.set_user_var_val(name, user_var.clone());
        }
        for (name, user_var_type) in session_states.user_var_types.iter().flatten() {
            self.state
                .user_vars
                .set_user_var_type(name, user_var_type.clone());
        }

        // Decode other session contexts.
        self.prepared_stmt_id = session_states.prepared_stmt_id;
        self.state.status = session_states.status;
        self.current_db = session_states.current_db.clone();
        self.last_txn_info = session_states.last_txn_info.clone();
        if let Some(q) = &session_states.last_query_info {
            self.last_query_info = q.clone();
        }
        if let Some(d) = &session_states.last_ddl_info {
            self.last_ddl_info = d.clone();
        }
        self.last_found_rows = session_states.last_found_rows;
        if let Some(m) = &session_states.sequence_latest_values {
            self.sequence_state.set_all_states(m);
        }
        self.found_in_plan_cache = session_states.found_in_plan_cache;
        self.found_in_binding = session_states.found_in_binding;
        self.set_resource_group_name(&session_states.resource_group_name);
        self.hypo_indexes = session_states.hypo_indexes.clone();
        self.hypo_tiflash_replicas = session_states.hypo_tiflash_replicas.clone();

        // Decode StatementContext.
        let sc = self.stmt_ctx();
        sc.set_affected_rows(session_states.last_affected_rows as u64);
        self.stmt_ctx_mut().prev_last_insert_id = session_states.last_insert_id;
        self.stmt_ctx()
            .set_warnings(session_states.warnings.clone());
        Ok(())
    }
}
