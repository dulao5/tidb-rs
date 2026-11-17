//! Replays the govec `logicalrules` vectors (VS1-23: the logical operators of the first
//! vertical slice, D09.14, and their rules: column pruning and projection elimination D09.7,
//! predicate push down and simplification D09.8, TopN / Limit push down D09.11) against Go
//! TiDB v8.5.8 (tracker `govec/domains/logicalrules.go`, which documents the record format).
//!
//! Go builds each record's query into a logical plan and records it ("in", expressions as
//! trees); this replay rebuilds the plan in the arena with Go's plan ids and column ids and
//! descriptor-only signatures (as the `rangerbuild` replay), runs the same rules in order and
//! compares the plan after each rule (operators, schemas, expressions with their types) and
//! the optimizer trace steps with Go's.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::blocked::{blocked, blocked_by};
use tidb_base::errors::RedactLogDisable;
use tidb_execdetails::tracing::opt_trace::{LogicalOptimizeTracer, PlanTrace};
use tidb_expression::eval::{self, BuiltinFunc};
use tidb_expression::{
    Column, Constant, CorrelatedColumn, Expr, ExprRef, ScalarFunction, new_schema,
};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_model::{ColumnInfo, StatePublic, TableInfo, new_extra_handle_col_info};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_planner::core::rule_column_pruning::ColumnPruner;
use tidb_planner::core::rule_eliminate_projection::ProjectionEliminator;
use tidb_planner::core::rule_predicate_push_down::PPDSolver;
use tidb_planner::core::rule_predicate_simplification::PredicateSimplification;
use tidb_planner::core::rule_topn_push_down::PushDownTopNOptimizer;
use tidb_planner::logicalop::{
    DataSource, LogicalLimit, LogicalProjection, LogicalSelection, LogicalSort, LogicalTableDual,
    LogicalTopN,
};
use tidb_planner::util::ByItems;
use tidb_planner::util::handle_cols::new_int_handle_cols;
use tidb_planner::util::optimizetrace::LogicalOptimizeOp;
use tidb_planner::{LogicalKind, LogicalOptRule, LogicalPlan, PlanCtx, PlanId};
use tidb_sessionctx::exprctx::EvalContext;
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::collate;
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::enum_type::Enum;
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::json::BinaryJSON;
use tidb_types::set_type::Set;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::parse_vector_float32;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

/// The rule steps that stop at a boundary, per `rule:block`, in the committed vectors (the
/// same on both targets; constitution §7.3). A boundary that starts answering, or a new
/// one, fails here: update the pin in the same PR.
///
/// All are the IN builtin (D08.9: its function class is not ported, so tidb-rs cannot build
/// or fold an `in(...)`): predicate simplification rebuilds the IN list of `a != c AND a IN
/// (...)` (an error in tidb-rs), constant propagation folds `a = c AND a IN (...)` into a
/// constant and the projection's column substitution rebuilds an IN over the projected
/// expressions (tidb-rs keeps the condition). A step of a record whose input has an IN and
/// whose result differs from Go's is counted here, not as a difference.
const EXPECTED_BLOCKED: &[(&str, usize)] = &[
    ("predicate_push_down:D08.9", 8),
    ("predicate_simplification:D08.9", 8),
];

/// Whether the record's input plan has an `in(...)` function (see `EXPECTED_BLOCKED`).
fn has_in(v: &Value) -> bool {
    match v {
        Value::Object(m) => {
            m.get("f").and_then(|f| f.get(0)).is_some_and(|n| n == "in") || m.values().any(has_in)
        }
        Value::Array(a) => a.iter().any(has_in),
        _ => false,
    }
}

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/logicalrules.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---------------------------------------------------------------- values

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
}

fn ungs(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Object(m) => unhex(m["hex"].as_str().unwrap()),
        _ => panic!("not a GoString: {v}"),
    }
}

fn hex_opt(v: &Value) -> Option<Vec<u8>> {
    if v.is_null() {
        None
    } else {
        Some(unhex(v.as_str().unwrap()))
    }
}

