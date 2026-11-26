//! Hand-written Rust AST for the ported subset (SELECT core and expressions); mirrors the
//! node and field names of tidb/pkg/parser/ast. `Restore()` lives in `src/restore.rs`.

pub mod funcs;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

use crate::span::Span;

use crate::actions_rt::{FromNode, Node};
use crate::gen_types::*;

/// glue: poc — hand-written stand-in for `model.CIStr` used by the PoC AST; the faithful
/// port is [`crate::model::CIStr`] (D02.3). Replaced when the AST nodes are ported.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CIStr {
    pub o: String,
}
impl CIStr {
    pub fn new(s: impl Into<String>) -> Self {
        CIStr { o: s.into() }
    }
    pub fn l(&self) -> String {
        self.o.to_lowercase()
    }
}
impl FromNode for CIStr {
    fn from_node(_: Node) -> Self {
        CIStr::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Op {
    #[default]
    LogicAnd,
    LogicOr,
    LogicXor,
    LeftShift,
    RightShift,
    GE,
    LE,
    EQ,
    NE,
    LT,
    GT,
    Plus,
    Minus,
    And,
    Or,
    Mod,
    Xor,
    Div,
    Mul,
    Not,
    Not2,
    BitNeg,
    IntDiv,
    NullEQ,
}
impl Op {
    /// Text of the operator and whether it is a keyword (`opcode.Op.Restore`, `IsKeyword`).
    pub fn literal(self) -> (&'static str, bool) {
        use Op::*;
        match self {
            LogicAnd => ("AND", true),
            LogicOr => ("OR", true),
            LogicXor => ("XOR", true),
            LeftShift => ("<<", false),
            RightShift => (">>", false),
            GE => (">=", false),
            LE => ("<=", false),
            EQ => ("=", false),
            NE => ("!=", false),
            LT => ("<", false),
            GT => (">", false),
            Plus => ("+", false),
            Minus => ("-", false),
            And => ("&", false),
            Or => ("|", false),
            Mod => ("%", false),
            Xor => ("^", false),
            Div => ("/", false),
            Mul => ("*", false),
            Not => ("NOT ", true),
            Not2 => ("!", false),
            BitNeg => ("~", false),
            IntDiv => ("DIV", true),
            NullEQ => ("<=>", false),
        }
    }
}
impl FromNode for Op {
    fn from_node(_: Node) -> Self {
        Op::default()
    }
}

// Function names the grammar builds (`ast.*` constants in functions.go).
pub const DATE_LITERAL: &str = "'tidb`.(dateliteral";
pub const TIME_LITERAL: &str = "'tidb`.(timeliteral";
pub const TIMESTAMP_LITERAL: &str = "'tidb`.(timestampliteral";
pub const JSON_MEMBER_OF: &str = "json_memberof";
pub const JSON_EXTRACT: &str = "json_extract";
pub const JSON_UNQUOTE: &str = "json_unquote";
pub const WEIGHT_STRING: &str = "weight_string";
pub const CONCAT: &str = "concat";
pub const INTERVAL: &str = "interval";
pub const CHAR_FUNC: &str = "char_func";
pub const INSERT_FUNC: &str = "insert_func";
pub const PASSWORD_FUNC: &str = "password";
pub const NEXT_VAL: &str = "nextval";
pub const LAST_VAL: &str = "lastval";
pub const SET_VAL: &str = "setval";
pub const AGG_FUNC_GROUP_CONCAT: &str = "group_concat";
pub const AGG_FUNC_VAR_POP: &str = "var_pop";
pub const AGG_FUNC_STDDEV_POP: &str = "stddev_pop";

/// Value of a `ValueExpr` (the test-driver `Datum` kinds the parser produces).
#[derive(Debug, Clone, PartialEq)]
pub enum Datum {
    Null,
    /// KindInt64 with `IsBooleanFlag` (TRUE / FALSE literals).
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    /// `MyDecimal`, kept as its canonical string.
    Decimal(String),
    Str(String),
    /// `HexLiteral` (x'..' / 0x..).
    Hex(Vec<u8>),
    /// `BitLiteral` (b'..' / 0b..).
    Bit(Vec<u8>),
}

/// `ast.ValueExpr` (test_driver.ValueExpr): a datum plus the charset/collation of its type.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueExpr {
    pub datum: Datum,
    /// `Type.GetCharset()`: the parser charset for strings, "binary" for other kinds (as
    /// `DefaultTypeForValue` sets it), or an explicit `_charset` introducer.
    pub charset: &'static str,
    pub collate: &'static str,
    /// `projectionOffset` (`-1` = unset); set when adjacent string literals are concatenated.
    pub projection_offset: i32,
    /// `Type.GetFlag() & mysql.UnderScoreCharsetFlag`: the literal has a character set
    /// introducer (`_utf8mb4'x'`, `N'x'`); the expression rewriter reads it.
    pub underscore_charset: bool,
}
impl ValueExpr {
    /// `ast.NewValueExpr(value, charset, collate)` with test_driver's `DefaultTypeForValue`.
    pub fn new(datum: Datum, charset: &'static str, collate: &'static str) -> Self {
        let (charset, collate) = match datum {
            Datum::Str(_) => (charset, collate),
            _ => (crate::charset::CHARSET_BIN, crate::charset::COLLATION_BIN),
        };
        ValueExpr {
            datum,
            charset,
            collate,
            projection_offset: -1,
            underscore_charset: false,
        }
    }
}

/// `ast.FuncCallExprType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FuncCallExprType {
    #[default]
    Keyword,
    Generic,
}

/// `ast.TimeUnitType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeUnitType {
    #[default]
    Invalid,
    Microsecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
    SecondMicrosecond,
    MinuteMicrosecond,
    MinuteSecond,
    HourMicrosecond,
    HourSecond,
    HourMinute,
    DayMicrosecond,
    DaySecond,
    DayMinute,
    DayHour,
    YearMonth,
}
impl FromNode for TimeUnitType {
    fn from_node(_: Node) -> Self {
        TimeUnitType::Invalid
    }
}

/// `ast.GetFormatSelectorType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GetFormatSelectorType {
    #[default]
    Date,
    Datetime,
    Time,
}
impl FromNode for GetFormatSelectorType {
    fn from_node(_: Node) -> Self {
        GetFormatSelectorType::Date
    }
}

/// `ast.TrimDirectionType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrimDirectionType {
    #[default]
    BothDefault,
    Both,
    Leading,
    Trailing,
}
impl FromNode for TrimDirectionType {
    fn from_node(_: Node) -> Self {
        TrimDirectionType::BothDefault
    }
}

/// `ast.CastFunctionType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastFunctionType {
    Cast,
    Convert,
    BinaryOperator,
}

