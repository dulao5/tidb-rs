# D11.2 统计信息加载（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D00.1）used-stats 记录：`StatementContext.GetUsedStatsInfo`、`StatementContext.RecordedStatsLoadStatusCnt`、`UsedStatsInfo.GetUsedInfo`、`UsedStatsInfo.Keys`、`UsedStatsInfo.RecordUsedInfo`、`UsedStatsInfo.Values`、`UsedStatsInfoForTable.FormatForExplain`、`UsedStatsInfoForTable.WriteToSlowLog`、`UsedStatsInfoForTable.collectFromColOrIdxStatus`、`UsedStatsInfoForTable.recordedColIdxCount`

## 从 VS1-29（D10.6 / D11.1 第 1 部分）移交

- [ ] 统计缓存：每张表一个 `Arc<Table>` 快照，更新时整体替换（D11.1 spec §3）；`getStatsTable` 从缓存取快照（VS1-28）
- [ ] `asyncload.AsyncLoadHistogramNeededItems`：`ColumnStatsIsInvalid` / `IndexStatsIsInvalid` 的加载请求目前被丢弃（桩 `neededStatsMap.Insert`，`crates/statistics/src/asyncload.rs`）
- [ ] `recordUsedItemStatsStatus`（D10.6，`crates/planner/src/cardinality/trace.rs` 的桩）与 P1-1 的 used-stats 记录一起
- [ ] 淘汰：`HistColl.DropEvicted`、`Column` / `Index` 的 `DropUnnecessaryData`（在副本上调用，Go `lfu_cache.go:190`）
