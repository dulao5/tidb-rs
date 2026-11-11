//! go-file: pkg/util/tracing/opt_trace.go
//!
//! The optimizer trace (`TRACE PLAN TARGET = 'estimation' / 'optimizer'`, plan replayer):
//! the logical rule steps, the physical candidates and their cost details, and the final
//! plan, marshalled to JSON with the struct tags of the Go types.
//!
//! Mapping:
//! - `Children []*PlanTrace` is `Vec<PlanTrace>` (the planner builds the tree and only reads
//!   it; flattening copies the nodes, as Go's).
//! - `LogicalOptimizeTracer.curRuleTracer` points at a step that `removeUselessStep` may drop
//!   from `Steps` (Go keeps appending to the dropped object, invisibly). The steps live in an
//!   arena (`rules`), `Steps` is a list of arena indexes and `curRuleTracer` an arena index,
//!   so the dropped step keeps receiving appends exactly as in Go.
//! - `CandidatePlanTrace` embeds `*PlanTrace`; `buildCandidatesInfo` sets `Selected` on it.
//!   The port owns the `PlanTrace` (Go may share it with another structure; the planner
//!   port, P3, does not).
//! - `map[string]any` cost params are [`CostParam`] values (the planner records float64, int,
//!   string and bool).
//! - JSON: `encoding/json` of the structs (field order and tags, `json:"-"`, maps sorted by
//!   key, int map keys sorted as strings, HTML escaping, nil slice / map / pointer as `null`,
//!   NaN / ±Inf an `UnsupportedValueError`): `marshal_json` methods.

use std::collections::{BTreeMap, BTreeSet};

use tidb_base::gostd::json::{append_string, format_float};

/// `json.UnsupportedValueError` for a NaN or infinite float (`json: unsupported value: NaN`).
#[derive(Clone, Debug, PartialEq)]
pub struct UnsupportedValueError(pub String);

impl UnsupportedValueError {
    // go-std: encoding/json.UnsupportedValueError.Error L1
    pub fn error(&self) -> String {
        format!("json: unsupported value: {}", self.0)
    }
}

type JsonResult = Result<(), UnsupportedValueError>;

// go-std: encoding/json.floatEncoder.encode L1 (bits 64)
fn json_float(out: &mut String, f: f64) -> JsonResult {
    if f.is_nan() || f.is_infinite() {
        return Err(UnsupportedValueError(
            tidb_base::gostd::strconv::format_float(f, b'g', -1, 64),
        ));
    }
    out.push_str(&format_float(f));
    Ok(())
}

// go-std: encoding/json.intEncoder L1
fn json_int(out: &mut String, i: i64) {
    out.push_str(&i.to_string());
}

/// A slice of pointers to structs: `null` when nil, else `[e,...]`.
// go-std: encoding/json.sliceEncoder L1 (nil slice is null)
fn json_slice<T>(
    out: &mut String,
    v: Option<&[T]>,
    mut each: impl FnMut(&mut String, &T) -> JsonResult,
) -> JsonResult {
    let Some(v) = v else {
        out.push_str("null");
        return Ok(());
    };
    out.push('[');
    for (i, e) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        each(out, e)?;
    }
    out.push(']');
    Ok(())
}

/// `PlanTrace` indicates for the Plan trace information.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanTrace {
    map_children: Option<BTreeSet<i64>>,
    pub tp: String,
    pub proper_type: String,
    /// ExplainInfo should be implemented by each implemented Plan
    pub explain_info: String,
    pub children: Vec<PlanTrace>,
    /// Go's nil slice (`null` in JSON) is `None`.
    pub children_id: Option<Vec<i64>>,
    pub id: i64,
    pub cost: f64,
    pub selected: bool,
}

impl PlanTrace {
    /// `AppendChildrenID` appends children ids.
    // go: PlanTrace.AppendChildrenID L1
    pub fn append_children_id(&mut self, ids: &[i64]) {
        let map = self.map_children.get_or_insert_with(BTreeSet::new);
        for &id in ids {
            if !map.insert(id) {
                continue;
            }
            self.children_id.get_or_insert_with(Vec::new).push(id);
        }
    }

