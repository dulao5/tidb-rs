//! go-file: pkg/privilege/privileges/privileges.go
//!
//! Faithful port of Go TiDB v8.5.8 `UserPrivileges` implementing [`PrivilegeManager`], including
//! memory-database rules (`information_schema`, `performance_schema`, `metrics_schema`).

#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::mysql::{
    AlterPriv, CreatePriv, CreateViewPriv, DeletePriv, DropPriv, ExecutePriv, IndexPriv,
    InsertPriv, LockTablesPriv, PrivilegeType, ProcessPriv, ReferencesPriv, SelectPriv,
    ShowViewPriv, UpdatePriv,
};
use tidb_parser::auth::{RoleIdentity, UserIdentity};

use crate::PrivilegeManager;
use crate::cache::Handle;

const INFORMATION_SCHEMA_LOWER: &str = "information_schema";
const PERFORMANCE_SCHEMA_LOWER: &str = "performance_schema";
const METRICS_SCHEMA_LOWER: &str = "metrics_schema";

/// Whether `db_lower` is one of TiDB's three memory databases (`util.IsMemDB`).
// glue: matches tidb_infoschema::metadef::is_mem_db without inverting rank 8 -> rank 9
fn is_mem_db(db_lower: &str) -> bool {
    matches!(
        db_lower,
        INFORMATION_SCHEMA_LOWER | PERFORMANCE_SCHEMA_LOWER | METRICS_SCHEMA_LOWER
    )
}

/// Callback checking whether a table in `metrics_schema` is a metric table
/// (`infoschema.IsMetricTable`).
pub type IsMetricTableFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// UserPrivileges implements [`PrivilegeManager`] for a session bound to `(user, host)` backed
/// by a live [`Handle`].
pub struct UserPrivileges {
    pub user: String,
    pub host: String,
    pub handle: Arc<Handle>,
    pub skip_with_grant: bool,
    pub is_metric_table: Option<IsMetricTableFn>,
}

impl UserPrivileges {
    /// Creates a `UserPrivileges` handle for `(user, host)`.
    // go: NewUserPrivileges L2 (no global SkipWithGrant flag or extension hooks yet)
    pub fn new(user: impl Into<String>, host: impl Into<String>, handle: Arc<Handle>) -> Self {
        Self {
            user: user.into(),
            host: host.into(),
            handle,
            skip_with_grant: false,
            is_metric_table: None,
        }
    }
}

impl PrivilegeManager for UserPrivileges {
    // go: UserPrivileges.RequestVerification L2 (SEM / extensionAccessCheckFuncs / authPlugin hook not ported yet)
    fn request_verification(
        &self,
        active_roles: &[RoleIdentity],
        db: &str,
        table: &str,
        column: &str,
        mut priv_: PrivilegeType,
    ) -> bool {
        if self.skip_with_grant {
            return true;
        }
        if self.user.is_empty() && self.host.is_empty() {
            return true;
        }

        let db_lower = db.to_ascii_lowercase();
        if is_mem_db(&db_lower) {
            if matches!(
                priv_,
                CreatePriv
                    | AlterPriv
                    | DropPriv
                    | IndexPriv
                    | CreateViewPriv
                    | InsertPriv
                    | UpdatePriv
                    | DeletePriv
                    | ReferencesPriv
                    | ExecutePriv
                    | ShowViewPriv
                    | LockTablesPriv
            ) {
                return false;
            }
            if db_lower == INFORMATION_SCHEMA_LOWER {
                return true;
            } else if db_lower == METRICS_SCHEMA_LOWER {
                // PROCESS is the same as SELECT for metrics_schema metric tables.
                let is_metric = self
                    .is_metric_table
                    .as_ref()
                    .is_none_or(|check| check(table));
                if priv_ == SelectPriv && is_metric {
                    priv_ = PrivilegeType(priv_.0 | ProcessPriv.0);
                }
            }
        }

        let mysql_priv = self.handle.get();
        mysql_priv.request_verification(
            active_roles,
            &self.user,
            &self.host,
            db,
            table,
            column,
            priv_,
        )
    }

    // go: UserPrivileges.RequestVerificationWithUser L2 (SEM not ported yet)
    fn request_verification_with_user(
        &self,
        db: &str,
        table: &str,
        column: &str,
        priv_: PrivilegeType,
        user: Option<&UserIdentity>,
    ) -> bool {
        if self.skip_with_grant {
            return true;
        }
        let Some(user) = user else {
            return false;
        };
        if user.username.is_empty() && user.hostname.is_empty() {
            return true;
        }
        if db.eq_ignore_ascii_case(INFORMATION_SCHEMA_LOWER) {
            return true;
        }
        let mysql_priv = self.handle.get();
        let roles = mysql_priv.get_default_roles(&user.username, &user.hostname);
        mysql_priv.request_verification(
            &roles,
            &user.username,
            &user.hostname,
            db,
            table,
            column,
            priv_,
        )
    }

