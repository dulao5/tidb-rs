//! tipb `DAGRequest` construction for TiKV: table scan, optional partial aggregation and
//! limit, built the way Go TiDB builds them (`pkg/planner/core/operator/physicalop/*ToPB`,
//! `pkg/util.ColumnsToProto`, `pkg/table/tables.SetPBColumnsDefaultValue`,
//! `pkg/executor/internal/builder.ConstructDAGReq`).

use protobuf::Message;
use tidb_meta::model::ColumnInfo;
use tidb_meta::model::FieldType;
use tidb_meta::model::TableInfo;
use tidb_meta::model::flag;
use tidb_meta::model::tp;
use tidb_query_datatype::codec::datum;
use tidb_query_datatype::codec::datum::Datum;
use tidb_query_datatype::codec::mysql::Decimal;
use tidb_query_datatype::expr::EvalContext;
use tipb::AggFunctionMode;
use tipb::Aggregation;
use tipb::DagRequest;
use tipb::EncodeType;
use tipb::ExecType;
use tipb::Executor;
use tipb::ExprType;
use tipb::Limit;
use tipb::TableScan;
use tipb_helper::ExprDefBuilder;

use crate::Error;
use crate::Result;

/// `DAGRequest.flags` bits (Go: `pkg/meta/model/flags.go`).
pub mod push_down_flag {
    pub const IGNORE_TRUNCATE: u64 = 1;
    pub const TRUNCATE_AS_WARNING: u64 = 1 << 1;
    pub const PAD_CHAR_TO_FULL_LENGTH: u64 = 1 << 2;
    pub const IN_INSERT_STMT: u64 = 1 << 3;
    pub const IN_UPDATE_OR_DELETE_STMT: u64 = 1 << 4;
    pub const IN_SELECT_STMT: u64 = 1 << 5;
    pub const OVERFLOW_AS_WARNING: u64 = 1 << 6;
    pub const IGNORE_ZERO_IN_DATE: u64 = 1 << 7;
    pub const DIVIDED_BY_ZERO_AS_WARNING: u64 = 1 << 8;
}

/// Statement-level settings sent with every DAG request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DagContext {
    /// Session time zone name (Go: `timeutil.Zone(loc)`; `"System"` for the TiDB server's
    /// local zone, an IANA name, or empty to use `time_zone_offset`).
    pub time_zone_name: String,
    /// Offset of the session time zone from UTC, in seconds.
    pub time_zone_offset: i64,
    /// Push-down flags (see [`push_down_flag`]).
    pub flags: u64,
    /// `div_precision_increment` if it differs from the default (4).
    pub div_precision_increment: Option<u32>,
    /// Whether the cluster runs with new collations (`new_collation_enabled` in
    /// `mysql.tidb`, true for clusters bootstrapped since v4.0); collation ids are then sent
    /// negated (Go: `collate.RewriteNewCollationIDIfNeeded`).
    pub new_collation_enabled: bool,
}

impl DagContext {
    /// A `SELECT` in the default (strict) SQL mode with time zone UTC: Go sets
    /// TruncateAsWarning (+ OverflowAsWarning), IgnoreZeroInDate, DividedByZeroAsWarning and
    /// InSelectStmt (`ResetContextOfStmt` + `StatementContext.PushDownFlags`).
    pub fn select_utc() -> DagContext {
        use push_down_flag::*;
        DagContext {
            time_zone_name: "UTC".to_owned(),
            time_zone_offset: 0,
            flags: IN_SELECT_STMT
                | TRUNCATE_AS_WARNING
                | OVERFLOW_AS_WARNING
                | IGNORE_ZERO_IN_DATE
                | DIVIDED_BY_ZERO_AS_WARNING,
            div_precision_increment: None,
            new_collation_enabled: true,
        }
    }
}

impl Default for DagContext {
    fn default() -> DagContext {
        DagContext::select_utc()
    }
}

