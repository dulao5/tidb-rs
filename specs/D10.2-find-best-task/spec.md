# D10.2 FindBestTask 与访问路径（spec，VS1-28 第 1 部分）

这一项移植 FindBestTask：把优化后的逻辑计划变成物理计划。对每个逻辑算子，枚举它能生成的物理算子，向子节点要求相应的物理属性，按代价选出最好的方案；对 DataSource，先算出每条访问路径（表本身、各个索引）的统计信息，用 skyline 裁剪去掉明显更差的路径，再把剩下的表路径变成 TableReader / TableScan 或 PointGet。同时移植这一步所依赖的统计信息推导（逻辑算子的 `DeriveStats`）、统计信息加载点中的索引裁剪（`PruneIndexesByWhereAndOrder`），以及记忆化（`taskMap`）。

Go 源码（v8.5.8）：`pkg/planner/core/find_best_task.go`、`stats.go`、`property_cols_prune.go`、`collect_column_stats_usage.go`、`rule_collect_plan_stats.go`（索引裁剪部分）、`rule/rule_prune_indexes.go`、`cardinality/cross_estimation.go`（limit 下的表扫描行数）、`operator/logicalop/` 中各算子的 `FindBestTask` / `DeriveStats` / `ExtractColGroups` / `PreparePossibleProperties` 和 `base_logical_plan.go` 的 `taskMap` 方法，另有 `planbuilder.go` 中几个访问路径辅助函数、`initialize.go` 的 `PointGetPlan.Init`、`plan.go` 的 `optimizeByShuffle`、`plan_cache_utils.go` 的 `isSafePointGetPath4PlanCache`、`explain.go` 的 `PhysicalTableScan.TP`。

## 1. 范围与结论

本 PR 是第 1 部分：记忆化、TableDual 与 DataSource 的表路径、skyline 裁剪、路径统计、索引裁剪。第 1 部分之后，第一个纵向切片的语句（点查、主键范围扫描、过滤、投影、limit、order by / TopN、恒假条件）在新栈上走完整的 `DoOptimize`，最终计划与 Go 相同。

| 部分 | 状态 |
|---|---|
| `findBestTask`、`enumeratePhysicalPlans4Task`、`iteratePhysicalPlan4BaseLogical`、`rebuildChildTasks`、`compareTaskCost`、`getTaskPlanCost`、记忆化（`GetTask` / `StoreTask` / `RollBackTaskMap`） | 已移植（§3） |
| `findBestTask4LogicalTableDual`、`findBestTask4LogicalDataSource`、`tryToGetDualTask` | 已移植 |
| skyline 裁剪（`skylinePruning`、`compareCandidates` 及其比较函数、`matchProperty`、`matchPartialOrderProperty`） | 已移植 |
| 表路径：`convertToTableScan`、`convertToPointGet`、`getOriginalPhysicalTableScan`、`addPushedDownSelection` | 已移植；TiFlash / MPP 扫描是边界（§5） |
| 索引路径：`convertToIndexScan` | 返回无效 task 的前置检查已移植；能生成 IndexReader / IndexLookUp 的情况是边界（第 2 部分，§5） |
| 路径统计（`stats.go`）：`deriveStats4DataSource`、`initStats`、`fillIndexPath`、`deriveTablePathStats`、`deriveIndexPathStats`、`derivePathStatsAndTryHeuristics` 等 | 已移植 |
| 统计信息推导：各算子的 `DeriveStats`、`ExtractColGroups`、`RecursiveDeriveStats`（原属 VS1-29 的函数清单，桩一直留着，这里一并移植） | 已移植 |
| 统计信息加载点的索引裁剪：`CollectColumnStatsUsage`（感兴趣列）、`pruneIndexesForDataSource`、`PruneIndexesByWhereAndOrder` | 已移植；统计信息的使用记录和加载部分仍是 D11.2 的边界 |

converge（`--part specs/VS1-vertical-slice/functions.tsv --item VS1-28`）：89 个函数中 73 个移植，12 个登记为桩，4 个是旧 cascades 优化器的例外（B3）。VS1-29 的 D10.6 部分由 38/54 + 16 桩变为 48/54 + 6 桩。

## 2. 计划向量（EXPLAIN 不含 estRows / cost）

tracker 新领域 `findbest`（tidb-rs 回放 `crates/planner/tests/govec_findbest.rs`）：

