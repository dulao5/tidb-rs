"""Hand-ported semantic actions (faithful translations of the Go actions in parser.y) for the
SELECT core and expressions.  Keys are (rule, "sym sym ...") exactly as emitted in the converted
grammar.  Action text uses grmtools syntax ($1.., $span, $lexer) and the parse-param `p: &Ctx`.
"""

# Rust names for Go types whose default stub name (package prefix + name) is not wanted.
TYPE_NAMES = {
    "types.FieldType": "FieldType",
}

# Type overrides where inference from the Go code is wrong/too weak for a typed AST.
TYPES = {
    "Start": "Vec<Stmt>",
    "StatementList": "Vec<Stmt>",
    # Go: *ast.TableSource in 3 alts but `$$ = $2` (*ast.Join) in '(' TableRefs ')': polymorphic.
    "TableFactor": "ResultSetNode",
    # Go: mysql.PriorityEnum (an int enum) -> u8 constants
    "Priority": "u8",
    "PriorityOpt": "u8",
    # Go: an untyped item (int64/uint64/float64/*MyDecimal literal value).
    "NumLiteral": "Datum",
    # Go: ast.FrameExtent, never asserted (only passed through WindowFrameExtent).
    "WindowFrameBetween": "FrameExtent",
    # Go: *ast.PatternLikeOrIlikeExpr (LIKE, with a nil Expr) or the WHERE ast.ExprNode.
    "ShowLikeOrWhereOpt": "Option<ShowLikeOrWhere>",
    # Go: []ast.Node holding *ast.SelectStmt / *ast.SetOprSelectList.
    "SetOprClauseList": "Vec<SetOprNode>",
    "SetOprClause": "Vec<SetOprNode>",
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
act("JoinTable", "TableRef CrossOpt TableRef using '(' ColumnNameList ')'",
    "Box::new(Join { left: Some($1), right: Some($3), tp: JoinType::CrossJoin, using: $6, ..Default::default() })")
act("JoinTable", "TableRef JoinType OuterOpt join TableRef using '(' ColumnNameList ')'",
    "Box::new(Join { left: Some($1), right: Some($5), tp: $2, using: $8, ..Default::default() })")
act("JoinTable", "TableRef natural join TableRef",
    "Box::new(Join { left: Some($1), right: Some($4), natural_join: true, ..Default::default() })")
act("JoinTable", "TableRef natural JoinType OuterOpt join TableRef",
    "Box::new(Join { left: Some($1), right: Some($6), tp: $3, natural_join: true, ..Default::default() })")
act("JoinTable", "TableRef straightJoin TableRef on Expression", """{
        let on = Box::new(OnCondition { expr: $5 });
        Box::new(Join { left: Some($1), right: Some($3), straight_join: true, on: Some(on), ..Default::default() })
    }""")
act("JoinTable", "TableRef straightJoin TableRef using '(' ColumnNameList ')'",
    "Box::new(Join { left: Some($1), right: Some($3), straight_join: true, using: $6, ..Default::default() })")
act("JoinType", "left", "JoinType::LeftJoin")
act("JoinType", "right", "JoinType::RightJoin")
act("JoinType", "fullJoinType", "JoinType::FullJoin")

# --- expressions -----------------------------------------------------------------------------
def binop(rule, syms, op, l=1, r=3):
    act(rule, syms, "Expr::Binary(Box::new(BinaryOperationExpr { op: Op::%s, l: $%d, r: $%d }))" % (op, l, r))


binop("Expression", "Expression logOr Expression", "LogicOr")
binop("Expression", "Expression xor Expression", "LogicXor")
binop("Expression", "Expression logAnd Expression", "LogicAnd")
act("BoolPri", "BoolPri IsOrNotOp null", "Expr::IsNull(Box::new(IsNullExpr { expr: $1, not: !$2 }))")
act("BoolPri", "BoolPri CompareOp PredicateExpr",
    "Expr::Binary(Box::new(BinaryOperationExpr { op: $2, l: $1, r: $3 }))")
for tok, op in [("ge", "GE"), ("'>'", "GT"), ("le", "LE"), ("'<'", "LT"), ("neq", "NE"),
                ("neqSynonym", "NE"), ("eq", "EQ"), ("nulleq", "NullEQ")]:
    act("CompareOp", tok, "Op::" + op)
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
act("Literal", "falseKwd", "val(Datum::Bool(false), p)")
act("Literal", "null", "val(Datum::Null, p)")
act("Literal", "trueKwd", "val(Datum::Bool(true), p)")
act("Literal", "floatLit", "val(Datum::Float($lexer.src_str(tok_span(&$1)).parse().unwrap_or(0.0)), p)")
act("Literal", "decLit", "val(Datum::Decimal(normalize_decimal($lexer.src_str(tok_span(&$1)))), p)")
act("Literal", "intLit", "val(int_lit_datum($lexer.src_str(tok_span(&$1))), p)")
act("StringLiteral", "stringLit", "val(Datum::Str(tok_text($lexer, &$1)), p)")
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
act("LimitOption", "LengthNum", "val(Datum::Uint($1), p)")
act("LengthNum", "NUM", "$1 as u64")
act("NUM", "intLit", "$lexer.src_str(tok_span(&$1)).parse::<i64>().unwrap_or(0)")

# --- expressions: literals, variables, markers ---------------------------------------------------
# NewValueExpr(x, parser.charset, parser.collation) -> val(Datum, p); NewValueExpr(x, "", "") -> val_nocs
act("StringLiteral", "StringLiteral stringLit", """{
        // Fix #4239: the first string literal is the projection name.
        let (str_lit, off) = match &$1 {
            Expr::Value(v) => match &v.datum {
                Datum::Str(s) => (s.clone(), v.projection_offset),
                _ => (String::new(), v.projection_offset),
            },
            _ => (String::new(), -1),
        };
        let first_len = str_lit.len() as i32;
        let mut expr = ValueExpr::new(Datum::Str(str_lit + &tok_text($lexer, &$2)), p.charset, p.collation);
        expr.projection_offset = if off >= 0 { off } else { first_len };
        Expr::Value(Box::new(expr))
    }""")
act("Literal", "hexLit", "val(hex_lit($lexer.src_str(tok_span(&$1))), p)")
act("Literal", "bitLit", "val(bit_lit($lexer.src_str(tok_span(&$1))), p)")
for k, (tok, conv) in enumerate([("stringLit", "Datum::Str(tok_text($lexer, &$2))"),
                                 ("hexLit", "hex_lit($lexer.src_str(tok_span(&$2)))"),
                                 ("bitLit", "bit_lit($lexer.src_str(tok_span(&$2)))")]):
    act("Literal", "underscoreCS " + tok, """{
        // See https://dev.mysql.com/doc/refman/5.7/en/charset-literal.html
        let cs = underscore_cs($lexer.src_str(tok_span(&$1)));
        match crate::charset::get_default_collation_legacy(cs) {
            None => {
                p.fail(format!("Unsupported character introducer: '{cs}'"), $span.start());
                Expr::default()
            }
            Some(co) => {
                // tp.SetCharset/SetCollate after NewValueExpr; flags only matter for evaluation.
                let mut expr = ValueExpr::new(%s, cs, co);
                expr.charset = cs;
                expr.collate = co;
                Expr::Value(Box::new(expr))
            }
        }
    }""" % conv)
act("SimpleExpr", "paramMarker", "Expr::ParamMarker(Box::new(ParamMarkerExpr { offset: tok_span(&$1).start(), order: 0 }))")
act("LimitOption", "paramMarker", "Expr::ParamMarker(Box::new(ParamMarkerExpr { offset: tok_span(&$1).start(), order: 0 }))")
act("SystemVariable", "doubleAtIdentifier", """{
        let mut v = at_ident_text($lexer.src_str(tok_span(&$1))).to_lowercase();
        let (mut is_global, mut is_instance, mut explicit_scope) = (false, false, true);
        if let Some(r) = v.strip_prefix("@@global.") {
            is_global = true;
            v = r.to_string();
        } else if let Some(r) = v.strip_prefix("@@instance.") {
            is_instance = true;
            v = r.to_string();
        } else if let Some(r) = v.strip_prefix("@@session.") {
            v = r.to_string();
        } else if let Some(r) = v.strip_prefix("@@local.") {
            v = r.to_string();
        } else if let Some(r) = v.strip_prefix("@@") {
            v = r.to_string();
            explicit_scope = false;
        }
        Expr::Variable(Box::new(VariableExpr { name: v, is_global, is_instance, is_system: true, explicit_scope, value: None }))
    }""")
act("UserVariable", "singleAtIdentifier",
    "Expr::Variable(Box::new(VariableExpr { name: at_ident_text($lexer.src_str(tok_span(&$1))), ..Default::default() }))")
act("Expression", "singleAtIdentifier assignmentEq Expression",
    "Expr::Variable(Box::new(VariableExpr { name: at_ident_text($lexer.src_str(tok_span(&$1))), value: Some($3), ..Default::default() }))")
act("BoolPri", "BoolPri CompareOp singleAtIdentifier assignmentEq PredicateExpr", """{
        let variable = Expr::Variable(Box::new(VariableExpr { name: at_ident_text($lexer.src_str(tok_span(&$3))), value: Some($5), ..Default::default() }));
        Expr::Binary(Box::new(BinaryOperationExpr { op: $2, l: $1, r: variable }))
    }""")

# --- predicates ----------------------------------------------------------------------------------
act("Expression", "not Expression", """{
        match $2 {
            Expr::Exists(mut e) => {
                e.not = !e.not;
                Expr::Exists(e)
            }
            other => Expr::Unary(Box::new(UnaryOperationExpr { op: Op::Not, v: other })),
        }
    }""")
act("Expression", "BoolPri IsOrNotOp trueKwd", "Expr::IsTruth(Box::new(IsTruthExpr { expr: $1, not: !$2, true_: 1 }))")
act("Expression", "BoolPri IsOrNotOp falseKwd", "Expr::IsTruth(Box::new(IsTruthExpr { expr: $1, not: !$2, true_: 0 }))")
act("Expression", "BoolPri IsOrNotOp unknown", "Expr::IsNull(Box::new(IsNullExpr { expr: $1, not: !$2 }))")
act("Expression", "match '(' ColumnNameList ')' against '(' BitExpr FulltextSearchModifierOpt ')'",
    "Expr::MatchAgainst(Box::new(MatchAgainst { column_names: $3, against: $7, modifier: $8 }))")
act("FulltextSearchModifierOpt", "", "0")
act("FulltextSearchModifierOpt", "in natural language mode", "0")
act("FulltextSearchModifierOpt", "in natural language mode with query expansion",
    "fulltext_modifier::WITH_QUERY_EXPANSION")
act("FulltextSearchModifierOpt", "in booleanType mode", "fulltext_modifier::BOOLEAN_MODE")
act("FulltextSearchModifierOpt", "with query expansion", "fulltext_modifier::WITH_QUERY_EXPANSION")
act("BoolPri", "BoolPri CompareOp AnyOrAll SubSelect", """{
        let mut sq = $4;
        if let Expr::Subquery(s) = &mut sq { s.multi_rows = true; }
        Expr::CompareSubquery(Box::new(CompareSubqueryExpr { op: $2, l: $1, r: sq, all: $3 }))
    }""")
act("PredicateExpr", "BitExpr InOrNotOp SubSelect", """{
        let mut sq = $3;
        if let Expr::Subquery(s) = &mut sq { s.multi_rows = true; }
        Expr::PatternIn(Box::new(PatternInExpr { expr: $1, not: !$2, list: Vec::new(), sel: Some(sq) }))
    }""")
act("PredicateExpr", "BitExpr InOrNotOp '(' ExpressionList ')'",
    "Expr::PatternIn(Box::new(PatternInExpr { expr: $1, not: !$2, list: $4, sel: None }))")
for kind, is_like in [("LikeOrNotOp", "true"), ("IlikeOrNotOp", "false")]:
    act("PredicateExpr", f"BitExpr {kind} SimpleExpr LikeOrIlikeEscapeOpt", """{
        let spec = $4;
        if spec.escape.len() > 1 {
            // goyacc: yylex.AppendError(ErrWrongArguments.GenWithStackByArgs("ESCAPE")); return 1
            p.fail("Incorrect arguments to ESCAPE", $span.start());
        }
        // ESCAPE '' is explicit with no escape character (Escape = 0).
        let escape = spec.escape.as_bytes().first().copied().unwrap_or(0);
        Expr::PatternLike(Box::new(PatternLikeOrIlikeExpr {
            expr: $1, pattern: $3, not: !$2, escape, escape_explicit: spec.explicit, is_like: %s,
        }))
    }""" % is_like)
act("LikeOrIlikeEscapeOpt", "", "Box::new(LikeEscapeSpec { escape: \"\\\\\".into(), explicit: false })")
act("LikeOrIlikeEscapeOpt", "escape stringLit",
    "Box::new(LikeEscapeSpec { escape: tok_text($lexer, &$2), explicit: true })")
act("PredicateExpr", "BitExpr RegexpOrNotOp SimpleExpr",
    "Expr::PatternRegexp(Box::new(PatternRegexpExpr { expr: $1, pattern: $3, not: !$2 }))")
act("PredicateExpr", "BitExpr memberof '(' SimpleExpr ')'", "call(JSON_MEMBER_OF, vec![$1, $4])")

# --- simple expressions --------------------------------------------------------------------------
act("SimpleExpr", "SimpleExpr collate CollationName",
    "Expr::SetCollation(Box::new(SetCollationExpr { expr: $1, collate: $3 }))")
act("SimpleExpr", "SimpleExpr pipes SimpleExpr", "call(CONCAT, vec![$1, $3])")
act("SimpleExpr", "'(' ExpressionList ',' Expression ')'",
    "{ let mut values = $2; values.push($4); Expr::Row(Box::new(RowExpr { values })) }")
act("SimpleExpr", "row '(' ExpressionList ',' Expression ')'",
    "{ let mut values = $3; values.push($5); Expr::Row(Box::new(RowExpr { values })) }")
act("SimpleExpr", "exists SubSelect", """{
        let mut sq = $2;
        if let Expr::Subquery(s) = &mut sq { s.exists = true; }
        Expr::Exists(Box::new(ExistsSubqueryExpr { sel: sq, not: false }))
    }""")
act("SimpleExpr", "'{' Identifier Expression '}'", """{
        // ODBC escape syntax, see https://dev.mysql.com/doc/refman/5.7/en/expressions.html
        let mut expr = $3;
        let name = match $2.as_str() {
            "d" => DATE_LITERAL,
            "t" => TIME_LITERAL,
            "ts" => TIMESTAMP_LITERAL,
            _ => return expr,
        };
        // tp := $3.GetType(); tp.SetCharset(""); tp.SetCollate("")
        if let Expr::Value(v) = &mut expr {
            v.charset = "";
            v.collate = "";
        }
        call(name, vec![expr])
    }""")
act("SimpleExpr", "binaryType SimpleExpr", """{
        // See https://dev.mysql.com/doc/refman/5.7/en/cast-functions.html#operator_binary
        let mut tp = FieldType::new(mysql_type::STRING);
        tp.set_bin();
        tp.flag |= mysql_flag::BINARY;
        Expr::FuncCast(Box::new(FuncCastExpr { expr: $2, tp: Box::new(tp), function_type: CastFunctionType::BinaryOperator, explicit_char_set: false }))
    }""")
act("SimpleExpr", "builtinCast '(' Expression as CastType ArrayKwdOpt ')'", """{
        // See https://dev.mysql.com/doc/refman/5.7/en/cast-functions.html#function_cast
        let mut tp = $5;
        let (default_flen, default_decimal) = default_length_and_decimal_for_cast(tp.tp);
        if tp.flen == UNSPECIFIED_LENGTH { tp.flen = default_flen; }
        if tp.decimal == UNSPECIFIED_LENGTH { tp.decimal = default_decimal; }
        let is_array = $6;
        tp.array = is_array;
        let explicit_charset = p.explicit_charset.get();
        if is_array && !explicit_charset && tp.charset != crate::charset::CHARSET_BIN {
            tp.charset = crate::charset::CHARSET_UTF8MB4.into();
            tp.collate = crate::charset::COLLATION_UTF8MB4.into();
        }
        p.explicit_charset.set(false);
        Expr::FuncCast(Box::new(FuncCastExpr { expr: $3, tp, function_type: CastFunctionType::Cast, explicit_char_set: explicit_charset }))
    }""")
act("SimpleExpr", "jsonSumCrc32 '(' Expression as CastType array ')'", """{
        // Copied from CAST, except that ARRAY is enforced to be true.
        let mut tp = $5;
        let (default_flen, default_decimal) = default_length_and_decimal_for_cast(tp.tp);
        if tp.flen == UNSPECIFIED_LENGTH { tp.flen = default_flen; }
        if tp.decimal == UNSPECIFIED_LENGTH { tp.decimal = default_decimal; }
        tp.array = true;
        let explicit_charset = p.explicit_charset.get();
        if !explicit_charset && tp.charset != crate::charset::CHARSET_BIN {
            tp.charset = crate::charset::CHARSET_UTF8MB4.into();
            tp.collate = crate::charset::COLLATION_UTF8MB4.into();
        }
        p.explicit_charset.set(false);
        Expr::JSONSumCrc32(Box::new(JSONSumCrc32Expr { expr: $3, tp, explicit_char_set: explicit_charset }))
    }""")
act("SimpleExpr", "caseKwd ExpressionOpt WhenClauseList ElseOpt end",
    "Expr::Case(Box::new(CaseExpr { value: $2, when_clauses: $3, else_clause: $4 }))")
act("WhenClauseList", "WhenClause", "vec![*$1]")
act("WhenClauseList", "WhenClauseList WhenClause", "{ let mut v = $1; v.push(*$2); v }")
act("WhenClause", "when Expression then Expression", "Box::new(WhenClause { expr: $2, result: $4 })")
act("ElseOpt", "elseKwd Expression", "Some($2)")
act("SimpleExpr", "convert '(' Expression ',' CastType ')'", """{
        // See https://dev.mysql.com/doc/refman/5.7/en/cast-functions.html#function_convert
        let mut tp = $5;
        let (default_flen, default_decimal) = default_length_and_decimal_for_cast(tp.tp);
        if tp.flen == UNSPECIFIED_LENGTH { tp.flen = default_flen; }
        if tp.decimal == UNSPECIFIED_LENGTH { tp.decimal = default_decimal; }
        let explicit_charset = p.explicit_charset.get();
        p.explicit_charset.set(false);
        Expr::FuncCast(Box::new(FuncCastExpr { expr: $3, tp, function_type: CastFunctionType::Convert, explicit_char_set: explicit_charset }))
    }""")
act("SimpleExpr", "convert '(' Expression using CharsetName ')'",
    "call(tok_text($lexer, &$1), vec![$3, val_nocs(Datum::Str($5))])")
act("SimpleExpr", "defaultKwd '(' SimpleIdent ')'",
    "Expr::Default(Box::new(DefaultExpr { name: Some(column_of($3)) }))")
act("SimpleExpr", "values '(' SimpleIdent ')'", "Expr::Values(column_of($3))")
act("SimpleExpr", "SimpleIdent jss stringLit", "call(JSON_EXTRACT, vec![$1, val(Datum::Str(tok_text($lexer, &$3)), p)])")
act("SimpleExpr", "SimpleIdent juss stringLit", """{
        let extract = call(JSON_EXTRACT, vec![$1, val(Datum::Str(tok_text($lexer, &$3)), p)]);
        call(JSON_UNQUOTE, vec![extract])
    }""")

# --- subqueries ----------------------------------------------------------------------------------
# goyacc also calls setLastSelectFieldText / setNodeText here; node texts are not ported yet.
act("SubSelect", "'(' SelectStmt ')'", "subquery(stmt_to_result_set($2))")
act("SubSelect", "'(' SetOprStmt ')'", "subquery(stmt_to_result_set($2))")
act("SubSelect", "'(' SelectStmtWithClause ')'", "subquery(stmt_to_result_set($2))")
act("SubSelect", "'(' SubSelect ')'", """{
        // Redundant brackets like '((select 1))': re-wrap the inner query in a fresh SubqueryExpr.
        match $2 {
            Expr::Subquery(sq) => subquery(sq.query),
            other => other,
        }
    }""")
act("TableFactor", "SubSelect TableAsNameOpt", """{
        match $1 {
            Expr::Subquery(sq) => ResultSetNode::TableSource(Box::new(TableSource { source: sq.query, as_name: $2 })),
            other => unreachable!("SubSelect yields a SubqueryExpr, got {other:?}"),
        }
    }""")

# --- interval arithmetic -------------------------------------------------------------------------
act("BitExpr", "BitExpr '+' interval Expression TimeUnit", "call(\"DATE_ADD\", vec![$1, $4, Expr::TimeUnit($5)])")
act("BitExpr", "BitExpr '-' interval Expression TimeUnit", "call(\"DATE_SUB\", vec![$1, $4, Expr::TimeUnit($5)])")
act("BitExpr", "interval Expression TimeUnit '+' BitExpr", "call(\"DATE_ADD\", vec![$5, $2, Expr::TimeUnit($3)])")
for tok, unit in [("secondMicrosecond", "SecondMicrosecond"), ("minuteMicrosecond", "MinuteMicrosecond"),
                  ("minuteSecond", "MinuteSecond"), ("hourMicrosecond", "HourMicrosecond"),
                  ("hourSecond", "HourSecond"), ("hourMinute", "HourMinute"),
                  ("dayMicrosecond", "DayMicrosecond"), ("daySecond", "DaySecond"),
                  ("dayMinute", "DayMinute"), ("dayHour", "DayHour"), ("yearMonth", "YearMonth")]:
    act("TimeUnit", tok, "TimeUnitType::" + unit)
for tok, unit in [("microsecond", "Microsecond"), ("second", "Second"), ("minute", "Minute"),
                  ("hour", "Hour"), ("day", "Day"), ("week", "Week"), ("month", "Month"),
                  ("quarter", "Quarter"), ("yearType", "Year"), ("sqlTsiSecond", "Second"),
                  ("sqlTsiMinute", "Minute"), ("sqlTsiHour", "Hour"), ("sqlTsiDay", "Day"),
                  ("sqlTsiWeek", "Week"), ("sqlTsiMonth", "Month"), ("sqlTsiQuarter", "Quarter"),
                  ("sqlTsiYear", "Year")]:
    act("TimestampUnit", tok, "TimeUnitType::" + unit)
for tok, sel in [("dateType", "Date"), ("datetimeType", "Datetime"), ("timeType", "Time"),
                 ("timestampType", "Datetime")]:
    act("GetFormatSelector", tok, "GetFormatSelectorType::" + sel)
for tok, d in [("both", "Both"), ("leading", "Leading"), ("trailing", "Trailing")]:
    act("TrimDirection", tok, "TrimDirectionType::" + d)

# --- function calls ------------------------------------------------------------------------------
act("FunctionCallGeneric", "identifier '(' ExpressionListOpt ')'", "call(tok_text($lexer, &$1), $3)")
act("FunctionCallGeneric", "Identifier '.' Identifier '(' ExpressionListOpt ')'", """{
        let tp = if is_in_token_map(&$3) { FuncCallExprType::Keyword } else { FuncCallExprType::Generic };
        Expr::FuncCall(Box::new(FuncCallExpr { tp, schema: CIStr::new($1), fn_name: CIStr::new($3), args: $5 }))
    }""")
act("FunctionCallKeyword", "FunctionNameConflict '(' ExpressionListOpt ')'", "call($1, $3)")
act("FunctionCallKeyword", "interval '(' Expression ',' Expression ')'", "call(INTERVAL, vec![$3, $5])")
act("FunctionCallKeyword", "interval '(' Expression ',' Expression ',' ExpressionList ')'",
    "{ let mut args = vec![$3, $5]; args.extend($7); call(INTERVAL, args) }")
act("FunctionCallKeyword", "builtinUser '(' ExpressionListOpt ')'", "call(tok_text($lexer, &$1), $3)")
act("FunctionCallKeyword", "FunctionNameOptionalBraces OptionalBraces", "call($1, Vec::new())")
act("FunctionCallKeyword", "builtinCurDate '(' ')'", "call(tok_text($lexer, &$1), Vec::new())")
act("FunctionCallKeyword", "FunctionNameDatetimePrecision FuncDatetimePrec",
    "{ let mut args = Vec::new(); if let Some(e) = $2 { args.push(e); } call($1, args) }")
act("FunctionCallKeyword", "charType '(' ExpressionList ')'",
    "{ let mut args = $3; args.push(val(Datum::Null, p)); call(CHAR_FUNC, args) }")
act("FunctionCallKeyword", "charType '(' ExpressionList using CharsetName ')'",
    "{ let mut args = $3; args.push(val_nocs(Datum::Str($5))); call(CHAR_FUNC, args) }")
for tok, name in [("dateType", "DATE_LITERAL"), ("timeType", "TIME_LITERAL"), ("timestampType", "TIMESTAMP_LITERAL")]:
    act("FunctionCallKeyword", f"{tok} stringLit", f"call({name}, vec![val_nocs(Datum::Str(tok_text($lexer, &$2)))])")
act("FunctionCallKeyword", "insert '(' ExpressionListOpt ')'", "call(INSERT_FUNC, $3)")
act("FunctionCallKeyword", "mod '(' Expression ',' Expression ')'",
    "Expr::Binary(Box::new(BinaryOperationExpr { op: Op::Mod, l: $3, r: $5 }))")
act("FunctionCallKeyword", "password '(' ExpressionListOpt ')'", "call(PASSWORD_FUNC, $3)")
act("FuncDatetimePrec", "'(' intLit ')'", "Some(val(int_lit_datum($lexer.src_str(tok_span(&$2))), p))")
act("FuncDatetimePrecList", "intLit", "vec![val(int_lit_datum($lexer.src_str(tok_span(&$1))), p)]")
act("FuncDatetimePrecListOpt", "", "Vec::new()")

act("FunctionCallNonKeyword", "builtinCurTime '(' FuncDatetimePrecListOpt ')'", "call(tok_text($lexer, &$1), $3)")
act("FunctionCallNonKeyword", "builtinSysDate '(' FuncDatetimePrecListOpt ')'", "call(tok_text($lexer, &$1), $3)")
act("FunctionCallNonKeyword", "FunctionNameDateArithMultiForms '(' Expression ',' Expression ')'",
    "call($1, vec![$3, $5, Expr::TimeUnit(TimeUnitType::Day)])")
act("FunctionCallNonKeyword", "FunctionNameDateArithMultiForms '(' Expression ',' interval Expression TimeUnit ')'",
    "call($1, vec![$3, $6, Expr::TimeUnit($7)])")
act("FunctionCallNonKeyword", "FunctionNameDateArith '(' Expression ',' interval Expression TimeUnit ')'",
    "call($1, vec![$3, $6, Expr::TimeUnit($7)])")
act("FunctionCallNonKeyword", "builtinExtract '(' TimeUnit from Expression ')'",
    "call(tok_text($lexer, &$1), vec![Expr::TimeUnit($3), $5])")
act("FunctionCallNonKeyword", "getFormat '(' GetFormatSelector ',' Expression ')'",
    "call(tok_text($lexer, &$1), vec![Expr::GetFormatSelector($3), $5])")
act("FunctionCallNonKeyword", "builtinPosition '(' BitExpr in Expression ')'", "call(tok_text($lexer, &$1), vec![$3, $5])")
act("FunctionCallNonKeyword", "builtinSubstring '(' Expression ',' Expression ')'", "call(tok_text($lexer, &$1), vec![$3, $5])")
act("FunctionCallNonKeyword", "builtinSubstring '(' Expression from Expression ')'", "call(tok_text($lexer, &$1), vec![$3, $5])")
act("FunctionCallNonKeyword", "builtinSubstring '(' Expression ',' Expression ',' Expression ')'",
    "call(tok_text($lexer, &$1), vec![$3, $5, $7])")
act("FunctionCallNonKeyword", "builtinSubstring '(' Expression from Expression forKwd Expression ')'",
    "call(tok_text($lexer, &$1), vec![$3, $5, $7])")
for tok in ["timestampAdd", "timestampDiff"]:
    act("FunctionCallNonKeyword", f"{tok} '(' TimestampUnit ',' Expression ',' Expression ')'",
        "call(tok_text($lexer, &$1), vec![Expr::TimeUnit($3), $5, $7])")
act("FunctionCallNonKeyword", "builtinTrim '(' Expression ')'", "call(tok_text($lexer, &$1), vec![$3])")
act("FunctionCallNonKeyword", "builtinTrim '(' Expression from Expression ')'", "call(tok_text($lexer, &$1), vec![$5, $3])")
act("FunctionCallNonKeyword", "builtinTrim '(' TrimDirection from Expression ')'", """{
        let space_val = val(Datum::Str(" ".into()), p);
        call(tok_text($lexer, &$1), vec![$5, space_val, Expr::TrimDirection($3)])
    }""")
act("FunctionCallNonKeyword", "builtinTrim '(' TrimDirection Expression from Expression ')'",
    "call(tok_text($lexer, &$1), vec![$6, $4, Expr::TrimDirection($3)])")
act("FunctionCallNonKeyword", "weightString '(' Expression ')'", "call(tok_text($lexer, &$1), vec![$3])")
act("FunctionCallNonKeyword", "weightString '(' Expression as Char FieldLen ')'",
    "call(tok_text($lexer, &$1), vec![$3, val(Datum::Str(\"CHAR\".into()), p), val(Datum::Int($6), p)])")
act("FunctionCallNonKeyword", "weightString '(' Expression as binaryType FieldLen ')'",
    "call(tok_text($lexer, &$1), vec![$3, val(Datum::Str(\"BINARY\".into()), p), val(Datum::Int($6), p)])")
act("FunctionCallNonKeyword", "builtinTranslate '(' Expression ',' Expression ',' Expression ')'",
    "call(tok_text($lexer, &$1), vec![$3, $5, $7])")
act("FunctionCallNonKeyword", "compress '(' ExpressionListOpt ')'", "call(tok_text($lexer, &$1), $3)")
act("FunctionNameSequence", "lastval '(' TableName ')'", "call(LAST_VAL, vec![Expr::TableName($3)])")
act("FunctionNameSequence", "setval '(' TableName ',' SignedNum ')'",
    "call(SET_VAL, vec![Expr::TableName($3), val(Datum::Int($5), p)])")
act("NextValueForSequence", "next value forKwd TableName", "call(NEXT_VAL, vec![Expr::TableName($4)])")
act("NextValueForSequence", "nextval '(' TableName ')'", "call(NEXT_VAL, vec![Expr::TableName($3)])")

# --- aggregates (SumExpr): AggregateFuncExpr, or WindowFuncExpr with an OVER clause --------------
def agg(syms, name, args, distinct="false", win=None):
    """name: Rust expr for F/Name; args: Rust vec expr; win: index of OptWindowingClause."""
    if win is None:
        act("SumExpr", syms, "agg_func(%s, %s, %s, None)" % (name, args, distinct))
    else:
        act("SumExpr", syms, "agg_or_window(%s, %s, %s, $%d)" % (name, args, distinct, win))


T1 = "tok_text($lexer, &$1)"
agg("avg '(' BuggyDefaultFalseDistinctOpt Expression ')' OptWindowingClause", T1, "vec![$4]", "$3", 6)
agg("builtinApproxCountDistinct '(' ExpressionList ')'", T1, "$3")
agg("builtinApproxPercentile '(' ExpressionList ')'", T1, "$3")
for tok in ["builtinBitAnd", "builtinBitOr", "builtinBitXor", "builtinMaxCount", "builtinMinCount", "jsonArrayagg"]:
    agg(f"{tok} '(' Expression ')' OptWindowingClause", T1, "vec![$3]", win=5)
    agg(f"{tok} '(' all Expression ')' OptWindowingClause", T1, "vec![$4]", win=6)
agg("builtinCount '(' DistinctKwd ExpressionList ')'", T1, "$4", "true")
agg("builtinCount '(' all Expression ')' OptWindowingClause", T1, "vec![$4]", win=6)
agg("builtinCount '(' Expression ')' OptWindowingClause", T1, "vec![$3]", win=5)
agg("builtinCount '(' '*' ')' OptWindowingClause", T1, "vec![val(Datum::Int(1), p)]", win=5)
act("SumExpr", "builtinGroupConcat '(' BuggyDefaultFalseDistinctOpt ExpressionList OrderByOptional OptGConcatSeparator ')' OptWindowingClause", """{
        let mut args = $4;
        args.push($6);
        match $8 {
            Some(spec) => Expr::Window(Box::new(WindowFuncExpr { name: tok_text($lexer, &$1), args, distinct: $3, spec: *spec, ..Default::default() })),
            None => Expr::Aggregate(Box::new(AggregateFuncExpr { f: tok_text($lexer, &$1), args, distinct: $3, order: $5 })),
        }
    }""")
for tok in ["builtinMax", "builtinMin", "builtinSum", "builtinSumInt", "builtinStddevSamp", "builtinVarSamp"]:
    agg(f"{tok} '(' BuggyDefaultFalseDistinctOpt Expression ')' OptWindowingClause", T1, "vec![$4]", "$3", 6)
agg("builtinStddevPop '(' BuggyDefaultFalseDistinctOpt Expression ')' OptWindowingClause",
    "AGG_FUNC_STDDEV_POP.to_string()", "vec![$4]", "$3", 6)
agg("builtinVarPop '(' BuggyDefaultFalseDistinctOpt Expression ')' OptWindowingClause",
    "AGG_FUNC_VAR_POP.to_string()", "vec![$4]", "$3", 6)
agg("jsonObjectAgg '(' Expression ',' Expression ')' OptWindowingClause", T1, "vec![$3, $5]", win=7)
agg("jsonObjectAgg '(' all Expression ',' Expression ')' OptWindowingClause", T1, "vec![$4, $6]", win=8)
agg("jsonObjectAgg '(' Expression ',' all Expression ')' OptWindowingClause", T1, "vec![$3, $6]", win=8)
agg("jsonObjectAgg '(' all Expression ',' all Expression ')' OptWindowingClause", T1, "vec![$4, $7]", win=9)
act("OptGConcatSeparator", "", "val(Datum::Str(\",\".into()), p)")
act("OptGConcatSeparator", "separator stringLit", "val(Datum::Str(tok_text($lexer, &$2)), p)")

# --- cast types ----------------------------------------------------------------------------------
act("CastType", "binaryType OptFieldLen", """{
        let mut tp = FieldType::new(mysql_type::VAR_STRING);
        tp.flen = $2; // TODO (Go): Flen should be the flen of expression
        if tp.flen != UNSPECIFIED_LENGTH { tp.tp = mysql_type::STRING; }
        tp.set_bin();
        tp.flag |= mysql_flag::BINARY;
        Box::new(tp)
    }""")
act("CastType", "Char OptFieldLen OptBinary", """{
        let mut tp = FieldType::new(mysql_type::VAR_STRING);
        tp.flen = $2; // TODO (Go): Flen should be the flen of expression
        let ob = $3;
        tp.charset = ob.charset.clone();
        if ob.is_binary {
            tp.flag |= mysql_flag::BINARY;
            tp.set_bin();
        } else if !tp.charset.is_empty() {
            match crate::charset::get_default_collation(&tp.charset) {
                Some(co) => tp.collate = co.to_string(),
                None => p.fail(format!("Get collation error for charset: {}", tp.charset), $span.start()),
            }
            p.explicit_charset.set(true);
        } else {
            tp.charset = p.charset.to_string();
            tp.collate = p.collation.to_string();
        }
        Box::new(tp)
    }""")
for syms, tp in [("dateType", "DATE"), ("yearType", "YEAR")]:
    act("CastType", syms, "{ let mut tp = FieldType::new(mysql_type::%s); tp.set_bin(); tp.flag |= mysql_flag::BINARY; Box::new(tp) }" % tp)
for syms, tp in [("datetimeType OptFieldLen", "DATETIME"), ("timeType OptFieldLen", "DURATION")]:
    act("CastType", syms, """{
        let mut tp = FieldType::new(mysql_type::%s);
        tp.flen = default_length_and_decimal_for_cast(mysql_type::%s).0;
        tp.decimal = $2;
        if tp.decimal > 0 { tp.flen += 1 + tp.decimal; }
        tp.set_bin();
        tp.flag |= mysql_flag::BINARY;
        Box::new(tp)
    }""" % (tp, tp))
act("CastType", "decimalType FloatOpt", """{
        let fopt = $2;
        let mut tp = FieldType::new(mysql_type::NEW_DECIMAL);
        tp.flen = fopt.flen;
        tp.decimal = fopt.decimal;
        tp.set_bin();
        tp.flag |= mysql_flag::BINARY;
        Box::new(tp)
    }""")
act("CastType", "signed OptInteger",
    "{ let mut tp = FieldType::new(mysql_type::LONGLONG); tp.set_bin(); tp.flag |= mysql_flag::BINARY; Box::new(tp) }")
act("CastType", "unsigned OptInteger",
    "{ let mut tp = FieldType::new(mysql_type::LONGLONG); tp.flag |= mysql_flag::UNSIGNED | mysql_flag::BINARY; tp.set_bin(); Box::new(tp) }")
act("CastType", "jsonType", """{
        let mut tp = FieldType::new(mysql_type::JSON);
        tp.flag |= mysql_flag::BINARY | mysql_flag::PARSE_TO_JSON;
        tp.charset = crate::charset::DEFAULT_CHARSET.into();
        tp.collate = crate::charset::COLLATION_UTF8MB4.into();
        Box::new(tp)
    }""")
FLOATISH = """{
        let mut tp = FieldType::new(%s);
        let (flen, decimal) = default_length_and_decimal_for_cast(tp.tp);
        tp.flen = flen;
        tp.decimal = decimal;
        tp.flag |= mysql_flag::BINARY;
        tp.set_bin();
        Box::new(tp)
    }"""
act("CastType", "doubleType", FLOATISH % "mysql_type::DOUBLE")
# REAL_AS_FLOAT sql mode is not supported by ParseOptions yet: REAL is DOUBLE.
act("CastType", "realType", FLOATISH % "mysql_type::DOUBLE")
act("CastType", "floatType FloatOpt", """{
        let fopt = $2;
        let mut t = mysql_type::FLOAT;
        if fopt.flen >= 54 {
            // goyacc: yylex.AppendError(ErrTooBigPrecision...) (no return 1)
            p.error(format!("Too-big precision {} specified for 'CAST'. Maximum is 53.", fopt.flen), $span.start());
        } else if fopt.flen >= 25 {
            t = mysql_type::DOUBLE;
        }
        let mut tp = FieldType::new(t);
        let (flen, decimal) = default_length_and_decimal_for_cast(tp.tp);
        tp.flen = flen;
        tp.decimal = decimal;
        tp.flag |= mysql_flag::BINARY;
        tp.set_bin();
        Box::new(tp)
    }""")
act("CastType", "vectorType OptVectorElementType OptFieldLen", """{
        if $2.tp != mysql_type::FLOAT {
            // goyacc: yylex.AppendError(yylex.Errorf("Only VECTOR is supported for now")) (no return 1)
            p.error("Only VECTOR is supported for now", $span.start());
        }
        let mut tp = FieldType::new(mysql_type::TIDB_VECTOR_FLOAT32);
        tp.flen = $3;
        tp.decimal = 0;
        tp.set_bin();
        Box::new(tp)
    }""")
act("OptVectorElementType", "", "Box::new(VectorElementType { tp: mysql_type::FLOAT })")
act("OptVectorElementType", "'<' floatType '>'", "Box::new(VectorElementType { tp: mysql_type::FLOAT })")
act("OptVectorElementType", "'<' doubleType '>'", "Box::new(VectorElementType { tp: mysql_type::DOUBLE })")
act("FieldLen", "'(' LengthNum ')'", "$2 as i64")
act("OptFieldLen", "", "UNSPECIFIED_LENGTH")
act("FloatOpt", "", "Box::new(FloatOpt { flen: UNSPECIFIED_LENGTH, decimal: UNSPECIFIED_LENGTH })")
act("FloatOpt", "FieldLen", "Box::new(FloatOpt { flen: $1, decimal: UNSPECIFIED_LENGTH })")
act("Precision", "'(' LengthNum ',' LengthNum ')'", "Box::new(FloatOpt { flen: $2 as i64, decimal: $4 as i64 })")
act("OptBinary", "", "Box::new(OptBinary { is_binary: false, charset: String::new() })")
act("OptBinary", "binaryType OptCharset", "Box::new(OptBinary { is_binary: true, charset: $2 })")
act("OptBinary", "CharsetKw CharsetName OptBinMod", "Box::new(OptBinary { is_binary: $3, charset: $2 })")
act("OptCharset", "", "String::new()")
act("OptCharset", "CharsetKw CharsetName", "$2")
act("CharsetName", "StringName", """{
        // Validate the charset name like MySQL; keep the canonical (lower-case) name.
        match crate::charset::get_charset_info(&$1) {
            Ok((name, _)) => name.to_string(),
            Err(_) => {
                p.fail(format!("Unknown character set: '{}'", $1), $span.start());
                String::new()
            }
        }
    }""")
act("CharsetName", "binaryType", "crate::charset::CHARSET_BIN.to_string()")
act("CollationName", "StringName", """{
        match crate::charset::get_collation_by_name(&$1) {
            Some(name) => name.to_string(),
            None => {
                p.fail(format!("Unknown collation: '{}'", $1), $span.start());
                String::new()
            }
        }
    }""")
act("CollationName", "binaryType", "crate::charset::COLLATION_BIN.to_string()")
act("OptCharsetWithOptBinary", "ascii", "Box::new(OptBinary { is_binary: false, charset: crate::charset::CHARSET_LATIN1.into() })")
act("OptCharsetWithOptBinary", "unicodeSym", """{
        // charset.GetCharsetInfo("ucs2") fails (ucs2 is not supported)
        p.fail("Unknown character set: 'ucs2'", $span.start());
        Box::new(OptBinary::default())
    }""")
act("OptCharsetWithOptBinary", "byteType", "Box::new(OptBinary { is_binary: false, charset: crate::charset::CHARSET_BIN.into() })")

# --- numbers, names, LIMIT ... FETCH -------------------------------------------------------------
act("Int64Num", "NUM", """{
        // getInt64FromNUM: NUM holds an uint64 above MaxInt64
        let s = $lexer.src_str($span);
        match s.parse::<i64>() {
            Ok(v) => v,
            Err(_) => {
                p.fail(format!("{s} is out of range [–9223372036854775808,9223372036854775807]"), $span.start());
                0
            }
        }
    }""")
act("SignedNum", "'+' Int64Num", "$2")
act("SignedNum", "'-' NUM", """{
        let s = $lexer.src_str(Span::new(tok_span(&$1).end(), $span.end())).trim();
        let unsigned_num = s.parse::<u64>().unwrap_or(u64::MAX);
        if unsigned_num > 9223372036854775808 {
            p.fail("the Signed Value should be at the range of [-9223372036854775808, 9223372036854775807].", $span.start());
            0
        } else if unsigned_num == 9223372036854775808 {
            i64::MIN
        } else {
            -(unsigned_num as i64)
        }
    }""")
act("ColumnName", "Identifier", "Box::new(ColumnName { name: CIStr::new($1), ..Default::default() })")
act("ColumnName", "Identifier '.' Identifier",
    "Box::new(ColumnName { table: CIStr::new($1), name: CIStr::new($3), ..Default::default() })")
act("ColumnName", "Identifier '.' Identifier '.' Identifier",
    "Box::new(ColumnName { schema: CIStr::new($1), table: CIStr::new($3), name: CIStr::new($5) })")
act("ColumnNameList", "ColumnName", "vec![*$1]")
act("ColumnNameList", "ColumnNameList ',' ColumnName", "{ let mut v = $1; v.push(*$3); v }")
act("SelectStmtLimit", "fetch FirstOrNext FetchFirstOpt RowOrRows only", "Box::new(Limit { count: $3, offset: None })")
act("FetchFirstOpt", "", "val(Datum::Uint(1), p)")

# --- window functions ----------------------------------------------------------------------------
act("NumLiteral", "intLit", "int_lit_datum($lexer.src_str(tok_span(&$1)))")
act("NumLiteral", "floatLit", "Datum::Float($lexer.src_str(tok_span(&$1)).parse().unwrap_or(0.0))")
act("NumLiteral", "decLit", "Datum::Decimal(normalize_decimal($lexer.src_str(tok_span(&$1))))")
act("WindowClauseOptional", "window WindowDefinitionList", "$2")
act("WindowDefinitionList", "WindowDefinition", "vec![$1]")
act("WindowDefinitionList", "WindowDefinitionList ',' WindowDefinition", "{ let mut v = $1; v.push($3); v }")
act("WindowDefinition", "WindowName as WindowSpec", "{ let mut spec = $3; spec.name = $1; spec }")
act("WindowName", "Identifier", "CIStr::new($1)")
act("WindowSpec", "'(' WindowSpecDetails ')'", "$2")
act("WindowSpecDetails", "OptExistingWindowName OptPartitionClause OptWindowOrderByClause OptWindowFrameClause",
    "WindowSpec { ref_: $1, partition_by: $2, order_by: $3, frame: $4, ..Default::default() }")
act("OptExistingWindowName", "", "CIStr::default()")
act("OptPartitionClause", "partition by ByList", "Some(Box::new(PartitionByClause { items: $3 }))")
act("OptWindowOrderByClause", "order by ByList", "Some(Box::new(OrderByClause { items: $3 }))")
act("OptWindowFrameClause", "WindowFrameUnits WindowFrameExtent", "Some(Box::new(FrameClause { tp: $1, extent: $2 }))")
act("WindowFrameUnits", "rows", "FrameType::Rows")
act("WindowFrameUnits", "rangeKwd", "FrameType::Ranges")
act("WindowFrameUnits", "groups", "FrameType::Groups")
act("WindowFrameExtent", "WindowFrameStart",
    "FrameExtent { start: $1, end: FrameBound { tp: BoundType::CurrentRow, ..Default::default() } }")
act("WindowFrameBetween", "between WindowFrameBound and WindowFrameBound", "FrameExtent { start: $2, end: $4 }")
for bound, kw in [("Preceding", "preceding"), ("Following", "following")]:
    rule = "WindowFrameStart" if bound == "Preceding" else "WindowFrameBound"
    act(rule, f"unbounded {kw}", "FrameBound { tp: BoundType::%s, unbounded: true, ..Default::default() }" % bound)
    act(rule, f"NumLiteral {kw}", "FrameBound { tp: BoundType::%s, expr: Some(val($1, p)), ..Default::default() }" % bound)
    act(rule, f"paramMarker {kw}",
        "FrameBound { tp: BoundType::%s, expr: Some(Expr::ParamMarker(Box::new(ParamMarkerExpr { offset: tok_span(&$1).start(), order: 0 }))), ..Default::default() }" % bound)
    act(rule, f"interval Expression TimeUnit {kw}",
        "FrameBound { tp: BoundType::%s, expr: Some($2), unit: $3, ..Default::default() }" % bound)
act("WindowFrameStart", "current row", "FrameBound { tp: BoundType::CurrentRow, ..Default::default() }")
act("OptWindowingClause", "WindowingClause", "Some(Box::new($1))")
act("WindowingClause", "over WindowNameOrSpec", "$2")
act("WindowNameOrSpec", "WindowName", "WindowSpec { name: $1, only_alias: true, ..Default::default() }")
for tok in ["rowNumber", "rank", "denseRank", "cumeDist", "percentRank"]:
    act("WindowFuncCall", f"{tok} '(' ')' WindowingClause",
        "Expr::Window(Box::new(WindowFuncExpr { name: tok_text($lexer, &$1), spec: $4, ..Default::default() }))")
act("WindowFuncCall", "ntile '(' SimpleExpr ')' WindowingClause",
    "Expr::Window(Box::new(WindowFuncExpr { name: tok_text($lexer, &$1), args: vec![$3], spec: $5, ..Default::default() }))")
for tok in ["lead", "lag"]:
    act("WindowFuncCall", f"{tok} '(' Expression OptLeadLagInfo ')' OptNullTreatment WindowingClause", """{
        let mut args = vec![$3];
        args.extend($4);
        Expr::Window(Box::new(WindowFuncExpr { name: tok_text($lexer, &$1), args, ignore_null: $6, spec: $7, ..Default::default() }))
    }""")
for tok in ["firstValue", "lastValue"]:
    act("WindowFuncCall", f"{tok} '(' Expression ')' OptNullTreatment WindowingClause",
        "Expr::Window(Box::new(WindowFuncExpr { name: tok_text($lexer, &$1), args: vec![$3], ignore_null: $5, spec: $6, ..Default::default() }))")
act("WindowFuncCall", "nthValue '(' Expression ',' SimpleExpr ')' OptFromFirstLast OptNullTreatment WindowingClause",
    "Expr::Window(Box::new(WindowFuncExpr { name: tok_text($lexer, &$1), args: vec![$3, $5], from_last: $7, ignore_null: $8, spec: $9, ..Default::default() }))")
act("OptLeadLagInfo", "',' NumLiteral OptLLDefault",
    "{ let mut args = vec![val($2, p)]; if let Some(d) = $3 { args.push(d); } args }")
act("OptLeadLagInfo", "',' paramMarker OptLLDefault", """{
        let mut args = vec![Expr::ParamMarker(Box::new(ParamMarkerExpr { offset: tok_span(&$2).start(), order: 0 }))];
        if let Some(d) = $3 { args.push(d); }
        args
    }""")
act("OptLLDefault", "',' Expression", "Some($2)")

# --- transactions ----------------------------------------------------------------------------------
def begin(syms, fields=""):
    act("BeginTransactionStmt", syms, "Stmt::Begin(Box::new(BeginStmt { %s..Default::default() }))" % fields)


begin("begin")
begin("begin pessimistic", 'mode: "PESSIMISTIC", ')
begin("begin optimistic", 'mode: "OPTIMISTIC", ')
begin("start transaction")
begin("start transaction read write")
begin("start transaction with consistent snapshot")
begin("start transaction with causal consistency only", "causal_consistency_only: true, ")
begin("start transaction read only", "read_only: true, ")
begin("start transaction read only AsOfClause", "read_only: true, as_of: Some($5), ")
act("CommitStmt", "commit", "Stmt::Commit(Box::default())")
act("CommitStmt", "commit CompletionTypeWithinTransaction",
    "Stmt::Commit(Box::new(CommitStmt { completion_type: $2 }))")
act("RollbackStmt", "rollback", "Stmt::Rollback(Box::default())")
act("RollbackStmt", "rollback CompletionTypeWithinTransaction",
    "Stmt::Rollback(Box::new(RollbackStmt { completion_type: $2, ..Default::default() }))")
act("RollbackStmt", "rollback to Identifier",
    "Stmt::Rollback(Box::new(RollbackStmt { savepoint_name: $3, ..Default::default() }))")
act("RollbackStmt", "rollback to savepoint Identifier",
    "Stmt::Rollback(Box::new(RollbackStmt { savepoint_name: $4, ..Default::default() }))")
for syms, ct in [("and chain no release", "Chain"), ("and no chain release", "Release"),
                 ("and no chain no release", "Default"), ("and chain", "Chain"),
                 ("and no chain", "Default"), ("release", "Release"), ("no release", "Default")]:
    act("CompletionTypeWithinTransaction", syms, "CompletionType::" + ct)
act("AsOfClause", "asof timestampType Expression", "Box::new(AsOfClause { ts_expr: $3 })")

# --- USE / SET -----------------------------------------------------------------------------------
act("UseStmt", "use DBName", "Stmt::Use(Box::new(UseStmt { db_name: $2 }))")
act("SetStmt", "set VariableAssignmentList", "Stmt::Set(Box::new(SetStmt { variables: $2 }))")
act("SetStmt", "set global transaction TransactionChars", """{
        let mut vars = $4;
        for v in &mut vars { v.is_global = true; }
        Stmt::Set(Box::new(SetStmt { variables: vars }))
    }""")
act("SetStmt", "set session transaction TransactionChars", "Stmt::Set(Box::new(SetStmt { variables: $4 }))")
act("SetStmt", "set transaction TransactionChars", """{
        let mut assigns = $3;
        for a in &mut assigns {
            if a.name == "tx_isolation" {
                // A special session variable that make setting tx_isolation take effect one time.
                a.name = "tx_isolation_one_shot".into();
            }
        }
        Stmt::Set(Box::new(SetStmt { variables: assigns }))
    }""")
act("TransactionChars", "TransactionChar", "$1")
act("TransactionChars", "TransactionChars ',' TransactionChar", "{ let mut v = $1; v.extend($3); v }")
TX_VAR = "vec![VariableAssignment { name: %s.into(), value: %s, is_system: true, ..Default::default() }]"
act("TransactionChar", "isolation level IsolationLevel", TX_VAR % ('"tx_isolation"', "val(Datum::Str($3), p)"))
act("TransactionChar", "read write", TX_VAR % ('"tx_read_only"', 'val(Datum::Str("0".into()), p)'))
act("TransactionChar", "read only", TX_VAR % ('"tx_read_only"', 'val(Datum::Str("1".into()), p)'))
act("TransactionChar", "read only AsOfClause", TX_VAR % ('"tx_read_ts"', "$3.ts_expr"))
for syms, lvl in [("repeatable read", "REPEATABLE-READ"), ("read committed", "READ-COMMITTED"),
                  ("read uncommitted", "READ-UNCOMMITTED"), ("serializable", "SERIALIZABLE")]:
    act("IsolationLevel", syms, '"%s".to_string()' % lvl)
act("SetExpr", "on", 'val(Datum::Str("ON".into()), p)')
act("SetExpr", "binaryType", 'val(Datum::Str("BINARY".into()), p)')
act("ExprOrDefault", "defaultKwd", "Expr::Default(Box::default())")
act("VariableName", "Identifier '.' Identifier", 'format!("{}.{}", $1, $3)')
for syms, fields in [("VariableName EqOrAssignmentEq SetExpr", "name: $1, value: $3"),
                     ("global VariableName EqOrAssignmentEq SetExpr", "name: $2, value: $4, is_global: true"),
                     ("instance VariableName EqOrAssignmentEq SetExpr", "name: $2, value: $4, is_instance: true"),
                     ("session VariableName EqOrAssignmentEq SetExpr", "name: $2, value: $4"),
                     ("local VariableName EqOrAssignmentEq SetExpr", "name: $2, value: $4")]:
    act("VariableAssignment", syms,
        "Box::new(VariableAssignment { %s, is_system: true, ..Default::default() })" % fields)
act("VariableAssignment", "doubleAtIdentifier EqOrAssignmentEq SetExpr", """{
        let mut v = at_ident_text($lexer.src_str(tok_span(&$1))).to_lowercase();
        let (mut is_global, mut is_instance) = (false, false);
        if let Some(r) = v.strip_prefix("@@global.") {
            is_global = true;
            v = r.to_string();
        } else if let Some(r) = v.strip_prefix("@@instance.") {
            is_instance = true;
            v = r.to_string();
        } else if let Some(r) = v.strip_prefix("@@session.") {
            v = r.to_string();
        } else if let Some(r) = v.strip_prefix("@@local.") {
            v = r.to_string();
        } else if let Some(r) = v.strip_prefix("@@") {
            v = r.to_string();
        }
        Box::new(VariableAssignment { name: v, value: $3, is_global, is_instance, is_system: true, ..Default::default() })
    }""")
act("VariableAssignment", "singleAtIdentifier EqOrAssignmentEq Expression",
    "Box::new(VariableAssignment { name: at_ident_text($lexer.src_str(tok_span(&$1))), value: $3, ..Default::default() })")
SET_NAMES = "Box::new(VariableAssignment { name: SET_NAMES.into(), value: val_nocs(Datum::Str($2)), %s..Default::default() })"
act("VariableAssignment", "names CharsetName", SET_NAMES % "")
act("VariableAssignment", "names CharsetName collate defaultKwd", SET_NAMES % "")
act("VariableAssignment", "names CharsetName collate StringName",
    SET_NAMES % "extend_value: Some(Box::new(ValueExpr::new(Datum::Str($4), \"\", \"\"))), ")
act("VariableAssignment", "names defaultKwd",
    "Box::new(VariableAssignment { name: SET_NAMES.into(), value: Expr::Default(Box::default()), ..Default::default() })")
act("VariableAssignment", "CharsetKw CharsetNameOrDefault",
    "Box::new(VariableAssignment { name: SET_CHARSET.into(), value: $2, ..Default::default() })")
act("CharsetNameOrDefault", "CharsetName", "val_nocs(Datum::Str($1))")
act("CharsetNameOrDefault", "defaultKwd", "Expr::Default(Box::default())")
act("VariableAssignmentList", "VariableAssignment", "vec![*$1]")
act("VariableAssignmentList", "VariableAssignmentList ',' VariableAssignment", "{ let mut v = $1; v.push(*$3); v }")

# --- SHOW ----------------------------------------------------------------------------------------
def show(tp, fields=""):
    """&ast.ShowStmt{Tp: ast.Show<tp>, <fields>}"""
    return "Box::new(ShowStmt { tp: ShowStmtType::%s, %s..Default::default() })" % (tp, fields)


def show_stmt(syms, code):
    act("ShowStmt", syms, "Stmt::Show(%s)" % code)


act("ShowStmt", "show ShowTargetFilterable ShowLikeOrWhereOpt", """{
        let mut stmt = $2;
        match $3 {
            Some(ShowLikeOrWhere::Like(pattern)) => stmt.pattern = Some(pattern),
            Some(ShowLikeOrWhere::Where(expr)) => stmt.where_ = Some(expr),
            None => {}
        }
        Stmt::Show(stmt)
    }""")
show_stmt("show create tableKwd TableName", show("CreateTable", "table: Some($4), "))
show_stmt("show create view TableName", show("CreateView", "table: Some($4), "))
show_stmt("show create database IfNotExists DBName", show("CreateDatabase", "if_not_exists: $4, db_name: $5, "))
show_stmt("show create sequence TableName", show("CreateSequence", "table: Some($4), "))
show_stmt("show create placement policy PolicyName", show("CreatePlacementPolicy", "db_name: $5, "))
show_stmt("show create resource group ResourceGroupName", show("CreateResourceGroup", "resource_group_name: $5, "))
show_stmt("show masking policies forKwd TableName WhereClauseOptional",
          show("MaskingPolicies", "table: Some($5), where_: $6, "))
act("ShowStmt", "show tableKwd TableName PartitionNameListOpt regions WhereClauseOptional", """{
        let mut table = $3;
        table.partition_names = $4;
        Stmt::Show(Box::new(ShowStmt { tp: ShowStmtType::Regions, table: Some(table), where_: $6, ..Default::default() }))
    }""")
show_stmt("show tableKwd TableName next_row_id", show("TableNextRowId", "table: Some($3), "))
act("ShowStmt", "show tableKwd TableName PartitionNameListOpt index Identifier regions WhereClauseOptional", """{
        let mut table = $3;
        table.partition_names = $4;
        Stmt::Show(Box::new(ShowStmt { tp: ShowStmtType::Regions, table: Some(table), index_name: CIStr::new($6), where_: $8, ..Default::default() }))
    }""")
show_stmt("show grants", show("Grants"))
show_stmt("show master status", show("MasterStatus"))
show_stmt("show binaryType log status", show("BinlogStatus"))
show_stmt("show Replica status", show("ReplicaStatus"))
show_stmt("show OptFull processlist", show("ProcessList", "full: $2, "))
show_stmt("show profiles", show("Profiles"))
show_stmt("show privileges", show("Privileges"))
show_stmt("show builtins", show("Builtins"))
show_stmt("show ShowImportJobTarget Int64Num", show("ImportJobs", "import_job_id: Some($3), import_job_raw: $2, "))
show_stmt("show distribution job Int64Num", show("DistributionJobs", "distribution_job_id: Some($4), "))
show_stmt("show create procedure TableName", show("CreateProcedure", "procedure: Some($4), "))
act("ShowStmt", "show tableKwd TableName PartitionNameListOpt distributions WhereClauseOptional", """{
        let mut table = $3;
        table.partition_names = $4;
        Stmt::Show(Box::new(ShowStmt { tp: ShowStmtType::Distributions, table: Some(table), where_: $6, ..Default::default() }))
    }""")


def stats_table(name):
    """&ast.TableName{Name: ast.NewCIStr(name), Schema: ast.NewCIStr(mysql.SystemDB)}"""
    return 'table: Some(Box::new(TableName { name: CIStr::new("%s"), schema: CIStr::new("mysql"), ..Default::default() })), ' % name


for syms, code in [
        ("engines", show("Engines")),
        ("storageClass transitions", show("StorageClassTransitions")),
        ("databases", show("Databases")),
        ("config", show("Config")),
        ("CharsetKw", show("Charset")),
        ("OptFull tables ShowDatabaseNameOpt", show("Tables", "db_name: $3, full: $1, ")),
        ("open tables ShowDatabaseNameOpt", show("OpenTables", "db_name: $3, ")),
        ("tableKwd status ShowDatabaseNameOpt", show("TableStatus", "db_name: $3, ")),
        ("ShowIndexKwd FromOrIn TableName", show("Index", "table: Some($3), ")),
        ("ShowIndexKwd FromOrIn Identifier FromOrIn Identifier",
         show("Index", "table: Some(Box::new(TableName { name: CIStr::new($3), schema: CIStr::new($5), ..Default::default() })), ")),
        ("OptFull FieldsOrColumns ShowTableAliasOpt ShowDatabaseNameOpt",
         show("Columns", "table: Some($3), db_name: $4, full: $1, ")),
        ("extended OptFull FieldsOrColumns ShowTableAliasOpt ShowDatabaseNameOpt",
         show("Columns", "table: Some($4), db_name: $5, full: $2, extended: true, ")),
        ("builtinCount '(' '*' ')' warnings", show("Warnings", "count_warnings_or_errors: true, ")),
        ("warnings", show("Warnings")),
        ("builtinCount '(' '*' ')' identSQLErrors", show("Errors", "count_warnings_or_errors: true, ")),
        ("identSQLErrors", show("Errors")),
        ("GlobalScope variables", show("Variables", "global_scope: $1, ")),
        ("GlobalScope status", show("Status", "global_scope: $1, ")),
        ("GlobalScope bindings", show("Bindings", "global_scope: $1, ")),
        ("collation", show("Collation")),
        ("triggers ShowDatabaseNameOpt", show("Triggers", "db_name: $2, ")),
        ("bindingCache status", show("BindingCacheStatus")),
        ("procedure status", show("ProcedureStatus")),
        ("function status", show("FunctionStatus")),
        ("events ShowDatabaseNameOpt", show("Events", "db_name: $2, ")),
        ("plugins", show("Plugins")),
        ("sessionStates", show("SessionStates")),
        ("statsExtended", show("StatsExtended")),
        ("statsMeta", show("StatsMeta", stats_table("STATS_META"))),
        ("statsHistograms", show("StatsHistograms", stats_table("STATS_HISTOGRAMS"))),
        ("statsTopN", show("StatsTopN")),
        ("statsBuckets", show("StatsBuckets", stats_table("STATS_BUCKETS"))),
        ("statsHealthy", show("StatsHealthy")),
        ("statsLocked", show("StatsLocked", stats_table("STATS_TABLE_LOCKED"))),
        ("histogramsInFlight", show("HistogramsInFlight")),
        ("columnStatsUsage", show("ColumnStatsUsage")),
        ("affinity", show("Affinity")),
        ("analyze status", show("AnalyzeStatus")),
        ("backups", show("Backups")),
        ("restores", show("Restores")),
        ("placement", show("Placement")),
        ("placement labels", show("PlacementLabels")),
        ("importKwd groups", show("ImportGroups")),
        ("importKwd group stringLit", show("ImportGroups", "show_group_key: tok_text($lexer, &$3), ")),
        ("ShowImportJobsTarget", show("ImportJobs", "import_job_raw: $1, ")),
        ("distribution jobs", show("DistributionJobs"))]:
    act("ShowTargetFilterable", syms, code)
act("ShowLikeOrWhereOpt", "", "None")
act("ShowLikeOrWhereOpt", "like SimpleExpr", "Some(ShowLikeOrWhere::Like($2))")
act("ShowLikeOrWhereOpt", "where Expression", "Some(ShowLikeOrWhere::Where($2))")
act("ShowDatabaseNameOpt", "", "String::new()")
act("ShowTableAliasOpt", "FromOrIn TableName", "$2")

# --- statement list entries ------------------------------------------------------------------------
act("Statement", "SubSelect", """{
        match $1 {
            Expr::Subquery(sq) => match sq.query {
                ResultSetNode::Select(mut s) => {
                    s.is_in_braces = true;
                    Some(Stmt::Select(s))
                }
                ResultSetNode::SetOpr(mut s) => {
                    s.is_in_braces = true;
                    Some(Stmt::SetOpr(s))
                }
                ResultSetNode::Unported(n) => Some(Stmt::Unported(n)),
                other => unreachable!("SubSelect yields a SELECT or a set operation, got {other:?}"),
            },
            other => unreachable!("SubSelect yields a SubqueryExpr, got {other:?}"),
        }
    }""")

# --- index hints, locking reads --------------------------------------------------------------------
act("IndexHintList", "IndexHint", "vec![*$1]")
act("IndexHintList", "IndexHintList IndexHint", "{ let mut v = $1; v.push(*$2); v }")
act("IndexHint", "IndexHintType IndexHintScope '(' IndexNameList ')'",
    "Box::new(IndexHint { index_names: $4, hint_type: $1, hint_scope: $2, unported: None })")
for tok, tp in [("use", "Use"), ("ignore", "Ignore"), ("force", "Force")]:
    act("IndexHintType", f"{tok} KeyOrIndex", "IndexHintType::" + tp)
act("IndexHintScope", "", "IndexHintScope::ForScan")
act("IndexHintScope", "forKwd join", "IndexHintScope::ForJoin")
act("IndexHintScope", "forKwd order by", "IndexHintScope::ForOrderBy")
act("IndexHintScope", "forKwd group by", "IndexHintScope::ForGroupBy")
act("IndexNameList", "Identifier", "vec![CIStr::new($1)]")
act("IndexNameList", "IndexNameList ',' Identifier", "{ let mut v = $1; v.push(CIStr::new($3)); v }")
act("IndexNameList", "primary", "vec![CIStr::new(tok_text($lexer, &$1))]")
act("IndexNameList", "IndexNameList ',' primary", "{ let mut v = $1; v.push(CIStr::new(tok_text($lexer, &$3))); v }")
LOCK = "Some(Box::new(SelectLockInfo { lock_type: SelectLockType::%s, tables: $3, ..Default::default() }))"
for syms, tp in [("forKwd update OfTablesOpt", "ForUpdate"), ("forKwd share OfTablesOpt", "ForShare"),
                 ("forKwd update OfTablesOpt nowait", "ForUpdateNoWait"),
                 ("forKwd share OfTablesOpt nowait", "ForShareNoWait"),
                 ("forKwd update OfTablesOpt skip locked", "ForUpdateSkipLocked"),
                 ("forKwd share OfTablesOpt skip locked", "ForShareSkipLocked")]:
    act("SelectLockOpt", syms, LOCK % tp)
act("SelectLockOpt", "forKwd update OfTablesOpt wait NUM",
    "Some(Box::new(SelectLockInfo { lock_type: SelectLockType::ForUpdateWaitN, wait_sec: $5 as u64, tables: $3, unported: None }))")
act("SelectLockOpt", "lock in share mode",
    "Some(Box::new(SelectLockInfo { lock_type: SelectLockType::ForShare, ..Default::default() }))")
act("OfTablesOpt", "of TableNameList", "$2")
act("TableNameList", "TableName", "vec![*$1]")
act("TableNameList", "TableNameList ',' TableName", "{ let mut v = $1; v.push(*$3); v }")
act("TableAsNameOpt", "", "CIStr::default()")
act("EscapedTableRef", "'{' Identifier TableRef '}'", """{
        // ODBC escape syntax for outer join is { OJ join_table }; use an Identifier for OJ.
        $3
    }""")
act("OuterOpt", "", "String::new()")

# --- set operations (UNION / EXCEPT / INTERSECT) and WITH ------------------------------------------
# goyacc also calls parser.setLastSelectFieldText on the last SELECT of the left list; node texts
# are not ported yet.
def setopr_with_subselect(syms, order=None, limit=None):
    act("SetOprStmtWithLimitOrderBy" if (order or limit) else "SetOprStmtWoutLimitOrderBy", syms, """{
        let mut list = $1;
        let (selects, with, limit2, order_by2) = subselect_operand($3);
        list.push(SetOprNode::List(Box::new(SetOprSelectList {
            selects, with, limit: limit2, order_by: order_by2, after_set_operator: Some(*$2),
        })));
        let mut set_opr = Box::new(SetOprStmt { select_list: Box::new(SetOprSelectList { selects: list, ..Default::default() }), ..Default::default() });
        %s%s
        Stmt::SetOpr(set_opr)
    }""" % (f"set_opr.order_by = Some(${order});" if order else "",
            f"set_opr.limit = Some(${limit});" if limit else ""))


act("SetOprStmtWoutLimitOrderBy", "SetOprClauseList SetOpr SelectStmt", """{
        let mut list = $1;
        let mut set_opr = Box::new(SetOprStmt::default());
        match $3 {
            Stmt::Select(mut st) => {
                set_opr.limit = st.limit.take();
                set_opr.order_by = st.order_by.take();
                st.after_set_operator = Some(*$2);
                list.push(SetOprNode::Select(st));
            }
            // A SELECT variant that is still a placeholder.
            Stmt::Unported(n) => list.push(SetOprNode::Unported(n)),
            other => unreachable!("SelectStmt yields a SELECT, got {other:?}"),
        }
        set_opr.select_list = Box::new(SetOprSelectList { selects: list, ..Default::default() });
        Stmt::SetOpr(set_opr)
    }""")
setopr_with_subselect("SetOprClauseList SetOpr SubSelect")
setopr_with_subselect("SetOprClauseList SetOpr SubSelect OrderBy", order=4)
setopr_with_subselect("SetOprClauseList SetOpr SubSelect SelectStmtLimit", limit=4)
setopr_with_subselect("SetOprClauseList SetOpr SubSelect OrderBy SelectStmtLimit", order=4, limit=5)
for syms, order, limit in [("SubSelect OrderBy", 2, None), ("SubSelect SelectStmtLimit", None, 2),
                           ("SubSelect OrderBy SelectStmtLimit", 2, 3)]:
    act("SetOprStmtWithLimitOrderBy", syms, """{
        let (selects, with, limit, order_by) = subselect_operand($1);
        let list = vec![SetOprNode::List(Box::new(SetOprSelectList { selects, with, limit, order_by, after_set_operator: None }))];
        let mut set_opr = Box::new(SetOprStmt { select_list: Box::new(SetOprSelectList { selects: list, ..Default::default() }), ..Default::default() });
        %s%s
        Stmt::SetOpr(set_opr)
    }""" % (f"set_opr.order_by = Some(${order});" if order else "",
            f"set_opr.limit = Some(${limit});" if limit else ""))
act("SetOprClauseList", "SetOprClauseList SetOpr SetOprClause", """{
        let mut list1 = $1;
        let mut list2 = $3;
        match list2.first_mut() {
            Some(SetOprNode::Select(x)) => x.after_set_operator = Some(*$2),
            Some(SetOprNode::List(x)) => x.after_set_operator = Some(*$2),
            _ => {}
        }
        list1.extend(list2);
        list1
    }""")
act("SetOprClause", "SelectStmt", """{
        match $1 {
            Stmt::Select(st) => vec![SetOprNode::Select(st)],
            Stmt::Unported(n) => vec![SetOprNode::Unported(n)],
            other => unreachable!("SelectStmt yields a SELECT, got {other:?}"),
        }
    }""")
act("SetOprClause", "SubSelect", """{
        let is_select = matches!(&$1, Expr::Subquery(sq) if matches!(sq.query, ResultSetNode::Select(_)));
        let (selects, with, limit, order_by) = subselect_operand($1);
        // Go: a plain SELECT operand becomes &ast.SetOprSelectList{Selects: []ast.Node{x}} (no With).
        let with = if is_select { None } else { with };
        vec![SetOprNode::List(Box::new(SetOprSelectList { selects, with, limit, order_by, after_set_operator: None }))]
    }""")
for tok, (distinct, all_) in [("union", ("Union", "UnionAll")), ("except", ("Except", "ExceptAll")),
                              ("intersect", ("Intersect", "IntersectAll"))]:
    act("SetOpr", f"{tok} SetOprOpt",
        "Box::new(if $2 { SetOprType::%s } else { SetOprType::%s })" % (distinct, all_))
for syms in ["WithClause SetOprStmtWithLimitOrderBy", "WithClause SetOprStmtWoutLimitOrderBy"]:
    act("SetOprStmt", syms, """{
        match $2 {
            Stmt::SetOpr(mut set_opr) => {
                set_opr.with = Some(std::rc::Rc::from($1));
                Stmt::SetOpr(set_opr)
            }
            other => other,
        }
    }""")
act("SelectStmtWithClause", "WithClause SelectStmt", """{
        match $2 {
            Stmt::Select(mut sel) => {
                sel.with = Some(std::rc::Rc::from($1));
                Stmt::Select(sel)
            }
            other => other,
        }
    }""")
act("SelectStmtWithClause", "WithClause SubSelect", """{
        let with = std::rc::Rc::from($1);
        match $2 {
            Expr::Subquery(sq) => match sq.query {
                ResultSetNode::Select(mut x) => {
                    x.is_in_braces = true;
                    x.with_before_braces = true;
                    x.with = Some(with);
                    Stmt::Select(x)
                }
                ResultSetNode::SetOpr(mut x) => {
                    x.is_in_braces = true;
                    x.with = Some(with);
                    Stmt::SetOpr(x)
                }
                ResultSetNode::Unported(n) => Stmt::Unported(n),
                other => unreachable!("SubSelect yields a SELECT or a set operation, got {other:?}"),
            },
            other => unreachable!("SubSelect yields a SubqueryExpr, got {other:?}"),
        }
    }""")
act("WithClause", "with recursive WithList", """{
        let mut ws = $3;
        ws.is_recursive = true;
        for cte in &mut ws.ctes { cte.is_recursive = true; }
        ws
    }""")
act("WithList", "WithList ',' CommonTableExpr", "{ let mut ws = $1; ws.ctes.push(*$3); ws }")
act("WithList", "CommonTableExpr",
    "{ let mut ctes = Vec::with_capacity(4); ctes.push(*$1); Box::new(WithClause { is_recursive: false, ctes, unported: None }) }")
act("CommonTableExpr", "Identifier IdentListWithParenOpt as SubSelect",
    "Box::new(CommonTableExpression { name: CIStr::new($1), col_name_list: $2, query: $4, is_recursive: false })")
act("IdentList", "Identifier", "vec![CIStr::new($1)]")
act("IdentList", "IdentList ',' Identifier", "{ let mut v = $1; v.push(CIStr::new($3)); v }")

# --- silent-fallback fixes ---------------------------------------------------------------------------
act("SelectStmtOpt", "TableOptimizerHints",
    "{ let mut opt = Box::new(SelectStmtOpts { sql_cache: true, ..Default::default() }); opt.table_hints = $1; opt }")
act("TableName", "'*' '.' Identifier",
    "Box::new(TableName { schema: CIStr::new(\"*\"), name: CIStr::new($3), ..Default::default() })")

# --- TABLE t / VALUES ROW(...) ---------------------------------------------------------------------
def wildcard_fields():
    return "Box::new(FieldList { fields: vec![SelectField { wild_card: Some(Box::default()), ..Default::default() }] })"


act("SelectStmt", "tableKwd TableName OrderByOptional SelectStmtLimitOpt SelectLockOpt SelectStmtIntoOption", """{
        let ts = ResultSetNode::TableSource(Box::new(TableSource { source: ResultSetNode::TableName($2), as_name: CIStr::default() }));
        let from = Box::new(TableRefsClause { table_refs: Box::new(Join { left: Some(ts), ..Default::default() }) });
        Stmt::Select(Box::new(SelectStmt {
            kind: SelectStmtKind::Table, fields: %s, from: Some(from),
            order_by: $3, limit: $4, lock_info: $5, select_into_opt: $6, ..Default::default()
        }))
    }""" % wildcard_fields())
act("SelectStmt", "values ValuesStmtList OrderByOptional SelectStmtLimitOpt SelectLockOpt SelectStmtIntoOption", """{
        Stmt::Select(Box::new(SelectStmt {
            kind: SelectStmtKind::Values, fields: %s, lists: $2,
            order_by: $3, limit: $4, lock_info: $5, select_into_opt: $6, ..Default::default()
        }))
    }""" % wildcard_fields())
act("ValuesStmtList", "RowStmt", "vec![*$1]")
act("ValuesStmtList", "ValuesStmtList ',' RowStmt", "{ let mut v = $1; v.push(*$3); v }")
act("RowStmt", "row RowValue", "Box::new(RowExpr { values: $2 })")
act("Values", "Values ',' ExprOrDefault", "{ let mut v = $1; v.push($3); v }")
act("Values", "ExprOrDefault", "vec![$1]")

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
        Expr::Value(v) => match v.datum {
            Datum::Int(i) => Expr::Position(i),
            Datum::Bool(b) => Expr::Position(b as i64),
            _ => Expr::Value(v),
        },
        e => e,
    }
}