    /// The JSON object (`json:"type"`, `"property"`, `"info"`, `"children"`, `"id"`,
    /// `"cost"`, `"selected"`; `Children` is `json:"-"`).
    // go-std: encoding/json.structEncoder L2 (PlanTrace's tags)
    fn write_fields(&self, out: &mut String) -> JsonResult {
        out.push_str("\"type\":");
        append_string(out, &self.tp);
        out.push_str(",\"property\":");
        append_string(out, &self.proper_type);
        out.push_str(",\"info\":");
        append_string(out, &self.explain_info);
        out.push_str(",\"children\":");
        json_slice(out, self.children_id.as_deref(), |out, id| {
            json_int(out, *id);
            Ok(())
        })?;
        out.push_str(",\"id\":");
        json_int(out, self.id);
        out.push_str(",\"cost\":");
        json_float(out, self.cost)?;
        out.push_str(",\"selected\":");
        out.push_str(if self.selected { "true" } else { "false" });
        Ok(())
    }

    /// `json.Marshal(p)`.
    // go-std: encoding/json.Marshal L2 (PlanTrace)
    pub fn marshal_json(&self) -> Result<String, UnsupportedValueError> {
        let mut out = String::new();
        self.write(&mut out)?;
        Ok(out)
    }

    // go-std: encoding/json.structEncoder L2 (PlanTrace)
    fn write(&self, out: &mut String) -> JsonResult {
        out.push('{');
        self.write_fields(out)?;
        out.push('}');
        Ok(())
    }
}

/// `LogicalOptimizeTracer` indicates the trace for the whole logicalOptimize processing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogicalOptimizeTracer {
    /// curRuleTracer indicates the current rule Tracer during optimize by rule (an index in
    /// `rules`).
    cur_rule_tracer: Option<usize>,
    /// Every rule tracer ever appended (the objects Go's pointers refer to).
    rules: Vec<LogicalRuleOptimizeTracer>,
    pub final_logical_plan: Option<Vec<PlanTrace>>,
    /// `Steps`: indexes in `rules`; Go's nil slice is `None`.
    steps: Option<Vec<usize>>,
}

impl LogicalOptimizeTracer {
    /// `AppendRuleTracerBeforeRuleOptimize` add plan tracer before optimize.
    // go: LogicalOptimizeTracer.AppendRuleTracerBeforeRuleOptimize L1
    pub fn append_rule_tracer_before_rule_optimize(
        &mut self,
        index: i64,
        name: &str,
        before: &PlanTrace,
    ) {
        let rule_tracer = build_logical_rule_optimize_tracer_before_optimize(index, name, before);
        self.rules.push(rule_tracer);
        let i = self.rules.len() - 1;
        self.steps.get_or_insert_with(Vec::new).push(i);
        self.cur_rule_tracer = Some(i);
    }

    /// `AppendRuleTracerStepToCurrent` add rule optimize step to current. Like Go it panics
    /// (nil pointer dereference) without a current rule tracer.
    // go: LogicalOptimizeTracer.AppendRuleTracerStepToCurrent L1
    pub fn append_rule_tracer_step_to_current(
        &mut self,
        id: i64,
        tp: &str,
        reason: &str,
        action: &str,
    ) {
        let cur = self
            .cur_rule_tracer
            .expect("runtime error: invalid memory address or nil pointer dereference");
        let cur = &mut self.rules[cur];
        let index = cur.steps.len() as i64;
        cur.steps.push(LogicalRuleOptimizeTraceStep {
            id,
            tp: tp.to_string(),
            reason: reason.to_string(),
            action: action.to_string(),
            index,
        });
    }

    /// `RecordFinalLogicalPlan` add plan trace after logical optimize.
    // go: LogicalOptimizeTracer.RecordFinalLogicalPlan L1
    pub fn record_final_logical_plan(&mut self, final_: &PlanTrace) {
        self.final_logical_plan = Some(to_flatten_plan_trace(final_));
        self.remove_useless_step();
    }

