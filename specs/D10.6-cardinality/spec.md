# D10.6 基数估算（spec）

状态：第 1 部分（VS1-29：pseudo 路径）评审中（2026-11-15）。块：D10.6（ROADMAP P3），crate `tidb-planner`，模块 `cardinality`。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（VS1-29）、§8 风险 7；[D11.1 spec](../D11.1-statistics/spec.md)（统计结构与共享规则）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/`。

## 1. 范围

Go 范围：`pkg/planner/cardinality`，加上 logicalop 中各算子的 `DeriveStats` / `ExtractColGroups`。tracker 中 104 个函数。

### 1.1 第 1 部分（本 PR）

函数清单是 VS1 `functions.tsv` 中 VS1-29 的 D10.6 行（55 个）：converge **matched 38/54，桩 16**（另 1 个 `init` 标为不移植，§1.3）。

| Go | Rust（`crates/planner/src/cardinality`） | 内容 |
|---|---|---|
| `pseudo.go` | `pseudo.rs` | 全部：pseudo 的比率、`pseudoSelectivity`、int / 列 / 索引范围的 pseudo 行数 |
| `selectivity.go` | `selectivity.rs` | `Selectivity`、`CalcTotalSelectivityForMVIdxPath`、`GetUsableSetsByGreedy`、`getMaskAndRanges`（范围构建是边界）、`findPrefixOfIndex(ByCol)`、`isColEqCorCol`、`getConstantColumnID`、`IsLastBucketEndValueUnderrepresented`、`outOfRangeEQSelectivity`、`outOfRangeFullNDV`、`findAvailableStatsForCol` |
| `row_count_column.go` | `row_count_column.rs` | `GetRowCountByColumnRanges`、`GetRowCountByIntColumnRanges` 的 pseudo 路径、`ColumnEqualRowCount` / `ColumnGreaterRowCount` / `columnLessRowCount` / `columnBetweenRowCount` |
| `row_count_index.go`（清单外，`Selectivity` 需要） | `row_count_index.rs` | `GetRowCountByIndexRanges` 的 pseudo 路径与 `canSkipIndexEstimation` |
| `ndv.go` | `ndv.rs` | 全部 |
| `row_size.go` | `row_size.rs` | 全部 |
| `trace.go` | `trace.rs` | 只有 trace 的边界与 `recordUsedItemStatsStatus` 桩 |

另外：`property.StatsInfo` 加上 `HistColl` 字段（`Option<Arc<HistColl>>`，D10.10 中原来留空），`Scale` / `DeriveLimitStats` 与 Go 一样传递它。

### 1.2 边界桩

| 桩 | 阻塞块 | 说明 |
|---|---|---|
| `GetColumnRowCount`、`equalRowCountOnColumn`、`betweenRowCountOnColumn` | D11.1 第 2 部分 | 用真实统计估算（TopN、CMSketch、越界估算）。没有统计加载（D11.2）之前统计总是 pseudo，走不到 |
| `getIndexRowCountForStatsV1` / `V2`、`getEqualCondSelectivity`、`crossValidationSelectivity` | D10.6 第 2 部分 | 用真实索引统计估算，同上 |
| `GetSelectivityByFilter` | D10.6 第 2 部分 | TopN 辅助估算；只有 `tidb_default_string_match_selectivity = 0` 时调用，`Selectivity` 此时返回边界错误 |
| 范围构建：`ranger.ExtractAccessConditionsForColumn`、`BuildColumnRange`、`DetachCondAndBuildRangeForIndex`、`MergeDNFItems4Col` | D10.4（VS1-25） | `Selectivity` 中涉及 `HistColl` 的列或索引的条件需要先建范围：在 VS1-25 合入前返回 `blocked-by:D10.4` |
| MV 索引：`collectFilters4MVIndex`、`buildPartialPaths4MVIndex`（及 D11.1 的 `PrepareCols4MVIndex`） | D10.7 | 有 MV 索引的表回退 |
| `recordUsedItemStatsStatus` | D11.2 | 记录语句用到的统计（慢日志 `Stats:`）；`StatementContext.UsedStatsInfo` 未移植（D11.2 tasks P1-1）。不影响估算 |
| optimizer debug trace / CE trace（`trace.go`、`debugtrace`） | D10.6 第 2 部分 | 入口（`Selectivity`、`GetRowCountBy*Ranges`）在 `EnableOptimizerDebugTrace` 或 `EnableOptimizerCETrace` 打开时返回 `blocked-by:D10.6`（`TRACE PLAN TARGET='estimation'`、plan replayer，VS1 不路由这些语句），不返回一个缺少 trace 的估算 |
| 各算子的 `DeriveStats`、`ExtractColGroups`、`RecursiveDeriveStats`（`logicalop/derive_stats.rs`） | D09.14（VS1-21 / VS1-23） | 需要逻辑算子本身和它们的 `StatsInfo` 槽位；算子移植时把这些函数移到算子文件中 |

块内其余函数（`cross_estimation.go`、`join.go`、`row_count_index.go` 的其余部分、`trace.go` 的其余部分、其他算子的 `DeriveStats`）是第 2 部分（`blocked-by:D10.6`）。

### 1.3 不移植

`row_count_column.go` 的 `init`：Go 把本包的 `GetRowCountBy*` 赋给 `statistics` 包的函数变量，以打破 import 环。Rust 的 `tidb-statistics` 不调用它们，没有这个环（`functions.tsv` 的 `not_ported` 列）。

## 2. 承载方式

1. Go 的 `sctx planctx.PlanContext` 是 `&PlanCtx<'_>`（N1 / N2）：会话变量 `ctx.vars`、表达式上下文 `ctx.expr_ctx`。
2. `Selectivity` 的 `retStatsNodes`：Go 由 `GetUsableSetsByGreedy` 原地排序后返回；Rust 返回同样排序后的 `Vec<StatsNode>`。`GetUsableSetsByGreedy` 返回位置而不是指针（L2）。
3. Go 按随机顺序遍历未覆盖条件的 map（`notCoveredConstants`、`notCoveredDNF` 等）；Rust 按条件位置遍历（`M-MAPORDER`）。乘法的顺序只在有两个以上 DNF 条件时影响最后一位，Go 自己每次运行也可能不同；govec 不生成这种用例。`findAvailableStatsForCol` 遍历 `Idx2ColUniqueIDs`，Rust 按索引 id 顺序。
4. DNF 递归中，`Selectivity` 的错误 Go 记日志后用 `selectionFactor`；Rust 对边界错误（`blocked-by`）原样返回，不把"未移植"当作"估算出错"。
5. `M-FMA`（tracker `data/go_fma_sites.tsv`，本 PR 加入两个包）：第 1 部分涉及的位置

   | 位置 | 形式 | 融合的目标 | Rust |
   |---|---|---|---|
   | `selectivity.go:370` `Selectivity` | `s + c - s*c` | arm64、riscv64、loong64（`FMSUBD`） | `msub` |
   | `selectivity.go:535` `CalcTotalSelectivityForMVIdxPath` | `(sel + t) - t*sel` | 同上 | `msub` |
   | `row_size.go:89` `GetAvgRowSize` | `size + n/8`（gc 改成 `*0.125` 后融合） | arm64、ppc64le、s390x、riscv64、loong64 | `madd`（结果与不融合相同：乘以 0.125 是精确的） |
   | `histogram.go:555` `LessRowCountWithBktIdx`（D11.1） | `pre + f*(...)` | 同上 | `histogram::madd` |

   桩中的位置（`row_count_column.go:237`、`:261`、`:310`，`row_count_index.go:211`、`:287`、`:369`–`:376`，`selectivity.go:1038`，`histogram.go:1073` 等）随第 2 部分移植。amd64（GOAMD64=v1）不融合任何位置。