/// `types.UnspecifiedLength`.
pub const UNSPECIFIED_LENGTH: i64 = -1;

/// `mysql.Type*` codes used by the cast grammar.
pub mod mysql_type {
    pub const FLOAT: u8 = 4;
    pub const DOUBLE: u8 = 5;
    pub const LONGLONG: u8 = 8;
    pub const DATE: u8 = 10;
    pub const DURATION: u8 = 11;
    pub const DATETIME: u8 = 12;
    pub const YEAR: u8 = 13;
    pub const JSON: u8 = 0xf5;
    pub const NEW_DECIMAL: u8 = 0xf6;
    pub const VAR_STRING: u8 = 0xfd;
    pub const STRING: u8 = 0xfe;
    pub const TIDB_VECTOR_FLOAT32: u8 = 0xe1;
}
/// `mysql.*Flag` bits used by the cast grammar.
pub mod mysql_flag {
    pub const UNSIGNED: u32 = 1 << 5;
    pub const BINARY: u32 = 1 << 7;
    pub const PARSE_TO_JSON: u32 = 1 << 18;
}

/// glue: poc — hand-written stand-in; the faithful port is [`crate::types::FieldType`]
/// (D02.3). Replaced when the AST nodes that hold it are ported.
///
/// `types.FieldType` (the fields the parser sets). Built by the cast grammar (`CastType`);
/// the column-definition grammar is not ported yet and yields `unported`.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldType {
    pub tp: u8,
    pub flag: u32,
    pub flen: i64,
    pub decimal: i64,
    pub charset: String,
    pub collate: String,
    pub array: bool,
    /// Built by a placeholder action (kind of the production).
    pub unported: Option<&'static str>,
}
impl FieldType {
    /// `types.NewFieldType(tp)`.
    pub fn new(tp: u8) -> Self {
        FieldType {
            tp,
            flag: 0,
            flen: UNSPECIFIED_LENGTH,
            decimal: UNSPECIFIED_LENGTH,
            charset: String::new(),
            collate: String::new(),
            array: false,
            unported: None,
        }
    }
    /// `SetCharset(charset.CharsetBin); SetCollate(charset.CollationBin)`.
    pub fn set_bin(&mut self) {
        self.charset = crate::charset::CHARSET_BIN.to_string();
        self.collate = crate::charset::COLLATION_BIN.to_string();
    }
}
impl FromNode for FieldType {
    fn from_node(n: Node) -> Self {
        FieldType {
            unported: Some(n.kind),
            ..FieldType::new(0)
        }
    }
}

/// `mysql.GetDefaultFieldLengthAndDecimalForCast`.
pub fn default_length_and_decimal_for_cast(tp: u8) -> (i64, i64) {
    use mysql_type::*;
    match tp {
        STRING => (0, -1),
        DATE => (10, 0),
        DATETIME => (19, 0),
        NEW_DECIMAL => (10, 0),
        DURATION => (10, 0),
        LONGLONG => (22, 0),
        DOUBLE => (22, -1),
        FLOAT => (12, -1),
        JSON => (4194304, 0),
        _ => (-1, -1),
    }
}

/// `ast.FloatOpt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FloatOpt {
    pub flen: i64,
    pub decimal: i64,
}
impl FromNode for FloatOpt {
    fn from_node(_: Node) -> Self {
        FloatOpt {
            flen: UNSPECIFIED_LENGTH,
            decimal: UNSPECIFIED_LENGTH,
        }
    }
}

/// `ast.OptBinary`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OptBinary {
    pub is_binary: bool,
    pub charset: String,
}
impl FromNode for OptBinary {
    fn from_node(_: Node) -> Self {
        OptBinary::default()
    }
}

/// `ast.VectorElementType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VectorElementType {
    pub tp: u8,
}
impl FromNode for VectorElementType {
    fn from_node(_: Node) -> Self {
        VectorElementType::default()
    }
}

/// Parser-internal `likeEscapeSpec` (LIKE ... ESCAPE).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LikeEscapeSpec {
    pub escape: String,
    pub explicit: bool,
}
impl FromNode for LikeEscapeSpec {
    fn from_node(_: Node) -> Self {
        LikeEscapeSpec {
            escape: "\\".into(),
            explicit: false,
        }
    }
}

#[derive(Debug)]
pub enum Expr {
    /// `ColumnNameExpr`.
    Column(Box<ColumnName>),
    Value(Box<ValueExpr>),
    ParamMarker(Box<ParamMarkerExpr>),
    Binary(Box<BinaryOperationExpr>),
    Unary(Box<UnaryOperationExpr>),
    FuncCall(Box<FuncCallExpr>),
    FuncCast(Box<FuncCastExpr>),
    JSONSumCrc32(Box<JSONSumCrc32Expr>),
    Aggregate(Box<AggregateFuncExpr>),
    Window(Box<WindowFuncExpr>),
    /// `ParenthesesExpr`.
    Paren(Box<Expr>),
    PatternIn(Box<PatternInExpr>),
    PatternLike(Box<PatternLikeOrIlikeExpr>),
    PatternRegexp(Box<PatternRegexpExpr>),
    Between(Box<BetweenExpr>),
    IsNull(Box<IsNullExpr>),
    IsTruth(Box<IsTruthExpr>),
    Case(Box<CaseExpr>),
    Subquery(Box<SubqueryExpr>),
    Exists(Box<ExistsSubqueryExpr>),
    CompareSubquery(Box<CompareSubqueryExpr>),
    Variable(Box<VariableExpr>),
    Row(Box<RowExpr>),
    SetCollation(Box<SetCollationExpr>),
    Default(Box<DefaultExpr>),
    /// `ValuesExpr` (`VALUES(col)`).
    Values(Box<ColumnName>),
    MatchAgainst(Box<MatchAgainst>),
    /// `TableNameExpr` (sequence functions).
    TableName(Box<TableName>),
    /// `TimeUnitExpr`.
    TimeUnit(TimeUnitType),
    /// `GetFormatSelectorExpr`.
    GetFormatSelector(GetFormatSelectorType),
    /// `TrimDirectionExpr`.
    TrimDirection(TrimDirectionType),
    /// `PositionExpr` (ORDER/GROUP BY position).
    Position(i64),
    Unported(Box<Node>),
}
impl Default for Expr {
    fn default() -> Self {
        Expr::Value(Box::new(ValueExpr::new(Datum::Null, "", "")))
    }
}
impl FromNode for Expr {
    fn from_node(n: Node) -> Self {
        Expr::Unported(Box::new(n))
    }
}

