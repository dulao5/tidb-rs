"""Hand-ported semantic actions (faithful translations of the Go actions in parser.y) for the
SELECT core.  Keys are (rule, "sym sym ...") exactly as emitted in the converted grammar.
Action text uses grmtools syntax ($1.., $span, $lexer) and the parse-param `p: &Ctx`.
"""

# Type overrides where inference from the Go code is wrong/too weak for a typed AST.
TYPES = {
    "Start": "Vec<Stmt>",
    "StatementList": "Vec<Stmt>",
    # Go: *ast.TableSource in 3 alts but `$$ = $2` (*ast.Join) in '(' TableRefs ')': polymorphic.
    "TableFactor": "ResultSetNode",
    # Go: mysql.PriorityEnum (an int enum) -> u8 constants
    "Priority": "u8",
    "PriorityOpt": "u8",
}

A = {}


def act(rule, syms, code):
    A[(rule, syms)] = code


# --- statement list (goyacc appends to parser.result as a side effect) -------------------------
act("Start", "StatementList", "$1")
act("StatementList", "Statement", "{ let mut v = Vec::new(); if let Some(s) = $1 { v.push(s) } v }")
act("StatementList", "StatementList ';' Statement",
    "{ let mut v = $1; if let Some(s) = $3 { v.push(s) } v }")

# --- SELECT ------------------------------------------------------------------------------------
act("SelectStmtBasic", "selectKwd SelectStmtOpts SelectStmtFieldList HavingClause", """{
        let mut opts = $2;
        let mut st = Box::new(SelectStmt {
            distinct: opts.distinct,
            fields: $3,
            kind: SelectStmtKind::Select,
            ..Default::default()
        });
        if !opts.table_hints.is_empty() {
            // Go shares the slice between SelectStmtOpts.TableHints and SelectStmt.TableHints;
            // Rust moves it (ownership: one owner).
            st.table_hints = std::mem::take(&mut opts.table_hints);
        }
        st.select_stmt_opts = opts;
        if let Some(h) = $4 { st.having = Some(h); }
        st
    }""")
act("SelectStmtFromDualTable", "SelectStmtBasic FromDual WhereClauseOptional", """{
        let mut st = $1;
        // goyacc: lastEnd := yyS[yypt-1].offset - 1  ($2 is FromDual: a nonterminal, so its offset
        // is only known through $span arithmetic -> keep the Field's own span instead)
        if let Some(w) = $3 { st.where_ = Some(w); }
        st
    }""")
act("SelectStmtFromTable",
    "SelectStmtBasic from TableRefsClause WhereClauseOptional SelectStmtGroup HavingClause WindowClauseOptional", """{
        let mut st = $1;
        st.from = Some($3);
        // goyacc: lastEnd := parser.endOffset(&yyS[yypt-5])  ==  start of FROM, trailing spaces trimmed
        let from_start = tok_span(&$2).start();
        if let Some(last) = st.fields.fields.last_mut() {
            if last.expr.is_some() && last.as_name.o.is_empty() {
                let raw = $lexer.src_str(Span::new(last.offset, from_start));
                last.text = Some(Span::new(last.offset, last.offset + raw.trim_end().len()));
            }
        }
        if let Some(w) = $4 { st.where_ = Some(w); }
        if let Some(g) = $5 { st.group_by = Some(g); }
        if let Some(h) = $6 { st.having = Some(h); }
        let ws = $7;
        if !ws.is_empty() { st.window_specs = ws; }
        st
    }""")
act("SelectStmt",
    "SelectStmtBasic WhereClauseOptional SelectStmtGroup OrderByOptional SelectStmtLimitOpt SelectLockOpt SelectStmtIntoOption", """{
        let mut st = $1;
        if let Some(l) = $6 { st.lock_info = Some(l); }
        if let Some(w) = $2 { st.where_ = Some(w); }
        if let Some(g) = $3 { st.group_by = Some(g); }
        if let Some(o) = $4 { st.order_by = Some(o); }
        if let Some(l) = $5 { st.limit = Some(l); }
        if let Some(i) = $7 { st.select_into_opt = Some(i); }
        Stmt::Select(st)
    }""")