/// ast.NewValueExpr(x, parser.charset, parser.collation)
fn val(d: Datum, p: &Ctx) -> Expr {
    Expr::Value(Box::new(ValueExpr::new(d, p.charset, p.collation)))
}

/// ast.NewValueExpr(x, "", "")
fn val_nocs(d: Datum) -> Expr {
    Expr::Value(Box::new(ValueExpr::new(d, "", "")))
}

/// &ast.FuncCallExpr{FnName: ast.NewCIStr(name), Args: args}
fn call(name: impl Into<String>, args: Vec<Expr>) -> Expr {
    Expr::FuncCall(Box::new(FuncCallExpr { fn_name: CIStr::new(name), args, ..Default::default() }))
}

/// $n.(*ast.ColumnNameExpr).Name -- SimpleIdent always builds a ColumnNameExpr.
fn column_of(e: Expr) -> Box<ColumnName> {
    match e {
        Expr::Column(c) => c,
        other => unreachable!("SimpleIdent yields a ColumnNameExpr, got {other:?}"),
    }
}

/// &ast.SubqueryExpr{Query: rs}
fn subquery(query: ResultSetNode) -> Expr {
    Expr::Subquery(Box::new(SubqueryExpr { query, multi_rows: false, exists: false }))
}

/// $n.(*ast.SelectStmt) / (*ast.SetOprStmt) as an ast.ResultSetNode.
fn stmt_to_result_set(s: Stmt) -> ResultSetNode {
    match s {
        Stmt::Select(s) => ResultSetNode::Select(s),
        Stmt::SetOpr(s) => ResultSetNode::SetOpr(s),
        Stmt::Unported(n) => ResultSetNode::Unported(n),
        other => unreachable!("a SELECT or set operation, got {other:?}"),
    }
}