/// `driver.ParamMarkerExpr` (pkg/types/parser_driver/value_expr.go): a `?` of a prepared
/// statement.
#[derive(Debug)]
pub struct ParamMarkerExpr {
    /// Byte offset of the `?`.
    pub offset: usize,
    /// `Order`: the marker's position among the statement's markers, by offset. The parser
    /// leaves it 0; PREPARE numbers the markers (`GeneratePlanCacheStmtWithAST` →
    /// [`ParamMarkerExpr::set_order`]). Go writes it through the marker pointers the
    /// extractor collected; the statement here is walked read-only (and shared once
    /// prepared), so the field is an atomic, written only by the numbering
    /// (`GeneratePlanCacheStmtWithAST`, `ExtractAndSortParamMarkers`) before the statement
    /// is shared.
    order: AtomicUsize,
}

impl ParamMarkerExpr {
    /// A marker at byte `offset`, order 0 (Go's parser leaves `Order` zero).
    // glue: constructor (the grammar's `&driver.ParamMarkerExpr{Offset: ...}`)
    pub fn new(offset: usize) -> Self {
        ParamMarkerExpr {
            offset,
            order: AtomicUsize::new(0),
        }
    }

    /// `n.Order`.
    // glue: accessor (Go reads the field)
    pub fn order(&self) -> usize {
        self.order.load(Relaxed)
    }

    /// SetOrder implements the ast.ParamMarkerExpr interface.
    // go: pkg/types/parser_driver/value_expr.go ParamMarkerExpr.SetOrder L1
    pub fn set_order(&self, order: usize) {
        self.order.store(order, Relaxed);
    }
}
#[derive(Debug)]
pub struct BinaryOperationExpr {
    pub op: Op,
    pub l: Expr,
    pub r: Expr,
}
#[derive(Debug)]
pub struct UnaryOperationExpr {
    pub op: Op,
    pub v: Expr,
}
#[derive(Debug, Default)]
pub struct FuncCallExpr {
    pub tp: FuncCallExprType,
    pub schema: CIStr,
    pub fn_name: CIStr,
    pub args: Vec<Expr>,
}
#[derive(Debug)]
pub struct FuncCastExpr {
    pub expr: Expr,
    pub tp: Box<FieldType>,
    pub function_type: CastFunctionType,
    pub explicit_char_set: bool,
}
#[derive(Debug)]
pub struct JSONSumCrc32Expr {
    pub expr: Expr,
    pub tp: Box<FieldType>,
    pub explicit_char_set: bool,
}
#[derive(Debug, Default)]
pub struct AggregateFuncExpr {
    pub f: String,
    pub args: Vec<Expr>,
    pub distinct: bool,
    pub order: Option<Box<OrderByClause>>,
}
#[derive(Debug, Default)]
pub struct WindowFuncExpr {
    pub name: String,
    pub args: Vec<Expr>,
    pub distinct: bool,
    pub ignore_null: bool,
    pub from_last: bool,
    pub spec: WindowSpec,
}
#[derive(Debug)]
pub struct PatternInExpr {
    pub expr: Expr,
    pub list: Vec<Expr>,
    pub not: bool,
    /// `Sel`: a `SubqueryExpr`.
    pub sel: Option<Expr>,
}
#[derive(Debug)]
pub struct PatternLikeOrIlikeExpr {
    pub expr: Expr,
    pub pattern: Expr,
    pub not: bool,
    pub escape: u8,
    pub escape_explicit: bool,
    pub is_like: bool,
}
impl FromNode for PatternLikeOrIlikeExpr {
    fn from_node(n: Node) -> Self {
        PatternLikeOrIlikeExpr {
            expr: Expr::from_node(n),
            pattern: Expr::default(),
            not: false,
            escape: b'\\',
            escape_explicit: false,
            is_like: true,
        }
    }
}
#[derive(Debug)]
pub struct PatternRegexpExpr {
    pub expr: Expr,
    pub pattern: Expr,
    pub not: bool,
}
#[derive(Debug)]
pub struct BetweenExpr {
    pub expr: Expr,
    pub left: Expr,
    pub right: Expr,
    pub not: bool,
}
#[derive(Debug)]
pub struct IsNullExpr {
    pub expr: Expr,
    pub not: bool,
}
#[derive(Debug)]
pub struct IsTruthExpr {
    pub expr: Expr,
    pub not: bool,
    pub true_: i64,
}
#[derive(Debug, Default)]
pub struct WhenClause {
    pub expr: Expr,
    pub result: Expr,
}
impl FromNode for WhenClause {
    fn from_node(n: Node) -> Self {
        WhenClause {
            expr: Expr::from_node(n),
            result: Expr::default(),
        }
    }
}
#[derive(Debug)]
pub struct CaseExpr {
    pub value: Option<Expr>,
    pub when_clauses: Vec<WhenClause>,
    pub else_clause: Option<Expr>,
}
#[derive(Debug)]
pub struct SubqueryExpr {
    pub query: ResultSetNode,
    pub multi_rows: bool,
    pub exists: bool,
}
#[derive(Debug)]
pub struct ExistsSubqueryExpr {
    /// A `SubqueryExpr`.
    pub sel: Expr,
    pub not: bool,
}
#[derive(Debug)]
pub struct CompareSubqueryExpr {
    pub l: Expr,
    pub op: Op,
    /// A `SubqueryExpr`.
    pub r: Expr,
    pub all: bool,
}
#[derive(Debug, Default)]
pub struct VariableExpr {
    pub name: String,
    pub is_global: bool,
    pub is_instance: bool,
    pub is_system: bool,
    pub explicit_scope: bool,
    pub value: Option<Expr>,
}
#[derive(Debug)]
pub struct RowExpr {
    pub values: Vec<Expr>,
}
impl FromNode for RowExpr {
    fn from_node(n: Node) -> Self {
        RowExpr {
            values: vec![Expr::from_node(n)],
        }
    }
}
#[derive(Debug)]
pub struct SetCollationExpr {
    pub expr: Expr,
    pub collate: String,
}
#[derive(Debug, Default)]
pub struct DefaultExpr {
    pub name: Option<Box<ColumnName>>,
}
#[derive(Debug)]
pub struct MatchAgainst {
    pub column_names: Vec<ColumnName>,
    pub against: Expr,
    /// `ast.FulltextSearchModifier`.
    pub modifier: i64,
}
/// `ast.WindowSpec`.
#[derive(Debug, Default)]
pub struct WindowSpec {
    pub name: CIStr,
    /// `Ref`: the window this one is based on (`w2 AS (w1 ORDER BY a)`).
    pub ref_: CIStr,
    pub partition_by: Option<Box<PartitionByClause>>,
    pub order_by: Option<Box<OrderByClause>>,
    pub frame: Option<Box<FrameClause>>,
    /// `OVER w` (as opposed to `OVER (w)`).
    pub only_alias: bool,
    /// Built by a placeholder action (kind of the production).
    pub unported: Option<&'static str>,
}
impl FromNode for WindowSpec {
    fn from_node(n: Node) -> Self {
        WindowSpec {
            unported: Some(n.kind),
            ..Default::default()
        }
    }
}
/// `ast.PartitionByClause`.
#[derive(Debug, Default)]
pub struct PartitionByClause {
    pub items: Vec<ByItem>,
}
impl FromNode for PartitionByClause {
    fn from_node(n: Node) -> Self {
        PartitionByClause {
            items: vec![ByItem::from_node(n)],
        }
    }
}
/// `ast.FrameType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameType {
    #[default]
    Rows,
    Ranges,
    Groups,
}
impl FromNode for FrameType {
    fn from_node(_: Node) -> Self {
        FrameType::Rows
    }
}
/// `ast.BoundType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoundType {
    #[default]
    Following,
    Preceding,
    CurrentRow,
}
/// `ast.FrameBound`.
#[derive(Debug, Default)]
pub struct FrameBound {
    pub tp: BoundType,
    pub unbounded: bool,
    pub expr: Option<Expr>,
    pub unit: TimeUnitType,
}
impl FromNode for FrameBound {
    fn from_node(n: Node) -> Self {
        FrameBound {
            expr: Some(Expr::from_node(n)),
            ..Default::default()
        }
    }
}
/// `ast.FrameExtent`.
#[derive(Debug, Default)]
pub struct FrameExtent {
    pub start: FrameBound,
    pub end: FrameBound,
}
impl FromNode for FrameExtent {
    fn from_node(n: Node) -> Self {
        FrameExtent {
            start: FrameBound::from_node(n),
            end: FrameBound::default(),
        }
    }
}
/// `ast.FrameClause`.
#[derive(Debug, Default)]
pub struct FrameClause {
    pub tp: FrameType,
    pub extent: FrameExtent,
}
impl FromNode for FrameClause {
    fn from_node(n: Node) -> Self {
        FrameClause {
            tp: FrameType::Rows,
            extent: FrameExtent::from_node(n),
        }
    }
}