act("SelectStmt",
    "SelectStmtFromDualTable SelectStmtGroup OrderByOptional SelectStmtLimitOpt SelectLockOpt SelectStmtIntoOption", """{
        let mut st = $1;
        if let Some(g) = $2 { st.group_by = Some(g); }
        if let Some(o) = $3 { st.order_by = Some(o); }
        if let Some(l) = $4 { st.limit = Some(l); }
        if let Some(l) = $5 { st.lock_info = Some(l); }
        if let Some(i) = $6 { st.select_into_opt = Some(i); }
        Stmt::Select(st)
    }""")
act("SelectStmt",
    "SelectStmtFromTable OrderByOptional SelectStmtLimitOpt SelectLockOpt SelectStmtIntoOption", """{
        let mut st = $1;
        if let Some(l) = $4 { st.lock_info = Some(l); }
        if let Some(o) = $2 { st.order_by = Some(o); }
        if let Some(l) = $3 { st.limit = Some(l); }
        if let Some(i) = $5 { st.select_into_opt = Some(i); }
        Stmt::Select(st)
    }""")

act("SelectStmtOpts", "", "Box::new(SelectStmtOpts { sql_cache: true, ..Default::default() })")
act("SelectStmtOpt", "DistinctOpt", """{
        let mut opt = Box::new(SelectStmtOpts { sql_cache: true, ..Default::default() });
        if $1 { opt.distinct = true; } else { opt.distinct = false; opt.explicit_all = true; }
        opt
    }""")
for tok, field in [("sqlSmallResult", "sql_small_result"), ("sqlBigResult", "sql_big_result"),
                   ("sqlBufferResult", "sql_buffer_result"), ("sqlCalcFoundRows", "calc_found_rows"),
                   ("straightJoin", "straight_join")]:
    act("SelectStmtOpt", tok,
        "{ let mut opt = Box::new(SelectStmtOpts { sql_cache: true, ..Default::default() }); opt.%s = true; opt }" % field)
act("SelectStmtOpt", "SelectStmtSQLCache",
    "{ let mut opt = Box::new(SelectStmtOpts { sql_cache: true, ..Default::default() }); opt.sql_cache = $1; opt }")
act("Priority", "lowPriority", "1")
act("Priority", "highPriority", "2")
act("Priority", "delayed", "3")
act("SelectStmtOpt", "Priority",
    "{ let mut opt = Box::new(SelectStmtOpts { sql_cache: true, ..Default::default() }); opt.priority = $1; opt }")
act("SelectStmtOptsList", "SelectStmtOptsList SelectStmtOpt", """{
        let mut opts = $1;
        let opt = $2;
        if !opt.table_hints.is_empty() && opts.table_hints.is_empty() { opts.table_hints = opt.table_hints; }
        if opt.distinct { opts.distinct = true; }
        if opt.priority != 0 { opts.priority = opt.priority; }
        if opt.sql_small_result { opts.sql_small_result = true; }
        if opt.sql_big_result { opts.sql_big_result = true; }
        if opt.sql_buffer_result { opts.sql_buffer_result = true; }
        if !opt.sql_cache { opts.sql_cache = false; }
        if opt.calc_found_rows { opts.calc_found_rows = true; }
        if opt.straight_join { opts.straight_join = true; }
        if opt.explicit_all { opts.explicit_all = true; }
        if opts.distinct && opts.explicit_all {
            // goyacc: yylex.AppendError(ErrWrongUsage.GenWithStackByArgs("ALL", "DISTINCT")); return 1
            p.fail("Incorrect usage of ALL and DISTINCT", $span.start());
        }
        opts
    }""")

# --- field list ------------------------------------------------------------------------------
act("SelectStmtFieldList", "FieldList", "Box::new(FieldList { fields: $1 })")
act("FieldList", "Field", """{
        let mut field = *$1;
        // goyacc: field.Offset = parser.startOffset(&yyS[yypt]); endOffset := parser.yylval.offset
        // (lookahead).  Here Field carries its own span (set in Field), trimmed like Go does.
        if let Some(sp) = field.text {
            field.offset = sp.start();
        }
        vec![field]
    }""")
act("FieldList", "FieldList ',' Field", """{
        let mut fl = $1;
        let mut field = *$3;
        if let Some(sp) = field.text {
            field.offset = sp.start();
        }
        fl.push(field);
        fl
    }""")
