//! go-file: pkg/table/column.go
//!
//! Column metadata, value casting for columns and column default values (block D06.4 part
//! 1). Not here: `NewColDesc` / `ColDescFieldNames` (SHOW COLUMNS, part 2), and the write
//! path (`CheckOnce`, `CheckNotNull`, `HandleBadNull`, `CheckNoDefaultValueForInsert`,
//! `FindOnUpdateCols`, `CastColumnValueWithStrictMode`), permanent exemption "write path".
//!
//! AST values (`GeneratedExpr`, `DefaultExpr`) are not kept in the column: the parser AST is
//! not `Send` (ADR 0004; VS1-04 changes that), and a table is shared between sessions. A
//! [`ClonableExprNode`] keeps Go's constructor and builds the node when it is asked for,
//! which is what Go's `Clone` does; `Internal` builds one too (L2, no shared node).

use std::ops::Deref;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_base::mysql::{
    self, TypeBit, TypeBlob, TypeDate, TypeDatetime, TypeDouble, TypeDuration, TypeEnum, TypeFloat,
    TypeInt24, TypeJSON, TypeLong, TypeLongBlob, TypeLonglong, TypeMediumBlob, TypeNewDecimal,
    TypeSet, TypeShort, TypeString, TypeTiDBVectorFloat32, TypeTimestamp, TypeTiny, TypeTinyBlob,
    TypeVarString, TypeVarchar, TypeYear,
};
use tidb_expression::column::Column as ExprColumn;
use tidb_expression::expression::EvalState;
use tidb_expression::helper::{TimeValueArg, get_time_value};
use tidb_model::ColumnInfo;
use tidb_model::DefaultValueRef;
use tidb_parser::ast;
use tidb_parser::gocharset::CharsetBin;
use tidb_parser::gocharset::encoding_gbk::ErrInvalidCharacterString;
use tidb_parser::types::FieldType;
use tidb_sessionctx::errctx::{self, ErrGroupDupKey};
use tidb_sessionctx::exprctx::BuildContext;
use tidb_sessionctx::variable::session::SessionVars;
use tidb_types::context::Context as TypeContext;
use tidb_types::datum::{
    Datum, GoValue, KindFloat32, KindFloat64, KindInt64, KindMysqlDecimal, KindNull,
    new_collate_mysql_enum_datum, new_datum,
};
use tidb_types::{enum_type, errors as terr, etc, helper, time as ttime};

use super::errGetDefaultFailed;
use super::{ErrColumnCantNull, ErrNoDefaultValue, ErrTruncatedWrongValueForField};

/// Column provides meta data describing a table column.
///
/// Go embeds `*model.ColumnInfo`; the column shares the table's info here (`Deref`).
pub struct Column {
    /// The embedded `*model.ColumnInfo`.
    pub info: Arc<ColumnInfo>,
    /// If this column is a generated column, the expression will be stored here.
    pub generated_expr: Option<ClonableExprNode>,
    /// If this column has default expr value, this expression will be stored here.
    pub default_expr: Option<ClonableExprNode>,
}

/// A shared `*table.Column`.
pub type ColumnRef = Arc<Column>;

impl Deref for Column {
    type Target = ColumnInfo;
    // glue: Go struct embedding of *model.ColumnInfo
    fn deref(&self) -> &ColumnInfo {
        &self.info
    }
}

/// The constructor of a [`ClonableExprNode`].
pub type ExprCtor = Arc<dyn Fn() -> Result<ast::Expr, Error> + Send + Sync>;

/// ClonableExprNode is a wrapper for ast.ExprNode.
#[derive(Clone)]
pub struct ClonableExprNode {
    ctor: ExprCtor,
}

/// NewClonableExprNode creates a ClonableExprNode. Go also takes the node built by the
/// caller (`internal`); it is not kept (module docs).
// go: NewClonableExprNode L2 (the internal node is rebuilt on demand, module docs)
pub fn new_clonable_expr_node(ctor: ExprCtor) -> ClonableExprNode {
    ClonableExprNode { ctor }
}

impl ClonableExprNode {
    /// Clone makes a "copy" of internal ast.ExprNode by reconstructing it.
    // go: ClonableExprNode.Clone L2 (the constructor's error is returned instead of the stale internal node)
    pub fn clone_node(&self) -> Result<ast::Expr, Error> {
        (self.ctor)()
    }

    /// Internal returns the reference of the internal ast.ExprNode. Here it is built by the
    /// constructor (module docs).
    // go: ClonableExprNode.Internal L2 (built by the constructor, module docs)
    pub fn internal(&self) -> Result<ast::Expr, Error> {
        (self.ctor)()
    }
}