/// Collation id of a collation name (Go: `mysql.CollationNames`), for the collations TiKV
/// implements.
pub fn collation_id(name: &str) -> Option<i32> {
    Some(match name {
        "binary" => 63,
        "utf8mb4_bin" => 46,
        "utf8mb4_general_ci" => 45,
        "utf8mb4_unicode_ci" => 224,
        "utf8mb4_0900_ai_ci" => 255,
        "utf8mb4_0900_bin" => 309,
        "utf8_bin" => 83,
        "utf8_general_ci" => 33,
        "utf8_unicode_ci" => 192,
        "ascii_bin" => 65,
        "latin1_bin" => 47,
        "gbk_bin" => 87,
        "gbk_chinese_ci" => 28,
        "gb18030_bin" => 249,
        "gb18030_chinese_ci" => 248,
        _ => return None,
    })
}

/// Collation id as sent to TiKV (Go: `collate.CollationToProto`). Empty means `binary`.
fn collation_to_proto(name: &str, ctx: &DagContext) -> Result<i32> {
    let name = if name.is_empty() { "binary" } else { name };
    let id = collation_id(name).ok_or_else(|| Error::Unsupported(format!("collation {name}")))?;
    Ok(if ctx.new_collation_enabled { -id } else { id })
}

/// tipb field type of a column/expression type (Go: `expression.ToPBFieldType`).
pub fn field_type_to_pb(ft: &FieldType, ctx: &DagContext) -> Result<tipb::FieldType> {
    let mut pb = tipb::FieldType::default();
    pb.set_tp(i32::from(ft.tp));
    pb.set_flag(ft.flag);
    // Go converts with `int32(...)`, which wraps (4294967295 becomes -1).
    pb.set_flen(ft.flen as i32);
    pb.set_decimal(ft.decimal as i32);
    pb.set_charset(ft.charset.clone());
    pb.set_collate(collation_to_proto(&ft.collate, ctx)?);
    pb.set_elems(ft.elems.clone().into());
    Ok(pb)
}

/// tipb column of a table scan (Go: `util.ColumnsToProto` + `SetPBColumnsDefaultValue`).
pub fn column_to_pb(
    table: &TableInfo,
    col: &ColumnInfo,
    ctx: &DagContext,
) -> Result<tipb::ColumnInfo> {
    let ft = &col.field_type;
    let mut pb = tipb::ColumnInfo::default();
    pb.set_column_id(col.id);
    pb.set_tp(i32::from(ft.tp));
    pb.set_collation(collation_to_proto(&ft.collate, ctx)?);
    pb.set_column_len(ft.flen as i32);
    pb.set_decimal(ft.decimal as i32);
    pb.set_flag(ft.flag as i32);
    pb.set_elems(ft.elems.clone().into());
    pb.set_pk_handle(table.pk_is_handle && ft.has_flag(flag::PRI_KEY));
    if col.is_virtual_generated() {
        // TiKV has no data for virtual columns; TiDB computes them later.
        pb.set_default_val(vec![datum::NIL_FLAG]);
    } else if let Some(d) = origin_default_datum(col)? {
        let mut ectx = EvalContext::default();
        pb.set_default_val(
            datum::encode_value(&mut ectx, &[d])
                .map_err(|e| Error::Codec(format!("origin default of {}: {e}", col.name.o)))?,
        );
    }
    Ok(pb)
}

/// The value TiKV must use for rows written before `col` existed (Go:
/// `table.GetColOriginDefaultValueWithoutStrictSQLMode`). `None` when the column has no
/// origin default (TiKV then returns NULL, or errors for NOT NULL columns).
fn origin_default_datum(col: &ColumnInfo) -> Result<Option<Datum>> {
    let v = &col.origin_default;
    if v.is_null() {
        return Ok(None);
    }
    let text = match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let ft = &col.field_type;
    let unsupported = || {
        Error::Unsupported(format!(
            "origin default {text:?} of column {} (type {})",
            col.name.o, ft.tp
        ))
    };
    let d = match ft.tp {
        tp::TINY | tp::SHORT | tp::INT24 | tp::LONG | tp::LONGLONG | tp::YEAR => {
            if ft.is_unsigned() {
                Datum::U64(text.trim().parse().map_err(|_| unsupported())?)
            } else {
                Datum::I64(text.trim().parse().map_err(|_| unsupported())?)
            }
        }
        tp::FLOAT | tp::DOUBLE => Datum::F64(text.trim().parse().map_err(|_| unsupported())?),
        tp::NEWDECIMAL => Datum::Dec(text.trim().parse::<Decimal>().map_err(|_| unsupported())?),
        tp::VARCHAR
        | tp::VAR_STRING
        | tp::STRING
        | tp::TINY_BLOB
        | tp::MEDIUM_BLOB
        | tp::LONG_BLOB
        | tp::BLOB => Datum::Bytes(text.into_bytes()),
        _ => return Err(unsupported()),
    };
    Ok(Some(d))
}