    // go: LogicalOptimizeTracer.removeUselessStep L1
    fn remove_useless_step(&mut self) {
        let new_steps: Vec<usize> = self
            .steps
            .iter()
            .flatten()
            .copied()
            .filter(|&i| !self.rules[i].steps.is_empty())
            .collect();
        self.steps = Some(new_steps);
    }

    /// `Steps`: the rule tracers in order.
    // glue: resolve the arena indexes of Steps
    pub fn steps(&self) -> Option<Vec<&LogicalRuleOptimizeTracer>> {
        self.steps
            .as_ref()
            .map(|s| s.iter().map(|&i| &self.rules[i]).collect())
    }

    /// `json.Marshal(tracer)`: `{"final":…,"steps":…}`.
    // go-std: encoding/json.structEncoder L2 (LogicalOptimizeTracer's tags)
    pub fn marshal_json(&self) -> Result<String, UnsupportedValueError> {
        let mut out = String::new();
        self.write(&mut out)?;
        Ok(out)
    }

    // go-std: encoding/json.structEncoder L2 (LogicalOptimizeTracer)
    fn write(&self, out: &mut String) -> JsonResult {
        out.push_str("{\"final\":");
        json_slice(out, self.final_logical_plan.as_deref(), |out, p| {
            p.write(out)
        })?;
        out.push_str(",\"steps\":");
        let steps = self.steps();
        json_slice(out, steps.as_deref(), |out, r| r.write(out))?;
        out.push('}');
        Ok(())
    }
}

/// `LogicalRuleOptimizeTracer` indicates the trace for the LogicalPlan tree before and after
/// logical rule optimize.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogicalRuleOptimizeTracer {
    pub rule_name: String,
    pub before: Vec<PlanTrace>,
    pub steps: Vec<LogicalRuleOptimizeTraceStep>,
    pub index: i64,
}

impl LogicalRuleOptimizeTracer {
    // go-std: encoding/json.structEncoder L2 (LogicalRuleOptimizeTracer's tags)
    fn write(&self, out: &mut String) -> JsonResult {
        out.push_str("{\"name\":");
        append_string(out, &self.rule_name);
        out.push_str(",\"before\":");
        json_slice(out, Some(&self.before), |out, p| p.write(out))?;
        out.push_str(",\"steps\":");
        json_slice(out, Some(&self.steps), |out, s| {
            s.write(out);
            Ok(())
        })?;
        out.push_str(",\"index\":");
        json_int(out, self.index);
        out.push('}');
        Ok(())
    }
}

/// `buildLogicalRuleOptimizeTracerBeforeOptimize` build rule tracer before rule optimize.
// go: buildLogicalRuleOptimizeTracerBeforeOptimize L1
fn build_logical_rule_optimize_tracer_before_optimize(
    index: i64,
    name: &str,
    before: &PlanTrace,
) -> LogicalRuleOptimizeTracer {
    LogicalRuleOptimizeTracer {
        index,
        before: to_flatten_plan_trace(before),
        rule_name: name.to_string(),
        steps: Vec::new(),
    }
}

/// `LogicalRuleOptimizeTraceStep` indicates the trace for the detailed optimize changing in
/// logical rule optimize.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogicalRuleOptimizeTraceStep {
    pub action: String,
    pub reason: String,
    pub tp: String,
    pub id: i64,
    pub index: i64,
}

impl LogicalRuleOptimizeTraceStep {
    // go-std: encoding/json.structEncoder L2 (LogicalRuleOptimizeTraceStep's tags)
    fn write(&self, out: &mut String) {
        out.push_str("{\"action\":");
        append_string(out, &self.action);
        out.push_str(",\"reason\":");
        append_string(out, &self.reason);
        out.push_str(",\"type\":");
        append_string(out, &self.tp);
        out.push_str(",\"id\":");
        json_int(out, self.id);
        out.push_str(",\"index\":");
        json_int(out, self.index);
        out.push('}');
    }
}

/// `toFlattenPlanTrace` transform plan into PlanTrace.
// go: toFlattenPlanTrace L1
fn to_flatten_plan_trace(root: &PlanTrace) -> Vec<PlanTrace> {
    let mut flatten = Vec::new();
    flatten_logical_plan_trace(root, &mut flatten);
    flatten
}

