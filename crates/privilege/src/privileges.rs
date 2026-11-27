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
            return !matches!(
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
            );
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
}