/// `ast.FulltextSearchModifier*`.
pub mod fulltext_modifier {
    pub const BOOLEAN_MODE: i64 = 1;
    pub const MODE_MASK: i64 = 0xF;
    pub const WITH_QUERY_EXPANSION: i64 = 1 << 4;
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ColumnName {
    pub schema: CIStr,
    pub table: CIStr,
    pub name: CIStr,
}
impl FromNode for ColumnName {
    fn from_node(_: Node) -> Self {
        ColumnName::default()
    }
}

#[derive(Debug)]
pub enum Stmt {
    Select(Box<SelectStmt>),
    /// `SetOprStmt` (UNION / EXCEPT / INTERSECT).
    SetOpr(Box<SetOprStmt>),
    /// `BeginStmt` (BEGIN / START TRANSACTION).
    Begin(Box<BeginStmt>),
    Commit(Box<CommitStmt>),
    Rollback(Box<RollbackStmt>),
    Use(Box<UseStmt>),
    Set(Box<SetStmt>),
    Show(Box<ShowStmt>),
    /// `ExecuteStmt` (EXECUTE of a prepared statement).
    Execute(Box<ExecuteStmt>),
    Unported(Box<Node>),
}

/// `ast.BeginStmt`.
#[derive(Debug, Default)]
pub struct BeginStmt {
    /// `ast.Pessimistic` / `ast.Optimistic`, or empty.
    pub mode: &'static str,
    pub causal_consistency_only: bool,
    pub read_only: bool,
    pub as_of: Option<Box<AsOfClause>>,
}

/// `ast.CompletionType` (COMMIT / ROLLBACK ... AND CHAIN / RELEASE).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionType {
    #[default]
    Default,
    Chain,
    Release,
}
impl FromNode for CompletionType {
    fn from_node(_: Node) -> Self {
        CompletionType::Default
    }
}

/// `ast.CommitStmt`.
#[derive(Debug, Default)]
pub struct CommitStmt {
    pub completion_type: CompletionType,
}

/// `ast.RollbackStmt`.
#[derive(Debug, Default)]
pub struct RollbackStmt {
    pub completion_type: CompletionType,
    pub savepoint_name: String,
}

/// `ast.ExecuteStmt`: a statement to execute a prepared statement (`EXECUTE name [USING
/// @var, ...]`), or the binary protocol's COM_STMT_EXECUTE (`session.ExecutePreparedStmt`
/// builds one with no name, the statement and its arguments).
///
/// Go's `BinaryArgs` and `PrepStmt` are `interface{}`: the parser does not know the types
/// that fill them (the planner's `PlanCacheStmt`, the session's argument values), so they are
/// `Any` here too, shared (`Arc`) because the prepared statement is shared with the session's
/// statement map.
#[derive(Default)]
pub struct ExecuteStmt {
    pub name: String,
    pub using_vars: Vec<Expr>,
    pub binary_args: Option<Arc<dyn std::any::Any + Send + Sync>>,
    /// the corresponding prepared statement
    pub prep_stmt: Option<Arc<dyn std::any::Any + Send + Sync>>,
    pub prep_stmt_id: u32,
    pub idx_in_multi: isize,
    /// FromGeneralStmt indicates whether this execute-stmt is converted from a general query.
    /// e.g. select * from t where a>2 --> execute 'select * from t where a>?' using 2
    pub from_general_stmt: bool,
}

impl std::fmt::Debug for ExecuteStmt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecuteStmt")
            .field("name", &self.name)
            .field("using_vars", &self.using_vars)
            .field("binary_args", &self.binary_args.is_some())
            .field("prep_stmt", &self.prep_stmt.is_some())
            .field("prep_stmt_id", &self.prep_stmt_id)
            .field("idx_in_multi", &self.idx_in_multi)
            .field("from_general_stmt", &self.from_general_stmt)
            .finish()
    }
}

/// `ast.UseStmt`.
#[derive(Debug, Default)]
pub struct UseStmt {
    pub db_name: String,
}

/// `ast.AsOfClause` (`AS OF TIMESTAMP expr`).
#[derive(Debug)]
pub struct AsOfClause {
    pub ts_expr: Expr,
}
impl FromNode for AsOfClause {
    fn from_node(n: Node) -> Self {
        AsOfClause {
            ts_expr: Expr::from_node(n),
        }
    }
}

/// `ast.SetNames`: `VariableAssignment.Name` of `SET NAMES`.
pub const SET_NAMES: &str = "SetNAMES";
/// `ast.SetCharset`: `VariableAssignment.Name` of `SET CHARSET`.
pub const SET_CHARSET: &str = "SetCharset";
/// `ast.TiDBCloudStorageURI`.
pub const TIDB_CLOUD_STORAGE_URI: &str = "tidb_cloud_storage_uri";

