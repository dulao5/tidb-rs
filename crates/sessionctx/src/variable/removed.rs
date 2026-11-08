//! go-file: pkg/sessionctx/variable/removed.go
//!
//! Variables that have been removed: SET on them succeeds silently (executor) and
//! `CheckSysVarIsRemoved` explains why.

use tidb_base::Error;

use super::error::ErrVariableNoLongerSupported;

/// removedSysVars: name -> reason.
#[allow(non_upper_case_globals)]
static removedSysVars: &[(&str, &str)] = &[
    ("tidb_enable_alter_placement", "alter placement is now always enabled"),
    ("tidb_enable_global_temporary_table", "temporary table support is now always enabled"),
    ("tidb_slow_log_masking", "use tidb_redact_log instead"),
    ("placement_checks", "placement_checks is removed and use tidb_placement_mode instead"),
    ("tidb_mem_quota_hashjoin", "use tidb_mem_quota_query instead"),
    ("tidb_mem_quota_mergejoin", "use tidb_mem_quota_query instead"),
    ("tidb_mem_quota_sort", "use tidb_mem_quota_query instead"),
    ("tidb_mem_quota_topn", "use tidb_mem_quota_query instead"),
    ("tidb_mem_quota_indexlookupreader", "use tidb_mem_quota_query instead"),
    ("tidb_mem_quota_indexlookupjoin", "use tidb_mem_quota_query instead"),
    ("tidb_enable_streaming", "streaming is no longer supported"),
    ("tidb_opt_broadcast_join", "tidb_opt_broadcast_join is removed and use tidb_allow_mpp instead"),
    ("tidb_enable_change_multi_schema", "alter multiple schema objects in a table is now always enabled"),
];

#[allow(non_upper_case_globals)]
const _: () = ();

fn reason(var_name: &str) -> Option<&'static str> {
    removedSysVars.iter().find(|(n, _)| *n == var_name).map(|(_, r)| *r)
}

/// IsRemovedSysVar returns true if the sysvar has been removed
// go: IsRemovedSysVar L1
pub fn is_removed_sys_var(var_name: &str) -> bool {
    reason(var_name).is_some()
}

/// CheckSysVarIsRemoved returns an error if the sysvar has been removed
// go: CheckSysVarIsRemoved L1
pub fn check_sys_var_is_removed(var_name: &str) -> Result<(), Error> {
    if let Some(reason) = reason(var_name) {
        return Err(ErrVariableNoLongerSupported.gen_with_stack_by_args(vec![var_name.to_owned().into(), reason.into()]));
    }
    Ok(())
}