/// A pushed-down aggregate function over a scan output column (by offset into
/// [`DagSpec::columns`]). Executed in TiDB's partial mode (`Partial1Mode`): each region
/// returns its partial result, which the caller merges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    Count(usize),
    Sum(usize),
    Max(usize),
    Min(usize),
    FirstRow(usize),
}

impl AggFunc {
    fn arg(self) -> usize {
        match self {
            AggFunc::Count(c)
            | AggFunc::Sum(c)
            | AggFunc::Max(c)
            | AggFunc::Min(c)
            | AggFunc::FirstRow(c) => c,
        }
    }

    fn expr_type(self) -> ExprType {
        match self {
            AggFunc::Count(_) => ExprType::Count,
            AggFunc::Sum(_) => ExprType::Sum,
            AggFunc::Max(_) => ExprType::Max,
            AggFunc::Min(_) => ExprType::Min,
            AggFunc::FirstRow(_) => ExprType::First,
        }
    }

    /// Result type (Go: `baseFuncDesc.typeInfer4*`).
    pub fn ret_type(self, arg: &FieldType) -> FieldType {
        let binary = |tp: u8, flen: i64, decimal: i64, extra_flag: u32| FieldType {
            tp,
            flag: flag::BINARY | extra_flag,
            flen,
            decimal,
            charset: "binary".to_owned(),
            collate: "binary".to_owned(),
            ..FieldType::default()
        };
        const MAX_DECIMAL_WIDTH: i64 = 65;
        const MAX_DECIMAL_SCALE: i64 = 30;
        const MAX_REAL_WIDTH: i64 = 23;
        match self {
            AggFunc::Count(_) => binary(tp::LONGLONG, 21, 0, flag::NOT_NULL),
            AggFunc::Sum(_) => match arg.tp {
                tp::TINY | tp::SHORT | tp::INT24 | tp::LONG | tp::LONGLONG | tp::YEAR => {
                    let flen = if arg.flen < 0 {
                        MAX_DECIMAL_WIDTH
                    } else {
                        (arg.flen + 21).min(MAX_DECIMAL_WIDTH)
                    };
                    binary(tp::NEWDECIMAL, flen, 0, 0)
                }
                tp::NEWDECIMAL => {
                    let decimal = arg.decimal.clamp(0, MAX_DECIMAL_SCALE);
                    let flen = (arg.flen + 22).min(MAX_DECIMAL_WIDTH);
                    binary(tp::NEWDECIMAL, flen, decimal, 0)
                }
                tp::FLOAT | tp::DOUBLE => binary(tp::DOUBLE, MAX_REAL_WIDTH, arg.decimal, 0),
                _ => binary(tp::DOUBLE, MAX_REAL_WIDTH, -1, 0),
            },
            AggFunc::Max(_) | AggFunc::Min(_) => {
                let mut ft = arg.clone();
                ft.flag &= !flag::NOT_NULL;
                ft
            }
            AggFunc::FirstRow(_) => arg.clone(),
        }
    }
}

/// What to push down: `TableScan(columns) [-> Aggregation] [-> Limit]`.
#[derive(Clone, Debug)]
pub struct DagSpec<'a> {
    pub table: &'a TableInfo,
    /// Scanned columns, in output order. For `pk_is_handle` tables the primary key column is
    /// filled from the handle.
    pub columns: Vec<&'a ColumnInfo>,
    /// Scan in descending handle order (the coprocessor request must set `desc` too).
    pub desc: bool,
    /// Partial aggregation: functions, then group-by columns (offsets into `columns`).
    pub aggregation: Option<(Vec<AggFunc>, Vec<usize>)>,
    /// Limit applied by each region task.
    pub limit: Option<u64>,
}

/// A built request and the types of the rows it returns.
#[derive(Clone, Debug)]
pub struct DagPlan {
    pub request: DagRequest,
    /// Output column types: the scanned columns, or for an aggregation its function results
    /// followed by the group-by columns (TiKV's aggregation output schema).
    pub output_types: Vec<FieldType>,
}

