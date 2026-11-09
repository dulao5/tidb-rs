//! go-file: pkg/planner/util/debugtrace/base.go
//!
//! The optimizer debug trace: a tree of contexts, each holding a list of steps, marshalled to
//! JSON with Go's `encoding/json` rules (HTML escaping off, a trailing newline from
//! `Encoder.Encode`, maps sorted by key, NaN / ±Inf rejected).
//!
//! Mapping:
//! - Go links the contexts with pointers (`steps []any` may hold `*baseDebugTraceContext`, each
//!   context points to its parent). Rust keeps the contexts in an arena owned by the root
//!   (`Vec` + index): [`TraceValue::Ctx`] is a pointer to a context, index 0 is the root's
//!   `traceCtx` (whose parent is itself, as in Go).
//! - A step is Go's `any`; [`TraceValue`] covers what the trace records: generic JSON values
//!   ([`Value`]), `map[string]any`, `[]any` and contexts. Strings are valid UTF-8 (Go strings
//!   with invalid bytes are not recorded by the ported callers).
//! - `GetOrInitDebugTraceRoot`, `EnterContextCommon`, `LeaveContextCommon` and
//!   `RecordAnyValuesWithNames` take a `planctx.PlanContext` to reach
//!   `SessionVars().StmtCtx.OptimizerDebugTrace`; they wait for the session's plan context (N2,
//!   D05.3). Their logic on the root is here as glue methods ([`OptimizerDebugTraceRoot::new`],
//!   [`OptimizerDebugTraceRoot::enter_context`], ...); `EnterContextCommon`'s
//!   `runtime.Caller(1)` function name is passed by the caller.
//! - Go's encoding errors are `*json.UnsupportedValueError` (NaN / ±Inf) and
//!   `*json.MarshalerError` (a nested context's `MarshalJSON` failed); [`EncodeError`] keeps that
//!   shape, its message, and the JSON Go writes for it (`EncodeJSONCommon` encodes the error
//!   value: `{"Value":{},"Str":"NaN"}`, `{"Type":{},"Err":...}`).

use std::collections::BTreeMap;
use std::fmt;

use tidb_base::gostd::json::{Value, append_string_opts, format_float};

/// A debug trace step (Go `any` as recorded by the trace).
#[derive(Clone, Debug, PartialEq)]
pub enum TraceValue {
    /// A generic JSON value (Go scalars, strings, maps and slices of them).
    Json(Value),
    /// Go `map[string]any` that may hold contexts (encoded with sorted keys).
    Map(BTreeMap<String, TraceValue>),
    /// Go `[]any` that may hold contexts.
    Array(Vec<TraceValue>),
    /// Go `*baseDebugTraceContext`: an index into the root's arena.
    Ctx(usize),
}

/// The error of Go's JSON encoding of a trace.
#[derive(Clone, Debug, PartialEq)]
pub enum EncodeError {
    /// `*json.UnsupportedValueError` for a float: `Str` is `strconv.FormatFloat(f, 'g', -1, 64)`.
    Unsupported(String),
    /// `*json.MarshalerError` for `*debugtrace.baseDebugTraceContext`, wrapping the error its
    /// `MarshalJSON` returned.
    Marshaler(Box<EncodeError>),
}

impl fmt::Display for EncodeError {
    // go-std: encoding/json.UnsupportedValueError.Error L1
    // go-std: encoding/json.MarshalerError.Error L1 (sourceFunc "MarshalJSON")
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::Unsupported(s) => write!(f, "json: unsupported value: {s}"),
            EncodeError::Marshaler(e) => write!(
                f,
                "json: error calling MarshalJSON for type *debugtrace.baseDebugTraceContext: {e}"
            ),
        }
    }
}

impl std::error::Error for EncodeError {}