// go: flattenLogicalPlanTrace L1
fn flatten_logical_plan_trace(node: &PlanTrace, flatten: &mut Vec<PlanTrace>) {
    let mut new_node = PlanTrace {
        id: node.id,
        tp: node.tp.clone(),
        children_id: Some(Vec::new()),
        cost: node.cost,
        explain_info: node.explain_info.clone(),
        ..Default::default()
    };
    if node.children.is_empty() {
        flatten.push(new_node);
        return;
    }
    for child in &node.children {
        new_node.append_children_id(&[child.id]);
    }
    for child in &node.children {
        flatten_logical_plan_trace(child, flatten);
    }
    flatten.push(new_node);
}

/// `CETraceRecord` records an expression and related cardinality estimation result.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CETraceRecord {
    pub table_name: String,
    pub tp: String,
    pub expr: String,
    pub table_id: i64,
    pub row_count: u64,
}

impl CETraceRecord {
    /// `json.Marshal(rec)` (`TableID` is `json:"-"`).
    // go-std: encoding/json.structEncoder L2 (CETraceRecord's tags)
    pub fn marshal_json(&self) -> String {
        let mut out = String::from("{\"table_name\":");
        append_string(&mut out, &self.table_name);
        out.push_str(",\"type\":");
        append_string(&mut out, &self.tp);
        out.push_str(",\"expr\":");
        append_string(&mut out, &self.expr);
        out.push_str(",\"row_count\":");
        out.push_str(&self.row_count.to_string());
        out.push('}');
        out
    }
}

/// `DedupCETrace` deduplicate a slice of `*CETraceRecord` and return the deduplicated slice
/// (the first of equal records, in order).
// go: DedupCETrace L1
pub fn dedup_ce_trace(records: &[CETraceRecord]) -> Vec<CETraceRecord> {
    let mut ret = Vec::with_capacity(records.len());
    let mut exists = std::collections::HashSet::with_capacity(records.len());
    for rec in records {
        if exists.insert(rec.clone()) {
            ret.push(rec.clone());
        }
    }
    ret
}

/// `PhysicalOptimizeTracer` indicates the trace for the whole physicalOptimize processing.
/// Go's nil maps / slice are `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhysicalOptimizeTracer {
    pub physical_plan_cost_details: Option<BTreeMap<String, PhysicalPlanCostDetail>>,
    pub candidates: Option<BTreeMap<i64, CandidatePlanTrace>>,
    /// final indicates the final physical plan trace
    pub final_: Option<Vec<PlanTrace>>,
}

impl PhysicalOptimizeTracer {
    /// `AppendCandidate` appends physical CandidatePlanTrace in tracer. If the candidate
    /// already exists, the previous candidate would be covered depends on whether it has
    /// mapping logical plan. Like Go it panics (assignment to entry in nil map) when
    /// `Candidates` is nil.
    // go: PhysicalOptimizeTracer.AppendCandidate L1
    pub fn append_candidate(&mut self, c: CandidatePlanTrace) {
        let id = c
            .plan_trace
            .as_ref()
            .map(|p| p.id)
            .expect("runtime error: invalid memory address or nil pointer dereference");
        let candidates = self
            .candidates
            .as_mut()
            .expect("assignment to entry in nil map");
        if let Some(old) = candidates.get(&id)
            && !old.mapping_logical_plan.is_empty()
            && c.mapping_logical_plan.is_empty()
        {
            return;
        }
        candidates.insert(id, c);
    }

    /// `RecordFinalPlanTrace` records final physical plan trace.
    // go: PhysicalOptimizeTracer.RecordFinalPlanTrace L1
    pub fn record_final_plan_trace(&mut self, root: &PlanTrace) {
        self.final_ = Some(to_flatten_plan_trace(root));
        self.build_candidates_info();
    }

    /// `buildCandidatesInfo` builds candidates info.
    // go: PhysicalOptimizeTracer.buildCandidatesInfo L1
    fn build_candidates_info(&mut self) {
        let Some(candidates) = self.candidates.as_mut().filter(|c| !c.is_empty()) else {
            return;
        };
        let f_id: BTreeSet<i64> = self.final_.iter().flatten().map(|p| p.id).collect();
        for candidate in candidates.values_mut() {
            if let Some(p) = candidate.plan_trace.as_mut()
                && f_id.contains(&p.id)
            {
                p.selected = true;
            }
        }
    }