impl Column {
    /// String implements fmt.Stringer interface.
    // go: Column.String L1 [M-NAME]
    pub fn string(&self) -> String {
        let mut ans = vec![
            self.name.o.clone(),
            tidb_parser::types::type_to_str(
                self.field_type.get_type(),
                self.field_type.get_charset(),
            ),
        ];
        if mysql::has_auto_increment_flag(self.field_type.get_flag()) {
            ans.push("AUTO_INCREMENT".to_owned());
        }
        if mysql::has_not_null_flag(self.field_type.get_flag()) {
            ans.push("NOT NULL".to_owned());
        }
        ans.join(" ")
    }

    /// ToInfo casts Column to model.ColumnInfo
    /// NOTE: DONT modify return value.
    // go: Column.ToInfo L1
    pub fn to_info(&self) -> &Arc<ColumnInfo> {
        &self.info
    }

    /// IsPKHandleColumn checks if the column is primary key handle column.
    // go: Column.IsPKHandleColumn L1
    pub fn is_pk_handle_column(&self, tb_info: &tidb_model::TableInfo) -> bool {
        mysql::has_pri_key_flag(self.field_type.get_flag()) && tb_info.pk_is_handle
    }

    /// IsCommonHandleColumn checks if the column is common handle column.
    // go: Column.IsCommonHandleColumn L1
    pub fn is_common_handle_column(&self, tb_info: &tidb_model::TableInfo) -> bool {
        mysql::has_pri_key_flag(self.field_type.get_flag()) && tb_info.is_common_handle
    }
}

/// FindCol finds column in cols by name.
// go: FindCol L1
pub fn find_col<'a>(cols: &'a [ColumnRef], name: &str) -> Option<&'a ColumnRef> {
    cols.iter()
        .find(|col| tidb_base::gostd::equal_fold(&col.name.o, name))
}

/// FindColLowerCase finds column in cols by name. It assumes the name is lowercase.
// go: FindColLowerCase L1
pub fn find_col_lower_case<'a>(cols: &'a [ColumnRef], name: &str) -> Option<&'a ColumnRef> {
    cols.iter().find(|col| col.name.l == name)
}

/// ToColumn converts a *model.ColumnInfo to *Column.
// go: ToColumn L1
pub fn to_column(col: Arc<ColumnInfo>) -> Column {
    Column {
        info: col,
        generated_expr: None,
        default_expr: None,
    }
}

/// The `&Column{ColumnInfo: model.NewExtraHandleColInfo()}` of `FindCols` / `FindColumns`,
/// with its offset set to `offset`.
// go-part: FindCols
fn extra_handle_column(offset: usize) -> ColumnRef {
    let mut info = tidb_model::new_extra_handle_col_info();
    info.offset = offset as isize;
    Arc::new(to_column(Arc::new(info)))
}

/// FindCols finds columns in cols by names. If pkIsHandle is false and name is
/// ExtraHandleName, the extra handle column will be added. If any columns don't match,
/// return nil and the first missing column's name. Please consider FindColumns() first for
/// a better performance.
// go: FindCols L1
pub fn find_cols(
    cols: &[ColumnRef],
    names: &[String],
    pk_is_handle: bool,
) -> (Option<Vec<ColumnRef>>, String) {
    let mut rcols: Vec<ColumnRef> = Vec::new();
    for name in names {
        if let Some(col) = find_col(cols, name) {
            rcols.push(col.clone());
        } else if *name == tidb_model::ExtraHandleName.l && !pk_is_handle {
            rcols.push(extra_handle_column(cols.len()));
        } else {
            return (None, name.clone());
        }
    }
    // Go returns a nil slice when names is empty.
    ((!rcols.is_empty()).then_some(rcols), String::new())
}

/// FindColumns finds columns in cols by names with a better performance than FindCols(). It
/// assumes names are lowercase.
// go: FindColumns L1 [M-INT]
pub fn find_columns(
    cols: &[ColumnRef],
    names: &[String],
    pk_is_handle: bool,
) -> (Option<Vec<ColumnRef>>, isize) {
    let mut rcols: Vec<ColumnRef> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        if let Some(col) = find_col_lower_case(cols, name) {
            rcols.push(col.clone());
        } else if *name == tidb_model::ExtraHandleName.l && !pk_is_handle {
            rcols.push(extra_handle_column(cols.len()));
        } else {
            return (None, i as isize);
        }
    }
    ((!rcols.is_empty()).then_some(rcols), -1)
}

/// truncateTrailingSpaces truncates trailing spaces for CHAR[(M)] column.
/// fix: https://github.com/pingcap/tidb/issues/3660
// go: truncateTrailingSpaces L1
fn truncate_trailing_spaces(v: &mut Datum) {
    if v.kind() == KindNull {
        return;
    }
    let b = v.get_bytes();
    let mut length = b.len();
    while length > 0 && b[length - 1] == b' ' {
        length -= 1;
    }
    let s = b[..length].to_vec();
    let collation = v.collation().to_owned();
    v.set_string(s, collation);
}