    // go: UserPrivileges.RequestDynamicVerification L2 (authPlugin hook not ported yet)
    fn request_dynamic_verification(
        &self,
        active_roles: &[RoleIdentity],
        priv_name: &str,
        grantable: bool,
    ) -> bool {
        if self.skip_with_grant {
            return true;
        }
        if self.user.is_empty() && self.host.is_empty() {
            return true;
        }
        let mysql_priv = self.handle.get();
        mysql_priv.request_dynamic_verification(
            active_roles,
            &self.user,
            &self.host,
            priv_name,
            grantable,
        )
    }

    // go: UserPrivileges.RequestDynamicVerificationWithUser L1
    fn request_dynamic_verification_with_user(
        &self,
        priv_name: &str,
        grantable: bool,
        user: Option<&UserIdentity>,
    ) -> bool {
        if self.skip_with_grant {
            return true;
        }
        let Some(user) = user else {
            return false;
        };
        if user.username.is_empty() && user.hostname.is_empty() {
            return true;
        }
        let mysql_priv = self.handle.get();
        let roles = mysql_priv.get_default_roles(&user.username, &user.hostname);
        mysql_priv.request_dynamic_verification(
            &roles,
            &user.username,
            &user.hostname,
            priv_name,
            grantable,
        )
    }