fn dec_of(s: &str) -> MyDecimal {
    let mut d = MyDecimal::default();
    let _ = d.from_string(s.as_bytes());
    d
}

fn bits_f(v: &Value) -> f64 {
    f64::from_bits(v.as_u64().unwrap())
}

fn datum_in(v: &Value) -> Datum {
    let p = v.as_array().unwrap();
    let k = p[0].as_u64().unwrap() as u8;
    let u = |i: usize| p[i].as_u64().unwrap();
    let i = |i: usize| p[i].as_i64().unwrap();
    let st = |i: usize| p[i].as_str().unwrap().to_owned();
    let mut d = Datum::default();
    match k {
        dm::KindNull => d.set_null(),
        dm::KindMinNotNull => d.set_min_not_null(),
        dm::KindMaxValue => d = dm::max_value_datum(),
        dm::KindInt64 => d.set_int64(i(1)),
        dm::KindUint64 => d.set_uint64(u(1)),
        dm::KindFloat32 => d.set_float32(bits_f(&p[1]) as f32),
        dm::KindFloat64 => d.set_float64(bits_f(&p[1])),
        dm::KindString => d.set_string(ungs(&p[1]), st(2)),
        dm::KindBytes => d.set_bytes(hex_opt(&p[1])),
        dm::KindBinaryLiteral => {
            d.set_binary_literal(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default()))
        }
        dm::KindMysqlBit => d.set_mysql_bit(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default())),
        dm::KindMysqlDecimal => {
            // The generator adds the decimal's state (MarshalJSON): constant folding leaves
            // states that FromString does not give (digitsInt rounded to a word).
            let mut dec = dec_of(p[1].as_str().unwrap());
            if let Some(st) = p.get(2) {
                dec.unmarshal_json(st.as_str().unwrap().as_bytes()).unwrap();
            }
            d.set_mysql_decimal(dec)
        }
        dm::KindMysqlDuration => d.set_mysql_duration(Duration {
            duration: i(1),
            fsp: i(2) as isize,
        }),
        dm::KindMysqlEnum => d.set_mysql_enum(
            Enum {
                value: u(1),
                name: ungs(&p[2]),
            },
            st(3),
        ),
        dm::KindMysqlSet => d.set_mysql_set(
            Set {
                value: u(1),
                name: ungs(&p[2]),
            },
            st(3),
        ),
        dm::KindMysqlTime => d.set_mysql_time(Time::from_raw(CoreTime(u(1)))),
        dm::KindRaw => d.set_raw(hex_opt(&p[1])),
        dm::KindMysqlJSON => d.set_mysql_json(BinaryJSON::owned(
            u(1) as u8,
            hex_opt(&p[2]).unwrap_or_default(),
        )),
        dm::KindVectorFloat32 => {
            d.set_vector_float32(parse_vector_float32(p[1].as_str().unwrap().as_bytes()).unwrap())
        }
        _ => panic!("datum_in: kind {k}"),
    }
    d
}

fn ft_in(v: &Value) -> FieldType {
    let p = v.as_array().unwrap();
    let mut ft = new_field_type_builder()
        .set_type(p[0].as_u64().unwrap() as u8)
        .set_flag(p[1].as_u64().unwrap() as usize)
        .set_flen(p[2].as_i64().unwrap() as isize)
        .set_decimal(p[3].as_i64().unwrap() as isize)
        .set_charset(p[4].as_str().unwrap())
        .set_collate(p[5].as_str().unwrap())
        .build_p();
    if !p[6].is_null() {
        ft.set_elems(Some(p[6].as_array().unwrap().iter().map(ungs).collect()));
    }
    ft
}

// ---------------------------------------------------------------- expressions

/// The error of evaluating a descriptor-only signature: the families are not ported here
/// (`tidb_shard` is D08.9).
fn sig_unsupported(f: &BuiltinFunc, _: &dyn EvalContext) -> Error {
    let block = match f.sig.name {
        "builtinTidbShardSig" => "D08.9",
        _ => "D08.x",
    };
    blocked(block, f.sig.name)
}

