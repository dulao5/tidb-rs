# D08.1：plan

## 第 1 部分（本 PR）

1. 新 crate `tidb-expression`（`crates/expression`，rank 8，layers.toml 中已规划）。
2. `errors.rs`、`context.rs`、`collation.rs`（collationInfo 部分，D08.2）、`column.rs`、`constant.rs`、`schema.rs`、`expression.rs`、`scalar_function.rs`（过渡形态，见 spec §3.3）。
3. govec `expr` 领域（tracker `govec/expr-D08.1`）：节点的逐行求值、`HashCode`、`String`/`StringWithCtx`、`ConstLevel`、`Equal`、`Coercibility`/`Repertoire`、`MemoryUsage`、Schema 的各个操作；`TableInfo2SchemaAndNames` 用 model 领域中真实集群导出的表结构做输入。Rust 回放 `tests/govec_expr.rs`。

回放：`cargo test -p tidb-expression --test govec_expr`；额外的种子：`GOVEC_EXPR_FILES=a.jsonl.gz,b.jsonl.gz`。

## 第 2 部分（交接）

见 spec §1 的表；先做 N4 §5 的门禁（宏原型 + 5 个热点签名的基准），再移植 `scalar_function.go` 与驱动。

## 第 1 部分完成情况

- crate `tidb-expression`（rank 8）：`errors.rs`、`context.rs`、`collation.rs`（D08.2 的 collationInfo）、`column.rs`、`constant.rs`、`schema.rs`、`expression.rs`、`scalar_function.rs`（过渡形态）、`size.rs`。
- 第 1 部分没有移植、留给第 2 部分的 D08.1 函数（6 个已开始的文件中）：`expression.go` 中构造函数调用的部分（`ComposeCNFCondition`、`EvaluateExprWithNull`、`wrapWithIsTrue`、`VecEvalBool`/`toBool`、`EvalExpr`、`Split*`/`Flatten*`、`BuildOption` 系列、`NewValuesFunc`、`PropagateType`、`StringifyExpressionsWithCtx`、`Assignment`）；`Column.EqualByExprAndID`、`ResolveIndicesByVirtualExpr`、`GetUsedList`（需要 `ScalarFunction.Equal`）；`wrapEvalAssert` 与 `assertionEvalContext`（需要 `builtinFunc`）。
- 第 2 部分开始前先做 N4 §5 的门禁：宏原型 + 5 个热点签名与 Go `BenchmarkVectorizedBuiltin*` 对比。