6. `M-INT`：`getPseudoRowCountBySignedIntRanges` 的 `high-low` 按 int64 回绕（`wrapping_sub`）。`M-F2I`：`getTotalRowCount` 的 `int64(...)`。

## 3. pseudo 估算的常数（逐个对照 Go，评审要求）

| 常数 | Go 位置 | 值 | Rust |
|---|---|---|---|
| `pseudoEqualRate` | `planner/cardinality/pseudo.go:31` | 1000 | `pseudo.rs` `pseudoEqualRate` |
| `pseudoLessRate` | `pseudo.go:32` | 3 | `pseudoLessRate` |
| `pseudoBetweenRate` | `pseudo.go:33` | 40 | `pseudoBetweenRate` |
| `selectionFactor` | `pseudo.go:37` | 0.8 | `selectionFactor` |
| int 主键点查最多 1 行 | `pseudo.go:124`、`:160` | 1 | `get_pseudo_row_count_by_{signed,unsigned}_int_ranges` |
| 索引前缀每列再除 100 | `pseudo.go:203` | 100 | `get_pseudo_row_count_by_index_ranges` |
| 索引行数超过表行数时 | `pseudo.go:208` | `tableRowCount / 3.0` | 同上 |
| `[MinNotNull, …]` 的 NULL 行数 | `pseudo.go:220` | `tableRowCount / pseudoEqualRate` | `get_pseudo_row_count_by_column_ranges` |
| `PseudoRowCount` | `statistics/table.go:41`（用于 `:1029`） | 10000 | `tidb_statistics::table::PseudoRowCount` |
| `PseudoVersion` | `statistics/table.go:35` | 0 | `PseudoVersion` |
| `distinctFactor` | `planner/cardinality/ndv.go:27`（用于 `:42`） | 0.8 | `ndv.rs` `distinctFactor` |
| `pseudoColSize` | `planner/cardinality/row_size.go:29`（用于 `:66`） | 8.0 | `row_size.rs` `pseudoColSize` |
| 索引键前缀 | `row_size.go:36`、`:39` | 19；非唯一索引再加 1 | `get_index_avg_row_size` |
| TiKV 行键 | `row_size.go:49`、`:51` | `tablecodec.RecordRowKeyLen`（19）减 8 | `get_table_avg_row_size` |
| TiFlash row_id | `row_size.go:54` | 8 | 同上 |
| 每列 null bitmap / flag 字节 | `row_size.go:89`、`:92` | 1/8；1 | `get_avg_row_size` |
| 每列落盘长度记录 | `row_size.go:114` | 8 | `get_avg_row_size_data_in_disk_by_rows` |
| `outOfRangeBetweenRate` | `planner/cardinality/selectivity.go:42` | 100 | `selectivity.rs` `outOfRangeBetweenRate` |
| `unknownColumnID` | `selectivity.go:583` | `math.MinInt64` | `unknownColumnID` |
| `staleLastBucketThreshold` | `selectivity.go:588` | 0.3 | `staleLastBucketThreshold` |
| `valueAwareRowAddedThreshold` | `selectivity.go:593` | 0.5 | `valueAwareRowAddedThreshold` |
| `AutoAnalyzeMinCnt` | `statistics/table.go:46` | 1000 | `AutoAnalyzeMinCnt` |
| `RatioOfPseudoEstimate` | `statistics/table.go:890` | 0.7 | `RatioOfPseudoEstimate`（`0x3FE6666666666666`） |
| `Version0` / `1` / `2` | `statistics/histogram.go:250`、`:258`、`:263` | 0 / 1 / 2 | `tidb_statistics::Version*` |
| `TopN.MemoryUsage` | `statistics/cmsketch.go:789`、`:791` | 32；每项 32 + `cap(Encoded)` | `TopN::memory_usage` |