/// Descriptor-only signatures for the trees Go built (as the `expr` replay): Go's signature
/// type name and pb code (the push down checks read it); evaluating one is a boundary error.
fn sig_of(name: &str, pb_code: i32) -> &'static eval::SigDesc {
    static SIGS: OnceLock<Mutex<HashMap<(String, i32), &'static eval::SigDesc>>> = OnceLock::new();
    let mut m = SIGS.get_or_init(Default::default).lock().unwrap();
    m.entry((name.to_owned(), pb_code)).or_insert_with(|| {
        Box::leak(Box::new(eval::SigDesc {
            name: Box::leak(name.to_owned().into_boxed_str()),
            pb_code,
            vectorized: false,
            drivers: eval::SigDrivers::Unsupported {
                err: sig_unsupported,
            },
            args_fallback: eval::ArgsFallback::None,
        }))
    })
}

fn col_of(a: &[Value]) -> Column {
    Column {
        id: a[0].as_i64().unwrap(),
        unique_id: a[1].as_i64().unwrap(),
        index: a[2].as_i64().unwrap() as isize,
        ret_type: Arc::new(ft_in(&a[3])),
        orig_name: String::from_utf8(ungs(&a[4])).unwrap(),
        is_hidden: a[5].as_bool().unwrap(),
        virtual_expr: a.get(6).filter(|x| !x.is_null()).map(tree),
        ..Column::default()
    }
}

fn tree(v: &Value) -> ExprRef {
    let o = v.as_object().unwrap();
    if let Some(a) = o.get("col") {
        return Expr::Column(col_of(a.as_array().unwrap())).into_ref();
    }
    if let Some(a) = o.get("corr") {
        let a = a.as_array().unwrap();
        return Expr::CorrelatedColumn(CorrelatedColumn::new(Column {
            id: a[0].as_i64().unwrap(),
            unique_id: a[1].as_i64().unwrap(),
            ret_type: Arc::new(ft_in(&a[2])),
            orig_name: String::from_utf8(ungs(&a[3])).unwrap(),
            ..Column::default()
        }))
        .into_ref();
    }
    if let Some(a) = o.get("const") {
        let a = a.as_array().unwrap();
        return Expr::Constant(Constant::new(datum_in(&a[0]), ft_in(&a[1]))).into_ref();
    }
    let a = o["f"].as_array().unwrap();
    let args: Vec<ExprRef> = a[4].as_array().unwrap().iter().map(tree).collect();
    let ret = ft_in(&a[3]);
    let f = BuiltinFunc::new(
        sig_of(a[2].as_str().unwrap(), a[7].as_i64().unwrap() as i32),
        args,
        ret.clone(),
        a[5].as_str().unwrap(),
        a[6].as_str().unwrap(),
    );
    let sf = ScalarFunction::new(a[1].as_str().unwrap(), ret, f);
    sf.set_coercibility(tidb_expression::collation::Coercibility(
        a[8].as_i64().unwrap() as i32,
    ));
    let mut sf = sf;
    sf.function
        .collation_info
        .set_repertoire(tidb_expression::collation::Repertoire(
            a[9].as_i64().unwrap() as i32,
        ));
    sf.func_name = CIStr::new(a[1].as_str().unwrap());
    Expr::ScalarFunction(sf).into_ref()
}

fn trees(v: &Value) -> Vec<ExprRef> {
    v.as_array().unwrap().iter().map(tree).collect()
}

// ---------------------------------------------------------------- session

struct Acc;

impl GlobalVarAccessor for Acc {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

/// Go's `mock.Client`: every request type `kv.RequestTypeSupportedChecker` accepts.
struct MockClient;

impl Client for MockClient {
    fn send(
        &self,
        _: &tidb_kv::Context,
        _: &Request,
        _: Option<tidb_kv::kv::OptionValue>,
        _: &ClientSendOption,
    ) -> Box<dyn Response> {
        unreachable!("MockClient.send")
    }