- 语料：`selectbuild` 的全部 SQL（固定 + 随机），加一张有 13 个二级索引的表 `tm` 的查询，加第一个纵向切片在 sbtest1 上的 27 条语句。Go 跑完整的 `core.DoOptimize`；mock 上下文绑定一个没有统计信息句柄的 domain，每张表都是 pseudo 统计，与新栈一致（VS1 plan §3.2）。
- 比较内容：最终物理计划的每个算子的类型（含 `TableFullScan` / `TableRangeScan`）、plan id、schema，以及 EXPLAIN 的 operator info 对应的字段：TableReader 的存储类型和表计划，TableScan 的 range、access 条件、keep order、desc，PointGet 的 handle / 索引和值，Selection 的条件，Projection 的表达式，Sort / TopN 的排序项，Limit / TopN 的 offset / count，TableDual 的行数；表达式按 EXPLAIN 的文本比较。不比较 estRows 和代价。
- 结果：1,787 条计划记录中 969 条与 Go 完全相同，0 条不同；其余停在已登记的边界上（数目在测试中固定）：索引路径 668 条（其中 528 条 Go 选了 IndexReader / IndexLookUp，140 条 Go 比较代价后仍选表路径，见 §5 第 1 条），index merge 60 条，BatchPointGet 7 条，其余是构建器已有的边界（未移植的内置函数、聚合、join、子查询、分区表等）。`prune` 420 条全部相同。
- **sbtest 上的第一个纵向切片语句**：27 条全部与 Go 相同；Go 中没有一条停在 IndexReader / IndexLookUp（测试断言）。sbtest 的二级索引 `k_1` 在这些语句中都被 skyline 裁剪去掉（表路径的 access 条件覆盖它、且是单读），所以不需要把索引读入纳入第一个纵向切片。

优化器驱动的回放（`govec_optdriver`）照旧通过。它的语料就是上面 `plan` 记录的语料：以前停在统计信息推导（D10.6）的语句，现在有 969 条走完 `DoOptimize` 并与 Go 相同。

## 3. 记忆化（taskMap）的表示

按 taskMap 小试点（`specs/VS1-vertical-slice/spike-taskmap.md`）和 N1 §3 第 9 条：

1. **旁表**：Go 每个逻辑算子上的 `taskMap` / `taskMapBak` / `taskMapBakTS` 合成一个 `TaskMap`（`logicalop/base_logical_plan.rs`），键为（arena 中的节点、`prop.HashCode()`）。回滚后 Go 写 `taskMap[key] = nil`，这里存 `None`。
2. **只在 physicalOptimize 内部存活**：`physical_optimize` 创建 `TaskMap`，FindBestTask 返回后立即 drop；再把最优计划从 task 中**移出**（不克隆 `Arc`），然后才 `ResolveIndices` 和计算代价。旧写法 `Arc::clone(t.plan())` 后 `make_mut_plan` 会在 task 还活着时复制根节点（#290 评审）。
3. **候选按值移入 `Attach2Task`**：`exhaustPhysicalPlans` 的结果用 `into_iter()` 逐个交出，不持有候选再克隆。
4. **共享与写入**：命中时返回旁表中同一个 `Arc`（Go 中同一个指针）；代价缓存经 `&self` 写在共享节点上。单元测试（`govec_findbest.rs`）：
   - `memo_hit_shares_the_task_without_copies`（试点 E2）：第二次以同一属性向同一子节点要 task，得到同一个 `Arc`、不新建计划、代价比较不复制节点、代价缓存已写在共享节点上；
   - `memo_rollback_clears_entries_after_the_time_stamp`（试点 E3）：`nth_plan` 时按时间戳回滚，ts 之后存的项变成 `None`，之前的保留；
   - `best_plan_is_owned_alone_after_the_memo`（试点 E1 与 physicalOptimize 末尾）：对 4 条第一个纵向切片语句，整个 `physicalOptimize`（含末尾的 `ResolveIndices`）`clone_node` 为 0，返回的计划每个节点独占（reader 的扁平列表 `table_plans` 除外，N1 §3 第 9.1 条），`postOptimize` 复制 0 次。
5. **Go 中写共享节点的位置**（试点 §3 要求逐处核对）：本部分新增的写入有 `matchProperty` 写 `path.GroupedRanges` / `GroupByColIdxs`、skyline 写 `path.ForcePartialOrder`、`pushDownNot` 写 `ds.AllConds`、`fillIndexPath` 写 `TableStats.HistColl.Idx2ColUniqueIDs`、索引裁剪写 `path.IsSingleScan`。前三处写的是 DataSource 自己的数据，Rust 照样写回 DataSource（候选持有路径的副本，写入同时写回）；`Idx2ColUniqueIDs` 写在 DataSource 自己的 `TableStats` 上（写时复制，此时只有 DataSource 持有）；`IsSingleScan` 写回 `AllPossibleAccessPaths` 并同步到 `PossibleAccessPaths`（§4 第 2 条）。没有"写共享节点并依赖可见性"的情况。