impl DagPlan {
    /// Protobuf bytes of the request, the payload of a coprocessor DAG request.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.request
            .write_to_bytes()
            .map_err(|e| Error::Codec(format!("encode DAGRequest: {e}")))
    }
}

/// Build the DAG request for `spec`.
///
/// Rows come back with `EncodeType::TypeDefault` (datum-encoded rows): it is what the
/// vendored codecs decode most directly ([`crate::decode_select_response`]), and does not
/// depend on TiKV's chunk-format type support. Go TiDB uses `TypeChunk` when possible; the
/// row values are the same.
pub fn build_dag(spec: &DagSpec<'_>, ctx: &DagContext) -> Result<DagPlan> {
    spec.table
        .check_int_handle_readable()
        .map_err(|e| Error::Unsupported(e.to_string()))?;
    if spec.columns.is_empty() {
        return Err(Error::Unsupported("table scan without columns".to_owned()));
    }
    if spec.limit == Some(0) {
        // TiKV (v8.5) asserts on the empty batch a pushed Limit 0 asks its scan for and
        // panics the whole store. Go never pushes one (LIMIT 0 plans as TableDual).
        return Err(Error::Unsupported("pushed-down Limit 0".to_owned()));
    }
    let mut executors = Vec::new();

    let mut scan = TableScan::default();
    scan.set_table_id(spec.table.id);
    scan.set_desc(spec.desc);
    for col in &spec.columns {
        if !col.is_public() {
            return Err(Error::Unsupported(format!(
                "column {} is not public",
                col.name.o
            )));
        }
        scan.mut_columns().push(column_to_pb(spec.table, col, ctx)?);
    }
    let mut exec = Executor::default();
    exec.set_tp(ExecType::TypeTableScan);
    exec.set_tbl_scan(scan);
    exec.set_executor_id(String::new());
    executors.push(exec);

    let mut output_types: Vec<FieldType> =
        spec.columns.iter().map(|c| c.field_type.clone()).collect();

    if let Some((funcs, group_by)) = &spec.aggregation {
        let input = output_types;
        let col_ref = |offset: usize| -> Result<tipb::Expr> {
            let ft = input.get(offset).ok_or_else(|| {
                Error::Unsupported(format!("aggregation input offset {offset} out of range"))
            })?;
            Ok(ExprDefBuilder::column_ref(offset, field_type_to_pb(ft, ctx)?).build())
        };
        let mut agg = Aggregation::default();
        let mut out = Vec::new();
        for &f in funcs {
            let arg = col_ref(f.arg())?;
            let ret = f.ret_type(&input[f.arg()]);
            let mut expr = ExprDefBuilder::aggr_func(f.expr_type(), field_type_to_pb(&ret, ctx)?)
                .push_child(arg)
                .build();
            expr.set_agg_func_mode(AggFunctionMode::Partial1Mode);
            agg.mut_agg_func().push(expr);
            out.push(ret);
        }
        for &g in group_by {
            agg.mut_group_by().push(col_ref(g)?);
            out.push(input[g].clone());
        }
        let mut exec = Executor::default();
        // Go pushes HashAgg as TypeAggregation (StreamAgg only over ordered input).
        exec.set_tp(ExecType::TypeAggregation);
        exec.set_aggregation(agg);
        exec.set_executor_id(String::new());
        executors.push(exec);
        output_types = out;
    }

    if let Some(n) = spec.limit {
        let mut limit = Limit::default();
        limit.set_limit(n);
        let mut exec = Executor::default();
        exec.set_tp(ExecType::TypeLimit);
        exec.set_limit(limit);
        exec.set_executor_id(String::new());
        executors.push(exec);
    }

    let mut request = DagRequest::default();
    request.set_executors(executors.into());
    request.set_output_offsets((0..output_types.len() as u32).collect());
    request.set_encode_type(EncodeType::TypeDefault);
    request.set_time_zone_name(ctx.time_zone_name.clone());
    request.set_time_zone_offset(ctx.time_zone_offset);
    request.set_flags(ctx.flags);
    if let Some(d) = ctx.div_precision_increment {
        request.set_div_precision_increment(d);
    }
    Ok(DagPlan {
        request,
        output_types,
    })
}