/// convertToIncorrectStringErr converts ErrInvalidCharacterString to
/// ErrTruncatedWrongValueForField. The first argument is the invalid character in bytes.
// go: convertToIncorrectStringErr L1 [M-ERR]
fn convert_to_incorrect_string_err(err: Error, col_name: &str) -> Error {
    let args = err.args();
    if args.len() != 2 {
        return err;
    }
    let Arg::Str(invalid_str_hex) = &args[1] else {
        return err;
    };
    let mut res = Vec::with_capacity(invalid_str_hex.len() * 2);
    for (i, &c) in invalid_str_hex.iter().enumerate() {
        if i % 2 == 0 {
            res.extend_from_slice(b"\\x");
        }
        res.push(c);
    }
    ErrTruncatedWrongValueForField.fast_gen(
        "Incorrect string value '%s' for column '%s'",
        vec![Arg::Str(res.into()), Arg::from(col_name.to_owned())],
    )
}

/// handleZeroDatetime handles Timestamp/Datetime/Date zero date and invalid dates.
/// Currently only called from CastValue.
/// returns:
///
///   value (possibly adjusted)
///   boolean; true if break error/warning handling in CastValue and return what was returned from this
///   error
// go: handleZeroDatetime L1 [M-ERRVAL]
fn handle_zero_datetime(
    ec: &errctx::Context,
    mode: mysql::SQLMode,
    ft: &FieldType,
    casted: Datum,
    str: &[u8],
    tm_is_invalid: bool,
) -> (Datum, bool, Result<(), Error>) {
    let tm = casted.get_mysql_time();

    let (zero_v, zero_t) = match ft.get_type() {
        TypeDate => (*ttime::ZeroDate, terr::DateStr),
        TypeDatetime => (*ttime::ZeroDatetime, terr::DateTimeStr),
        TypeTimestamp => (*ttime::ZeroTimestamp, terr::TimestampStr),
        // Go's zero values: types.Time{} and "".
        _ => (ttime::Time::default(), ""),
    };

    // ref https://dev.mysql.com/doc/refman/8.0/en/sql-mode.html#sqlmode_no_zero_date
    // (see the Go source for the full rules)
    let ignore_err = ec.level_for_group(ErrGroupDupKey) != errctx::LevelError;

    let wrong_value = || {
        terr::ErrWrongValue.fast_gen_by_args(vec![
            Arg::from(zero_t.to_owned()),
            Arg::Str(str.to_vec().into()),
        ])
    };
    // Timestamp in MySQL is since EPOCH 1970-01-01 00:00:00 UTC and can by definition not
    // have invalid dates! Zero date is special for MySQL timestamp and *NOT* 1970-01-01
    // 00:00:00, but 0000-00-00 00:00:00!
    if tm.is_zero() && ft.get_type() == TypeTimestamp {
        let inner_err = wrong_value();
        if mode.has_strict_mode() && !ignore_err && (tm_is_invalid || mode.has_no_zero_date_mode())
        {
            return (new_datum(GoValue::Time(zero_v)), true, Err(inner_err));
        }

        if tm_is_invalid || mode.has_no_zero_date_mode() {
            ec.append_warning(inner_err);
        }
        return (new_datum(GoValue::Time(zero_v)), true, Ok(()));
    } else if tm_is_invalid && ft.get_type() == TypeTimestamp {
        // Prevent from being stored! Invalid timestamp!
        let warn = wrong_value();
        if mode.has_strict_mode() {
            return (new_datum(GoValue::Time(zero_v)), true, Err(warn));
        }
        // no strict mode, truncate to 0000-00-00 00:00:00
        ec.append_warning(warn);
        return (new_datum(GoValue::Time(zero_v)), true, Ok(()));
    } else if tm.is_zero() || tm.invalid_zero() {
        if tm.is_zero() {
            // Don't care NoZeroDate mode if time val is invalid.
            if !tm_is_invalid && !mode.has_no_zero_date_mode() {
                return (new_datum(GoValue::Time(zero_v)), true, Ok(()));
            }
        } else if tm.invalid_zero() && !mode.has_no_zero_in_date_mode() {
            return (casted, true, Ok(()));
        }

        let inner_err = wrong_value();
        if mode.has_strict_mode() && !ignore_err {
            return (new_datum(GoValue::Time(zero_v)), true, Err(inner_err));
        }

        // TODO: as in MySQL 8.0's implement, warning message is `types.ErrWarnDataOutOfRange`,
        // but this error message need a `rowIdx` argument, in this context, the `rowIdx` is
        // missing. And refactor this function seems too complicated, so we set the warning
        // message the same to error's.
        ec.append_warning(inner_err);
        return (new_datum(GoValue::Time(zero_v)), true, Ok(()));
    }

    (casted, false, Ok(()))
}