    // go: UserPrivileges.DBIsVisible L1
    fn db_is_visible(&self, active_roles: &[RoleIdentity], db: &str) -> bool {
        if self.skip_with_grant {
            return true;
        }
        let mysql_priv = self.handle.get();
        if mysql_priv.db_is_visible(&self.user, &self.host, db) {
            return true;
        }
        let all_roles =
            mysql_priv.find_all_user_effective_roles(&self.user, &self.host, active_roles);
        for role in &all_roles {
            if mysql_priv.db_is_visible(&role.username, &role.hostname, db) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{ColumnsPrivRecord, DbRecord, MySQLPrivilege, TablesPrivRecord, UserRecord};

    #[test]
    fn match_columns_star_skips_non_select_records_and_finds_select_column() {
        let mut mp = MySQLPrivilege::new();
        mp.add_user(UserRecord::new("%", "u_mixed", PrivilegeType(0)));
        // First column record is UPDATE(a), second is SELECT(b): "*" must skip UPDATE(a) and match SELECT(b).
        mp.add_column_priv(ColumnsPrivRecord::new(
            "u_mixed", "%", "test", "t", "a", UpdatePriv,
        ));
        mp.add_column_priv(ColumnsPrivRecord::new(
            "u_mixed", "%", "test", "t", "b", SelectPriv,
        ));
        let handle = Arc::new(Handle::new(mp));
        let up = UserPrivileges::new("u_mixed", "%", handle);

        assert!(up.request_verification(&[], "test", "t", "*", SelectPriv));
        assert!(up.request_verification(&[], "test", "t", "b", SelectPriv));
        assert!(!up.request_verification(&[], "test", "t", "a", SelectPriv));
        // Empty column requires table/db/global SELECT, never satisfied by column-only SELECT(b).
        assert!(!up.request_verification(&[], "test", "t", "", SelectPriv));
    }

    #[test]
    fn memory_database_rules_match_go() {
        let mut mp = MySQLPrivilege::new();
        mp.add_user(UserRecord::new("%", "u_none", PrivilegeType(0)));
        mp.add_user(UserRecord::new("%", "u_proc", ProcessPriv));
        mp.add_table_priv(TablesPrivRecord::new(
            "u_proc",
            "%",
            "performance_schema",
            "threads",
            SelectPriv,
        ));
        mp.add_db(DbRecord::new("u_db_ins", "%", "test", InsertPriv));
        let handle = Arc::new(Handle::new(mp));

        let u_none = UserPrivileges::new("u_none", "%", Arc::clone(&handle));
        assert!(u_none.request_verification(&[], "information_schema", "tables", "*", SelectPriv));
        assert!(!u_none.request_verification(&[], "information_schema", "tables", "", InsertPriv));
        assert!(!u_none.request_verification(
            &[],
            "performance_schema",
            "threads",
            "*",
            SelectPriv
        ));
        assert!(!u_none.request_verification(&[], "metrics_schema", "tidb_qps", "*", SelectPriv));

        let u_proc = UserPrivileges::new("u_proc", "%", Arc::clone(&handle));
        assert!(u_proc.request_verification(&[], "metrics_schema", "tidb_qps", "*", SelectPriv));
        assert!(u_proc.request_verification(&[], "performance_schema", "threads", "*", SelectPriv));

        let u_db_ins = UserPrivileges::new("u_db_ins", "%", handle);
        assert!(!u_db_ins.request_verification(&[], "test", "nosuch", "*", SelectPriv));
        assert!(u_db_ins.request_verification(&[], "test", "nosuch", "", InsertPriv));
    }

    #[test]
    fn effective_roles_and_live_revoke() {
        let mut mp = MySQLPrivilege::new();
        mp.add_user(UserRecord::new("%", "u1", PrivilegeType(0)));
        mp.add_user(UserRecord::new("%", "r_reader", PrivilegeType(0)));
        mp.add_table_priv(TablesPrivRecord::new(
            "r_reader", "%", "test", "t", SelectPriv,
        ));
        mp.add_role_edge("r_reader", "%", "u1", "%");

        let handle = Arc::new(Handle::new(mp));
        let up = UserPrivileges::new("u1", "%", Arc::clone(&handle));
        let role = RoleIdentity {
            username: "r_reader".to_owned(),
            hostname: "%".to_owned(),
        };
        assert!(!up.request_verification(&[], "test", "t", "*", SelectPriv));
        assert!(up.request_verification(std::slice::from_ref(&role), "test", "t", "*", SelectPriv));

        // Live revoke via Handle::replace takes effect immediately.
        let mut revoked = (*handle.get()).clone();
        revoked.revoke_user_all_privs("r_reader");
        handle.replace(revoked);
        assert!(!up.request_verification(&[role], "test", "t", "*", SelectPriv));
    }

    #[test]
    fn host_specificity_sorting_independent_of_insertion_order() {
        // Must-fix #1: u@'%' (no priv) + u@'localhost' (SELECT) inserted in both orders must
        // always match the more specific host first (allowing localhost, denying remote IP).
        for wildcard_first in [true, false] {
            let mut mp = MySQLPrivilege::new();
            let rec_wild = UserRecord::new("%", "u", PrivilegeType(0));
            let rec_local = UserRecord::new("localhost", "u", SelectPriv);
            if wildcard_first {
                mp.add_user(rec_wild);
                mp.add_user(rec_local);
            } else {
                mp.add_user(rec_local);
                mp.add_user(rec_wild);
            }
            // Also verify db record specificity (exact host before prefix% before %).
            let db_wild = DbRecord::new("u_db", "%", "test", PrivilegeType(0));
            let db_prefix = DbRecord::new("u_db", "192.168.%", "test", InsertPriv);
            let db_more = DbRecord::new("u_db", "192.168.1.%", "test", SelectPriv);
            if wildcard_first {
                mp.add_db(db_wild);
                mp.add_db(db_prefix);
                mp.add_db(db_more);
            } else {
                mp.add_db(db_more);
                mp.add_db(db_prefix);
                mp.add_db(db_wild);
            }
            let handle = Arc::new(Handle::new(mp));

            let u_local = UserPrivileges::new("u", "localhost", Arc::clone(&handle));
            let u_loopback = UserPrivileges::new("u", "127.0.0.1", Arc::clone(&handle));
            let u_remote = UserPrivileges::new("u", "10.0.0.5", Arc::clone(&handle));
            assert!(
                u_local.request_verification(&[], "test", "t", "*", SelectPriv),
                "wildcard_first={wildcard_first}"
            );
            assert!(
                u_loopback.request_verification(&[], "test", "t", "*", SelectPriv),
                "localhost loopback 127.0.0.1 must match localhost, wildcard_first={wildcard_first}"
            );
            assert!(
                !u_remote.request_verification(&[], "test", "t", "*", SelectPriv),
                "wildcard_first={wildcard_first}"
            );

            let u_db_spec = UserPrivileges::new("u_db", "192.168.1.42", Arc::clone(&handle));
            let u_db_broad = UserPrivileges::new("u_db", "192.168.2.42", Arc::clone(&handle));
            assert!(u_db_spec.request_verification(&[], "test", "t", "*", SelectPriv));
            assert!(!u_db_broad.request_verification(&[], "test", "t", "*", SelectPriv));
            assert!(u_db_broad.request_verification(&[], "test", "t", "", InsertPriv));
        }
    }

    #[test]
    fn host_netmask_and_ipv6_loopback_matching() {
        let mut mp = MySQLPrivilege::new();
        mp.add_user(UserRecord::new(
            "192.168.1.0/255.255.255.0",
            "u_net",
            SelectPriv,
        ));
        mp.add_user(UserRecord::new("localhost", "u_v6", SelectPriv));
        let handle = Arc::new(Handle::new(mp));

        let ok_net = UserPrivileges::new("u_net", "192.168.1.99", Arc::clone(&handle));
        let bad_net = UserPrivileges::new("u_net", "192.168.2.99", Arc::clone(&handle));
        let v6_lo = UserPrivileges::new("u_v6", "::1", Arc::clone(&handle));
        assert!(ok_net.request_verification(&[], "test", "t", "*", SelectPriv));
        assert!(!bad_net.request_verification(&[], "test", "t", "*", SelectPriv));
        assert!(v6_lo.request_verification(&[], "test", "t", "*", SelectPriv));
    }

    #[test]
    fn role_chain_two_levels_and_empty_host_fallback() {
        let mut mp = MySQLPrivilege::new();
        mp.add_user(UserRecord::new("%", "u", PrivilegeType(0)));
        mp.add_user(UserRecord::new("%", "r1", PrivilegeType(0)));
        mp.add_user(UserRecord::new("%", "r2", PrivilegeType(0)));
        mp.add_table_priv(TablesPrivRecord::new("r1", "%", "test", "t1", SelectPriv));
        mp.add_table_priv(TablesPrivRecord::new("r2", "%", "test", "t2", SelectPriv));
        // Two-level chain: u -> r1 -> r2.
        mp.add_role_edge("r2", "%", "r1", "%");
        mp.add_role_edge("r1", "%", "u", "%");
        let handle = Arc::new(Handle::new(mp));

        let up = UserPrivileges::new("u", "%", handle);
        // Active role r1@% traverses two levels u -> r1 -> r2.
        let role_r1 = RoleIdentity {
            username: "r1".to_owned(),
            hostname: "%".to_owned(),
        };
        assert!(up.request_verification(
            std::slice::from_ref(&role_r1),
            "test",
            "t1",
            "*",
            SelectPriv
        ));
        assert!(up.request_verification(&[role_r1], "test", "t2", "*", SelectPriv));

        // Active role given with empty hostname "" must fall back to "%" in RoleGraphEdgesTable::find
        // (Go cache.go:268), matching r1@% direct privileges.
        let role_empty_host = RoleIdentity {
            username: "r1".to_owned(),
            hostname: String::new(),
        };
        assert!(up.request_verification(&[role_empty_host], "test", "t1", "*", SelectPriv));
    }

    #[test]
    fn memory_db_full_deny_list_and_skip_with_grant() {
        let mut mp = MySQLPrivilege::new();
        mp.add_user(UserRecord::new("%", "u_all", tidb_base::mysql::AllPrivMask));
        mp.add_user(UserRecord::new("%", "u_none", PrivilegeType(0)));
        let handle = Arc::new(Handle::new(mp));

        let u_all = UserPrivileges::new("u_all", "%", Arc::clone(&handle));
        for mem_db in ["information_schema", "performance_schema", "metrics_schema"] {
            for denied in [
                CreatePriv,
                AlterPriv,
                DropPriv,
                IndexPriv,
                CreateViewPriv,
                InsertPriv,
                UpdatePriv,
                DeletePriv,
                ReferencesPriv,
                ExecutePriv,
                ShowViewPriv,
                LockTablesPriv,
            ] {
                assert!(
                    !u_all.request_verification(&[], mem_db, "tables", "", denied),
                    "{mem_db} must deny {denied:?}"
                );
            }
        }

        // skip_with_grant short-circuits all verification paths.
        let mut skip = UserPrivileges::new("u_none", "%", handle);
        skip.skip_with_grant = true;
        let user_id = UserIdentity {
            username: "u_none".to_owned(),
            hostname: "%".to_owned(),
            ..Default::default()
        };
        assert!(skip.request_verification(&[], "test", "t", "*", SelectPriv));
        assert!(skip.request_verification_with_user("test", "t", "*", SelectPriv, Some(&user_id)));
        assert!(skip.request_dynamic_verification(&[], "SYSTEM_VARIABLES_ADMIN", true));
        assert!(skip.request_dynamic_verification_with_user(
            "SYSTEM_VARIABLES_ADMIN",
            true,
            Some(&user_id)
        ));
        assert!(skip.db_is_visible(&[], "test"));
    }

    #[test]
    fn db_is_visible_request_with_user_dynamic_privs_and_unicode_folding() {
        use crate::cache::DynamicPrivRecord;
        use tidb_base::mysql::SuperPriv;

        let mut mp = MySQLPrivilege::new();
        mp.add_user(UserRecord::new("%", "u_none", PrivilegeType(0)));
        mp.add_user(UserRecord::new("%", "u_proc", ProcessPriv));
        mp.add_user(UserRecord::new("%", "u_super", SuperPriv));
        mp.add_user(UserRecord::new("%", "u_dyn", PrivilegeType(0)));
        mp.add_dynamic_priv(DynamicPrivRecord::new(
            "u_dyn",
            "%",
            "RESTRICTED_TABLES_ADMIN",
            false,
        ));
        mp.add_dynamic_priv(DynamicPrivRecord::new(
            "u_dyn",
            "%",
            "SYSTEM_VARIABLES_ADMIN",
            true,
        ));
        // tables_priv row with table_priv == 0 but column_priv != 0 (Go cache.go:1704).
        let mut tp_col_only =
            TablesPrivRecord::new("u_colvis", "%", "db_col", "t", PrivilegeType(0));
        tp_col_only.column_priv = SelectPriv;
        mp.add_user(UserRecord::new("%", "u_colvis", PrivilegeType(0)));
        mp.add_table_priv(tp_col_only);
        // Non-ASCII case-folded table name (`db.ТАБ` vs `db.таб`).
        mp.add_user(UserRecord::new("%", "u_cyr", PrivilegeType(0)));
        mp.add_table_priv(TablesPrivRecord::new(
            "u_cyr", "%", "db_cyr", "ТАБ", SelectPriv,
        ));
        // Default role for request_verification_with_user.
        mp.add_user(UserRecord::new("%", "u_def", PrivilegeType(0)));
        mp.add_user(UserRecord::new("%", "r_def", PrivilegeType(0)));
        mp.add_table_priv(TablesPrivRecord::new("r_def", "%", "test", "t", SelectPriv));
        mp.add_role_edge("r_def", "%", "u_def", "%");
        mp.add_default_role("u_def", "%", "r_def", "%");

        let handle = Arc::new(Handle::new(mp));

        // DBIsVisible: INFORMATION_SCHEMA always visible, metrics_schema visible with PROCESS,
        // tables_priv with column_priv != 0 visible.
        let u_none = UserPrivileges::new("u_none", "%", Arc::clone(&handle));
        assert!(u_none.db_is_visible(&[], "information_schema"));
        assert!(!u_none.db_is_visible(&[], "metrics_schema"));
        assert!(!u_none.db_is_visible(&[], "test"));

        let u_proc = UserPrivileges::new("u_proc", "%", Arc::clone(&handle));
        assert!(u_proc.db_is_visible(&[], "metrics_schema"));

        let u_colvis = UserPrivileges::new("u_colvis", "%", Arc::clone(&handle));
        assert!(u_colvis.db_is_visible(&[], "db_col"));

        // RequestVerificationWithUser: information_schema returns true (matches Go privileges.go:219),
        // default role grants work.
        let def_id = UserIdentity {
            username: "u_def".to_owned(),
            hostname: "%".to_owned(),
            ..Default::default()
        };
        assert!(u_none.request_verification_with_user(
            "information_schema",
            "tables",
            "",
            InsertPriv,
            Some(&def_id)
        ));
        assert!(u_none.request_verification_with_user("test", "t", "*", SelectPriv, Some(&def_id)));
        assert!(!u_none.request_verification_with_user("test", "t", "*", SelectPriv, None));

        // Dynamic privileges: with_grant respected, SUPER fallback works.
        let u_dyn = UserPrivileges::new("u_dyn", "%", Arc::clone(&handle));
        assert!(u_dyn.request_dynamic_verification(&[], "RESTRICTED_TABLES_ADMIN", false));
        assert!(!u_dyn.request_dynamic_verification(&[], "RESTRICTED_TABLES_ADMIN", true));
        assert!(u_dyn.request_dynamic_verification(&[], "SYSTEM_VARIABLES_ADMIN", true));
        let u_super = UserPrivileges::new("u_super", "%", Arc::clone(&handle));
        assert!(u_super.request_dynamic_verification(&[], "RESTRICTED_TABLES_ADMIN", false));

        // Non-ASCII case folding: `db_cyr.ТАБ` matches query on `db_cyr.таб`.
        let u_cyr = UserPrivileges::new("u_cyr", "%", handle);
        assert!(u_cyr.request_verification(&[], "db_cyr", "таб", "*", SelectPriv));
    }
}