    /// `json.Marshal(tracer)`: `{"costs":…,"candidates":…,"final":…}`.
    // go-std: encoding/json.structEncoder L2 (PhysicalOptimizeTracer's tags)
    pub fn marshal_json(&self) -> Result<String, UnsupportedValueError> {
        let mut out = String::new();
        self.write(&mut out)?;
        Ok(out)
    }

    // go-std: encoding/json.structEncoder L2 (PhysicalOptimizeTracer; map keys sorted, int keys as strings)
    fn write(&self, out: &mut String) -> JsonResult {
        out.push_str("{\"costs\":");
        match &self.physical_plan_cost_details {
            None => out.push_str("null"),
            Some(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    append_string(out, k);
                    out.push(':');
                    v.write(out)?;
                }
                out.push('}');
            }
        }
        out.push_str(",\"candidates\":");
        match &self.candidates {
            None => out.push_str("null"),
            Some(m) => {
                let mut keys: Vec<(String, &CandidatePlanTrace)> =
                    m.iter().map(|(k, v)| (k.to_string(), v)).collect();
                keys.sort_by(|a, b| a.0.cmp(&b.0));
                out.push('{');
                for (i, (k, v)) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    append_string(out, k);
                    out.push(':');
                    v.write(out)?;
                }
                out.push('}');
            }
        }
        out.push_str(",\"final\":");
        json_slice(out, self.final_.as_deref(), |out, p| p.write(out))?;
        out.push('}');
        Ok(())
    }
}

/// `CandidatePlanTrace` indicates info for candidate. Go's embedded `*PlanTrace` may be nil
/// (`None`): its promoted fields are then left out of the JSON.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CandidatePlanTrace {
    pub plan_trace: Option<PlanTrace>,
    pub mapping_logical_plan: String,
}

impl CandidatePlanTrace {
    // go-std: encoding/json.structEncoder L2 (CandidatePlanTrace: embedded *PlanTrace promoted)
    fn write(&self, out: &mut String) -> JsonResult {
        out.push('{');
        if let Some(p) = &self.plan_trace {
            p.write_fields(out)?;
            out.push(',');
        }
        out.push_str("\"mapping\":");
        append_string(out, &self.mapping_logical_plan);
        out.push('}');
        Ok(())
    }
}

/// `CodecPlanName` returns tp_id of plan.
// go: CodecPlanName L1
pub fn codec_plan_name(tp: &str, id: i64) -> String {
    format!("{tp}_{id}")
}

/// `OptimizeTracer` indicates tracer for optimizer. Go's nil pointers are `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OptimizeTracer {
    /// Logical indicates logical plan
    pub logical: Option<LogicalOptimizeTracer>,
    /// Physical indicates physical plan
    pub physical: Option<PhysicalOptimizeTracer>,
    /// FinalPlan indicates the plan after post optimize
    pub final_plan: Option<Vec<PlanTrace>>,
    /// IsFastPlan indicates whether the plan is generated by fast plan
    pub is_fast_plan: bool,
}

impl OptimizeTracer {
    /// `SetFastPlan` sets fast plan.
    // go: OptimizeTracer.SetFastPlan L1
    pub fn set_fast_plan(&mut self, final_: &PlanTrace) {
        self.final_plan = Some(to_flatten_plan_trace(final_));
        self.is_fast_plan = true;
    }

    /// `RecordFinalPlan` records plan after post optimize.
    // go: OptimizeTracer.RecordFinalPlan L1
    pub fn record_final_plan(&mut self, final_: &PlanTrace) {
        self.final_plan = Some(to_flatten_plan_trace(final_));
    }