act("Field", "'*'", "Box::new(SelectField { wild_card: Some(Box::new(WildCardField::default())), ..Default::default() })")
act("Field", "Identifier '.' '*'",
    "Box::new(SelectField { wild_card: Some(Box::new(WildCardField { table: CIStr::new($1), ..Default::default() })), ..Default::default() })")
act("Field", "Identifier '.' Identifier '.' '*'",
    "Box::new(SelectField { wild_card: Some(Box::new(WildCardField { schema: CIStr::new($1), table: CIStr::new($3) })), ..Default::default() })")
act("Field", "Expression FieldAsNameOpt", """{
        let expr = $1;
        let as_name = $2;
        Box::new(SelectField { offset: $span.start(), expr: Some(expr), as_name: CIStr::new(as_name), text: Some($span), ..Default::default() })
    }""")
act("FieldAsNameOpt", "", "String::new()")

# --- FROM ------------------------------------------------------------------------------------
act("TableRefsClause", "TableRefs", "Box::new(TableRefsClause { table_refs: $1 })")
act("TableRefs", "EscapedTableRef", """{
        match $1 {
            ResultSetNode::Join(j) => j,
            other => Box::new(Join { left: Some(other), right: None, ..Default::default() }),
        }
    }""")
act("TableRefs", "TableRefs ',' EscapedTableRef",
    "Box::new(Join { left: Some(ResultSetNode::Join($1)), right: Some($3), tp: JoinType::CrossJoin, ..Default::default() })")
act("TableRef", "JoinTable", "ResultSetNode::Join($1)")
act("TableFactor", "TableName PartitionNameListOpt TableAsNameOpt AsOfClauseOpt IndexHintListOpt TableSampleOpt", """{
        let mut tn = $1;
        tn.partition_names = $2;
        tn.index_hints = $5;
        if let Some(ts) = $6 { tn.table_sample = Some(ts); }
        if let Some(a) = $4 { tn.as_of = Some(a); }
        ResultSetNode::TableSource(Box::new(TableSource { source: ResultSetNode::TableName(tn), as_name: $3 }))
    }""")
act("TableFactor", "'(' TableRefs ')'", """{
        let mut j = $2;
        j.explicit_parens = true;
        ResultSetNode::Join(j)
    }""")
act("TableName", "Identifier", "Box::new(TableName { name: CIStr::new($1), ..Default::default() })")
act("TableName", "Identifier '.' Identifier", """{
        let schema = $1;
        if is_incorrect_identifier_name(&schema) {
            // goyacc: yylex.AppendError(ErrWrongDBName.GenWithStackByArgs(schema)); return 1
            p.fail(format!("Incorrect database name '{schema}'"), $span.start());
        }
        Box::new(TableName { schema: CIStr::new(schema), name: CIStr::new($3), ..Default::default() })
    }""")
act("TableAsName", "Identifier", "CIStr::new($1)")
act("TableAsName", "as Identifier", "CIStr::new($2)")
act("JoinTable", "TableRef CrossOpt TableRef", "new_cross_join($1, $3)")
act("PartitionNameList", "Identifier", "vec![CIStr::new($1)]")
act("PartitionNameList", "PartitionNameList ',' Identifier", "{ let mut v = $1; v.push(CIStr::new($3)); v }")
act("JoinTable", "TableRef CrossOpt TableRef on Expression", """{
        let on = Box::new(OnCondition { expr: $5 });
        Box::new(Join { left: Some($1), right: Some($3), tp: JoinType::CrossJoin, on: Some(on), ..Default::default() })
    }""")
act("JoinTable", "TableRef JoinType OuterOpt join TableRef on Expression", """{
        let on = Box::new(OnCondition { expr: $7 });
        Box::new(Join { left: Some($1), right: Some($5), tp: $2, on: Some(on), ..Default::default() })
    }""")
act("JoinTable", "TableRef straightJoin TableRef",
    "Box::new(Join { left: Some($1), right: Some($3), straight_join: true, ..Default::default() })")
act("JoinType", "left", "JoinType::LeftJoin")
act("JoinType", "right", "JoinType::RightJoin")
act("JoinType", "fullJoinType", "JoinType::FullJoin")

# --- expressions -----------------------------------------------------------------------------
def binop(rule, syms, op, l=1, r=3):
    act(rule, syms, "Expr::Binary(Box::new(BinaryOperationExpr { op: Op::%s, l: $%d, r: $%d }))" % (op, l, r))