govec 的 `row_count`、`col_est`、`selectivity`、`pseudo_table`、`row_size` 记录按结果覆盖了上表的每一个常数（例如 `GetRowCountByColumnRanges` 的等值 / 小于 / 区间 / `[NULL, +inf]`、`[MinNotNull, …]` 各形状，`EstimateColumnNDV` 的 pseudo 分支）。

## 4. 与 Go 的差异（均已在代码中标注）

1. trace 与 used-stats 记录（§1.2）。
2. DNF 递归中边界错误不降级为默认选择率（§2 第 4 条）。
3. 未覆盖条件的遍历顺序（§2 第 3 条）。
4. `getMaskAndSelectivityForMVIndex` 返回 `Result`（承载 D10.7 的边界，L2）。

## 5. 可能的上游问题（记录，不提 issue）

1. `pseudoSelectivity`（`pseudo.go:52`、`:60`）用 `getConstantColumnID` 返回的 `col.ID`（列定义的 ID）调用 `coll.GetCol`，而语句的 `HistColl`（`GenerateHistCollFromColumnInfo`）以 `UniqueID` 为键。只有 ID 与某个 UniqueID 恰好相同时才找得到列，所以"唯一键等值 → `1/RealtimeCount`"的捷径与唯一索引检查在这条路径（条件超过 63 个、或 `HistColl` 没有列和索引）上基本不起作用。照搬。
2. `GetRowCountByIntColumnRanges`（`row_count_column.go:104`）按第一个范围的下界是否为 `KindInt64` 选择有符号 / 无符号的 pseudo 算法：无符号列的范围下界若是 `NULL` / `MinNotNull`，会走有符号算法，上界的 `uint64` 被当成 `int64` 读。照搬。
3. 生成向量时发现（不属于本块）：DATETIME 列与整数常量比较时，Go 的 mock 上下文与 tidb-rs 测试用的表达式上下文对 `refineNumericConstantCmpDatetime` 中常量转换的错误处理不同（Go 得到常量，tidb-rs 得到 `cast(col)`），疑与表达式上下文的类型标志有关；govec 不生成这种条件，交给 D08.4 / D00.1 的负责人核对。