## 4. 与 Go 表示不同的地方

1. **属性的哈希缓存**：`findBestTask4LogicalDataSource` 在探索"加排序"的方案时，Go 临时清空调用方 `prop` 的排序项，返回前再恢复；由于 `prop` 的哈希码已经缓存，记忆化的键始终是原来的属性。Rust 不改调用方的属性，而是复制一份（复制时带上已缓存的哈希码），效果相同。
2. **访问路径的指针共享**：Go 的 `PossibleAccessPaths` 和 `AllPossibleAccessPaths` 保存同一批 `*AccessPath`，推导统计信息时改的是共享对象。Rust 两者都是值，统计信息推导在 `all_possible_access_paths` 上进行，然后按位置重建 `possible_access_paths`（`PathAliases`）。一条可能路径不在全部路径中（只有 index merge 备选会这样）时返回 `blocked-by:D10.7`。
3. **候选路径**：Go 的 `candidatePath.path` 指向 DataSource 的路径；Rust 的候选持有一份副本（DataSource 每被要一次 task 就复制一次候选路径；属于每条语句的额外拷贝，留待 VS1-32 的规划耗时测量时一并评估，不提前优化）。
4. **nil 属性**：Go 的 `prop == nil` 只出现在 index join 的内侧（由 join 自己生成内侧计划）；index join 未移植，Rust 的子属性为 nil 时返回 `blocked-by:D10.3`。
5. **统计信息的使用记录**（`StmtCtx.GetUsedStatsInfo`、`RecordUsedInfo`，`usedStatsInfo`）：慢日志和 EXPLAIN ANALYZE 读它，新栈不记录（D14.2）。`loadTableStats` 只读取统计信息表。
6. **统计信息句柄**：新栈没有统计信息句柄，`getStatsTable` 走 Go 的第一种情况（句柄未初始化），返回 pseudo 表（VS1 plan §3.2）。
7. **统计信息加载点**：`CollectPredicateColumnsPoint` 现在收集感兴趣列并裁剪索引（与 Go 一致，计划向量包含这一步）；谓词列的使用记录（`UpdateColStatsUsage`）和统计信息的同步 / 异步加载不做，原因同上。`SyncWaitStatsLoadPoint` 仍直接返回原计划。

## 5. 边界（blocked-by）

| 位置 | 触发条件 | 桩 |
|---|---|---|
| `convertToIndexScan` | skyline 之后留下的索引路径能生成 task（IndexReader / IndexLookUp） | `blocked-by:D10.2`（第 2 部分） |
| `matchProperty` | 索引的多个点 range 需要归并排序（`GroupRangesByCols`） | `blocked-by:D10.2` |
| `CheckPartialIndexes` | 表有部分索引（带条件的索引） | `blocked-by:D10.2` |
| `convertToBatchPointGet` | 唯一键上有多个点 range | `blocked-by:D10.8` |
| `generateIndexMergePath`、skyline / 候选循环中的 index merge 路径 | 条件中有 `OR` 且有两条以上路径，或有多值索引，或 index merge hint | `blocked-by:D10.7` |
| `convertToTableScan` | TiFlash 路径、MPP 属性 | `blocked-by:D10.1` |
| `matchProperty` | 向量索引属性 | `blocked-by:D10.1` |
| `crossEstimateTableRowCount` | 真实统计信息上的相关性估算（pseudo 表上 Go 直接跳过） | `blocked-by:D10.6` |
| `CollectColumnStatsUsage` | plan replayer 捕获打开 | `blocked-by:D02.2` |
| SHOW 算子的 FindBestTask | SHOW / ADMIN SHOW DDL JOBS（不路由到新栈） | `blocked-by:D12.12` |
| 访问路径辅助函数：`tryGetCommonHandleCols`、`setExtraPhysTblIDColsOnDataSource`、`addColumnsWithVirtualExprs`、`checkIsAllSpecialGlobalIndex` | ADMIN、DML、ANALYZE、分区表（调用方都不在第一个纵向切片） | `blocked-by:D06.4` / `D09.12` / `D09.17` |