    fn is_request_type_supported(&self, req_type: i64, sub_type: i64) -> bool {
        RequestTypeSupportedChecker.is_request_type_supported(req_type, sub_type)
    }
}

// ---------------------------------------------------------------- plan input

fn by_items(v: &Value) -> Vec<ByItems> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|b| ByItems {
            expr: tree(&b[0]),
            desc: b[1].as_bool().unwrap(),
        })
        .collect()
}

fn schema_of(v: &Value) -> tidb_expression::Schema {
    new_schema(
        v.as_array()
            .unwrap()
            .iter()
            .map(|c| Arc::new(col_of(c["col"].as_array().unwrap())))
            .collect(),
    )
}

fn handle_of(v: &Value) -> Option<tidb_planner::util::HandleCols> {
    if v.is_null() {
        return None;
    }
    if let Some(c) = v.get("int") {
        return Some(new_int_handle_cols(Arc::new(col_of(
            c["col"].as_array().unwrap(),
        ))));
    }
    panic!("common handles are not in the vectors: {v}");
}

fn table_of(v: &Value) -> Arc<TableInfo> {
    let cols = v["cols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            Arc::new(ColumnInfo {
                id: c[0].as_i64().unwrap(),
                name: CIStr::new(c[1].as_str().unwrap()),
                offset: c[2].as_i64().unwrap() as isize,
                field_type: ft_in(&c[3]),
                state: StatePublic,
                ..ColumnInfo::default()
            })
        })
        .collect();
    Arc::new(TableInfo {
        id: v["id"].as_i64().unwrap(),
        name: CIStr::new(v["name"].as_str().unwrap()),
        pk_is_handle: v["pk_handle"].as_bool().unwrap(),
        columns: Some(cols),
        state: StatePublic,
        ..TableInfo::default()
    })
}

