//! `pkg/util/tracing` (block D14.0). Only the optimizer trace structures
//! ([`opt_trace`]); `util.go` (opentracing spans, `runtime/trace` regions, the `TRACE`
//! statement's span recorder) waits for the tracing carrier decided with the session /
//! `ExecCtx` (M-GOCTX, D05.3) and the `TRACE` executor (block spec §4).

pub mod opt_trace;
