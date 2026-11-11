//! go-file: pkg/expression/expropt/current_user.go
//!
//! Mapping: Go's function type `CurrentUserPropProvider` is a struct holding the closure
//! (`Arc<dyn Fn>`); `*auth.UserIdentity` is `Option<UserIdentity>` (Go's nil user).

use std::any::Any;
use std::sync::Arc;

use tidb_base::Error;
use tidb_parser::auth::{RoleIdentity, UserIdentity};

use super::optional::{RequireOptionalEvalProps, get_prop_provider};
use crate::exprctx::{
    EvalContext, OptPropCurrentUser, OptionalEvalPropDesc, OptionalEvalPropKeySet,
    OptionalEvalPropProvider,
};

/// The function a `CurrentUserPropProvider` calls.
pub type CurrentUserFn = dyn Fn() -> (Option<UserIdentity>, Vec<RoleIdentity>) + Send + Sync;

/// CurrentUserPropProvider is a provider to get the current user
#[derive(Clone)]
pub struct CurrentUserPropProvider(pub Arc<CurrentUserFn>);

impl OptionalEvalPropProvider for CurrentUserPropProvider {
    /// Desc returns the description for the property key.
    // go: CurrentUserPropProvider.Desc L1
    fn desc(&self) -> &'static OptionalEvalPropDesc {
        OptPropCurrentUser.desc()
    }

    // glue: Go type assertion on an interface value
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// CurrentUserPropReader is used by expression to read property context.OptPropCurrentUser
#[derive(Clone, Copy, Debug, Default)]
pub struct CurrentUserPropReader;

impl RequireOptionalEvalProps for CurrentUserPropReader {
    /// RequiredOptionalEvalProps implements the RequireOptionalEvalProps interface.
    // go: CurrentUserPropReader.RequiredOptionalEvalProps L1
    fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet {
        OptPropCurrentUser.as_prop_key_set()
    }
}

impl CurrentUserPropReader {
    /// CurrentUser returns the current user
    // go: CurrentUserPropReader.CurrentUser L1
    pub fn current_user(&self, ctx: &dyn EvalContext) -> Result<Option<UserIdentity>, Error> {
        let p = self.get_provider(ctx)?;
        let (user, _) = (p.0)();
        Ok(user)
    }

    /// ActiveRoles returns the active roles
    // go: CurrentUserPropReader.ActiveRoles L1
    pub fn active_roles(&self, ctx: &dyn EvalContext) -> Result<Vec<RoleIdentity>, Error> {
        let p = self.get_provider(ctx)?;
        let (_, roles) = (p.0)();
        Ok(roles)
    }

    // go: CurrentUserPropReader.getProvider L1
    fn get_provider(&self, ctx: &dyn EvalContext) -> Result<Arc<CurrentUserPropProvider>, Error> {
        get_prop_provider::<CurrentUserPropProvider>(ctx, OptPropCurrentUser)
    }
}
