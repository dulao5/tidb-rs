//! go-file: pkg/expression/expropt/infoschema.go
//!
//! Mapping: Go's function type `InfoSchemaPropProvider` is a struct holding the closure.
//! `infoschema/context.MetaOnlyInfoSchema` belongs to D06.3 (not ported); [`MetaOnlyInfoSchema`]
//! is its stand-in until then.

use std::any::Any;
use std::sync::Arc;

use tidb_base::Error;

use super::optional::{RequireOptionalEvalProps, get_prop_provider};
use crate::exprctx::{
    EvalContext, OptPropInfoSchema, OptionalEvalPropDesc, OptionalEvalPropKeySet,
    OptionalEvalPropProvider,
};

/// MetaOnlyInfoSchema is a workaround: `infoschema/context.MetaOnlyInfoSchema`.
// glue: stand-in: D06.3, infoschema/context.MetaOnlyInfoSchema (methods with D06.3)
pub trait MetaOnlyInfoSchema: Send + Sync {
    /// SchemaMetaVersion is the version of the schema meta.
    // glue: stand-in: D06.3, interface method MetaOnlyInfoSchema.SchemaMetaVersion
    fn schema_meta_version(&self) -> i64;
}

/// A shared `infoschema.MetaOnlyInfoSchema` interface value.
pub type MetaOnlyInfoSchemaRef = Arc<dyn MetaOnlyInfoSchema>;

/// The function an `InfoSchemaPropProvider` calls (`isDomain`).
pub type InfoSchemaFn = dyn Fn(bool) -> Option<MetaOnlyInfoSchemaRef> + Send + Sync;

/// InfoSchemaPropProvider is the function to provide information schema.
#[derive(Clone)]
pub struct InfoSchemaPropProvider(pub Arc<InfoSchemaFn>);

impl OptionalEvalPropProvider for InfoSchemaPropProvider {
    /// Desc returns the description for the property key.
    // go: InfoSchemaPropProvider.Desc L1
    fn desc(&self) -> &'static OptionalEvalPropDesc {
        OptPropInfoSchema.desc()
    }

    // glue: Go type assertion on an interface value
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// InfoSchemaPropReader is used to get the information schema.
#[derive(Clone, Copy, Debug, Default)]
pub struct InfoSchemaPropReader;

impl RequireOptionalEvalProps for InfoSchemaPropReader {
    /// RequiredOptionalEvalProps implements the RequireOptionalEvalProps interface.
    // go: InfoSchemaPropReader.RequiredOptionalEvalProps L1
    fn required_optional_eval_props(&self) -> OptionalEvalPropKeySet {
        OptPropInfoSchema.as_prop_key_set()
    }
}

impl InfoSchemaPropReader {
    /// GetSessionInfoSchema returns session information schema.
    // go: InfoSchemaPropReader.GetSessionInfoSchema L1
    pub fn get_session_info_schema(
        &self,
        ctx: &dyn EvalContext,
    ) -> Result<Option<MetaOnlyInfoSchemaRef>, Error> {
        let p = get_prop_provider::<InfoSchemaPropProvider>(ctx, OptPropInfoSchema)?;
        Ok((p.0)(false))
    }

    /// GetDomainInfoSchema return domain information schema.
    // go: InfoSchemaPropReader.GetDomainInfoSchema L1
    pub fn get_domain_info_schema(
        &self,
        ctx: &dyn EvalContext,
    ) -> Result<Option<MetaOnlyInfoSchemaRef>, Error> {
        let p = get_prop_provider::<InfoSchemaPropProvider>(ctx, OptPropInfoSchema)?;
        Ok((p.0)(true))
    }
}