/// CastValue casts a value based on column type. If forceIgnoreTruncate is true, truncated
/// errors will be ignored. If returnErr is true, directly return any conversion errors.
/// It's safe now and it's the same as the behavior of select statement. Set it to true only
/// in FillVirtualColumnValue and UnionScanExec.Next() If the handle of err is changed
/// latter, the behavior of forceIgnoreTruncate also need to change.
// go: CastValue L1 [M-CTX, M-ERRVAL]
pub fn cast_value(
    vars: &SessionVars,
    val: Datum,
    col: &ColumnInfo,
    return_err: bool,
    force_ignore_truncate: bool,
) -> (Datum, Result<(), Error>) {
    let sc = vars.stmt_ctx();
    cast_column_value_inner(
        &sc.type_ctx(),
        &sc.err_ctx(),
        mysql::SQLMode(vars.sql_mode),
        val,
        &col.field_type,
        &col.name.o,
        vars.connection_id,
        return_err,
        force_ignore_truncate,
    )
}

/// CastColumnValue casts a value based on column type with expression BuildContext
// go: CastColumnValue L1 [M-ERRVAL]
pub fn cast_column_value(
    ctx: &dyn BuildContext,
    val: Datum,
    col: &ColumnInfo,
    return_err: bool,
    force_ignore_truncate: bool,
) -> (Datum, Result<(), Error>) {
    let eval_ctx = ctx.get_eval_ctx();
    cast_column_value_inner(
        &eval_ctx.type_ctx(),
        &eval_ctx.err_ctx(),
        eval_ctx.sql_mode(),
        val,
        &col.field_type,
        &col.name.o,
        ctx.connection_id(),
        return_err,
        force_ignore_truncate,
    )
}

/// castColumnValue casts a value based on column type.
// go: castColumnValue L1 [M-ERRVAL]
#[allow(clippy::too_many_arguments)]
fn cast_column_value_inner(
    tc: &TypeContext,
    ec: &errctx::Context,
    sql_mode: mysql::SQLMode,
    mut val: Datum,
    ft: &FieldType,
    col_name: &str,
    conn_id: u64,
    return_err: bool,
    force_ignore_truncate: bool,
) -> (Datum, Result<(), Error>) {
    let (mut casted, mut err) = val.convert_to(tc, ft);
    // TODO: make sure all truncate errors are handled by ConvertTo.
    if return_err && err.is_err() {
        return (casted, err);
    }
    let is_err = |e: &Result<(), Error>, target: &Error| matches!(e, Err(x) if target.equal(x));
    if is_err(&err, &terr::ErrTruncated) && ft.get_type() != TypeSet && ft.get_type() != TypeEnum {
        let s = match val.to_string() {
            Ok(s) => s,
            Err(err1) => {
                log::warn!(
                    "Datum ToString failed, Datum: {}, error: {err1}",
                    String::from_utf8_lossy(&val.string())
                );
                Vec::new()
            }
        };
        err = Err(terr::ErrTruncatedWrongVal
            .gen_with_stack_by_args(vec![Arg::from(ft.compact_str()), Arg::Str(s.into())]));
    } else if !casted.is_null()
        && (ft.get_type() == TypeDate
            || ft.get_type() == TypeDatetime
            || ft.get_type() == TypeTimestamp)
    {
        let s = match val.to_string() {
            Ok(s) => s,
            Err(err1) => {
                log::warn!(
                    "Datum ToString failed, Datum: {}, error: {err1}",
                    String::from_utf8_lossy(&val.string())
                );
                val.get_string().to_vec()
            }
        };
        let tm_is_invalid = is_err(&err, &terr::ErrWrongValue);
        let (inn_casted, exit, inn_err) =
            handle_zero_datetime(ec, sql_mode, ft, casted, &s, tm_is_invalid);
        if exit {
            return (inn_casted, inn_err);
        }
        casted = inn_casted;
    } else if is_err(&err, &ErrInvalidCharacterString) {
        err = Err(convert_to_incorrect_string_err(err.unwrap_err(), col_name));
        log::debug!("incorrect string value, conn: {conn_id}, error: {err:?}");
    }

    let mut err = tc.handle_truncate(err);

    if force_ignore_truncate {
        err = Ok(());
    } else if err.is_err() {
        return (casted, err);
    }

    if ft.get_type() == TypeString && !etc::is_binary_str(ft) {
        truncate_trailing_spaces(&mut casted);
    }
    (casted, err)
}

