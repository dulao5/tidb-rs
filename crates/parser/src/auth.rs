//! go-file: pkg/parser/auth/auth.go
//!
//! Block D02.3: the user and role identities (the password plugins of `parser/auth` belong
//! to the privilege port). User and host names are SQL identifiers, held as `String`.

#![allow(non_upper_case_globals)]

/// UserNameMaxLength is the max length of username.
pub const UserNameMaxLength: isize = 32;
/// HostNameMaxLength is the max length of host name.
pub const HostNameMaxLength: isize = 255;

/// UserIdentity represents username and hostname.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct UserIdentity {
    /// Username.
    pub username: String,
    /// Hostname.
    pub hostname: String,
    /// CurrentUser: `CURRENT_USER`.
    pub current_user: bool,
    /// Username matched in privileges system.
    pub auth_username: String,
    /// Match in privs system (i.e. could be a wildcard).
    pub auth_hostname: String,
    /// The plugin specified in handshake, only used during authentication.
    pub auth_plugin: String,
}

/// `ctx.WriteName` with `format.DefaultRestoreFlags` (back quotes, `` ` `` doubled).
// go: pkg/parser/format/format.go RestoreCtx.WriteName L2 (DefaultRestoreFlags only)
fn write_name(out: &mut String, name: &str) {
    out.push('`');
    out.push_str(&name.replace('`', "``"));
    out.push('`');
}

impl UserIdentity {
    /// Restore implements Node interface (`format.DefaultRestoreFlags`).
    // go: UserIdentity.Restore L1
    pub fn restore(&self, out: &mut String) {
        if self.current_user {
            out.push_str("CURRENT_USER");
        } else {
            write_name(out, &self.username);
            out.push('@');
            write_name(out, &self.hostname);
        }
    }

    /// String converts UserIdentity to the format user@host. It defaults to providing the
    /// AuthIdentity (the matching entry in priv tables). A nil `*UserIdentity` (`""` in Go)
    /// is `Option::None` at the call site.
    // go: UserIdentity.String L1 [M-NAME]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        // TODO: Escape username and hostname.
        if !self.auth_username.is_empty() {
            return format!("{}@{}", self.auth_username, self.auth_hostname);
        }
        format!("{}@{}", self.username, self.hostname)
    }

    /// LoginString returns matched identity in user@host format. It matches the login user.
    // go: UserIdentity.LoginString L1
    pub fn login_string(&self) -> String {
        // TODO: Escape username and hostname.
        format!("{}@{}", self.username, self.hostname)
    }
}

/// RoleIdentity represents a role name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct RoleIdentity {
    /// Username.
    pub username: String,
    /// Hostname.
    pub hostname: String,
}

impl RoleIdentity {
    /// Restore implements Node interface (`format.DefaultRestoreFlags`).
    // go: RoleIdentity.Restore L1
    pub fn restore(&self, out: &mut String) {
        write_name(out, &self.username);
        if !self.hostname.is_empty() {
            out.push('@');
            write_name(out, &self.hostname);
        }
    }

    /// String converts RoleIdentity to the format `user`@`host`.
    // go: RoleIdentity.String L1 [M-NAME]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        // TODO: Escape username and hostname.
        format!("`{}`@`{}`", self.username, self.hostname)
    }
}

/// GetUserAndHostName returns the user and host name if the given user is not nil.
// go: GetUserAndHostName L1
pub fn get_user_and_host_name(user: Option<&UserIdentity>) -> (String, String) {
    let Some(user) = user else {
        return (String::new(), String::new());
    };
    if !user.auth_username.is_empty() && !user.auth_hostname.is_empty() {
        return (user.auth_username.clone(), user.auth_hostname.clone());
    }
    (user.username.clone(), user.hostname.clone())
}