impl EncodeError {
    /// The JSON `json.Encoder.Encode(err)` writes for the error value (without the newline):
    /// exported fields only, `reflect.Value` / `reflect.Type` have none (`{}`).
    // glue: encoding/json of *UnsupportedValueError / *MarshalerError (EncodeJSONCommon's fallback)
    fn append_json(&self, out: &mut String) {
        match self {
            EncodeError::Unsupported(s) => {
                out.push_str(r#"{"Value":{},"Str":"#);
                append_string_opts(out, s, false);
                out.push('}');
            }
            EncodeError::Marshaler(e) => {
                out.push_str(r#"{"Type":{},"Err":"#);
                e.append_json(out);
                out.push('}');
            }
        }
    }
}

/// baseDebugTraceContext is the core of the debug trace.
/// The steps field can be used to record any information, or point to another baseDebugTraceContext.
#[derive(Clone, Debug, Default, PartialEq)]
struct BaseDebugTraceContext {
    name: String,
    steps: Vec<TraceValue>,
    parent_ctx: usize,
}

/// OptimizerDebugTraceRoot is for recording the optimizer debug trace.
/// Each debug information, which is a "step" and can be any type, is placed
/// in a "context" (baseDebugTraceContext) as an interface{}.
/// Overall, the debug trace is a tree-like hierarchical structure of baseDebugTraceContext.
/// This structure can reflect the function call hierarchy of each step during optimization.
/// In the end, the entire recorded baseDebugTraceContext will be marshalled to JSON as the result.
#[derive(Clone, Debug, PartialEq)]
pub struct OptimizerDebugTraceRoot {
    /// The arena; index 0 is Go's `traceCtx`.
    ctxs: Vec<BaseDebugTraceContext>,
    /// currentCtx indicates in which baseDebugTraceContext we should record the debug information.
    current_ctx: usize,
}

impl Default for OptimizerDebugTraceRoot {
    // glue: the initialisation part of GetOrInitDebugTraceRoot
    fn default() -> Self {
        Self::new()
    }
}

impl OptimizerDebugTraceRoot {
    /// A new root, as `GetOrInitDebugTraceRoot` creates it: the current context is `traceCtx`,
    /// whose parent is itself.
    // glue: the initialisation part of GetOrInitDebugTraceRoot (the PlanContext lookup waits for D05.3)
    pub fn new() -> Self {
        OptimizerDebugTraceRoot {
            ctxs: vec![BaseDebugTraceContext::default()],
            current_ctx: 0,
        }
    }

    /// MarshalJSON overrides the default MarshalJSON behavior and marshals the unexported traceCtx.
    // go: OptimizerDebugTraceRoot.MarshalJSON L1
    pub fn marshal_json(&self) -> (String, Option<EncodeError>) {
        let root = &self.ctxs[0];
        if root.steps.is_empty() {
            // A nil `[]any` encodes as null.
            return self.encode_json_common(&TraceValue::Json(Value::Null));
        }
        self.encode_common_with(|this, out| this.append_array(out, &root.steps))
    }

    /// `(*baseDebugTraceContext).MarshalJSON` of the context at `idx`.
    // go: baseDebugTraceContext.MarshalJSON L1
    pub fn ctx_marshal_json(&self, idx: usize) -> (String, Option<EncodeError>) {
        self.encode_common_with(|this, out| this.append_ctx_content(out, idx))
    }

    /// AppendStepToCurrentContext records debug information to the current context of the debug trace.
    // go: OptimizerDebugTraceRoot.AppendStepToCurrentContext L1
    pub fn append_step_to_current_context(&mut self, step: TraceValue) {
        let cur = self.current_ctx;
        self.ctxs[cur].steps.push(step);
    }

    /// AppendStepWithNameToCurrentContext records debug information and a name to the current context of the debug trace.
    // go: OptimizerDebugTraceRoot.AppendStepWithNameToCurrentContext L1
    pub fn append_step_with_name_to_current_context(&mut self, step: TraceValue, name: &str) {
        let mut tmp = BTreeMap::new();
        tmp.insert(name.to_owned(), step);
        let cur = self.current_ctx;
        self.ctxs[cur].steps.push(TraceValue::Map(tmp));
    }

    /// EncodeJSONCommon contains some common logic for the debug trace,
    /// like disabling EscapeHTML and recording error.
    ///
    /// Contexts in `input` are resolved in this root's arena.
    // go: EncodeJSONCommon L2 (contexts are arena indexes, resolved through the root)
    pub fn encode_json_common(&self, input: &TraceValue) -> (String, Option<EncodeError>) {
        self.encode_common_with(|this, out| this.append_value(out, input))
    }

    /// The body of `EnterContextCommon` after `GetOrInitDebugTraceRoot`: creates a context named
    /// `func_name` (Go: the caller's `runtime.FuncForPC(pc).Name()`), appends it to the current
    /// context and enters it.
    // glue: EnterContextCommon on the root (the PlanContext lookup waits for D05.3)
    pub fn enter_context(&mut self, func_name: &str) {
        let new_ctx = BaseDebugTraceContext {
            name: func_name.to_owned(),
            steps: Vec::new(),
            parent_ctx: self.current_ctx,
        };
        let idx = self.ctxs.len();
        self.ctxs.push(new_ctx);
        let cur = self.current_ctx;
        self.ctxs[cur].steps.push(TraceValue::Ctx(idx));
        self.current_ctx = idx;
    }

    /// The body of `LeaveContextCommon`: makes the debug trace go to its parent context.
    // glue: LeaveContextCommon on the root (the PlanContext lookup waits for D05.3)
    pub fn leave_context(&mut self) {
        self.current_ctx = self.ctxs[self.current_ctx].parent_ctx;
    }

    /// The body of `RecordAnyValuesWithNames`: `vals` is `["name1", value1, "name2", value2]`;
    /// a name that is not a string becomes `""` (Go's `str, _ := vals[i].(string)`), a later
    /// equal name replaces the earlier value, and an odd length panics like Go's index out of
    /// range.
    // glue: RecordAnyValuesWithNames on the root (the PlanContext lookup waits for D05.3)
    pub fn record_any_values_with_names(&mut self, vals: Vec<TraceValue>) {
        let n = vals.len();
        let mut tmp = BTreeMap::new();
        let mut it = vals.into_iter();
        let mut i = 0;
        while i < n {
            let name = it.next().unwrap_or(TraceValue::Json(Value::Null));
            let Some(val) = it.next() else {
                panic!("runtime error: index out of range [{n}] with length {n}");
            };
            let s = match &name {
                TraceValue::Json(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            tmp.insert(s, val);
            i += 2;
        }
        self.append_step_to_current_context(TraceValue::Map(tmp));
    }

    /// The step list of the context at `idx` (index 0 is the root's `traceCtx`).
    // glue: read access for tests and the replay
    pub fn steps(&self, idx: usize) -> &[TraceValue] {
        &self.ctxs[idx].steps
    }

    /// The index of the current context.
    // glue: read access for tests and the replay
    pub fn current_ctx(&self) -> usize {
        self.current_ctx
    }

    // --- encoding (Go's encodeState with escapeHTML false) ---

    /// `json.NewEncoder(&buf)` with `SetEscapeHTML(false)`, then `Encode(input)`; on error,
    /// `Encode(err)`. `Encode` writes nothing when encoding fails, and appends a newline.
    // glue: the Encoder plumbing of EncodeJSONCommon
    fn encode_common_with(
        &self,
        f: impl FnOnce(&Self, &mut String) -> Result<(), EncodeError>,
    ) -> (String, Option<EncodeError>) {
        let mut out = String::new();
        match f(self, &mut out) {
            Ok(()) => {
                out.push('\n');
                (out, None)
            }
            Err(e) => {
                let mut eo = String::new();
                e.append_json(&mut eo);
                eo.push('\n');
                (eo, Some(e))
            }
        }
    }

    /// The content `baseDebugTraceContext.MarshalJSON` encodes: no steps → null, one step →
    /// the step, more → the slice; wrapped in `{name: content}` when the name is not empty.
    // glue: the body of baseDebugTraceContext.MarshalJSON
    fn append_ctx_content(&self, out: &mut String, idx: usize) -> Result<(), EncodeError> {
        let c = &self.ctxs[idx];
        if !c.name.is_empty() {
            out.push('{');
            append_string_opts(out, &c.name, false);
            out.push(':');
        }
        match c.steps.len() {
            0 => out.push_str("null"),
            1 => self.append_value(out, &c.steps[0])?,
            _ => self.append_array(out, &c.steps)?,
        }
        if !c.name.is_empty() {
            out.push('}');
        }
        Ok(())
    }

    /// `marshalerEncoder` for a context: calls its `MarshalJSON`; an error becomes a
    /// `*json.MarshalerError`, otherwise the output is compacted into the buffer (our output is
    /// already compact apart from `Encode`'s newline).
    // go-std: encoding/json.marshalerEncoder L2 (compact = drop Encode's trailing newline)
    fn append_ctx(&self, out: &mut String, idx: usize) -> Result<(), EncodeError> {
        let mut inner = String::new();
        match self.append_ctx_content(&mut inner, idx) {
            Ok(()) => {
                out.push_str(&inner);
                Ok(())
            }
            Err(e) => Err(EncodeError::Marshaler(Box::new(e))),
        }
    }

    // go-std: encoding/json.arrayEncoder L2 ([]any)
    fn append_array(&self, out: &mut String, items: &[TraceValue]) -> Result<(), EncodeError> {
        out.push('[');
        for (i, v) in items.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            self.append_value(out, v)?;
        }
        out.push(']');
        Ok(())
    }

    // go-std: encoding/json.encodeState.reflectValue L2 (the value kinds a trace records)
    fn append_value(&self, out: &mut String, v: &TraceValue) -> Result<(), EncodeError> {
        match v {
            TraceValue::Json(j) => append_json_value(out, j),
            TraceValue::Array(a) => self.append_array(out, a),
            TraceValue::Map(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    append_string_opts(out, k, false);
                    out.push(':');
                    self.append_value(out, v)?;
                }
                out.push('}');
                Ok(())
            }
            TraceValue::Ctx(idx) => self.append_ctx(out, *idx),
        }
    }
}

/// A generic value with `escapeHTML` off, failing on NaN / ±Inf like `floatEncoder`.
// go-std: encoding/json.encodeState.reflectValue L2 (generic values, escapeHTML false)
fn append_json_value(out: &mut String, v: &Value) -> Result<(), EncodeError> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(f) => {
            if f.is_nan() || f.is_infinite() {
                let s = if f.is_nan() {
                    "NaN"
                } else if *f > 0.0 {
                    "+Inf"
                } else {
                    "-Inf"
                };
                return Err(EncodeError::Unsupported(s.to_owned()));
            }
            out.push_str(&format_float(*f));
        }
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::String(s) => append_string_opts(out, s, false),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                append_json_value(out, x)?;
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                append_string_opts(out, k, false);
                out.push(':');
                append_json_value(out, x)?;
            }
            out.push('}');
        }
    }
    Ok(())
}