/// Rebuilds a plan node (and its children) in the arena with Go's plan id.
fn build(ctx: &mut PlanCtx<'_>, v: &Value) -> PlanId {
    let children: Vec<PlanId> = v["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| build(ctx, c))
        .collect();
    let qb = v["qb"].as_i64().unwrap() as isize;
    let id = v["id"].as_i64().unwrap() as i32;
    let mut node: Box<dyn LogicalPlan> = match v["tp"].as_str().unwrap() {
        "DataSource" => {
            let tbl = table_of(&v["tbl"]);
            let cols = v["cols"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| {
                    let cid = c.as_i64().unwrap();
                    tbl.columns
                        .as_ref()
                        .unwrap()
                        .iter()
                        .find(|ci| ci.id == cid)
                        .cloned()
                        .unwrap_or_else(|| Arc::new(new_extra_handle_col_info()))
                })
                .collect();
            let db = CIStr::new(v["tbl"]["db"].as_str().unwrap());
            let mut ds = DataSource::init(ctx, tbl, db, cols, schema_of(&v["schema"]), qb);
            ds.handle_cols = handle_of(&v["handle"]);
            ds.un_mutable_handle_cols = handle_of(&v["unmut"]);
            ds.pushed_down_conds = trees(&v["pushed"]);
            ds.all_conds = trees(&v["all"]);
            for c in v["tbl_cols"].as_array().unwrap() {
                ds.append_table_col(Arc::new(col_of(c["col"].as_array().unwrap())));
            }
            Box::new(ds)
        }
        "Selection" => Box::new(LogicalSelection::init(trees(&v["conds"]), ctx, qb)),
        "Projection" => {
            let mut p = LogicalProjection::init(trees(&v["exprs"]), ctx, qb);
            p.calculate_no_delay = v["nodelay"].as_bool().unwrap();
            p.sp.set_schema(Some(schema_of(&v["schema"])));
            Box::new(p)
        }
        "Limit" => {
            let mut l = LogicalLimit::init(
                v["offset"].as_u64().unwrap(),
                v["count"].as_u64().unwrap(),
                ctx,
                qb,
            );
            l.sp.set_schema(Some(schema_of(&v["schema"])));
            Box::new(l)
        }
        "Sort" => Box::new(LogicalSort::init(by_items(&v["by"]), ctx, qb)),
        "TopN" => Box::new(LogicalTopN::init(
            by_items(&v["by"]),
            v["offset"].as_u64().unwrap(),
            v["count"].as_u64().unwrap(),
            ctx,
            qb,
        )),
        "TableDual" => {
            let mut d = LogicalTableDual::init(ctx, qb);
            d.row_count = v["rows"].as_i64().unwrap() as isize;
            d.sp.set_schema(Some(schema_of(&v["schema"])));
            Box::new(d)
        }
        tp => panic!("operator {tp}"),
    };
    node.base_mut().plan.set_id(id);
    node.set_children(children);
    ctx.arena.alloc(node)
}

// ---------------------------------------------------------------- plan output (light form)

fn ft_out(ft: &FieldType) -> Value {
    let elems = ft
        .get_elems()
        .map(|es| Value::Array(es.iter().map(|e| gs(e)).collect()))
        .unwrap_or(Value::Null);
    json!([
        ft.get_type(),
        ft.get_flag(),
        ft.get_flen(),
        ft.get_decimal(),
        ft.get_charset(),
        ft.get_collate(),
        elems
    ])
}

fn light(ctx: &PlanCtx<'_>, e: &Expr) -> Value {
    match e {
        Expr::Column(c) => json!([
            "c",
            c.unique_id,
            c.id,
            c.index,
            ft_out(&c.ret_type),
            gs(c.orig_name.as_bytes())
        ]),
        Expr::CorrelatedColumn(c) => json!(["r", c.column.unique_id, ft_out(&c.column.ret_type)]),
        Expr::Constant(k) => {
            let ectx = ctx.eval_ctx();
            json!([
                "k",
                gs(&k.string_with_ctx(&*ectx, RedactLogDisable)),
                ft_out(&k.ret_type)
            ])
        }
        Expr::ScalarFunction(f) => {
            let args: Vec<Value> = f.get_args().iter().map(|a| light(ctx, a)).collect();
            json!([
                "f",
                f.func_name.l,
                f.function.sig.name,
                ft_out(f.ret_type()),
                args
            ])
        }
    }
}

fn lights(ctx: &PlanCtx<'_>, es: &[ExprRef]) -> Value {
    Value::Array(es.iter().map(|e| light(ctx, e)).collect())
}

fn col_lights(ctx: &PlanCtx<'_>, cs: &[Arc<Column>]) -> Value {
    Value::Array(
        cs.iter()
            .map(|c| light(ctx, &Expr::Column(Column::clone(c))))
            .collect(),
    )
}

fn by_out(ctx: &PlanCtx<'_>, by: &[ByItems]) -> Value {
    Value::Array(
        by.iter()
            .map(|b| json!([light(ctx, &b.expr), b.desc]))
            .collect(),
    )
}

fn handle_out(ctx: &PlanCtx<'_>, h: &Option<tidb_planner::util::HandleCols>) -> Value {
    match h {
        None => Value::Null,
        Some(h) if h.is_int() => {
            json!({"int": light(ctx, &Expr::Column(Column::clone(h.get_col(0).unwrap())))})
        }
        Some(h) => {
            let cols: Vec<Value> = (0..h.num_cols())
                .map(|i| light(ctx, &Expr::Column(Column::clone(h.get_col(i).unwrap()))))
                .collect();
            json!({ "common": cols })
        }
    }
}

fn dump(ctx: &PlanCtx<'_>, p: PlanId) -> Value {
    let node = ctx.arena.get(p);
    let mut o = serde_json::Map::new();
    o.insert("tp".into(), json!(node.tp()));
    o.insert("id".into(), json!(node.id()));
    o.insert(
        "schema".into(),
        col_lights(ctx, &node.schema(&ctx.arena).columns),
    );
    match node.kind() {
        LogicalKind::DataSource => {
            let x = node.as_any().downcast_ref::<DataSource>().unwrap();
            o.insert(
                "cols".into(),
                Value::Array(x.columns.iter().map(|c| json!(c.id)).collect()),
            );
            o.insert("handle".into(), handle_out(ctx, &x.handle_cols));
            o.insert("unmut".into(), handle_out(ctx, &x.un_mutable_handle_cols));
            o.insert("pushed".into(), lights(ctx, &x.pushed_down_conds));
            o.insert("all".into(), lights(ctx, &x.all_conds));
        }
        LogicalKind::Selection => {
            let x = node.as_any().downcast_ref::<LogicalSelection>().unwrap();
            o.insert("conds".into(), lights(ctx, &x.conditions));
        }
        LogicalKind::Projection => {
            let x = node.as_any().downcast_ref::<LogicalProjection>().unwrap();
            o.insert("exprs".into(), lights(ctx, &x.exprs));
            o.insert("nodelay".into(), json!(x.calculate_no_delay));
        }
        LogicalKind::Limit => {
            let x = node.as_any().downcast_ref::<LogicalLimit>().unwrap();
            o.insert("offset".into(), json!(x.offset));
            o.insert("count".into(), json!(x.count));
        }
        LogicalKind::Sort => {
            let x = node.as_any().downcast_ref::<LogicalSort>().unwrap();
            o.insert("by".into(), by_out(ctx, &x.by_items));
        }
        LogicalKind::TopN => {
            let x = node.as_any().downcast_ref::<LogicalTopN>().unwrap();
            o.insert("by".into(), by_out(ctx, &x.by_items));
            o.insert("offset".into(), json!(x.offset));
            o.insert("count".into(), json!(x.count));
        }
        LogicalKind::TableDual => {
            let x = node.as_any().downcast_ref::<LogicalTableDual>().unwrap();
            o.insert("rows".into(), json!(x.row_count));
        }
        k => panic!("dump {k:?}"),
    }
    let children: Vec<Value> = node.children().iter().map(|&c| dump(ctx, c)).collect();
    o.insert("children".into(), Value::Array(children));
    Value::Object(o)
}

// ---------------------------------------------------------------- replay

fn rule_of(name: &str) -> Box<dyn LogicalOptRule> {
    match name {
        "column_prune" => Box::new(ColumnPruner),
        "projection_eliminate" => Box::new(ProjectionEliminator),
        "predicate_push_down" => Box::new(PPDSolver),
        "predicate_simplification" => Box::new(PredicateSimplification),
        "topn_push_down" => Box::new(PushDownTopNOptimizer),
        n => panic!("rule {n}"),
    }
}

enum StepGot {
    Plan(Value),
    Blocked(String),
}

/// Runs the record's rules; returns, per step, the plan or the boundary, plus the trace.
fn replay(args: &[Value], out: &Value) -> Vec<(StepGot, Value)> {
    collate::set_new_collation_enabled_for_test(true);
    let mut vars = SessionVars::new(Box::new(Acc));
    let mut ctx = PlanCtx::new(&mut vars, es::new_expr_context(vec![]));
    ctx.client = Some(Arc::new(MockClient));
    let mut p = build(&mut ctx, &out["in"]);
    ctx.vars.state.plan_id.0.store(
        out["plan_id"].as_i64().unwrap() as i32,
        std::sync::atomic::Ordering::SeqCst,
    );
    ctx.vars.state.plan_column_id.0.store(
        out["col_id"].as_i64().unwrap(),
        std::sync::atomic::Ordering::SeqCst,
    );
    let tracer = Arc::new(parking_lot::Mutex::new(LogicalOptimizeTracer::default()));
    let mut opt = LogicalOptimizeOp::default();
    opt.with_enable_optimize_tracer(Some(Arc::clone(&tracer)));
    let mut got = Vec::new();
    for (i, name) in args[2].as_array().unwrap().iter().enumerate() {
        let name = name.as_str().unwrap();
        opt.append_before_rule_optimize(i as isize, name, PlanTrace::default);
        match rule_of(name).optimize(&mut ctx, p, &opt) {
            Ok((np, _)) => {
                p = np;
                let t = tracer.lock();
                let steps = t.steps().unwrap();
                let trace: Vec<Value> = steps
                    .last()
                    .unwrap()
                    .steps
                    .iter()
                    .map(|s| json!([s.id, s.tp, gs(s.action.as_bytes()), gs(s.reason.as_bytes())]))
                    .collect();
                drop(t);
                got.push((StepGot::Plan(dump(&ctx, p)), Value::Array(trace)));
            }
            Err(e) => match blocked_by(&e) {
                Some(b) => {
                    got.push((StepGot::Blocked(b.to_string()), Value::Null));
                    break;
                }
                None if e
                    .to_string()
                    .contains("the function class of in is not ported yet") =>
                {
                    got.push((StepGot::Blocked("D08.9".to_string()), Value::Null));
                    break;
                }
                None => {
                    got.push((StepGot::Plan(json!({"err": e.to_string()})), Value::Null));
                    break;
                }
            },
        }
    }
    got
}

/// The first path where two JSON values differ, with both sides (for the report).
fn first_diff(a: &Value, b: &Value, path: String) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                match y.get(k) {
                    Some(w) => {
                        if let Some(d) = first_diff(v, w, format!("{path}.{k}")) {
                            return Some(d);
                        }
                    }
                    None => return Some(format!("{path}.{k}: missing in rust")),
                }
            }
            y.keys()
                .find(|k| !x.contains_key(*k))
                .map(|k| format!("{path}.{k}: extra in rust"))
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .find_map(|(i, (v, w))| first_diff(v, w, format!("{path}[{i}]"))),
        _ if a == b => None,
        _ => Some(format!("{path}:\n    go:   {a}\n    rust: {b}")),
    }
}