/// The origin default value / default value of a column as Go's `any`.
// glue: model's interface{} default value -> types.NewDatum's argument
fn default_value_to_go(v: &DefaultValueRef<'_>) -> GoValue {
    use tidb_base::gostd::json::Value;
    // glue: json.Unmarshal's dynamic values -> GoValue
    fn json(v: &Value) -> GoValue {
        match v {
            Value::Null => GoValue::Nil,
            Value::Bool(b) => GoValue::Bool(*b),
            Value::Number(f) => GoValue::Float64(*f),
            Value::Int(i) => GoValue::Int64(*i as i64),
            Value::String(s) => GoValue::String(s.as_bytes().to_vec()),
            Value::Array(a) => GoValue::Slice(a.iter().map(json).collect()),
            Value::Object(m) => GoValue::Other(Arc::new(m.clone())),
        }
    }
    match v {
        DefaultValueRef::Bytes(b) => GoValue::String(b.to_vec()),
        DefaultValueRef::Value(tidb_model::AnyValue::GoString(b)) => GoValue::String(b.clone()),
        DefaultValueRef::Value(tidb_model::AnyValue::Value(v)) => json(v),
    }
}

/// Whether a default value is Go's nil interface.
// glue: `defaultVal == nil`
fn default_value_is_nil(v: &DefaultValueRef<'_>) -> bool {
    matches!(v, DefaultValueRef::Value(a) if a.is_nil())
}

/// getColOriginDefaultValue is the optional argument of getColDefaultValue.
struct GetColOriginDefaultValue {
    strict_sql_mode: bool,
}

/// GetColOriginDefaultValue gets default value of the column from original default value.
// go: GetColOriginDefaultValue L1
pub fn get_col_origin_default_value(
    ctx: &dyn BuildContext,
    col: &ColumnInfo,
) -> Result<Datum, Error> {
    get_col_default_value_inner(ctx, col, &col.get_origin_default_value(), None)
}

/// GetColOriginDefaultValueWithoutStrictSQLMode gets default value of the column from
/// original default value with Strict SQL mode.
// go: GetColOriginDefaultValueWithoutStrictSQLMode L1
pub fn get_col_origin_default_value_without_strict_sql_mode(
    ctx: &dyn BuildContext,
    col: &ColumnInfo,
) -> Result<Datum, Error> {
    get_col_default_value_inner(
        ctx,
        col,
        &col.get_origin_default_value(),
        Some(&GetColOriginDefaultValue {
            strict_sql_mode: false,
        }),
    )
}

/// GetColDefaultValue gets default value of the column.
// go: GetColDefaultValue L1
pub fn get_col_default_value(ctx: &dyn BuildContext, col: &ColumnInfo) -> Result<Datum, Error> {
    let default_value = col.get_default_value();
    if !col.default_is_expr {
        return get_col_default_value_inner(ctx, col, &default_value, None);
    }
    // Go: defaultValue.(string), which panics for any other dynamic type.
    let s = match &default_value {
        DefaultValueRef::Value(tidb_model::AnyValue::Value(
            tidb_base::gostd::json::Value::String(s),
        )) => s.clone(),
        _ => panic!("interface conversion: interface {{}} is not string"),
    };
    get_col_default_expr_value(ctx, col, &s)
}

/// EvalColDefaultExpr eval default expr node to explicit default value.
///
/// Needs `expression.EvalSimpleAst`, which the expression rewriter assigns (D09.2).
// glue: stub, blocked-by:D09.2 (EvalColDefaultExpr: expression.EvalSimpleAst)
pub fn eval_col_default_expr(
    _ctx: &dyn BuildContext,
    _col: &ColumnInfo,
    _default_expr: &ast::Expr,
) -> Result<Datum, Error> {
    Err(tidb_base::blocked::blocked(
        "D09.2",
        "a column default expression (expression.EvalSimpleAst)",
    ))
}

/// getColDefaultExprValue: `select <default expr>` evaluated by `expression.EvalSimpleAst`
/// (D09.2).
// glue: stub, blocked-by:D09.2 (getColDefaultExprValue: expression.EvalSimpleAst)
fn get_col_default_expr_value(
    _ctx: &dyn BuildContext,
    _col: &ColumnInfo,
    _default_value: &str,
) -> Result<Datum, Error> {
    Err(tidb_base::blocked::blocked(
        "D09.2",
        "a column default expression (expression.EvalSimpleAst)",
    ))
}

