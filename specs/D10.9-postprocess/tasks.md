# D10.9 后处理与 EXPLAIN 输出（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.12）`pkg/expression/explain.go` 全部 20 个函数（表达式的 EXPLAIN 与 plan digest 文本）：`Column.ColumnExplainInfo`、`Column.ColumnExplainInfoNormalized`、`Column.ExplainInfo`、`Column.ExplainNormalizedInfo`、`Column.ExplainNormalizedInfo4InList`、`Constant.ExplainInfo`、`Constant.ExplainNormalizedInfo`、`Constant.ExplainNormalizedInfo4InList`、`Constant.format`、`ExplainColumnList`、`ExplainExpressionList`、`ScalarFunction.ExplainInfo`、`ScalarFunction.ExplainNormalizedInfo`、`ScalarFunction.ExplainNormalizedInfo4InList`、`ScalarFunction.explainInfo`、`SortedExplainExpressionList`、`SortedExplainExpressionListIgnoreInlist`、`SortedExplainNormalizedExpressionList`、`SortedExplainNormalizedScalarFuncList`、`sortedExplainExpressionList`