#[test]
fn govec_logicalrules() {
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"))
        .read_to_string(&mut s)
        .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "logicalrules");
    let (mut records, mut steps_equal, mut traces_equal) = (0usize, 0usize, 0usize);
    let mut blocked: BTreeMap<String, usize> = BTreeMap::new();
    let mut diffs = Vec::new();
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        records += 1;
        let args = rec["args"].as_array().unwrap();
        let out = &rec["out"];
        let got = match catch_unwind(AssertUnwindSafe(|| replay(args, out))) {
            Ok(g) => g,
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                diffs.push(format!("{}: panic {msg}", args[1]));
                continue;
            }
        };
        let want = out["steps"].as_array().unwrap();
        for (i, (g, trace)) in got.iter().enumerate() {
            let w = &want[i];
            match g {
                StepGot::Blocked(b) => {
                    *blocked
                        .entry(format!("{}:{b}", w["rule"].as_str().unwrap()))
                        .or_default() += 1;
                }
                StepGot::Plan(plan) => {
                    if plan == &w["plan"] {
                        steps_equal += 1;
                    } else if has_in(&out["in"]) {
                        *blocked
                            .entry(format!("{}:D08.9", w["rule"].as_str().unwrap()))
                            .or_default() += 1;
                        break;
                    } else {
                        diffs.push(format!(
                            "{} {} step {i} {}: {}",
                            args[1],
                            args[2],
                            w["rule"],
                            first_diff(&w["plan"], plan, String::new()).unwrap_or_default()
                        ));
                        break;
                    }
                    if trace == &w["trace"] {
                        traces_equal += 1;
                    } else {
                        diffs.push(format!(
                            "{} {} step {i} {} trace: {}",
                            args[1],
                            args[2],
                            w["rule"],
                            first_diff(&w["trace"], trace, String::new()).unwrap_or_default()
                        ));
                    }
                }
            }
        }
    }
    eprintln!(
        "logicalrules: {records} records, {steps_equal} rule steps equal to Go ({traces_equal} traces), blocked {blocked:?}, {} differences",
        diffs.len()
    );
    for d in diffs.iter().take(15) {
        eprintln!("{d}");
    }
    let pinned: BTreeMap<String, usize> = EXPECTED_BLOCKED
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();
    assert_eq!(blocked, pinned, "boundary counts changed");
    assert!(diffs.is_empty(), "{} differences", diffs.len());
}
