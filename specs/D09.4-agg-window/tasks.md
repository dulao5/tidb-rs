# D09.4 聚合、窗口构建（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.2）`grouping_sets.go` 全部 29 个函数（ROLLUP / Expand；`GroupingSet.ToPB` 由 D10.9 的 `PhysicalExpand.ToPB` 调用）：`NewGroupingSets`、`GroupingSets.Merge`、`GroupingSets.MergeOne`、`GroupingSets.TargetOne`、`GroupingSets.NeedCloneColumn`、`GroupingSet.IsEmpty`、`GroupingSet.AllColIDs`、`GroupingSet.ExtractCols`、`GroupingSet.Clone`、`GroupingSet.StringWithCtx`、`GroupingSet.MemoryUsage`、`GroupingSet.ToPB`、`GroupingSets.IsEmpty`、`GroupingSets.AllSetsColIDs`、`GroupingSets.StringWithCtx`、`GroupingSets.ToPB`、`newGroupingSet`、`GroupingExprs.IsEmpty`、`GroupingExprs.SubSetOf`、`GroupingExprs.IDSet`、`GroupingExprs.Clone`、`GroupingExprs.StringWithCtx`、`GroupingExprs.MemoryUsage`、`RollupGroupingSets`、`AdjustNullabilityFromGroupingSets`、`DeduplicateGbyExpression`、`RestoreGbyExpression`、`GroupingSets.DistinctSize`、`GroupingSets.DistinctSizeWithThreshold`
- [ ] P1-2 （D08.12）GROUP BY / ORDER BY 的位置项：`ConstructPositionExpr`、`PosFromPositionExpr`；窗口 frame 的参数检查：`ParamMarkerInPrepareChecker.Enter`、`ParamMarkerInPrepareChecker.Leave`
