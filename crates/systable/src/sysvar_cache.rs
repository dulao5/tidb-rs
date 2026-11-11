//! go-file: pkg/domain/sysvar_cache.go
//!
//! The domain's sysvar cache: the session and global values of every registered variable,
//! rebuilt from `mysql.global_variables` (at start, on the etcd notification, every 30 s),
//! running each SetGlobal hook on the way so that a `SET GLOBAL` on another instance takes
//! effect here too (spec §6.1 "读与同步").
//!
//! The table scan is the caller's: [`decode_global_variables`] turns the scanned record
//! key-values into the name → value map Go's `fetchTableValues` builds, and
//! [`SysVarCache::rebuild`] does the rest of `rebuildSysVarCache`.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gostd::time::utc;
use tidb_model::TableInfo;
use tidb_parser::types::FieldType;
use tidb_sessionctx::variable::error::ErrUnknownSystemVar;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_sessionctx::variable::variable::{ScopeGlobal, get_sys_vars};
use tidb_tablecodec::tablecodec as tc;

/// sysVarCache represents the cache of system variables broken up into session and global
/// scope.
// glue: sysVarCache (type)
#[derive(Default)]
pub struct SysVarCache {
    /// protects global and session maps
    maps: RwLock<Maps>,
    /// protects concurrent rebuild
    rebuild_lock: Mutex<()>,
}

#[derive(Default, Clone)]
struct Maps {
    global: Arc<HashMap<String, String>>,
    session: Arc<HashMap<String, String>>,
}

impl SysVarCache {
    /// A cache that was never built (Go's zero value: `rebuildSysVarCacheIfNeeded` builds it
    /// on first use).
    // glue: zero value of sysVarCache
    pub fn new() -> SysVarCache {
        SysVarCache::default()
    }

    /// Whether either map is empty (`rebuildSysVarCacheIfNeeded`'s condition; the caller
    /// then rebuilds, since only it can scan the table).
    // go: Domain.rebuildSysVarCacheIfNeeded L2 (returns the condition; the caller, which can scan the table, rebuilds)
    pub fn needs_rebuild(&self) -> bool {
        let m = self.maps.read();
        m.session.is_empty() || m.global.is_empty()
    }

    /// GetSessionCache gets a copy of the session sysvar cache.
    /// The intention is to copy it directly to the systems[] map
    /// on creating a new session.
    // go-part: Domain.GetSessionCache L1 (after the caller's rebuildSysVarCacheIfNeeded)
    pub fn session_cache(&self) -> HashMap<String, String> {
        (*self.maps.read().session).clone()
    }

    /// GetGlobalVar gets an individual global var from the sysvar cache.
    // go-part: Domain.GetGlobalVar L1 (after the caller's rebuildSysVarCacheIfNeeded)
    pub fn global_var(&self, name: &str) -> Result<String, Error> {
        if let Some(v) = self.maps.read().global.get(name) {
            return Ok(v.clone());
        }
        Err(ErrUnknownSystemVar.gen_with_stack_by_args(vec![name.to_owned().into()]))
    }

    /// rebuildSysVarCache rebuilds the sysvar cache both globally and for session vars.
    /// It needs to be called when sysvars are added or removed.
    ///
    /// `fetch` is `fetchTableValues` (run under the rebuild lock, as Go does, so that an
    /// earlier fetch cannot finish last); `ctx` is the session the SetGlobal hooks run on.
    /// A hook error is logged by Go and does not stop the rebuild: the errors are returned
    /// for the caller to log.
    // go: Domain.rebuildSysVarCache L2 (the fetch and the hook session are the caller's; infoCache.ReSize is D06.3)
    pub fn rebuild(
        &self,
        ctx: &mut SessionVars,
        fetch: &mut dyn FnMut() -> Result<HashMap<String, String>, Error>,
    ) -> Result<Vec<(String, Error)>, Error> {
        let mut new_session_cache = HashMap::new();
        let mut new_global_cache = HashMap::new();
        // Only one rebuild can be in progress at a time, this prevents a lost update race
        // where an earlier fetchTableValues() finishes last.
        let _rebuild = self.rebuild_lock.lock();
        let table_contents = fetch()?;
        let mut hook_errors = Vec::new();
        for sv in get_sys_vars() {
            let mut s_val = sv.value.clone();
            if let Some(v) = table_contents.get(sv.name) {
                s_val = v.clone();
            }
            // session cache stores non-skippable variables, which essentially means session scope.
            // for historical purposes there are some globals, but these should eventually be removed.
            if !sv.skip_init() {
                new_session_cache.insert(sv.name.to_owned(), s_val.clone());
            }
            if sv.has_global_scope() {
                new_global_cache.insert(sv.name.to_owned(), s_val.clone());
                // Call the SetGlobal func for this sysvar if it exists.
                // SET GLOBAL only calls the SetGlobal func on the calling instances.
                // This ensures it is run on all tidb servers.
                // This does not apply to INSTANCE scoped vars (HasGlobalScope() is false)
                if let Some(set_global) = sv.set_global
                    && !sv.skip_sysvar_cache()
                {
                    s_val = sv.validate_with_relaxed_validation(ctx, &s_val, ScopeGlobal);
                    if let Err(e) = set_global(ctx, &s_val) {
                        // logutil.BgLogger().Error("load global variable %s error")
                        hook_errors.push((sv.name.to_owned(), e));
                    }
                }
            }
        }
        let mut m = self.maps.write();
        m.session = Arc::new(new_session_cache);
        m.global = Arc::new(new_global_cache);
        Ok(hook_errors)
    }
}

/// `fetchTableValues` over the scanned records of `mysql.global_variables` (record key,
/// value): `SELECT variable_name, variable_value FROM mysql.global_variables`, a NULL value
/// read as "" (`row.GetString`).
// go: Domain.fetchTableValues L2 (decodes the records the caller scanned instead of running SELECT)
pub fn decode_global_variables(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
) -> Result<HashMap<String, String>, Error> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let (Some(name), Some(value)) = (
        cols.iter().find(|c| c.name.l == "variable_name"),
        cols.iter().find(|c| c.name.l == "variable_value"),
    ) else {
        return Err(errors::new(
            "mysql.global_variables has no variable_name / variable_value column",
        ));
    };
    let types: HashMap<i64, Arc<FieldType>> = [name, value]
        .iter()
        .map(|c| (c.id, Arc::new(c.field_type.clone())))
        .collect();
    let loc = utc();
    let mut out = HashMap::with_capacity(records.len());
    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let m = tc::decode_row_to_datum_map(v, &types, Some(&loc))?;
        let get = |id: i64| {
            m.get(&id)
                .filter(|d| !d.is_null())
                .map(|d| String::from_utf8_lossy(d.get_bytes()).into_owned())
                .unwrap_or_default()
        };
        out.insert(get(name.id), get(value.id));
    }
    Ok(out)
}
