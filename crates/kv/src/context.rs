//! The values of Go's `context.Context` that `pkg/kv` reads (mapping rule M-GOCTX).
//!
//! Go threads a `context.Context` through every KV call; `pkg/kv` reads one value from it,
//! the request source (`util.RequestSourceKey`, set by `util.WithInternalSourceType`).
//! Cancellation is not a context value in the port: dropping the future cancels the
//! request (N3 3.1 item 6). [`Context`] carries the values; it is cheap to clone, as Go's
//! derived contexts are cheap to create.

/// RequestSource contains the source label of the request, used for tracking resource
/// consuming.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestSource {
    pub request_source_internal: bool,
    pub request_source_type: String,
    /// ExplicitRequestSourceType is a type that is set from the session variable and may
    /// specified by the client or users. It is a complement to the RequestSourceType and
    /// provides additional information about how a request was initiated.
    pub explicit_request_source_type: String,
}

/// The values of a Go `context.Context` that the KV layer reads.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// `ctx.Value(util.RequestSourceKey)`.
    request_source: Option<RequestSource>,
}

impl Context {
    /// `context.Background()`.
    // glue: Go context.Background (M-GOCTX)
    pub fn background() -> Context {
        Context::default()
    }

    /// `context.WithValue(ctx, util.RequestSourceKey, source)`.
    // glue: Go context.WithValue for RequestSourceKey (M-GOCTX)
    pub fn with_request_source(&self, source: RequestSource) -> Context {
        Context {
            request_source: Some(source),
        }
    }

    /// `ctx.Value(util.RequestSourceKey)`.
    // glue: Go ctx.Value(RequestSourceKey) (M-GOCTX)
    pub fn request_source(&self) -> Option<&RequestSource> {
        self.request_source.as_ref()
    }
}

/// WithInternalSourceType create context with internal source.
// go-dep: client-go util.WithInternalSourceType L1
pub fn with_internal_source_type(ctx: &Context, source: &str) -> Context {
    ctx.with_request_source(RequestSource {
        request_source_internal: true,
        request_source_type: source.to_owned(),
        ..Default::default()
    })
}
