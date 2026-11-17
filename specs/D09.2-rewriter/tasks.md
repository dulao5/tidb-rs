# D09.2 表达式改写（tasks）

spec：[spec.md](spec.md)。

## 第 1 部分：子查询之外的改写（VS1-22）

进度记录（额度可能中断，随做随记）：
- 2026-11-17：spec 初稿；分支 `vs1/expr-rewrite`（tidb-rs 与 tracker 同名）。
- 2026-11-17：移植完成，govec 1,467 条一致；tidb-rs #258、tracker#102（草稿）。剩 amd64 向量（等 tracker CI artifact）。

- [x] T1 `expression_rewriter.rs`：`rewrite`、`Enter` / `Leave`、列、常量、参数、一元 / 二元、IN、BETWEEN、LIKE、IS NULL / IS TRUE、函数调用、CAST、CASE、行、COLLATE；子查询等为桩（spec §4）
- [x] T2 `FindFieldName`、`ParamMarkerExpression`（P1-3、P1-4 的这两个函数）
- [x] T3 govec `rewrite` 领域与回放
- [ ] T4 amd64 向量（tracker CI artifact）
- [x] T5 converge（44/53 + 9 桩）、stubs 登记（14 条）

## 第 2 部分（未开始）

- [ ] 子查询（与 D09.6 一起）、系统变量（把会话变量传给改写器）、`buildSimpleExpr` / `evalAstExpr` / `BuildOption`（下面 P1-1、P1-3 的其余函数）

以下是从其他块移交来的条目。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.1）`BuildOption`：`WithAllowCastArray`、`WithCastExprTo`、`WithInputSchemaAndNames`、`WithTableInfo`，即 `BuildSimpleExpr` / `ParseSimpleExpr` 的选项
- [ ] P1-2 （D08.1）`NewValuesFunc`：改写器 `ValuesExpr` 分支是唯一调用者（INSERT ... ON DUPLICATE KEY UPDATE 之外 `VALUES(col)` 为 NULL），需要 D08.9 的 `valuesFunctionClass`
- [ ] P1-3 （D08.2）`simple_rewriter.go`：`ParseSimpleExprWithTableInfo`、`ParseSimpleExpr`（`FindFieldName`、`FindFieldNameIdxByColName` 已在第 1 部分移植）
- [x] P1-4 （D08.12）`ParamMarkerExpression`：parser driver `ParamMarkerExpr`（预处理语句的 `?`）的常量（第 1 部分）