/// `ast.SetStmt`.
#[derive(Debug, Default)]
pub struct SetStmt {
    pub variables: Vec<VariableAssignment>,
}

/// `ast.VariableAssignment`.
#[derive(Debug, Default)]
pub struct VariableAssignment {
    pub name: String,
    pub value: Expr,
    pub is_instance: bool,
    pub is_global: bool,
    pub is_system: bool,
    /// `ExtendValue`: the collation of `SET NAMES cs COLLATE co`.
    pub extend_value: Option<Box<ValueExpr>>,
    /// Built by a placeholder action (kind of the production).
    pub unported: Option<&'static str>,
}
impl FromNode for VariableAssignment {
    fn from_node(n: Node) -> Self {
        VariableAssignment {
            unported: Some(n.kind),
            ..Default::default()
        }
    }
}

/// `ast.ShowStmtType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShowStmtType {
    #[default]
    None,
    Engines,
    Databases,
    Tables,
    TableStatus,
    Columns,
    Warnings,
    Charset,
    Variables,
    Status,
    Collation,
    CreateTable,
    CreateView,
    CreateUser,
    CreateSequence,
    CreatePlacementPolicy,
    Grants,
    Triggers,
    ProcedureStatus,
    FunctionStatus,
    Index,
    ProcessList,
    CreateDatabase,
    Config,
    Events,
    StatsExtended,
    StatsMeta,
    StatsHistograms,
    StatsTopN,
    StatsBuckets,
    StatsHealthy,
    StatsLocked,
    HistogramsInFlight,
    ColumnStatsUsage,
    Plugins,
    Profile,
    Profiles,
    MasterStatus,
    Privileges,
    Errors,
    Bindings,
    BindingCacheStatus,
    OpenTables,
    AnalyzeStatus,
    Regions,
    Builtins,
    TableNextRowId,
    Backups,
    Restores,
    Placement,
    PlacementForDatabase,
    PlacementForTable,
    PlacementForPartition,
    PlacementLabels,
    SessionStates,
    CreateResourceGroup,
    ImportJobs,
    ImportGroups,
    CreateProcedure,
    BinlogStatus,
    ReplicaStatus,
    Distributions,
    DistributionJobs,
    Affinity,
    StorageClassTransitions,
    MaskingPolicies,
}

/// `ast.ShowStmt` (the fields the ported alternatives set).
#[derive(Debug, Default)]
pub struct ShowStmt {
    pub tp: ShowStmtType,
    pub db_name: String,
    pub table: Option<Box<TableName>>,
    pub procedure: Option<Box<TableName>>,
    pub index_name: CIStr,
    pub resource_group_name: String,
    pub full: bool,
    pub if_not_exists: bool,
    pub extended: bool,
    pub count_warnings_or_errors: bool,
    pub global_scope: bool,
    /// `Pattern.Pattern` of `SHOW ... LIKE pattern`.
    pub pattern: Option<Expr>,
    pub where_: Option<Expr>,
    pub show_group_key: String,
    pub import_job_id: Option<i64>,
    pub import_job_raw: bool,
    pub distribution_job_id: Option<i64>,
    /// Built by a placeholder action (kind of the production).
    pub unported: Option<&'static str>,
}
impl FromNode for ShowStmt {
    fn from_node(n: Node) -> Self {
        ShowStmt {
            unported: Some(n.kind),
            ..Default::default()
        }
    }
}

/// `ShowLikeOrWhereOpt`: Go yields either a `*PatternLikeOrIlikeExpr` with a nil `Expr`
/// (`LIKE pattern`) or the `WHERE` expression; the Rust grammar keeps the two apart.
#[derive(Debug)]
pub enum ShowLikeOrWhere {
    Like(Expr),
    Where(Expr),
}
impl FromNode for ShowLikeOrWhere {
    fn from_node(n: Node) -> Self {
        ShowLikeOrWhere::Where(Expr::from_node(n))
    }
}
impl FromNode for Stmt {
    fn from_node(n: Node) -> Self {
        Stmt::Unported(Box::new(n))
    }
}

/// `ast.SetOprType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SetOprType {
    #[default]
    Union,
    UnionAll,
    Except,
    ExceptAll,
    Intersect,
    IntersectAll,
}
impl FromNode for SetOprType {
    fn from_node(_: Node) -> Self {
        SetOprType::Union
    }
}

/// An element of `SetOprSelectList.Selects` (`[]ast.Node` in Go).
#[derive(Debug)]
pub enum SetOprNode {
    Select(Box<SelectStmt>),
    /// A parenthesised operand that is itself a list (`SetOprSelectList`).
    List(Box<SetOprSelectList>),
    Unported(Box<Node>),
}
impl FromNode for SetOprNode {
    fn from_node(n: Node) -> Self {
        SetOprNode::Unported(Box::new(n))
    }
}

/// `ast.SetOprSelectList`.
#[derive(Debug, Default)]
pub struct SetOprSelectList {
    pub with: Option<Arc<WithClause>>,
    pub after_set_operator: Option<SetOprType>,
    pub selects: Vec<SetOprNode>,
    pub limit: Option<Box<Limit>>,
    pub order_by: Option<Box<OrderByClause>>,
}

/// `ast.SetOprStmt`.
#[derive(Debug, Default)]
pub struct SetOprStmt {
    pub is_in_braces: bool,
    pub select_list: Box<SetOprSelectList>,
    pub order_by: Option<Box<OrderByClause>>,
    pub limit: Option<Box<Limit>>,
    pub with: Option<Arc<WithClause>>,
}

/// `ast.WithClause`.
#[derive(Debug, Default)]
pub struct WithClause {
    pub is_recursive: bool,
    pub ctes: Vec<CommonTableExpression>,
    /// Built by a placeholder action (kind of the production).
    pub unported: Option<&'static str>,
}
impl FromNode for WithClause {
    fn from_node(n: Node) -> Self {
        WithClause {
            unported: Some(n.kind),
            ..Default::default()
        }
    }
}

