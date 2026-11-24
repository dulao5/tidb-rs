//! Replays the govec `basicexec` vectors (tidb-rs-tracker `govec/domains/basicexec.go`): the
//! limit, selection and table dual executors (D12.2) built by the executor builder from
//! physical plans and run Open-Next-Close as Go's `executor_required_rows_test.go` runs
//! them, and `LimitExec.adjustRequiredRows` on its boundaries.
//!
//! As in Go, the child of the limit and the selection is the table dual executor the
//! builder built for the plan's child, replaced by a data source that returns, on each
//! `Next`, min(required rows, rows left) rows (Go's `requiredRowsDataSource`).

use std::io::Read;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::mysql::TypeLonglong;
use tidb_chunk::chunk::Chunk;
use tidb_expression::expression::Expr;
use tidb_expression::schema::new_schema;
use tidb_expression::{Column, Constant, ExprRef, Schema};
use tidb_memory::tracker::new_tracker;
use tidb_planner::PhysicalPlan;
use tidb_planner::core::physical_plans::{PhysicalLimit, PhysicalSelection, PhysicalTableDual};
use tidb_planner::property::stats_info::StatsInfo;
use tidb_types::datum::new_int_datum;
use tidb_types::field_type::new_field_type;

use crate::adapter::TelemetryInfo;
use crate::adapter::tests::MockSession;
use crate::builder::new_executor_builder;
use crate::exec::ExecCtx;
use crate::glue::{Plan, SessionContext};
use crate::internal::exec::executor::{
    self as exec, BaseExecutorV2, Executor, new_base_executor, new_base_executor_v2,
};
use crate::select::LimitExec;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

/// Go's `defaultCtx()` of `executor_required_rows_test.go` with `MaxChunkSize = max`.
fn ctx_with_max(max: i64) -> MockSession {
    let mut ctx = MockSession::new(1);
    let vars = &mut ctx.vars;
    vars.init_chunk_size = 32;
    vars.max_chunk_size = max;
    let quota = vars.mem_quota_query;
    vars.stmt_ctx_mut().mem_tracker = Some(new_tracker(-1, quota));
    vars.snapshot_ts = 1;
    ctx
}

/// `n` BIGINT columns (UniqueID i+1, Index i).
fn schema(n: usize) -> Schema {
    new_schema(
        (0..n)
            .map(|i| {
                Arc::new(Column {
                    unique_id: i as i64 + 1,
                    index: i as isize,
                    ret_type: Arc::new(new_field_type(TypeLonglong)),
                    ..Column::default()
                })
            })
            .collect(),
    )
}

fn stats() -> Arc<StatsInfo> {
    Arc::new(StatsInfo {
        row_count: 1.0,
        ..StatsInfo::default()
    })
}

fn dual(schema: Schema, row_count: isize) -> Plan {
    let mut d = PhysicalTableDual {
        row_count,
        ..PhysicalTableDual::default()
    };
    d.set_schema(schema);
    d.sp.base.plan.set_stats(Some(stats()));
    Arc::new(d)
}

/// The value of row `i`, column `col` (`None` for NULL).
type ValueFn = Box<dyn Fn(usize, usize) -> Option<i64> + Send>;

/// Go's `requiredRowsDataSource` (the govec `beSource`).
struct Source {
    base: BaseExecutorV2,
    total: usize,
    count: usize,
    val: ValueFn,
    returned: Arc<Mutex<Vec<usize>>>,
}