## 6. 测试

govec 领域 `cardinality`（tracker `govec/domains/cardinality.go`，seed 20261115，5,685 条），tidb-rs `crates/planner/tests/govec_cardinality.rs` 回放，arm64 本地全部通过：

| op | 条数 | 结果 |
|---|--:|---|
| `pseudo_table` / `hist_coll` / `table_ops` | 252 / 250 / 250 | 全部一致 |
| `row_count`（列 / int / 索引范围） | 1,119 | 全部一致 |
| `col_est`（`TestPseudoTable` 与随机） | 502 | 全部一致 |
| `selectivity` | 1,250 | 808 一致；410 `blocked-by:D10.4`（需要建范围）、32 `blocked-by:D08.9`（IN 内置函数未移植）；0 不一致 |
| `row_size` / `avg_col` | 400 / 300 | 全部一致 |
| `hist` / `col_stats` | 350 / 400 | 全部一致 |
| `existence` / `ndv` / `mv_sel` / `topn` | 150 / 250 / 150 / 62 | 全部一致 |

回放断言只允许 `selectivity` 停在 D08.9 / D10.4 / D10.6 / D10.7 的边界；VS1-25 合入后，D10.4 的 410 条应变为一致。amd64 向量来自 tracker CI 的 artifact。

单元测试：`selectivity::tests::selectivity_greedy_algo`（Go `TestSelectivityGreedyAlgo`）、`out_of_range_helpers`；`govec_cardinality.rs` 的 `pseudo_table_go_test`（Go `TestPseudoTable`，含未导出的 `columnLessRowCount` / `columnBetweenRowCount`）。`selectivity_test.go` 的其余用例都依赖存储（`testkit`）和真实统计，它们的 pseudo 部分由上表的向量按函数覆盖；EXPLAIN 级的 pseudo 用例（`TestIgnoreRealtimeStats`、`TestBuiltinInEstWithoutStats` 的 `stats:pseudo` 行）随 VS1-28 的计划向量验收。
