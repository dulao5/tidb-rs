# D10.6 基数估算（tasks）

spec：[spec.md](spec.md)。状态截至 2026-11-15：第 1 部分评审中（VS1-29）。

## 第 1 部分：pseudo 路径（VS1-29）

- [x] T1 `pseudo.go` 全部
- [x] T2 `selectivity.go`：`Selectivity` 与辅助函数（范围构建、MV 索引、TopN 辅助估算是边界）
- [x] T3 `row_count_column.go` 的 pseudo 路径与测试辅助函数；`row_count_index.go` 的 `GetRowCountByIndexRanges` pseudo 路径
- [x] T4 `ndv.go`、`row_size.go`
- [x] T5 `StatsInfo.HistColl`
- [x] T6 govec `cardinality` 领域（与 D11.1 共用）与回放；`go_fma_sites.tsv` 加入两个包
- [x] T7 amd64 向量（tracker#91 CI artifact，run 43653955539；与 arm64 有 8 条只差最后几位，M-FMA）

## 第 2 部分（`blocked-by:D10.6`）

- 用真实统计的估算：`getIndexRowCountForStatsV1` / `V2`、`estimateRowCountWithUniformDistribution`、`equalRowCountOnIndex`、`expBackoffEstimation`、`betweenRowCountOnIndex`、`outOfRangeOnIndex`、`getEqualCondSelectivity`、`crossValidationSelectivity`、`GetSelectivityByFilter`（与 D11.1 第 2 部分一起）
- 跨列估算：`cross_estimation.go`、`join.go`
- trace：`trace.go`、debug trace（入口目前返回边界错误）
- 其他算子的 `DeriveStats` / `ExtractColGroups`（聚合、join、apply、window、CTE 等，随各自的算子）

## 交给其他项

- VS1-21 / VS1-23（D09.14）：`BaseLogicalPlan.DeriveStats` / `RecursiveDeriveStats` / `ExtractColGroups`，`DataSource` / `LogicalSelection` / `LogicalProjection` / `LogicalLimit` / `LogicalTopN` / `LogicalTableDual` 的 `DeriveStats`（桩在 `logicalop/derive_stats.rs`，移植时移到算子文件）
- VS1-25（D10.4）：范围构建；`Selectivity` 中被阻塞的向量（410 条）应在其合入后一致
- VS1-28（D10.2）：`getStatsTable`（取统计快照、`ShallowCopy` / pseudo 的改写，D11.1 spec §3 表第 3、4 行）