binop("Expression", "Expression logOr Expression", "LogicOr")
binop("Expression", "Expression xor Expression", "LogicXor")
binop("Expression", "Expression logAnd Expression", "LogicAnd")
act("Expression", "not Expression", """{
        // goyacc: if $2 is *ast.ExistsSubqueryExpr, flip its Not flag instead (unported: EXISTS)
        Expr::Unary(Box::new(UnaryOperationExpr { op: Op::Not, v: $2 }))
    }""")
act("BoolPri", "BoolPri IsOrNotOp null", "Expr::IsNull(Box::new(IsNullExpr { expr: $1, not: !$2 }))")
act("BoolPri", "BoolPri CompareOp PredicateExpr",
    "Expr::Binary(Box::new(BinaryOperationExpr { op: $2, l: $1, r: $3 }))")
for tok, op in [("ge", "GE"), ("'>'", "GT"), ("le", "LE"), ("'<'", "LT"), ("neq", "NE"),
                ("neqSynonym", "NE"), ("eq", "EQ"), ("nulleq", "NullEQ")]:
    act("CompareOp", tok, "Op::" + op)
act("PredicateExpr", "BitExpr InOrNotOp '(' ExpressionList ')'",
    "Expr::PatternIn(Box::new(PatternInExpr { expr: $1, not: !$2, list: $4 }))")
act("PredicateExpr", "BitExpr BetweenOrNotOp BitExpr and PredicateExpr",
    "Expr::Between(Box::new(BetweenExpr { expr: $1, left: $3, right: $5, not: !$2 }))")
for tok, op in [("'|'", "Or"), ("'&'", "And"), ("lsh", "LeftShift"), ("rsh", "RightShift"),
                ("'+'", "Plus"), ("'-'", "Minus"), ("'*'", "Mul"), ("'/'", "Div"), ("'%'", "Mod"),
                ("div", "IntDiv"), ("mod", "Mod"), ("'^'", "Xor")]:
    binop("BitExpr", f"BitExpr {tok} BitExpr", op)
for tok, op in [("'!'", "Not2"), ("'~'", "BitNeg"), ("'-'", "Minus"), ("'+'", "Plus"), ("not2", "Not2")]:
    act("SimpleExpr", f"{tok} SimpleExpr", "Expr::Unary(Box::new(UnaryOperationExpr { op: Op::%s, v: $2 }))" % op)
act("SimpleExpr", "'(' Expression ')'", """{
        // goyacc: parser.setNodeText(expr, src[startOffset(yyS[yypt-1]) : endOffset(yyS[yypt])])
        Expr::Paren(Box::new($2))
    }""")
act("SimpleIdent", "Identifier",
    "Expr::Column(Box::new(ColumnName { name: CIStr::new($1), ..Default::default() }))")
act("SimpleIdent", "Identifier '.' Identifier",
    "Expr::Column(Box::new(ColumnName { table: CIStr::new($1), name: CIStr::new($3), ..Default::default() }))")
act("SimpleIdent", "Identifier '.' Identifier '.' Identifier",
    "Expr::Column(Box::new(ColumnName { schema: CIStr::new($1), table: CIStr::new($3), name: CIStr::new($5) }))")
act("Literal", "falseKwd", "Expr::Value(Box::new(Datum::Bool(false)))")
act("Literal", "null", "Expr::Value(Box::new(Datum::Null))")
act("Literal", "trueKwd", "Expr::Value(Box::new(Datum::Bool(true)))")
act("Literal", "floatLit", "Expr::Value(Box::new(Datum::Float($lexer.src_str(tok_span(&$1)).parse().unwrap_or(0.0))))")
act("Literal", "decLit", "Expr::Value(Box::new(Datum::Decimal(normalize_decimal($lexer.src_str(tok_span(&$1))))))")
act("Literal", "intLit", "Expr::Value(Box::new(int_lit_datum($lexer.src_str(tok_span(&$1)))))")
act("StringLiteral", "stringLit", "Expr::Value(Box::new(Datum::Str(tok_text($lexer, &$1))))")
act("FunctionCallGeneric", "identifier '(' ExpressionListOpt ')'",
    "Expr::FuncCall(Box::new(FuncCallExpr { fn_name: CIStr::new(tok_text($lexer, &$1)), args: $3, ..Default::default() }))")