第 1 条要说明：skyline 之后还留着索引路径时，Go 会算出索引读的代价再与表路径比较；第 1 部分不移植索引读，所以只要留下了能生成 task 的索引路径就返回边界（VS1 plan §3.2 的约定），由路由器回退到原型。计划向量中这样的记录有 668 条，其中 140 条 Go 最后仍选了表路径——第 2 部分移植索引读之后它们会与 Go 相同。第一个纵向切片的语句都不受影响（§2）。

tracker 的桩登记表新增 22 行、删除 6 行。`scripts/stub_ratchet.tsv` 中 planner 一项由 363 变为 372（planner 目录中 `blocked-by` 标记 +30 / −21，净 +9）。删除的 21 个是原来的整函数桩：统计信息推导 11 个（`derive_stats.rs` 10 个、`LogicalProjection.getGroupNDVs` 1 个）、`taskMap` 3 个、FindBestTask / `preparePossibleProperties` 2 个、`IsSingleScan` / `CheckPartialIndexes` 2 个、`CollectPredicateColumnsPoint` 2 个、`PointGetPlan.Init` 1 个。新增的 30 个是第 1 部分代码在 Go 分支处的显式边界（每处多为注释加 `blocked(...)` 调用两行）：index merge（D10.7）9 个（skyline、候选循环、`generateIndexMergePath`、访问路径别名、三个辅助函数）、TiFlash / MPP / 向量索引（D10.1）4 个、BatchPointGet（D10.8）2 个、分区表点查与 planbuilder 辅助函数（D09.12）3 个、索引读 / `GroupRangesByCols` / 部分索引 / `appendExtraHandleCol`（D10.2）若干、ANALYZE 辅助函数（D09.17）2 个、SHOW（D12.12）3 个，以及 D10.3、D10.6、D06.4、D02.2、D14.2 各 1 个。以前这些分支所在的函数整体未移植，没有单独计数。

## 6. 已知差异

- 除 §4 的表示差异外，没有用户可见的差异。计划向量不比较 estRows 和代价；代价比较中用到的行数由路径统计、`AdjustRowCountForTableScanByLimit` 和代价模型 v2 给出，与 Go 的选择一致（向量中 0 条不同）。
- 代价模型 v1（`tidb_cost_model_version = 1`，例外 B5）：FindBestTask 比较代价时返回 `blocked-by:EXC.B`，路由器回退到原型（测试 `select_with_cost_model_v1_is_the_cost_model_boundary`、路由器测试 `select_with_cost_model_v1_falls_back_to_the_prototype`）。EXPLAIN 仍不路由到新栈（已有测试 `explain_is_never_routed_to_the_new_stack`）。
- 没有发现 Go 本身的问题（无 upstream-pitfall / go-gap）。

## 7. 验证

- `govec_findbest`：计划向量 969 条与 Go 相同、0 条不同，边界数目固定；`prune` 420 条相同；第一个纵向切片在 sbtest1 上的 27 条语句全部相同，且 Go 中没有索引读。
- 记忆化单元测试 3 个（§3 第 4 条），代价模型 v1 的规划器测试和路由器测试各 1 个。
- `govec_optdriver`、`govec_selectbuild` 等原有回放照旧通过；`plan_cache_tests` 中两个原先期望停在统计信息推导（D10.6）的用例，改为期望规划成功。
- converge：VS1-28 73/89 + 12 桩 + 4 例外，OK；VS1-24（D09.13）56/60 + 4 桩，不变；VS1-29（D10.6）48/54 + 6 桩。

## 8. 第 2 部分（后续）

索引路径（`convertToIndexScan`、`getOriginalPhysicalIndexScan`、`PhysicalIndexScan` 的方法、`GroupRangesByCols`）、部分索引、index merge 的候选（与 D10.7 一起）、index join 的内侧候选、CTE / memory table 的 task。

## 本文所用 ID

- VS1-24：优化器驱动；VS1-26：taskMap 小试点；VS1-28：FindBestTask 与访问路径（本项）；VS1-29：pseudo 基数估算
- D10.1：物理算子与 task（含 TiFlash / MPP）；D10.2：FindBestTask 与访问路径；D10.3：物理实现枚举（含 index join）；D10.6：基数估算；D10.7：index merge；D10.8：点查快速路径（含 BatchPointGet）
- D02.2：parser 未覆盖的语法（含 PLAN REPLAYER）；D06.4：table 抽象；D09.12：分区表；D09.17：ADMIN / ANALYZE 构建；D11.2：统计信息缓存；D12.12：SHOW 等执行器；D14.2：慢日志与语句摘要
- N1：计划树和表达式树的表示（§3 第 9 条：物理计划的写法补充规则）
- B3：旧 cascades 优化器（不移植）；B5：代价模型 v1（不移植）