/// The `switch x := $n.(*ast.SubqueryExpr).Query.(type)` of the set-operation actions: the
/// operand's selects, With, Limit and OrderBy (a SELECT keeps its own With / Limit / OrderBy;
/// Go shares the With pointer, hence the `Rc`).
#[allow(clippy::type_complexity)]
fn subselect_operand(e: Expr) -> (Vec<SetOprNode>, Option<std::rc::Rc<WithClause>>, Option<Box<Limit>>, Option<Box<OrderByClause>>) {
    match e {
        Expr::Subquery(sq) => match sq.query {
            ResultSetNode::Select(x) => {
                let with = x.with.clone();
                (vec![SetOprNode::Select(x)], with, None, None)
            }
            ResultSetNode::SetOpr(x) => {
                let x = *x;
                (x.select_list.selects, x.with, x.limit, x.order_by)
            }
            ResultSetNode::Unported(n) => (vec![SetOprNode::Unported(n)], None, None, None),
            other => unreachable!("SubSelect yields a SELECT or a set operation, got {other:?}"),
        },
        other => unreachable!("SubSelect yields a SubqueryExpr, got {other:?}"),
    }
}

/// &ast.AggregateFuncExpr{F: f, Args: args, Distinct: distinct}
fn agg_func(f: String, args: Vec<Expr>, distinct: bool, order: Option<Box<OrderByClause>>) -> Expr {
    Expr::Aggregate(Box::new(AggregateFuncExpr { f, args, distinct, order }))
}

