//! go-file: pkg/privilege/privilege.go
//!
//! Block D04.1 core: `PrivilegeManager` trait (Go `privilege.Manager` verification surface),
//! in-memory `MySQLPrivilege` matching engine (`cache.rs`), and `UserPrivileges` (`privileges.rs`).
//! System-table loading (`LoadAll` via `sqlexec`), connection authentication plugins, LDAP, and
//! session tokens belong to later D04.1 parts.

#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::mysql::PrivilegeType;
use tidb_parser::auth::{RoleIdentity, UserIdentity};

pub mod cache;
pub mod privileges;

pub use cache::{
    BaseRecord, ColumnsPrivRecord, DbRecord, DynamicPrivRecord, Handle, MySQLPrivilege,
    TablesPrivRecord, UserRecord,
};
pub use privileges::UserPrivileges;

/// Go `privilege.Manager` verification surface read by the planner, preprocessor, and session.
// glue: interface privilege.Manager (verification subset for D04.1 R29-R32)
pub trait PrivilegeManager: Send + Sync {
    /// RequestVerification verifies user privilege for the request.
    ///
    /// - `column == ""`: requires global, database, or table-level privilege (rejects column-only
    ///   grants such as `SELECT(a)`).
    /// - `column == "*"`: satisfied by global, database, table-level `SELECT`, or column-level
    ///   `SELECT(col)` on at least one column of the table.
    /// - `column == "<name>"`: satisfied by global, database, table-level, or matching column-level
    ///   privilege.
    // glue: interface method Manager.RequestVerification
    fn request_verification(
        &self,
        active_roles: &[RoleIdentity],
        db: &str,
        table: &str,
        column: &str,
        priv_: PrivilegeType,
    ) -> bool;

    /// RequestVerificationWithUser verifies specific user privilege for the request.
    // glue: interface method Manager.RequestVerificationWithUser
    fn request_verification_with_user(
        &self,
        db: &str,
        table: &str,
        column: &str,
        priv_: PrivilegeType,
        user: Option<&UserIdentity>,
    ) -> bool;

    /// RequestDynamicVerification verifies user privilege for a DYNAMIC privilege.
    // glue: interface method Manager.RequestDynamicVerification
    fn request_dynamic_verification(
        &self,
        active_roles: &[RoleIdentity],
        priv_name: &str,
        grantable: bool,
    ) -> bool;

    /// RequestDynamicVerificationWithUser verifies a DYNAMIC privilege for a specific user.
    // glue: interface method Manager.RequestDynamicVerificationWithUser
    fn request_dynamic_verification_with_user(
        &self,
        priv_name: &str,
        grantable: bool,
        user: Option<&UserIdentity>,
    ) -> bool;

    /// DBIsVisible returns true if the database is visible to the current user.
    // glue: interface method Manager.DBIsVisible
    fn db_is_visible(&self, active_roles: &[RoleIdentity], db: &str) -> bool;
}

/// Shared reference to a [`PrivilegeManager`].
pub type PrivilegeManagerRef = Arc<dyn PrivilegeManager>;
