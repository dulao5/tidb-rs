# D09.13 优化器驱动（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D00.1）`StatementContext.SaveLogicalPlanBuildState`、`StatementContext.RestoreLogicalPlanBuildState`：`planner/optimize.go` 在构建逻辑计划前后保存、恢复（TableInfo 为键的 map、intset）
- [ ] P1-2 （D08.12 T7）`intset.FastIntSet`（D00.3，按需移植）移植后，`ExtractColumnSet` 改用它（D08.12 spec §4 第 1 条）