/// SumExpr: a WindowFuncExpr when an OVER clause follows, else an AggregateFuncExpr.
fn agg_or_window(f: String, args: Vec<Expr>, distinct: bool, spec: Option<Box<WindowSpec>>) -> Expr {
    match spec {
        Some(spec) => Expr::Window(Box::new(WindowFuncExpr { name: f, args, distinct, spec: *spec, ..Default::default() })),
        None => agg_func(f, args, distinct, None),
    }
}

/// goyacc isInTokenMap: exact (case-sensitive) lookup in tokenMap.
fn is_in_token_map(s: &str) -> bool {
    crate::keywords_gen::KEYWORDS.iter().any(|(k, _)| *k == s)
}

/// Lexer value of an underscoreCS token: the canonical charset name (`N'..'` is utf8).
fn underscore_cs(text: &str) -> &'static str {
    if text.eq_ignore_ascii_case("n") {
        return "utf8";
    }
    crate::charset::introducer_charset(&text[1..]).unwrap_or("")
}

/// Lexer value of a single/doubleAtIdentifier (lexer.go startWithAt): `@x` -> "x",
/// `@'x'` -> "x", `@@global.x` -> "@@global.x", `@@global.'x'` -> "@@global.x".
fn at_ident_text(raw: &str) -> String {
    let quoted = |s: &str| s.starts_with(['\'', '"', '`']);
    if let Some(rest) = raw.strip_prefix("@@") {
        let lower = rest.to_ascii_lowercase();
        let plen = ["global.", "session.", "local."]
            .iter()
            .find(|p| lower.starts_with(*p))
            .map_or(0, |p| p.len());
        let (prefix, name) = rest.split_at(plen);
        if quoted(name) {
            return format!("@@{prefix}{}", unquote_text(name));
        }
        return raw.to_string();
    }
    let name = &raw[1..];
    if quoted(name) { unquote_text(name) } else { name.to_string() }
}