#[async_trait]
impl Executor for Source {
    fn base(&self) -> &BaseExecutorV2 {
        &self.base
    }
    fn base_mut(&mut self) -> &mut BaseExecutorV2 {
        &mut self.base
    }
    async fn open(&mut self, _ctx: &ExecCtx) -> Result<(), Error> {
        Ok(())
    }
    async fn next(&mut self, _ctx: &ExecCtx, req: &mut Chunk) -> Result<(), Error> {
        req.reset();
        let mut n = 0;
        if self.count <= self.total {
            n = req.required_rows().min(self.total - self.count);
            for i in 0..n {
                for c in 0..req.num_cols() {
                    match (self.val)(self.count + i, c) {
                        Some(v) => req.append_int64(c, v),
                        None => req.append_null(c),
                    }
                }
            }
            self.count += n;
        }
        self.returned.lock().push(n);
        Ok(())
    }
    async fn close(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Builds `p`, replaces the child by a source, runs Open, a Next per required rows, Close.
async fn run(
    sctx: &mut MockSession,
    p: &Plan,
    child_schema: Schema,
    total: usize,
    val: ValueFn,
    required: &[i64],
    max: usize,
) -> Result<Value, Error> {
    let mut b = new_executor_builder(&*sctx, None, Arc::new(TelemetryInfo::default()));
    let Some(mut e) = b.build(sctx, Some(p)).await else {
        return Err(b.err.expect("no executor without an error"));
    };
    let returned = Arc::new(Mutex::new(Vec::new()));
    // The base of the built table dual: its schema, init cap 0 (its row count).
    let mut base = new_base_executor_v2(
        sctx.get_session_vars(),
        &sctx.sql_killer(),
        Some(child_schema),
        0,
        Vec::new(),
    );
    base.alloc.set_init_cap(0);
    e.set_all_children(vec![Box::new(Source {
        base,
        total,
        count: 0,
        val,
        returned: Arc::clone(&returned),
    })]);
    let ctx = ExecCtx::new(sctx.vars.stmt_ctx_arc());
    e.open(&ctx).await?;
    let mut chk = exec::new_first_chunk(e.as_ref());
    let mut next = Vec::new();
    for &r in required {
        chk.set_required_rows(r as isize, max);
        e.next(&ctx, &mut chk).await?;
        let vals: Vec<Value> = (0..chk.num_rows())
            .map(|i| {
                let row = chk.get_row(i);
                if row.is_null(0) {
                    Value::Null
                } else {
                    json!(row.get_int64(0))
                }
            })
            .collect();
        next.push(Value::Array(vals));
    }
    e.close().await?;
    let ds = returned.lock().clone();
    Ok(json!({"next": next, "ds": ds}))
}

fn ints(v: &Value) -> Vec<i64> {
    v.as_array()
        .map(|a| a.iter().map(|x| x.as_i64().unwrap()).collect())
        .unwrap_or_default()
}

async fn limit(cfg: &Value) -> Result<Value, Error> {
    let max = cfg["max"].as_i64().unwrap();
    let mut sctx = ctx_with_max(max);
    let child = schema(2);
    let cols = ints(&cfg["cols"])
        .into_iter()
        .map(|c| Arc::clone(&child.columns[c as usize]))
        .collect();
    let mut p = PhysicalLimit {
        offset: cfg["offset"].as_u64().unwrap(),
        count: cfg["count"].as_u64().unwrap(),
        ..PhysicalLimit::default()
    };
    p.set_schema(new_schema(cols));
    p.sp.base.children = vec![dual(child.clone(), 0)];
    let p: Plan = Arc::new(p);
    let val: ValueFn = Box::new(|i, c| Some(if c == 1 { -(i as i64) } else { i as i64 }));
    run(
        &mut sctx,
        &p,
        child,
        cfg["total"].as_u64().unwrap() as usize,
        val,
        &ints(&cfg["required"]),
        max as usize,
    )
    .await
}

async fn selection(cfg: &Value) -> Result<Value, Error> {
    let max = cfg["max"].as_i64().unwrap();
    let mut sctx = ctx_with_max(max);
    let child = schema(2);
    let col: ExprRef = Expr::Column(Column::clone(&child.columns[1])).into_ref();
    let mut conds = Vec::new();
    for f in cfg["filters"].as_array().into_iter().flatten() {
        let func = f["fn"].as_str().unwrap();
        let mut args = vec![ExprRef::clone(&col)];
        if func != "isnull" {
            args.push(
                Expr::Constant(Constant::new(
                    new_int_datum(f["val"].as_i64().unwrap()),
                    new_field_type(TypeLonglong),
                ))
                .into_ref(),
            );
        }
        conds.push(tidb_expression::scalar_function::new_function(
            &*sctx.get_expr_ctx(),
            func,
            new_field_type(TypeLonglong),
            args,
        )?);
    }
    let mut p = PhysicalSelection {
        conditions: conds,
        ..PhysicalSelection::default()
    };
    p.base.children = vec![dual(child.clone(), 0)];
    let p: Plan = Arc::new(p);
    let vals: Vec<Option<i64>> = cfg["vals"]
        .as_array()
        .unwrap()
        .iter()
        .map(Value::as_i64)
        .collect();
    let val: ValueFn = Box::new(move |i, c| {
        if c == 0 {
            Some(i as i64)
        } else {
            vals[i % vals.len()]
        }
    });
    run(
        &mut sctx,
        &p,
        child,
        cfg["total"].as_u64().unwrap() as usize,
        val,
        &ints(&cfg["required"]),
        max as usize,
    )
    .await
}

/// A limit as the builder builds it for `Offset = begin, Count = end - begin`, its cursor
/// at `cursor`, adjusting a chunk whose required rows are `required`.
fn adjust_required_rows(a: &[Value]) -> Value {
    let (begin, end, cursor) = (
        a[0].as_u64().unwrap(),
        a[1].as_u64().unwrap(),
        a[2].as_u64().unwrap(),
    );
    let (required, max) = (a[3].as_i64().unwrap(), a[4].as_i64().unwrap());
    let sctx = ctx_with_max(max);
    let count = end.wrapping_sub(begin);
    let mut base = new_base_executor(
        sctx.get_session_vars(),
        &sctx.sql_killer(),
        Some(schema(1)),
        0,
        Vec::new(),
    );
    base.base.alloc.set_init_cap(count.min(max as u64) as isize);
    let e = LimitExec {
        base,
        begin,
        end: begin.wrapping_add(count),
        cursor,
        meet_first_batch: false,
        child_result: None,
        column_idxs_used_by_child: None,
        column_swap_helper: None,
    };
    let mut chk = exec::new_first_chunk(&e);
    chk.set_required_rows(required as isize, max as usize);
    json!(e.adjust_required_rows(&mut chk).required_rows())
}

async fn table_dual(a: &[Value]) -> Result<Value, Error> {
    let row_count = a[0].as_i64().unwrap() as isize;
    let ncols = a[1].as_u64().unwrap() as usize;
    let max = a[2].as_i64().unwrap();
    let nexts = a[3].as_u64().unwrap();
    let mut sctx = ctx_with_max(max);
    let p = dual(schema(ncols), row_count);
    let mut b = new_executor_builder(&sctx, None, Arc::new(TelemetryInfo::default()));
    let Some(mut e) = b.build(&mut sctx, Some(&p)).await else {
        return Err(b.err.expect("no executor without an error"));
    };
    let ctx = ExecCtx::new(sctx.vars.stmt_ctx_arc());
    e.open(&ctx).await?;
    let mut chk = exec::new_first_chunk(e.as_ref());
    let mut out = Vec::new();
    for _ in 0..nexts {
        e.next(&ctx, &mut chk).await?;
        let nulls =
            (0..chk.num_rows()).all(|i| (0..chk.num_cols()).all(|c| chk.get_row(i).is_null(c)));
        out.push(json!({"rows": chk.num_rows(), "cols": chk.num_cols(), "nulls": nulls}));
    }
    e.close().await?;
    Ok(Value::Array(out))
}

#[tokio::test]
async fn govec_basicexec() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/basicexec.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(&path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "basicexec");
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let a = rec["args"].as_array().unwrap();
        let got = match rec["op"].as_str().unwrap() {
            "limit" => limit(&a[0]).await,
            "selection" => selection(&a[0]).await,
            "adjust_required_rows" => Ok(adjust_required_rows(a)),
            "table_dual" => table_dual(a).await,
            op => panic!("unknown op {op}"),
        };
        match (&rec["err"], got) {
            (Value::Null, Ok(out)) => assert_eq!(out, rec["out"], "{line}"),
            (Value::Null, Err(e)) => panic!("{line}: unexpected error {e}"),
            (err, Ok(out)) => panic!("{line}: Go's error {err}, got {out}"),
            (err, Err(e)) => assert_eq!(e.to_string(), err["msg"].as_str().unwrap(), "{line}"),
        }
    }
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}