/// `ast.CommonTableExpression`.
#[derive(Debug, Default)]
pub struct CommonTableExpression {
    pub name: CIStr,
    /// A `SubqueryExpr`.
    pub query: Expr,
    pub col_name_list: Vec<CIStr>,
    pub is_recursive: bool,
}
impl FromNode for CommonTableExpression {
    fn from_node(n: Node) -> Self {
        CommonTableExpression {
            query: Expr::from_node(n),
            ..Default::default()
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SelectStmtKind {
    #[default]
    Select,
    Table,
    Values,
}

#[derive(Debug, Default)]
pub struct SelectStmt {
    pub kind: SelectStmtKind,
    /// `IsInBraces`: a parenthesised SELECT used as a statement (or set-operation operand).
    pub is_in_braces: bool,
    /// `With`; shared (`Arc`) where Go shares the `*WithClause` with an enclosing
    /// `SetOprSelectList`.
    pub with: Option<Arc<WithClause>>,
    /// `WithBeforeBraces`: `WITH ... (SELECT ...)`.
    pub with_before_braces: bool,
    /// `AfterSetOperator`: the operator preceding this SELECT in a set operation.
    pub after_set_operator: Option<SetOprType>,
    /// `Lists`: the rows of a `VALUES ROW(...), ...` statement.
    pub lists: Vec<RowExpr>,
    pub select_stmt_opts: Box<SelectStmtOpts>,
    pub distinct: bool,
    pub fields: Box<FieldList>,
    pub from: Option<Box<TableRefsClause>>,
    pub where_: Option<Expr>,
    pub group_by: Option<Box<GroupByClause>>,
    pub having: Option<Box<HavingClause>>,
    pub window_specs: Vec<WindowSpec>,
    pub order_by: Option<Box<OrderByClause>>,
    pub limit: Option<Box<Limit>>,
    pub lock_info: Option<Box<SelectLockInfo>>,
    pub select_into_opt: Option<Box<SelectIntoOption>>,
    pub table_hints: Vec<TableOptimizerHint>,
    pub text: Option<Span>,
}
impl FromNode for SelectStmt {
    fn from_node(_: Node) -> Self {
        SelectStmt::default()
    }
}

#[derive(Debug, Default)]
pub struct SelectStmtOpts {
    pub distinct: bool,
    pub sql_big_result: bool,
    pub sql_buffer_result: bool,
    pub sql_cache: bool,
    pub sql_small_result: bool,
    pub calc_found_rows: bool,
    pub straight_join: bool,
    pub explicit_all: bool,
    pub priority: u8,
    pub table_hints: Vec<TableOptimizerHint>,
}
impl FromNode for SelectStmtOpts {
    fn from_node(_: Node) -> Self {
        SelectStmtOpts {
            sql_cache: true,
            ..Default::default()
        }
    }
}

#[derive(Debug, Default)]
pub struct FieldList {
    pub fields: Vec<SelectField>,
}
impl FromNode for FieldList {
    fn from_node(_: Node) -> Self {
        FieldList::default()
    }
}

#[derive(Debug, Default)]
pub struct WildCardField {
    pub schema: CIStr,
    pub table: CIStr,
}

#[derive(Debug, Default)]
pub struct SelectField {
    pub offset: usize,
    pub wild_card: Option<Box<WildCardField>>,
    pub expr: Option<Expr>,
    pub as_name: CIStr,
    /// setNodeText(field, src[offset:end]) -- stored as a span, not a copied string
    pub text: Option<Span>,
}
impl FromNode for SelectField {
    fn from_node(_: Node) -> Self {
        SelectField::default()
    }
}

#[derive(Debug)]
pub enum ResultSetNode {
    TableName(Box<TableName>),
    TableSource(Box<TableSource>),
    Join(Box<Join>),
    Select(Box<SelectStmt>),
    SetOpr(Box<SetOprStmt>),
    Unported(Box<Node>),
}
impl FromNode for ResultSetNode {
    fn from_node(n: Node) -> Self {
        ResultSetNode::Unported(Box::new(n))
    }
}

#[derive(Debug, Default)]
pub struct TableRefsClause {
    pub table_refs: Box<Join>,
}
impl FromNode for TableRefsClause {
    fn from_node(_: Node) -> Self {
        TableRefsClause::default()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    #[default]
    CrossJoin,
    LeftJoin,
    RightJoin,
    FullJoin,
}
impl FromNode for JoinType {
    fn from_node(_: Node) -> Self {
        JoinType::default()
    }
}

#[derive(Debug)]
pub struct OnCondition {
    pub expr: Expr,
}

#[derive(Debug, Default)]
pub struct Join {
    pub left: Option<ResultSetNode>,
    pub right: Option<ResultSetNode>,
    pub tp: JoinType,
    pub on: Option<Box<OnCondition>>,
    pub using: Vec<ColumnName>,
    pub natural_join: bool,
    pub straight_join: bool,
    pub explicit_parens: bool,
}
impl FromNode for Join {
    fn from_node(_: Node) -> Self {
        Join::default()
    }
}

#[derive(Debug)]
pub struct TableSource {
    pub source: ResultSetNode,
    pub as_name: CIStr,
}
impl FromNode for TableSource {
    fn from_node(n: Node) -> Self {
        TableSource {
            source: ResultSetNode::Unported(Box::new(n)),
            as_name: CIStr::default(),
        }
    }
}

#[derive(Debug, Default)]
pub struct TableName {
    pub schema: CIStr,
    pub name: CIStr,
    pub partition_names: Vec<CIStr>,
    pub index_hints: Vec<IndexHint>,
    pub as_of: Option<Box<AsOfClause>>,
    pub table_sample: Option<Box<TableSample>>,
}
impl FromNode for TableName {
    fn from_node(_: Node) -> Self {
        TableName::default()
    }
}

/// `ast.IndexHintType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexHintType {
    #[default]
    Use,
    Ignore,
    Force,
    OrderIndex,
    NoOrderIndex,
}
impl FromNode for IndexHintType {
    fn from_node(_: Node) -> Self {
        IndexHintType::Use
    }
}

/// `ast.IndexHintScope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexHintScope {
    #[default]
    ForScan,
    ForJoin,
    ForOrderBy,
    ForGroupBy,
}
impl FromNode for IndexHintScope {
    fn from_node(_: Node) -> Self {
        IndexHintScope::ForScan
    }
}

/// `ast.IndexHint` (`USE INDEX (a, b)` ...).
#[derive(Debug, Default)]
pub struct IndexHint {
    pub index_names: Vec<CIStr>,
    pub hint_type: IndexHintType,
    pub hint_scope: IndexHintScope,
    /// Built by a placeholder action (kind of the production).
    pub unported: Option<&'static str>,
}
impl FromNode for IndexHint {
    fn from_node(n: Node) -> Self {
        IndexHint {
            unported: Some(n.kind),
            ..Default::default()
        }
    }
}

/// `ast.SelectLockType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectLockType {
    #[default]
    None,
    ForUpdate,
    ForShare,
    ForUpdateNoWait,
    ForUpdateWaitN,
    ForShareNoWait,
    ForUpdateSkipLocked,
    ForShareSkipLocked,
}

/// `ast.SelectLockInfo` (`FOR UPDATE [OF t] [NOWAIT | WAIT n | SKIP LOCKED]`, ...).
#[derive(Debug, Default)]
pub struct SelectLockInfo {
    pub lock_type: SelectLockType,
    pub wait_sec: u64,
    pub tables: Vec<TableName>,
    /// Built by a placeholder action (kind of the production).
    pub unported: Option<&'static str>,
}
impl FromNode for SelectLockInfo {
    fn from_node(n: Node) -> Self {
        SelectLockInfo {
            unported: Some(n.kind),
            ..Default::default()
        }
    }
}

#[derive(Debug)]
pub struct ByItem {
    pub expr: Expr,
    pub desc: bool,
    pub null_order: bool,
}
impl FromNode for ByItem {
    fn from_node(n: Node) -> Self {
        ByItem {
            expr: Expr::from_node(n),
            desc: false,
            null_order: false,
        }
    }
}
#[derive(Debug, Default)]
pub struct GroupByClause {
    pub items: Vec<ByItem>,
    pub rollup: bool,
}
impl FromNode for GroupByClause {
    fn from_node(_: Node) -> Self {
        GroupByClause::default()
    }
}
#[derive(Debug, Default)]
pub struct OrderByClause {
    pub items: Vec<ByItem>,
}
impl FromNode for OrderByClause {
    fn from_node(_: Node) -> Self {
        OrderByClause::default()
    }
}
#[derive(Debug, Default)]
pub struct HavingClause {
    pub expr: Expr,
}
impl FromNode for HavingClause {
    fn from_node(n: Node) -> Self {
        HavingClause {
            expr: Expr::from_node(n),
        }
    }
}
#[derive(Debug, Default)]
pub struct Limit {
    pub count: Expr,
    pub offset: Option<Expr>,
}
impl FromNode for Limit {
    fn from_node(n: Node) -> Self {
        Limit {
            count: Expr::from_node(n),
            offset: None,
        }
    }
}

/// `ast.ExpressionDeepEqual`: compares the structural equivalence of two expressions,
/// ignoring source text positions and comparing `FuncCallExpr.fn_name` case-insensitively.
// go: pkg/parser/ast/expressions.go ExpressionDeepEqual L2 (recursive structural equality; Rust AST is immutable and carries no text positions)
pub fn expression_deep_equal(a: &Expr, b: &Expr) -> bool {
    expression_deep_equal_with(a, b, &|e| e)
}

/// Variant of [`expression_deep_equal`] that applies `resolve_a` to each node of `a` before
/// comparison, allowing callers with an immutable AST to compare expressions containing
/// local column-alias substitutions (such as `GROUP BY` resolved expressions).
// glue: immutable-AST helper for ExpressionDeepEqual with local substitutions
pub fn expression_deep_equal_with<'a, F>(a: &'a Expr, b: &'a Expr, resolve_a: &F) -> bool
where
    F: Fn(&'a Expr) -> &'a Expr,
{
    let a = resolve_a(a);
    match (a, b) {
        (Expr::Column(x), Expr::Column(y)) => x == y,
        (Expr::Value(x), Expr::Value(y)) => x == y,
        (Expr::ParamMarker(x), Expr::ParamMarker(y)) => {
            x.offset == y.offset && x.order() == y.order()
        }
        (Expr::Binary(x), Expr::Binary(y)) => {
            x.op == y.op
                && expression_deep_equal_with(&x.l, &y.l, resolve_a)
                && expression_deep_equal_with(&x.r, &y.r, resolve_a)
        }
        (Expr::Unary(x), Expr::Unary(y)) => {
            x.op == y.op && expression_deep_equal_with(&x.v, &y.v, resolve_a)
        }
        (Expr::FuncCall(x), Expr::FuncCall(y)) => {
            x.tp == y.tp
                && x.schema == y.schema
                && x.fn_name.l() == y.fn_name.l()
                && expr_slice_deep_equal(&x.args, &y.args, resolve_a)
        }
        (Expr::FuncCast(x), Expr::FuncCast(y)) => {
            x.function_type == y.function_type
                && x.explicit_char_set == y.explicit_char_set
                && x.tp == y.tp
                && expression_deep_equal_with(&x.expr, &y.expr, resolve_a)
        }
        (Expr::JSONSumCrc32(x), Expr::JSONSumCrc32(y)) => {
            x.explicit_char_set == y.explicit_char_set
                && x.tp == y.tp
                && expression_deep_equal_with(&x.expr, &y.expr, resolve_a)
        }
        (Expr::Aggregate(x), Expr::Aggregate(y)) => {
            x.f == y.f
                && x.distinct == y.distinct
                && expr_slice_deep_equal(&x.args, &y.args, resolve_a)
                && opt_order_by_deep_equal(x.order.as_deref(), y.order.as_deref(), resolve_a)
        }
        (Expr::Window(x), Expr::Window(y)) => {
            x.name == y.name
                && x.distinct == y.distinct
                && x.ignore_null == y.ignore_null
                && x.from_last == y.from_last
                && expr_slice_deep_equal(&x.args, &y.args, resolve_a)
                && window_spec_deep_equal(&x.spec, &y.spec, resolve_a)
        }
        (Expr::Paren(x), Expr::Paren(y)) => expression_deep_equal_with(x, y, resolve_a),
        (Expr::PatternIn(x), Expr::PatternIn(y)) => {
            x.not == y.not
                && expression_deep_equal_with(&x.expr, &y.expr, resolve_a)
                && expr_slice_deep_equal(&x.list, &y.list, resolve_a)
                && opt_expr_deep_equal(x.sel.as_ref(), y.sel.as_ref(), resolve_a)
        }
        (Expr::PatternLike(x), Expr::PatternLike(y)) => {
            x.not == y.not
                && x.escape == y.escape
                && x.escape_explicit == y.escape_explicit
                && x.is_like == y.is_like
                && expression_deep_equal_with(&x.expr, &y.expr, resolve_a)
                && expression_deep_equal_with(&x.pattern, &y.pattern, resolve_a)
        }
        (Expr::PatternRegexp(x), Expr::PatternRegexp(y)) => {
            x.not == y.not
                && expression_deep_equal_with(&x.expr, &y.expr, resolve_a)
                && expression_deep_equal_with(&x.pattern, &y.pattern, resolve_a)
        }
        (Expr::Between(x), Expr::Between(y)) => {
            x.not == y.not
                && expression_deep_equal_with(&x.expr, &y.expr, resolve_a)
                && expression_deep_equal_with(&x.left, &y.left, resolve_a)
                && expression_deep_equal_with(&x.right, &y.right, resolve_a)
        }
        (Expr::IsNull(x), Expr::IsNull(y)) => {
            x.not == y.not && expression_deep_equal_with(&x.expr, &y.expr, resolve_a)
        }
        (Expr::IsTruth(x), Expr::IsTruth(y)) => {
            x.not == y.not
                && x.true_ == y.true_
                && expression_deep_equal_with(&x.expr, &y.expr, resolve_a)
        }
        (Expr::Case(x), Expr::Case(y)) => {
            opt_expr_deep_equal(x.value.as_ref(), y.value.as_ref(), resolve_a)
                && x.when_clauses.len() == y.when_clauses.len()
                && x.when_clauses.iter().zip(&y.when_clauses).all(|(wx, wy)| {
                    expression_deep_equal_with(&wx.expr, &wy.expr, resolve_a)
                        && expression_deep_equal_with(&wx.result, &wy.result, resolve_a)
                })
                && opt_expr_deep_equal(x.else_clause.as_ref(), y.else_clause.as_ref(), resolve_a)
        }
        (Expr::Variable(x), Expr::Variable(y)) => {
            x.name == y.name
                && x.is_global == y.is_global
                && x.is_instance == y.is_instance
                && x.is_system == y.is_system
                && x.explicit_scope == y.explicit_scope
                && opt_expr_deep_equal(x.value.as_ref(), y.value.as_ref(), resolve_a)
        }
        (Expr::Row(x), Expr::Row(y)) => expr_slice_deep_equal(&x.values, &y.values, resolve_a),
        (Expr::SetCollation(x), Expr::SetCollation(y)) => {
            x.collate == y.collate && expression_deep_equal_with(&x.expr, &y.expr, resolve_a)
        }
        (Expr::Default(x), Expr::Default(y)) => x.name == y.name,
        (Expr::Values(x), Expr::Values(y)) => x == y,
        (Expr::MatchAgainst(x), Expr::MatchAgainst(y)) => {
            x.column_names == y.column_names
                && x.modifier == y.modifier
                && expression_deep_equal_with(&x.against, &y.against, resolve_a)
        }
        (Expr::TableName(x), Expr::TableName(y)) => {
            x.schema == y.schema && x.name == y.name && x.partition_names == y.partition_names
        }
        (Expr::TimeUnit(x), Expr::TimeUnit(y)) => x == y,
        (Expr::GetFormatSelector(x), Expr::GetFormatSelector(y)) => x == y,
        (Expr::TrimDirection(x), Expr::TrimDirection(y)) => x == y,
        (Expr::Position(x), Expr::Position(y)) => x == y,
        (Expr::Exists(x), Expr::Exists(y)) => {
            x.not == y.not && expression_deep_equal_with(&x.sel, &y.sel, resolve_a)
        }
        (Expr::CompareSubquery(x), Expr::CompareSubquery(y)) => {
            x.op == y.op
                && x.all == y.all
                && expression_deep_equal_with(&x.l, &y.l, resolve_a)
                && expression_deep_equal_with(&x.r, &y.r, resolve_a)
        }
        // Subqueries and unported nodes: compare by pointer identity (no two distinct
        // parsed subqueries in the same statement share an address).
        (Expr::Subquery(x), Expr::Subquery(y)) => std::ptr::eq(&**x, &**y),
        (Expr::Unported(x), Expr::Unported(y)) => std::ptr::eq(&**x, &**y),
        _ => false,
    }
}

fn expr_slice_deep_equal<'a, F>(a: &'a [Expr], b: &'a [Expr], resolve_a: &F) -> bool
where
    F: Fn(&'a Expr) -> &'a Expr,
{
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| expression_deep_equal_with(x, y, resolve_a))
}

fn opt_expr_deep_equal<'a, F>(a: Option<&'a Expr>, b: Option<&'a Expr>, resolve_a: &F) -> bool
where
    F: Fn(&'a Expr) -> &'a Expr,
{
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => expression_deep_equal_with(x, y, resolve_a),
        _ => false,
    }
}

fn by_item_slice_deep_equal<'a, F>(a: &'a [ByItem], b: &'a [ByItem], resolve_a: &F) -> bool
where
    F: Fn(&'a Expr) -> &'a Expr,
{
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.desc == y.desc
                && x.null_order == y.null_order
                && expression_deep_equal_with(&x.expr, &y.expr, resolve_a)
        })
}

fn opt_order_by_deep_equal<'a, F>(
    a: Option<&'a OrderByClause>,
    b: Option<&'a OrderByClause>,
    resolve_a: &F,
) -> bool
where
    F: Fn(&'a Expr) -> &'a Expr,
{
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => by_item_slice_deep_equal(&x.items, &y.items, resolve_a),
        _ => false,
    }
}