/// ParseHexStr: x'..' / X'..' / 0x.. (the lexer guarantees the digits).
fn hex_lit(s: &str) -> Datum {
    let digits = if s.starts_with('0') { &s[2..] } else { s[2..].trim_end_matches('\'') };
    let padded = if digits.len() % 2 == 1 { format!("0{digits}") } else { digits.to_string() };
    let bytes = (0..padded.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&padded[i..i + 2], 16).unwrap_or(0))
        .collect();
    Datum::Hex(bytes)
}

/// ParseBitStr: b'..' / B'..' / 0b.. (the lexer guarantees the digits).
fn bit_lit(s: &str) -> Datum {
    let digits = if s.starts_with('0') { &s[2..] } else { s[2..].trim_end_matches('\'') };
    let aligned = digits.len().div_ceil(8) * 8;
    let padded = format!("{}{digits}", "0".repeat(aligned - digits.len()));
    let bytes = (0..padded.len())
        .step_by(8)
        .map(|i| u8::from_str_radix(&padded[i..i + 8], 2).unwrap_or(0))
        .collect();
    Datum::Bit(bytes)
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

/// toInt: int64 if it fits, else uint64, else (out of uint64 range) a decimal.
fn int_lit_datum(s: &str) -> Datum {
    match s.parse::<i64>() {
        Ok(i) => Datum::Int(i),
        Err(_) => match s.parse::<u64>() {
            Ok(u) => Datum::Uint(u),
            Err(_) => Datum::Decimal(normalize_decimal(s)),
        },
    }
}

/// MyDecimal.FromString + String(): no leading zeros in the integer part, no trailing '.'.
fn normalize_decimal(s: &str) -> String {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let int = int.trim_start_matches('0');
    let int = if int.is_empty() { "0" } else { int };
    if frac.is_empty() { int.to_string() } else { format!("{int}.{frac}") }
}
'''
