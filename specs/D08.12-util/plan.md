# D08.12 表达式工具（plan）

## 第 1 批（本 PR）

见 spec §1a。govec：`util` 操作的 `x`（提取与判断）、`pnot`（NOT 下推、去掉 CAST）、`dnf`（DNF 提取、放宽）、`const`、`fmt` 五类，外加针对性的 `ub`（FindUpperBound）、`elim`（去掉 CAST 的各种类型和 collation 组合）、`dnfdup`（一个 DNF 项中重复的条件只计一次）。

## 第 2 批（交接）

1. `explain.go`：`ExplainInfo`/`ExplainNormalizedInfo`、`Sorted*ExplainExpressionList`、`Constant.format` 等。govec：对已移植的函数族构建表达式，比较输出文字。
2. `helper.go`：`GetTimeValue`、`GetTimeCurrentTimestamp`、`IsValidCurrentTimestampExpr`、`getStmtTimestamp`（替换 D08.8 的 glue 边界桩）。时钟用固定的语句时间。
3. 参数标记：`ParamMarkerExpression`、`ParamMarkerInPrepareChecker`、`ConstructPositionExpr`、`PosFromPositionExpr`（需要 parser driver 的 `ParamMarkerExpr`）。
4. `SubstituteCorCol2Constant`：先确定规划器调用方如何提供关联列的值（`EvalState`）。
5. `ExecBinaryParam` 和 `binary*`：随协议层 D03.1。