fn frame_bound_deep_equal<'a, F>(a: &'a FrameBound, b: &'a FrameBound, resolve_a: &F) -> bool
where
    F: Fn(&'a Expr) -> &'a Expr,
{
    a.tp == b.tp
        && a.unbounded == b.unbounded
        && a.unit == b.unit
        && opt_expr_deep_equal(a.expr.as_ref(), b.expr.as_ref(), resolve_a)
}

fn window_spec_deep_equal<'a, F>(a: &'a WindowSpec, b: &'a WindowSpec, resolve_a: &F) -> bool
where
    F: Fn(&'a Expr) -> &'a Expr,
{
    a.name == b.name
        && a.ref_ == b.ref_
        && a.only_alias == b.only_alias
        && a.unported == b.unported
        && match (a.partition_by.as_deref(), b.partition_by.as_deref()) {
            (None, None) => true,
            (Some(x), Some(y)) => by_item_slice_deep_equal(&x.items, &y.items, resolve_a),
            _ => false,
        }
        && opt_order_by_deep_equal(a.order_by.as_deref(), b.order_by.as_deref(), resolve_a)
        && match (a.frame.as_deref(), b.frame.as_deref()) {
            (None, None) => true,
            (Some(x), Some(y)) => {
                x.tp == y.tp
                    && frame_bound_deep_equal(&x.extent.start, &y.extent.start, resolve_a)
                    && frame_bound_deep_equal(&x.extent.end, &y.extent.end, resolve_a)
            }
            _ => false,
        }
}

#[cfg(test)]
mod deep_equal_tests {
    use super::*;
    use crate::{ParseOptions, parse};

    fn parse_select_exprs(sql: &str) -> Vec<Expr> {
        let out = parse(sql, &ParseOptions::default()).unwrap();
        let Stmt::Select(sel) = out.stmts.into_iter().next().unwrap() else {
            panic!("expected select");
        };
        sel.fields
            .fields
            .into_iter()
            .map(|f| f.expr.unwrap())
            .collect()
    }

    #[test]
    fn expression_deep_equal_func_case_and_structure() {
        let exprs = parse_select_exprs("SELECT ABS(a + 1), abs(a + 1), abs(a + 2), ABS(A + 1)");
        assert!(expression_deep_equal(&exprs[0], &exprs[1]));
        assert!(!expression_deep_equal(&exprs[0], &exprs[2]));
        // Go's exprCleaner only folds FuncCallExpr.FnName.O, not ColumnName.Name.O.
        assert!(!expression_deep_equal(&exprs[0], &exprs[3]));
    }
}