    /// `json.Marshal(tracer)`: `{"logical":…,"physical":…,"final":…,"isFastPlan":…}`.
    // go-std: encoding/json.structEncoder L2 (OptimizeTracer's tags)
    pub fn marshal_json(&self) -> Result<String, UnsupportedValueError> {
        let mut out = String::from("{\"logical\":");
        match &self.logical {
            None => out.push_str("null"),
            Some(l) => l.write(&mut out)?,
        }
        out.push_str(",\"physical\":");
        match &self.physical {
            None => out.push_str("null"),
            Some(p) => p.write(&mut out)?,
        }
        out.push_str(",\"final\":");
        json_slice(&mut out, self.final_plan.as_deref(), |out, p| p.write(out))?;
        out.push_str(",\"isFastPlan\":");
        out.push_str(if self.is_fast_plan { "true" } else { "false" });
        out.push('}');
        Ok(out)
    }
}

/// A value of the cost detail's `Params map[string]any`.
#[derive(Clone, Debug, PartialEq)]
pub enum CostParam {
    Float(f64),
    Int(i64),
    String(String),
    Bool(bool),
}

impl CostParam {
    // go-std: encoding/json.encodeState.reflectValue L2 (the dynamic types the planner records)
    fn write(&self, out: &mut String) -> JsonResult {
        match self {
            CostParam::Float(f) => json_float(out, *f)?,
            CostParam::Int(i) => json_int(out, *i),
            CostParam::String(s) => append_string(out, s),
            CostParam::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        }
        Ok(())
    }
}

/// `PhysicalPlanCostDetail` indicates cost detail.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhysicalPlanCostDetail {
    pub params: BTreeMap<String, CostParam>,
    pub tp: String,
    pub desc: String,
    pub id: i64,
    pub cost: f64,
}

/// `NewPhysicalPlanCostDetail` creates a cost detail.
// go: NewPhysicalPlanCostDetail L1
pub fn new_physical_plan_cost_detail(id: i64, tp: &str) -> PhysicalPlanCostDetail {
    PhysicalPlanCostDetail {
        id,
        tp: tp.to_string(),
        params: BTreeMap::new(),
        ..Default::default()
    }
}

impl PhysicalPlanCostDetail {
    /// `AddParam` adds param (an empty string value is discarded).
    // go: PhysicalPlanCostDetail.AddParam L1
    pub fn add_param(&mut self, k: &str, v: CostParam) -> &mut Self {
        // discard empty param value
        if let CostParam::String(s) = &v
            && s.is_empty()
        {
            return self;
        }
        self.params.insert(k.to_string(), v);
        self
    }

    /// `SetDesc` sets desc.
    // go: PhysicalPlanCostDetail.SetDesc L1
    pub fn set_desc(&mut self, desc: &str) {
        self.desc = desc.to_string();
    }

    /// `GetPlanID` gets plan id.
    // go: PhysicalPlanCostDetail.GetPlanID L1
    pub fn get_plan_id(&self) -> i64 {
        self.id
    }

    /// `GetPlanType` gets plan type.
    // go: PhysicalPlanCostDetail.GetPlanType L1
    pub fn get_plan_type(&self) -> &str {
        &self.tp
    }

    /// `Exists` checks whether key exists in params.
    // go: PhysicalPlanCostDetail.Exists L1
    pub fn exists(&self, k: &str) -> bool {
        self.params.contains_key(k)
    }

    /// `json.Marshal(d)`.
    // go-std: encoding/json.Marshal L2 (PhysicalPlanCostDetail)
    pub fn marshal_json(&self) -> Result<String, UnsupportedValueError> {
        let mut out = String::new();
        self.write(&mut out)?;
        Ok(out)
    }

    // go-std: encoding/json.structEncoder L2 (PhysicalPlanCostDetail's tags)
    fn write(&self, out: &mut String) -> JsonResult {
        out.push_str("{\"params\":{");
        for (i, (k, v)) in self.params.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            append_string(out, k);
            out.push(':');
            v.write(out)?;
        }
        out.push_str("},\"type\":");
        append_string(out, &self.tp);
        out.push_str(",\"desc\":");
        append_string(out, &self.desc);
        out.push_str(",\"id\":");
        json_int(out, self.id);
        out.push_str(",\"cost\":");
        json_float(out, self.cost)?;
        out.push('}');
        Ok(())
    }
}
