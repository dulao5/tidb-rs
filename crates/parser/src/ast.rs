//! Hand-written Rust AST for the ported subset (SELECT core and expressions); mirrors the
//! node and field names of tidb/pkg/parser/ast. `Restore()` lives in `src/restore.rs`.
use crate::span::Span;

use crate::actions_rt::{FromNode, Node};
use crate::gen_types::*;

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

#[derive(Debug)]
pub struct ParamMarkerExpr {
    /// Byte offset of the `?`.
    pub offset: usize,
    pub order: usize,
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
    /// `BeginStmt` (BEGIN / START TRANSACTION).
    Begin(Box<BeginStmt>),
    Commit(Box<CommitStmt>),
    Rollback(Box<RollbackStmt>),
    Use(Box<UseStmt>),
    Set(Box<SetStmt>),
    Show(Box<ShowStmt>),
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