// go: getColDefaultValue L1 [M-ERRVAL]
fn get_col_default_value_inner(
    ctx: &dyn BuildContext,
    col: &ColumnInfo,
    default_val: &DefaultValueRef<'_>,
    args: Option<&GetColOriginDefaultValue>,
) -> Result<Datum, Error> {
    if default_value_is_nil(default_val) {
        return get_col_default_value_from_nil(ctx, col, args);
    }

    match col.field_type.get_type() {
        TypeTimestamp | TypeDate | TypeDatetime => {}
        _ => {
            let (value, err) = cast_column_value(
                ctx,
                new_datum(default_value_to_go(default_val)),
                col,
                false,
                false,
            );
            err?;
            return Ok(value);
        }
    }

    // Check and get timestamp/datetime default value.
    let mut need_change_time_zone = false;
    let mut explicit_tz = None;
    // The dynamic value as `string` (Go: `defaultVal.(string)`); model defaults are JSON
    // strings, so UTF-8.
    let go_str = match default_value_to_go(default_val) {
        GoValue::String(s) => Some(String::from_utf8_lossy(&s).into_owned()),
        _ => None,
    };
    // If the column's default value is not ZeroDatetimeStr nor CurrentTimestamp, should use
    // the time zone of the default value itself.
    if col.field_type.get_type() == TypeTimestamp
        && let Some(vv) = &go_str
        && vv != ttime::ZeroDatetimeStr
        && !tidb_base::gostd::equal_fold(vv, ast_funcs::CurrentTimestamp)
    {
        need_change_time_zone = true;
        // For col.Version = 0, the timezone information of default value is already lost,
        // so use the system timezone as the default value timezone.
        explicit_tz = Some(tidb_base::gostd::time::local()); // timeutil.SystemLocation()
        if col.version >= tidb_model::ColumnInfoVersion1 {
            explicit_tz = Some(tidb_base::gostd::time::utc());
        }
    }
    let arg = match &go_str {
        Some(s) => TimeValueArg::Str(s),
        None => TimeValueArg::Other,
    };
    let mut value = match get_time_value(
        ctx,
        arg,
        col.field_type.get_type(),
        col.field_type.get_decimal(),
        explicit_tz.clone(),
    ) {
        Ok(v) => v,
        Err(_) => {
            return Err(errGetDefaultFailed
                .gen_with_stack_by_args(vec![crate::infoschema_v2::cistr_arg(&col.name)]));
        }
    };
    // If the column's default value is not ZeroDatetimeStr or CurrentTimestamp, convert the
    // default value to the current session time zone.
    if need_change_time_zone && let Some(explicit_tz) = explicit_tz {
        let mut t = value.get_mysql_time();
        t.convert_time_zone(&explicit_tz, &ctx.get_eval_ctx().location())?;
        value.set_mysql_time(t);
    }
    Ok(value)
}

use tidb_expression::ast_funcs;

// go: getColDefaultValueFromNil L1
fn get_col_default_value_from_nil(
    ctx: &dyn BuildContext,
    col: &ColumnInfo,
    args: Option<&GetColOriginDefaultValue>,
) -> Result<Datum, Error> {
    if !mysql::has_not_null_flag(col.field_type.get_flag()) {
        return Ok(Datum::default());
    }
    if col.field_type.get_type() == TypeEnum {
        // For enum type, if no default value and not null is set, the default value is the
        // first element of the enum list
        let def_enum =
            enum_type::parse_enum_value(col.field_type.get_elems().unwrap_or_default(), 1)?;
        return Ok(new_collate_mysql_enum_datum(
            def_enum,
            col.field_type.get_collate().to_owned(),
        ));
    }
    if mysql::has_auto_increment_flag(col.field_type.get_flag()) {
        // Auto increment column doesn't have default value and we should not return error.
        return Ok(get_zero_value(col));
    }
    let eval_ctx = ctx.get_eval_ctx();
    let strict_sql_mode = match args {
        Some(args) => args.strict_sql_mode,
        None => eval_ctx.sql_mode().has_strict_mode(),
    };
    if !strict_sql_mode {
        eval_ctx.append_warning(
            ErrNoDefaultValue.fast_gen_by_args(vec![crate::infoschema_v2::cistr_arg(&col.name)]),
        );
        return Ok(get_zero_value(col));
    }
    let ec = eval_ctx.err_ctx();
    let err = if mysql::has_no_default_value_flag(col.field_type.get_flag()) {
        ErrNoDefaultValue.fast_gen_by_args(vec![crate::infoschema_v2::cistr_arg(&col.name)])
    } else {
        ErrColumnCantNull.fast_gen_by_args(vec![crate::infoschema_v2::cistr_arg(&col.name)])
    };
    if ec.handle_error(err).is_ok() {
        return Ok(get_zero_value(col));
    }
    Err(ErrNoDefaultValue.gen_with_stack_by_args(vec![crate::infoschema_v2::cistr_arg(&col.name)]))
}