act("ExpressionList", "Expression", "vec![$1]")
act("ExpressionList", "ExpressionList ',' Expression", "{ let mut v = $1; v.push($3); v }")
act("ExpressionListOpt", "", "Vec::new()")

# --- GROUP BY / HAVING / ORDER BY / LIMIT ------------------------------------------------------
act("GroupByClause", "group by ByList WithRollupClause", "Box::new(GroupByClause { items: $3, rollup: $4 })")
act("HavingClause", "having Expression", "Some(Box::new(HavingClause { expr: $2 }))")
act("OrderBy", "order by ByList", "Box::new(OrderByClause { items: $3 })")
act("ByList", "ByItem", "vec![*$1]")
act("ByList", "ByList ',' ByItem", "{ let mut v = $1; v.push(*$3); v }")
act("ByItem", "Expression", "Box::new(ByItem { expr: position_expr($1), desc: false, null_order: true })")
act("ByItem", "Expression Order", "Box::new(ByItem { expr: position_expr($1), desc: $2, null_order: false })")
act("SelectStmtLimit", "limit LimitOption", "Box::new(Limit { count: $2, offset: None })")
act("SelectStmtLimit", "limit LimitOption ',' LimitOption", "Box::new(Limit { offset: Some($2), count: $4 })")
act("SelectStmtLimit", "limit LimitOption offset LimitOption", "Box::new(Limit { offset: Some($4), count: $2 })")
act("LimitOption", "LengthNum", "Expr::Value(Box::new(Datum::Uint($1)))")
act("LengthNum", "NUM", "$1 as u64")
act("NUM", "intLit", "$lexer.src_str(tok_span(&$1)).parse::<i64>().unwrap_or(0)")

ACTIONS = A

PROGRAMS = r'''
#[allow(unused_imports)] use crate::span::Span;


/// goyacc: isInCorrectIdentifierName
fn is_incorrect_identifier_name(name: &str) -> bool {
    name.is_empty() || name.ends_with(' ')
}

/// ByItem: a bare integer literal in ORDER/GROUP BY is a column position (ast.PositionExpr).
fn position_expr(e: Expr) -> Expr {
    match e {
        Expr::Value(d) => match *d {
            Datum::Int(i) => Expr::Position(i),
            Datum::Bool(b) => Expr::Position(b as i64),
            other => Expr::Value(Box::new(other)),
        },
        e => e,
    }
}

/// ast.NewCrossJoin: re-associates `a JOIN (b LEFT JOIN c ...)` into `(a JOIN b) LEFT JOIN c`.
fn new_cross_join(left: ResultSetNode, right: ResultSetNode) -> Box<Join> {
    let mut rj = match right {
        ResultSetNode::Join(j) if j.right.is_some() && !j.explicit_parens => j,
        other => {
            return Box::new(Join { left: Some(left), right: Some(other), tp: JoinType::CrossJoin, ..Default::default() })
        }
    };
    let mut cur: &mut Join = &mut rj;
    loop {
        if cur.tp == JoinType::RightJoin && cur.explicit_parens {
            std::mem::swap(&mut cur.left, &mut cur.right);
            cur.tp = JoinType::LeftJoin;
        }
        if !matches!(&cur.left, Some(ResultSetNode::Join(j)) if j.right.is_some()) {
            break;
        }
        cur = match cur.left.as_mut() {
            Some(ResultSetNode::Join(j)) => j,
            _ => unreachable!(),
        };
    }
    let ll = cur.left.take();
    cur.left = Some(ResultSetNode::Join(Box::new(Join { left: Some(left), right: ll, tp: JoinType::CrossJoin, ..Default::default() })));
    rj
}

fn int_lit_datum(s: &str) -> Datum {
    match s.parse::<i64>() {
        Ok(i) => Datum::Int(i),
        Err(_) => Datum::Uint(s.parse::<u64>().unwrap_or(u64::MAX)),
    }
}

fn normalize_decimal(s: &str) -> String {
    let s = if s.starts_with('.') { format!("0{s}") } else { s.to_string() };
    if s.ends_with('.') { s[..s.len() - 1].to_string() } else { s }
}
'''
