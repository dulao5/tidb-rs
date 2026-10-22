//! Minimal hand-written Rust AST for the SELECT core (mirrors tidb/pkg/parser/ast field names),
//! plus a Restore() port (DefaultRestoreFlags: single-quoted strings, uppercase keywords,
//! back-quoted names) used to verify the hand-ported actions against parser_test.go.
use crate::span::Span;

use crate::actions_rt::{FromNode, Node};
use crate::gen_types::*;

pub type R = Result<(), String>;

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
    fn literal(self) -> (&'static str, bool) {
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

#[derive(Debug, Clone, PartialEq)]
pub enum Datum {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Decimal(String),
    Str(String),
}

#[derive(Debug)]
pub enum Expr {
    Column(Box<ColumnName>),
    Value(Box<Datum>),
    Binary(Box<BinaryOperationExpr>),
    Unary(Box<UnaryOperationExpr>),
    FuncCall(Box<FuncCallExpr>),
    Paren(Box<Expr>),
    PatternIn(Box<PatternInExpr>),
    Between(Box<BetweenExpr>),
    IsNull(Box<IsNullExpr>),
    Position(i64),
    Unported(Box<Node>),
}
impl Default for Expr {
    fn default() -> Self {
        Expr::Value(Box::new(Datum::Null))
    }
}
impl FromNode for Expr {
    fn from_node(n: Node) -> Self {
        Expr::Unported(Box::new(n))
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
    pub generic: bool,
    pub schema: CIStr,
    pub fn_name: CIStr,
    pub args: Vec<Expr>,
}
#[derive(Debug)]
pub struct PatternInExpr {
    pub expr: Expr,
    pub list: Vec<Expr>,
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
    Unported(Box<Node>),
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

// ---------------------------------------------------------------------------------------------
// Restore (port of the Go Restore methods for the subset above, DefaultRestoreFlags)
// ---------------------------------------------------------------------------------------------
fn name(out: &mut String, s: &str) {
    out.push('`');
    out.push_str(&s.replace('`', "``"));
    out.push('`');
}

fn go_float_e(f: f64) -> String {
    // strconv.FormatFloat(f, 'e', -1, 64): shortest repr, exponent with sign and >= 2 digits
    let s = format!("{f:e}");
    let (m, e) = s.split_once('e').unwrap();
    let (sign, digits) = if let Some(d) = e.strip_prefix('-') {
        ('-', d)
    } else {
        ('+', e)
    };
    format!("{m}e{sign}{digits:0>2}")
}

impl Datum {
    fn restore(&self, out: &mut String) -> R {
        match self {
            Datum::Null => out.push_str("NULL"),
            Datum::Bool(b) => out.push_str(if *b { "TRUE" } else { "FALSE" }),
            Datum::Int(i) => out.push_str(&i.to_string()),
            Datum::Uint(u) => out.push_str(&u.to_string()),
            Datum::Float(f) => out.push_str(&go_float_e(*f)),
            Datum::Decimal(d) => out.push_str(d),
            Datum::Str(s) => {
                // NewValueExpr(s, parser.charset="utf8mb4", ...) -> restored with its introducer
                out.push_str("_UTF8MB4");
                // KindString with the default utf8mb4 charset -> "_utf8mb4'...'"? No: the Go
                // ValueExpr only gets a charset when the literal has an explicit introducer.
                out.push('\'');
                out.push_str(&s.replace('\'', "''"));
                out.push('\'');
            }
        }
        Ok(())
    }
}

impl Expr {
    pub fn restore(&self, out: &mut String) -> R {
        match self {
            Expr::Column(c) => c.restore(out),
            Expr::Value(d) => d.restore(out),
            Expr::Binary(b) => {
                b.l.restore(out)?;
                let (lit, kw) = b.op.literal();
                if kw {
                    out.push(' ');
                }
                out.push_str(lit);
                if kw {
                    out.push(' ');
                }
                b.r.restore(out)
            }
            Expr::Unary(u) => {
                out.push_str(u.op.literal().0);
                u.v.restore(out)
            }
            Expr::FuncCall(f) => {
                if !f.schema.o.is_empty() {
                    name(out, &f.schema.o);
                    out.push('.');
                }
                if f.generic {
                    name(out, &f.fn_name.o);
                } else {
                    out.push_str(&f.fn_name.o.to_uppercase());
                }
                out.push('(');
                for (i, a) in f.args.iter().enumerate() {
                    if i != 0 {
                        out.push_str(", ");
                    }
                    a.restore(out)?;
                }
                out.push(')');
                Ok(())
            }
            Expr::Paren(e) => {
                out.push('(');
                e.restore(out)?;
                out.push(')');
                Ok(())
            }
            Expr::PatternIn(p) => {
                p.expr.restore(out)?;
                out.push_str(if p.not { " NOT IN " } else { " IN " });
                out.push('(');
                for (i, e) in p.list.iter().enumerate() {
                    if i != 0 {
                        out.push(',');
                    }
                    e.restore(out)?;
                }
                out.push(')');
                Ok(())
            }
            Expr::Between(b) => {
                b.expr.restore(out)?;
                out.push_str(if b.not { " NOT BETWEEN " } else { " BETWEEN " });
                b.left.restore(out)?;
                out.push_str(" AND ");
                b.right.restore(out)
            }
            Expr::IsNull(n) => {
                n.expr.restore(out)?;
                out.push_str(if n.not { " IS NOT NULL" } else { " IS NULL" });
                Ok(())
            }
            Expr::Position(n) => {
                out.push_str(&n.to_string());
                Ok(())
            }
            Expr::Unported(n) => Err(format!("unported expr {}", n.kind)),
        }
    }
}

impl ColumnName {
    fn restore(&self, out: &mut String) -> R {
        if !self.schema.o.is_empty() {
            name(out, &self.schema.o);
            out.push('.');
        }
        if !self.table.o.is_empty() {
            name(out, &self.table.o);
            out.push('.');
        }
        name(out, &self.name.o);
        Ok(())
    }
}

impl Stmt {
    pub fn restore(&self, out: &mut String) -> R {
        match self {
            Stmt::Select(s) => s.restore(out),
            Stmt::Unported(n) => Err(format!("unported stmt {}", n.kind)),
        }
    }
}

fn unported_if<T>(o: &Option<T>, what: &str) -> R {
    if o.is_some() {
        Err(format!("unported {what}"))
    } else {
        Ok(())
    }
}

impl SelectStmt {
    pub fn restore(&self, out: &mut String) -> R {
        if self.kind != SelectStmtKind::Select {
            return Err("unported TABLE/VALUES".into());
        }
        out.push_str("SELECT ");
        let o = &self.select_stmt_opts;
        if !o.table_hints.is_empty() {
            return Err("unported hints".into());
        }
        match o.priority {
            1 => out.push_str("LOW_PRIORITY "),
            2 => out.push_str("HIGH_PRIORITY "),
            3 => out.push_str("DELAYED "),
            _ => {}
        }
        if o.sql_small_result {
            out.push_str("SQL_SMALL_RESULT ");
        }
        if o.sql_big_result {
            out.push_str("SQL_BIG_RESULT ");
        }
        if o.sql_buffer_result {
            out.push_str("SQL_BUFFER_RESULT ");
        }
        if !o.sql_cache {
            out.push_str("SQL_NO_CACHE ");
        }
        if o.calc_found_rows {
            out.push_str("SQL_CALC_FOUND_ROWS ");
        }
        if self.distinct {
            out.push_str("DISTINCT ");
        } else if o.explicit_all {
            out.push_str("ALL ");
        }
        if o.straight_join {
            out.push_str("STRAIGHT_JOIN ");
        }
        for (i, f) in self.fields.fields.iter().enumerate() {
            if i != 0 {
                out.push(',');
            }
            f.restore(out)?;
        }
        if let Some(from) = &self.from {
            out.push_str(" FROM ");
            from.table_refs.restore(out)?;
        }
        if self.from.is_none() && self.where_.is_some() {
            out.push_str(" FROM DUAL");
        }
        if let Some(w) = &self.where_ {
            out.push_str(" WHERE ");
            w.restore(out)?;
        }
        if let Some(g) = &self.group_by {
            out.push_str(" GROUP BY ");
            for (i, it) in g.items.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                it.restore(out)?;
            }
            if g.rollup {
                out.push_str(" WITH ROLLUP");
            }
        }
        if let Some(h) = &self.having {
            out.push_str(" HAVING ");
            h.expr.restore(out)?;
        }
        if !self.window_specs.is_empty() {
            return Err("unported WINDOW".into());
        }
        if let Some(ob) = &self.order_by {
            out.push_str(" ORDER BY ");
            for (i, it) in ob.items.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                it.restore(out)?;
            }
        }
        if let Some(l) = &self.limit {
            out.push_str(" LIMIT ");
            if let Some(off) = &l.offset {
                off.restore(out)?;
                out.push(',');
            }
            l.count.restore(out)?;
        }
        unported_if(&self.lock_info, "lock")?;
        unported_if(&self.select_into_opt, "into")?;
        Ok(())
    }
}

impl SelectField {
    fn restore(&self, out: &mut String) -> R {
        if let Some(w) = &self.wild_card {
            if !w.schema.o.is_empty() {
                name(out, &w.schema.o);
                out.push('.');
            }
            if !w.table.o.is_empty() {
                name(out, &w.table.o);
                out.push('.');
            }
            out.push('*');
        }
        if let Some(e) = &self.expr {
            e.restore(out)?;
        }
        if !self.as_name.o.is_empty() {
            out.push_str(" AS ");
            name(out, &self.as_name.o);
        }
        Ok(())
    }
}

impl ByItem {
    fn restore(&self, out: &mut String) -> R {
        self.expr.restore(out)?;
        if self.desc {
            out.push_str(" DESC");
        }
        Ok(())
    }
}

impl ResultSetNode {
    fn restore(&self, out: &mut String) -> R {
        match self {
            ResultSetNode::TableName(t) => t.restore_with_alias(out, &CIStr::default()),
            ResultSetNode::TableSource(t) => t.restore(out),
            ResultSetNode::Join(j) => j.restore(out),
            ResultSetNode::Select(_) => Err("unported subquery".into()),
            ResultSetNode::Unported(n) => Err(format!("unported resultset {}", n.kind)),
        }
    }
}

impl TableSource {
    fn restore(&self, out: &mut String) -> R {
        match &self.source {
            ResultSetNode::TableName(tn) => tn.restore_with_alias(out, &self.as_name),
            ResultSetNode::TableSource(_) => Err("nested table source".into()),
            ResultSetNode::Join(j) => {
                j.restore(out)?;
                if !self.as_name.o.is_empty() {
                    out.push_str(" AS ");
                    name(out, &self.as_name.o);
                }
                Ok(())
            }
            ResultSetNode::Select(_) | ResultSetNode::Unported(_) => self.source.restore(out),
        }
    }
}

impl TableName {
    pub fn restore_with_alias(&self, out: &mut String, as_name: &CIStr) -> R {
        if !self.schema.o.is_empty() {
            name(out, &self.schema.o);
            out.push('.');
        }
        name(out, &self.name.o);
        if !self.partition_names.is_empty() {
            out.push_str(" PARTITION(");
            for (i, p) in self.partition_names.iter().enumerate() {
                if i != 0 {
                    out.push_str(", ");
                }
                name(out, &p.o);
            }
            out.push(')');
        }
        if !as_name.o.is_empty() {
            out.push_str(" AS ");
            name(out, &as_name.o);
        }
        unported_if(&self.as_of, "as of")?;
        if !self.index_hints.is_empty() {
            return Err("unported index hints".into());
        }
        unported_if(&self.table_sample, "tablesample")
    }
}

impl Join {
    fn restore(&self, out: &mut String) -> R {
        let left = self.left.as_ref().ok_or("join without left")?;
        let left_is_join = matches!(left, ResultSetNode::Join(_));
        let mut use_comma_join = false;
        if let ResultSetNode::Join(lj) = left
            && lj.right.is_none()
            && let Some(ResultSetNode::TableSource(ts)) = &lj.left
            && matches!(ts.source, ResultSetNode::Select(_))
        {
            use_comma_join = true;
        }
        if left_is_join && !use_comma_join {
            out.push('(');
        }
        left.restore(out)?;
        if left_is_join && !use_comma_join {
            out.push(')');
        }
        let Some(right) = &self.right else {
            return Ok(());
        };
        if self.natural_join {
            out.push_str(" NATURAL");
        }
        match self.tp {
            JoinType::LeftJoin => out.push_str(" LEFT"),
            JoinType::RightJoin => out.push_str(" RIGHT"),
            JoinType::FullJoin => out.push_str(" FULL OUTER"),
            JoinType::CrossJoin => {}
        }
        if self.straight_join {
            out.push_str(" STRAIGHT_JOIN ");
        } else if use_comma_join {
            out.push_str(", ");
        } else {
            out.push_str(" JOIN ");
        }
        let right_is_join = matches!(right, ResultSetNode::Join(_));
        if right_is_join {
            out.push('(');
        }
        right.restore(out)?;
        if right_is_join {
            out.push(')');
        }
        if let Some(on) = &self.on {
            out.push_str(" ON ");
            on.expr.restore(out)?;
        }
        if !self.using.is_empty() {
            return Err("unported USING".into());
        }
        Ok(())
    }
}