/// GetZeroValue gets zero value for given column type.
// go: GetZeroValue L1
pub fn get_zero_value(col: &ColumnInfo) -> Datum {
    let ft = &col.field_type;
    let mut d = Datum::default();
    match ft.get_type() {
        TypeTiny | TypeInt24 | TypeShort | TypeLong | TypeLonglong => {
            if mysql::has_unsigned_flag(ft.get_flag()) {
                d.set_uint64(0);
            } else {
                d.set_int64(0);
            }
        }
        TypeYear => d.set_int64(0),
        TypeFloat => d.set_float32(0.0),
        TypeDouble => d.set_float64(0.0),
        TypeNewDecimal => {
            d.set_length(ft.get_flen());
            d.set_frac(ft.get_decimal());
            d.set_mysql_decimal(Default::default());
        }
        TypeString => {
            if ft.get_flen() > 0 && ft.get_charset() == CharsetBin {
                d.set_bytes(vec![0; ft.get_flen() as usize]);
            } else {
                d.set_string(Vec::new(), ft.get_collate().to_owned());
            }
        }
        TypeVarString | TypeVarchar | TypeBlob | TypeTinyBlob | TypeMediumBlob | TypeLongBlob => {
            d.set_string(Vec::new(), ft.get_collate().to_owned())
        }
        TypeDuration => d.set_mysql_duration(tidb_types::time::ZeroDuration),
        TypeDate => d.set_mysql_time(*ttime::ZeroDate),
        TypeTimestamp => d.set_mysql_time(*ttime::ZeroTimestamp),
        TypeDatetime => d.set_mysql_time(*ttime::ZeroDatetime),
        TypeBit => d.set_mysql_bit(tidb_types::binary_literal::ZeroBinaryLiteral),
        TypeSet => d.set_mysql_set(Default::default(), ft.get_collate().to_owned()),
        TypeEnum => d.set_mysql_enum(Default::default(), ft.get_collate().to_owned()),
        TypeJSON => d.set_mysql_json(tidb_types::json::create_binary_json(
            tidb_types::json::JSONValue::Nil,
        )),
        TypeTiDBVectorFloat32 => {
            d.set_vector_float32(tidb_types::vector::ZeroVectorFloat32.clone())
        }
        _ => {}
    }
    d
}

/// OptionalFsp convert a FieldType.GetDecimal() to string.
// go: OptionalFsp L1
pub fn optional_fsp(field_type: &FieldType) -> String {
    let fsp = field_type.get_decimal();
    if fsp == 0 {
        return String::new();
    }
    format!("({fsp})")
}

/// FillVirtualColumnValue will calculate the virtual column value by evaluating generated
/// expression using rows from a chunk, and then fill this value into the chunk.
// go: FillVirtualColumnValue L2 (the filled column is copied into req, not shared with virCols)
pub fn fill_virtual_column_value(
    virtual_ret_types: &[FieldType],
    virtual_column_index: &[usize],
    exp_cols: &[Arc<ExprColumn>],
    col_infos: &[Arc<ColumnInfo>],
    ectx: &dyn BuildContext,
    req: &mut tidb_chunk::chunk::Chunk,
) -> Result<(), Error> {
    if virtual_column_index.is_empty() {
        return Ok(());
    }

    let mut vir_cols =
        tidb_chunk::chunk::new_chunk_with_capacity(virtual_ret_types, req.capacity());
    let eval_ctx = ectx.get_eval_ctx();
    let tc = eval_ctx.type_ctx();
    let st = EvalState::new();
    for (i, &idx) in virtual_column_index.iter().enumerate() {
        for r in 0..req.num_rows() {
            let row = req.get_row(r);
            let datum = exp_cols[idx].eval_virtual_column(&*eval_ctx, &st, row)?;
            // Because the expression might return different type from the generated column,
            // we should wrap a CAST on the result.
            let (mut cast_datum, err) =
                cast_column_value(ectx, datum.clone(), &col_infos[idx], false, true);
            err?;

            // Clip to zero if get negative value after cast to unsigned.
            if mysql::has_unsigned_flag(col_infos[idx].field_type.get_flag())
                && !cast_datum.is_null()
                && tc.flags().allow_negative_to_unsigned()
            {
                match datum.kind() {
                    KindInt64 if datum.get_int64() < 0 => {
                        cast_datum = get_zero_value(&col_infos[idx]);
                    }
                    KindFloat32 | KindFloat64 if helper::round_float(datum.get_float64()) < 0.0 => {
                        cast_datum = get_zero_value(&col_infos[idx]);
                    }
                    KindMysqlDecimal if datum.get_mysql_decimal().is_negative() => {
                        cast_datum = get_zero_value(&col_infos[idx]);
                    }
                    _ => {}
                }
            }

            // Handle the bad null error.
            if (mysql::has_not_null_flag(col_infos[idx].field_type.get_flag())
                || mysql::has_prevent_null_insert_flag(col_infos[idx].field_type.get_flag()))
                && cast_datum.is_null()
            {
                cast_datum = get_zero_value(&col_infos[idx]);
            }
            vir_cols.append_datum(i, &cast_datum);
        }
        req.set_col(idx, tidb_chunk::ColArc::new(vir_cols.column(i).clone()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tidb_base::mysql::{NotNullFlag, TypeLonglong, UnsignedFlag};
    use tidb_expression::column::Column as ExprColumn;
    use tidb_expression::expression::Expr;
    use tidb_model::ColumnInfo;
    use tidb_sessionctx::exprstatic as es;
    use tidb_types::context::Flags;
    use tidb_types::datum::Datum;
    use tidb_types::field_type::new_field_type;

    use super::fill_virtual_column_value;

    /// FillVirtualColumnValue: column 1 is virtual and evaluates column 0; casts to the
    /// column type, clips negatives of an unsigned column to zero (AllowNegativeToUnsigned),
    /// and turns NULL into the zero value of a NOT NULL column (the cases of the Go body).
    #[test]
    fn fill_virtual_column_value_cases() {
        let int = new_field_type(TypeLonglong);
        let mut uint_nn = new_field_type(TypeLonglong);
        uint_nn.add_flag(UnsignedFlag | NotNullFlag);
        let src = ExprColumn {
            ret_type: int.clone(),
            index: 0,
            unique_id: 1,
            ..Default::default()
        };
        let vcol = ExprColumn {
            ret_type: uint_nn.clone(),
            index: 1,
            unique_id: 2,
            virtual_expr: Some(Arc::new(Expr::Column(src.clone()))),
            ..Default::default()
        };
        let exp_cols = vec![Arc::new(src), Arc::new(vcol)];
        let infos = vec![
            Arc::new(ColumnInfo {
                field_type: int.clone(),
                ..Default::default()
            }),
            Arc::new(ColumnInfo {
                field_type: uint_nn.clone(),
                ..Default::default()
            }),
        ];
        let mut chk =
            tidb_chunk::chunk::new_chunk_with_capacity(&[int.clone(), uint_nn.clone()], 4);
        for v in [Some(5), Some(-3), None] {
            let d = match v {
                Some(v) => {
                    let mut d = Datum::default();
                    d.set_int64(v);
                    d
                }
                None => Datum::default(),
            };
            chk.append_datum(0, &d);
            chk.append_datum(1, &Datum::default());
        }
        let ev = es::new_eval_context(vec![es::with_type_flags(
            Flags::default().with_allow_negative_to_unsigned(true),
        )]);
        let ctx = es::new_expr_context(vec![es::with_eval_ctx(ev)]);
        fill_virtual_column_value(
            std::slice::from_ref(&uint_nn),
            &[1],
            &exp_cols,
            &infos,
            &*ctx,
            &mut chk,
        )
        .unwrap();
        let col = chk.column(1);
        assert_eq!(col.get_uint64(0), 5);
        assert_eq!(col.get_uint64(1), 0);
        assert!(!col.is_null(2));
        assert_eq!(col.get_uint64(2), 0);
        // Nothing to do without virtual columns.
        fill_virtual_column_value(&[], &[], &exp_cols, &infos, &*ctx, &mut chk).unwrap();
    }

    /// A version-0 TIMESTAMP default is in the system time zone (`timeutil.SystemLocation`),
    /// then converted to the session's: with the session in the same zone the wall clock is
    /// kept. (The govec vectors avoid this branch: it depends on the host's zone.)
    #[test]
    fn version0_timestamp_default_uses_the_system_location() {
        use tidb_base::mysql::TypeTimestamp;
        use tidb_model::AnyValue;

        let ft = new_field_type(TypeTimestamp);
        let mut col = ColumnInfo {
            field_type: ft,
            version: 0,
            ..Default::default()
        };
        col.default_value = AnyValue::Value(tidb_base::gostd::json::Value::String(
            "2019-05-06 12:48:49".to_owned(),
        ));
        let ev = es::new_eval_context(vec![es::with_location(Some(
            tidb_base::gostd::time::local(),
        ))]);
        let ctx = es::new_expr_context(vec![es::with_eval_ctx(ev)]);
        let d = super::get_col_default_value(&*ctx, &col).unwrap();
        assert_eq!(
            String::from_utf8(d.get_mysql_time().string()).unwrap(),
            "2019-05-06 12:48:49"
        );
    }
}
