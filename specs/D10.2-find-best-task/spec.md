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

---

## 9. 第 2 部分：索引访问路径范围与拆分（#435）

状态：第 2 部分 spec 草案（2026-11-26），按本仓库 `D09.4` / `D12.1` 惯例追加于同块 `spec.md`，不改动上方第 1 部分（§1–§8）已确认的内容，也不直接修改相邻块 spec。基线：Go TiDB `v8.5.8`（`8b857efa2`），下文 Go 路径相对于 `pkg/`。本任务主 crate 为 `tidb-planner`，同时在 D12.1 第 2B/2C 部分合入前，于 `crates/executor/src/builder.rs:230-245` 为 `PhysicalKind::IndexReader` / `PhysicalKind::IndexLookUpReader` 放置临时 `blocked-by:D12.1` 边界桩，并为 `PhysicalKind::IndexScan` / `PhysicalKind::LocalIndexLookUp` 按 `TableScan` 同等处理返回 `ErrUnknownPlan`。

相关：[D10.1 spec](../D10.1-physical-plans/spec.md)（物理算子定义与 `Arc` 树、`§5 第 1 条` 虚拟列待定问题）、[D10.5 spec](../D10.5-cost-model/spec.md)（代价模型 v2 与 `M-FMA`）、[D10.9 spec](../D10.9-postprocess/spec.md)（`ResolveIndices`、`ToPB`、`accessObject`）、[D12.1 spec](../D12.1-read-executors/spec.md)（第 2B/2C 部分执行器消费本任务产物）、[N1 spec](../exceptions/N1-plan-representation/spec.md)、[A3 spec](../exceptions/A3-plan-cache/spec.md)（实例级计划缓存）、[N13 spec](../exceptions/N13-shared-metadata/spec.md)。

### 9.1 三部分实现拆分（2A / 2B / 2C）与依赖

为配合 [D12.1 spec](../D12.1-read-executors/spec.md) 第 2B（`IndexReaderExecutor`）与第 2C（`IndexLookUpExecutor`）部分逐层解锁端到端执行，并使每个 PR 独立可测，第 2 部分拆为三个子部分：

| 子部分 | 涵盖能力 | 前置依赖 | 完成后解锁 |
|---|---|---|---|
| **第 2A 部分：IndexReader 单读路径** | `PhysicalIndexScan`、`PhysicalIndexReader`、`GroupRangesByCols`、`convertToIndexScan` 单读分支、`PhysicalIndexScan` 与 `PhysicalIndexReader` 的 `CostVer2` / `ResolveIndices` / `ToPB` / `accessObject` / `ExplainID` / `TP`、`getTblStats`、`AdjustRowCountForIndexScanByLimit`、A3 第 2 阶段单读支持 | **无未满足前置块** | 解锁 **D12.1 第 2B 部分**；使仅保留单读索引路径（及表路径）的语句完整通过 `DoOptimize` 与实例计划缓存 |
| **第 2B 部分：IndexLookUpReader + LocalIndexLookUp 回表双读与下推** | `PhysicalIndexLookUpReader`、`PhysicalLocalIndexLookUp`、`PhysicalIndexScan.NeedExtraOutputCol`、`PhysicalTableScan.appendExtraHandleCol`、`convertToIndexScan` 双读分支、`buildIndexLookUpTask`、`finishIndexPlan`、`sinkIntoIndexLookUp`、`detachRootTableScanPlan`（N1 纯 `Arc` 替代）+ `buildPushDownIndexLookUpPlan`、`checkIndexLookUpPushDownSupported`、双读 `CostVer2` / `ResolveIndices` / `accessObject` / `LocalIndexLookUp.ToPB`、A3 第 2 阶段双读与下推支持 | 依赖 **第 2A 部分** | 解锁 **D12.1 第 2C 部分**；`govec_findbest` 中 D10.2 边界（528 + 140）与 `govec_postprocess` 中 D10.2 边界（11 + 12）全部清零 |
| **第 2C 部分：部分索引（Partial Indexes）** | `DataSource.CheckPartialIndexes`、`CheckPartialIndexByFilters` 与 `pkg/planner/core/partidx/check_constraint.go`（7 个函数），以及 `PartIdxCondNotAlwaysValid` 缓存判定 | 硬依赖 **D09.2 `build_simple_expr`**（`ParseSimpleExpr`，当前 `crates/planner/src/core/expression_rewriter.rs:441-442` 为 `blocked-by:D09.2`） | 在 D09.2 解锁之前继续维持 `crates/planner/src/core/stats.rs:217-218` 边界桩；D09.2 就绪后独立合入 |

**2A 阶段 `convertToIndexScan` 的边界纪律（关键）**：在第 2A 部分尚未移植 `PhysicalIndexLookUpReader` 时，先行运行 Part 1 已移植的前置 `InvalidTask` 校验（`crates/planner/src/core/find_best_task.rs:1844-1873`，对应 `find_best_task.go:2538-2567`，如 `mv_index`、`!is_single_scan && prop.task_tp == CopSingleReadTaskType`、排序项与 `force_*` 约束冲突等）；一旦某个 `!path.is_single_scan` 索引候选通过上述校验、即进入实际构造双读 `CopTask` / `PhysicalIndexLookUpReader` 的分支时，`convert_to_index_scan` 必须直接返回 `blocked-by:D10.2`，由路由器回退到 PoC；**绝不可跳过双读候选而在仅剩表路径或次优单读路径中选择计划**，防止 2A 阶段产出与 Go 不同的查询计划（同时也保留 `CopSingleReadTaskType` 下合法拒绝双读、继续单读的 Go 行为）。同时，在 `D12.1` 第 2B/2C 合入之前，`crates/executor/src/builder.rs:214` 必须在执行器打开前将 `PhysicalKind::IndexReader` / `PhysicalKind::IndexLookUpReader` 停在 `blocked-by:D12.1`，确保路由器在切换前安全回退。

### 9.2 对现有边界与 `govec` 向量的影响

基于 `crates/planner/tests/vectors/v8.5.8/arm64/` 下现有提交向量的实测统计：

1. **`govec_findbest`**（共 2,209 条：`plan` 1,787 + `prune` 420 + 元数据 2）：
   - Go 基准的 `plan` 输出中：`IndexReader` **342** 条、`IndexLookUp` **229** 条、`IndexMerge` **2** 条、报错/不支持 **176** 条、其余（`TableReader` / `PointGet` / `TableDual`）**1,038** 条。其中 `342 + 229 = 571` 条在早先环节已先停在其它未移植块（如 `D08.9`、`D09.4`、`D13.3` 等）的有 43 条，直接停在 `convertToIndexScan` 的有 **528** 条，加上 Go 算出索引代价后仍选表路径的 **140** 条，构成 `crates/planner/tests/govec_findbest.rs:96-99` `EXPECTED_BLOCKED` 中的 `("D10.2", 528)` + `("D10.2 (Go: table)", 140)` = **668** 条。
   - **第 2A 部分**后，仅涉及单读索引与表路径竞争的记录开始脱离边界；**第 2B 部分**合入后，上述 **528 + 140 = 668 条边界全部清零**。
   - 当前 `govec_findbest.rs:301`（及 tracker 侧 `govec/domains/findbest.go`）将非 `TableReader`/`TableScan`/`PointGet` 算子统一折叠为 `{"other": true}`，实施第 2A/2B 部分时必须同步扩展 Go 生成器 `fbDump` 与 Rust `dump`（详见 §13），绝不可用 `other: true` 跳过字段比较。
2. **`govec_postprocess`**（共 1,034 条：`plan` 620 + `missing` 360 + `access` 52 + `tables` 1 + 元数据 1）：
   - Go `plan` 中含 `IndexReader` 10 条、`IndexLookUp` 4 条（其中 3 条停在更早边界），对应 `crates/planner/tests/govec_postprocess.rs:92,98` 中的 `("plan D10.2", 11)` 与 `("missing D10.2", 12)`（含 `IndexReader` / `IndexLookUp` 计划节点缺列测试变体）。
   - **第 2A + 2B 部分**合入后，`plan D10.2`（11 条）与 `missing D10.2`（12 条）**全部清零**，同步扩展 `ppDump` 与 `check_reader_schemas`（详见 §13）。
3. **端到端执行栈（结合 D12.1 2B/2C）**：与 D12.1 第 2B/2C 合入后，原本停在 D10.2 的单索引覆盖读（如仅查 `id`/`k` 的二级索引覆盖扫描）与二级索引回表范围/保序/分页查询正式走通新栈执行；带有 `USE/FORCE/IGNORE INDEX` hint 的语句仍停在 `D13.3`，含 `OR` 多路径/`MVIndex` 的语句仍停在 `D10.7`，部分索引语句仍停在 `D10.2`（第 2C 部分）/`D09.2`。

---

## 10. 函数清单与 tracker 对账（含跨块职能）

### 10.1 `D10.2` 全量 181 个函数对账

核对 tracker `data/func_blocks.tsv` 与 `data/part_funcs.tsv`，`D10.2` 全部 **181** 个函数分布如下（0 遗漏、0 重复）：

| 类别 | 数量 | 明细与归属说明 |
|---|--:|---|
| **`VS1-21` + `VS1-28` 已登记** | 98 | `VS1-21`（9）+ `VS1-28`（89，含 `PhysicalTableScan.appendExtraHandleCol` 的桩登记）；第 2B 部分正式转正 `appendExtraHandleCol` |
| **第 1 部分既已移植、待补登 `part_funcs.tsv`** | 14 | `collect_column_stats_usage.go` 9 个（`newColumnStatsUsageCollector`、`addPredicateColumn`、`addPredicateColumnsFromExpressions`、`updateColMap`、`updateColMapFromExpressions`、`collectPredicateColumnsForDataSource`、`collectInterestingColumnsForDataSource`、`collectFromPlan`、`CollectColumnStatsUsage`）、`find_best_task.go` 2 个（`splitIndexFilterConditions` L1 + `convertToIndexScan` L2 边界）、`planbuilder.go` 2 个（`genTiFlashPath` L1 + `checkAutoForceIndexLookUpPushDown` L2 边界）、`stats.go` 1 个（`getGroupNDVs` L1） |
| **本任务第 2A 部分（新增 6 + 升级 1）** | 6 | `GroupRangesByCols`（`find_best_task.go:1084`）、`PhysicalIndexScan.{getScanRowSize (:2681), initSchema (:2705), addSelectionConditionForGlobalIndex (:2779), addPushedDownSelection (:2852)}`、`getOriginalPhysicalIndexScan`（`:3469`）；并同步升级上述已计入 14 的 `convertToIndexScan`（`:2536`）为单读可用 |
| **本任务第 2B 部分（新增 2 + 转正 3）** | 2 | `PhysicalIndexScan.NeedExtraOutputCol`（`find_best_task.go:2902`）、`checkIndexLookUpPushDownSupported`（`planbuilder.go:1204`）；并同步转正 `convertToIndexScan` 双读分支、`checkAutoForceIndexLookUpPushDown`（`planbuilder.go:1236`）与 `PhysicalTableScan.appendExtraHandleCol`（`find_best_task.go:2524`） |
| **`EXC.B3`（旧 cascades 优化器例外）** | 3 | `GetPhysicalIndexScan4LogicalIndexScan`（`find_best_task.go:2969`，实查全仓仅 `cascades/implementation_rules.go:223` 调用）、`deriveStats4LogicalTableScan`（`stats.go:60`）、`deriveStats4LogicalIndexScan`（`stats.go:91`，仅 `core_init.go:69-70` 挂接 `LogicalTableScan`/`LogicalIndexScan`） |
| **`X`（仅 Go 测试导出）** | 2 | `RecursiveDeriveStats4Test`（`stats.go:51`）、`GetStats4Test`（`stats.go:56`） |
| **后续未做块（保留未移植）** | 56 | IndexMerge `D10.7`（`find_best_task.go` 13 个：`matchPropForIndexMergeAlternatives`、`removeCoveredIndexMergeTopLevelFilters`、`indexMergeTopLevelFilterCovered`、`indexMergePartialPathCoversFilter`、`convergeIndexMergeCandidate`、`getIndexMergeCandidate`、`convertToIndexMergeScan`、`convertToPartialIndexScan`、`convertToPartialTableScan`、`overwritePartialTableScanSchema`、`setIndexMergeTableScanHandleCols`、`buildIndexMergeTableScan`、`extractFiltersForIndexMerge`）；IndexJoin `D10.3`/`D09.3`（`index_join_path.go` 23 个 + `getIndexCandidateForIndexJoin` 1 个）；运维 `ADMIN CHECK` `D09.17`（`PlanBuilder.buildPhysicalIndexLookUpReader` `:1754` 与 `buildPhysicalIndexLookUpReaders` `:1914` 2 个，全仓唯一调用点为 `buildAdminCheckTable` `:2016,2018`）；`D10.8` BatchPointGet（`convertToBatchPointGet` 1 个）；`D02.2`/`D10.1` TableSample（`convertToSampleTable`、`validateTableSamplePlan` 2 个）；`D09.5` Sequence / CTE（`iterateChildPlan4LogicalSequence`、`findBestTask4LogicalCTE`、`findBestTask4LogicalCTETable`、`LogicalCTE.FindBestTask`、`LogicalCTETable.FindBestTask` 5 个）；`D10.11` 内存表（`findBestTask4LogicalMemTable`、`LogicalMemTable.FindBestTask` 2 个）；`D12.12` SHOW（`findBestTask4LogicalShow`、`findBestTask4LogicalShowDDLJobs` 2 个）；其余算子 / 谓词列收集（`{Aggregation,Join,Window}.PreparePossibleProperties` 3 个、`collectPredicateColumnsFor{Join,UnionAll}` 2 个） |
| **合计** | **181** | **98 + 14 + 6 + 2 + 3 + 2 + 56 = 181** |

（注：上表末行"后续未做块" 56 个按文件分布为：`index_join_path.go` 23 + `find_best_task.go` [IndexMerge 13 + `getIndexCandidateForIndexJoin` 1 + `convertToBatchPointGet` 1 + TableSample 2 + `iterateChildPlan4LogicalSequence` 1 + `findBestTask4LogicalCTE` 1 + `findBestTask4LogicalCTETable` 1 + `findBestTask4LogicalMemTable` 1 + `findBestTask4LogicalShow` 1 + `findBestTask4LogicalShowDDLJobs` 1 = 23] + `planbuilder.go` [`buildPhysicalIndexLookUpReader` 1 + `buildPhysicalIndexLookUpReaders` 1 = 2] + `collect_column_stats_usage.go` 2 + `logicalop/*.go` 6 = **56**。）

### 10.2 按依赖跨块一并移植/更新的函数

按宪法第九条 2（"依赖都已就绪的小块或辅助函数提前移植"）及 [D10.1 spec](../D10.1-physical-plans/spec.md)、[D10.5 spec](../D10.5-cost-model/spec.md)、[D10.9 spec](../D10.9-postprocess/spec.md) 的划分，本任务分 2A/2B/2C 顺带移植以下跨块函数，并在 tracker 中**保留并计入各自所属块**（同时登记 Cascades 例外与保留的桩）：

1. **D10.1（物理算子与 task；现总计 356 个中已指派 157 个，余 199 个中本任务覆盖 33 个 [30 新实现 + 1 `#[derive]` 标注 + 2 继续留桩] + 1 例外；另有 1 个已在 VS1-27 移植的 `PhysPlanPartInfo.MemoryUsage` 待补登 `part_funcs.tsv`）**：
   - **第 2A 部分（15 个：13 新移植 + 1 `#[derive]` 标注 + 1 桩）**：
     - `PhysicalIndexScan`（5）：`Clone`（`physical_plans.go:849`）、`ExtractCorrelatedCols`（`:878`）、`MemoryUsage`（`:889`）、`IsPartition`（`:2475`）、`IsPointGetByUniqueKey`（`:2480`）；
     - `PhysicalIndexReader`（9 = 8 + 1 桩）：`Clone`（`:363`）、`SetSchema`（`:382`）、`SetChildren`（`:397`）、`ExtractCorrelatedCols`（`:403`）、`BuildPlanTrace`（`:411`）、`AppendChildCandidate`（`:420`）、`MemoryUsage`（`:428`）、`LoadTableStats`（`:448`，保持 `blocked-by:D10.2` 边界与 `PhysicalTableReader::load_table_stats` 一致），以及 `PhysicalIndexReader.GetNetDataSize`（`plan_cost_ver1.go:243`，tracker `func_blocks.tsv` 归入 `D10.1`；因 `D12.1` 第 2B 部分 `IndexReaderExecutor` 构建器需直接调用它，依赖 2A 同步移植的 `D10.5` `getTblStats` 即可一次性实现并供 `D12.1` 复用）；
     - `PhysPlanPartInfo.Clone`（`physical_plans.go:181`，已由 `crates/planner/src/core/physical_plans.rs:142` `#[derive(Clone)]` 生成，补 `// go: PhysPlanPartInfo.Clone L2 (N1 immutable Arc<Expr> / Arc<Column> sharing)` 标注；另注 `PhysPlanPartInfo.MemoryUsage` `:194` 已在 VS1-27 于 `physical_plans.rs:162` 移植，可一并补登 `part_funcs.tsv`）；
     - 同步更新已有函数：`IsPointGetWithPKOrUniqueKeyByAutoCommit`（`crates/planner/src/core/common_plans.rs:47`，VS1-19 已登记）补齐 `PhysicalProjection` / `PhysicalIndexReader` / `PhysicalTableReader` 分支（`PhysicalIndexScan::is_point_get_by_unique_key` 就绪，`:59` 移除桩）；放通 `CopTask::convert_to_root_task_impl`（`task_base.rs:548`）的单读 `PhysicalIndexReader` 分支；`collect_plan_stats_version`（`crates/planner/src/core/physical_plans.rs:1334`，VS1-27 已登记 `D10.1`）补齐 `PhysicalKind::IndexReader`（递归 `index_plan`）与 `PhysicalKind::IndexScan` 分支。
     - **例外 `EXC.B`**：`GetPhysicalIndexReader`（`physical_plans.go:275`），经全仓 grep 核对仅被 `pkg/planner/cascades/implementation_rules.go:180` 调用，按 EXC.B 不移植。
   - **第 2B 部分（18 个 = 17 新移植 + 1 桩 + 既有函数转正）**：
     - `PhysicalIndexLookUpReader`（9 = 8 + 1 桩）：`Clone`（`:513`）、`ExtractCorrelatedCols`（`:551`）、`GetIndexNetDataSize`（`:562`）、`GetAvgTableRowSize`（`:567`）、`BuildPlanTrace`（`:572`）、`AppendChildCandidate`（`:584`）、`MemoryUsage`（`:595`）、`LoadTableStats`（`:627`，保持 `blocked-by:D10.2` 边界）、`tryPushDownLookUp`（`:633`）；
     - `PhysicalLocalIndexLookUp`（5）：`Init`（`:3194`）、`ToPB`（`:3204`）、`ExplainInfo`（`:3217`）、`MemoryUsage`（`:3222`）、`Clone`（`:3233`）；
     - 构建与下推辅助（4）：`buildPushDownIndexLookUpPlan`（`:3146`）、`AddExtraPhysTblIDColumn`（`:928`）、`buildIndexLookUpTask`（`task.go:524`）、`setTableScanToTableRowIDScan`（`task.go:606`）；
     - 同时转正 VS1-27 预留或带边界分支的既有函数：`detachRootTableScanPlan`（`physical_plans.rs:1422`，改为 N1 纯 `Arc` 递归替换 `replace_leaf_table_scan_with_local_lookup`，见 §11.3）、`CopTask.finish_index_plan`（`task.rs:142`）、`CopTask.convert_to_root_task_impl` 双读分支（`task_base.rs:542`）、`PhysicalLimit::sink_into_index_look_up`（`task.rs:506`），以及 `collect_plan_stats_version`（`physical_plans.rs:1334`）补齐 `PhysicalKind::IndexLookUpReader` 分支（照 Go `physical_plans.go:2776-2779` 仅递归 `index_plan`）。注：`resetPlanIDRecursively`（`physical_plans.rs:1408`）与 `resetRowCountAsZeroRecursively`（`:396`）已在 VS1-27 移植就绪。
2. **D10.9（后处理：`ResolveIndices` / `ToPB` / `accessObject` / `ExplainID` / `TP`）**：
   - **第 2A 部分（10 个）**：`PhysicalIndexReader.{ResolveIndices (resolve_indices.go:403), accessObject (access_object.go:490)}`、`PhysicalIndexScan.{AccessObject (access_object.go:237), ToPB (plan_to_pb.go:501), ExplainID (explain.go:53), TP (explain.go:63), haveCorCol (explain.go:137), isFullScan (explain.go:146)}`、`checkCoverIndex`（`plan_to_pb.go:351`）、`getAccessObjectFromIndexScan`（`access_object.go:475`）。
   - **第 2B 部分（2 个）**：`PhysicalIndexLookUpReader.{ResolveIndices (resolve_indices.go:430), accessObject (access_object.go:494)}`（`PhysicalLocalIndexLookUp` 继承 `BasePhysicalPlan.ResolveIndices` 递归子节点，已在 `resolve_indices.rs:327` 移植；`PhysicalLocalIndexLookUp.ToPB` 已在 tracker `func_blocks.tsv` 归入上文 `D10.1`）。
   - **继续保留为桩/例外**：`PhysicalIndexScan.{ExplainInfo (:71), ExplainNormalizedInfo (:76), OperatorInfo (:81)}`、`PhysicalIndexReader.{ExplainInfo (:349), ExplainNormalizedInfo (:354)}`、`PhysicalIndexLookUpReader.ExplainInfo (:359)` 共 6 个继续使用 `explain_stub`（`blocked-by:D10.9`，等 VS1-38 EXPLAIN 文本输出）；4 个 `CloneForPlanCache`（`plan_clone_generated.go:72,231,376,420`）属 A3 例外，由 `PhysicalPlan::instance_cacheable` 替代。
3. **D10.5（代价模型 v2）**：
   - **第 2A 部分**：`PhysicalIndexScan.GetPlanCostVer2`（`plan_cost_ver2.go:123`，调用已位于 `crates/planner/src/core/task.rs:181` 的 `core::task::get_avg_row_size` 即 Go `plan_cost_ver1.go:getAvgRowSize`）、`PhysicalIndexReader.GetPlanCostVer2`（`:219`，同调 `core::task::get_avg_row_size`）、`getTblStats`（`plan_cost_ver1.go:1284`，仅读 `PhysicalTableScan`/`PhysicalIndexScan` 的 `tbl_col_hists` 并沿 `children[0]` 向下，被 2A `PhysicalIndexReader.GetNetDataSize` 直接调用，若 `D12.1` 2A 尚未解除 `physical_plans.rs:214` 则可一并解锁）；补齐 `plan_cost_ver2.rs` 中 `get_table_info`（`:944`）、`get_task_scan_factor_ver2`（`:884`，新增 `PhysicalIndexScan.desc` 判断）、`get_number_of_ranges`（`:515`）对 `IndexScan` / `IndexReader` 的分支。
   - **第 2B 部分**：`PhysicalIndexLookUpReader.GetPlanCostVer2`（`:288`）；补齐 `get_table_info` 与 `get_number_of_ranges` 的 `IndexLookUpReader`（及 `LocalIndexLookUp` 经默认子节点递归）分支。**注意区分两个同名 `get_avg_row_size`**：`PhysicalIndexLookUpReader.GetPlanCostVer2`（`:301-302`）必须调用 **`cardinality::row_size::get_avg_row_size`**（已位于 `crates/planner/src/cardinality/row_size.rs:77`，索引侧传 `is_encoded_key = true`、回表侧传 `false`，均为 `is_for_scan = false`，统计信息经 `get_tbl_stats` 获取），不可误用 `core::task::get_avg_row_size`。
4. **D10.6（基数估算）**：
   - **第 2A 部分**：`AdjustRowCountForIndexScanByLimit`（`pkg/planner/cardinality/cross_estimation.go:83`，含 `:110` 的 `M-FMA` 位置，见 §11.8）与 `crossEstimateIndexRowCount`（`:120`）；照 `cross_estimation.rs:75` 模式，在 `ds_statistic_table.pseudo || filters.is_empty() || !enable_extended_stats || !enable_correlation_adjustment` 时返回 `(0.0, false, 0.0)`，仅在真实统计启用相关列交叉估算时于 `getMostCorrCol4Index`（`:248`）停在 `blocked-by:D10.6`。
   - **Cascades 例外（`EXC.B` / `B3`）**：全仓 grep 核对 `LogicalIndexScan` 仅由 `Convert2Gathers` / `cascades/{transformation,implementation}_rules.go` 构造与消费，其 `LogicalIndexScan.DeriveStats`（`D10.6`）连同下方 `D09.14` 中 `LogicalIndexScan` 体系的 6 个函数/方法一并按 `EXC.B3` 不移植。
5. **D09.0（`Init` 初始化）**：
   - **第 2A 部分**：`PhysicalIndexScan.Init`（`initialize.go:186`）、`PhysicalIndexReader.Init`（`:379`）；
   - **第 2B 部分**：`PhysicalIndexLookUpReader.Init`（`:254`）。
6. **D13.1（实例级计划缓存 A3 第 2 阶段扩展）**：
   - **第 2A / 2B 部分**：扩展 `is_physical_plan_cacheable`（`plan_cacheable_checker.rs:664`）支持 `IndexReader`、`IndexLookUpReader`、`IndexScan`（检查 `Index.HasCondition() && NotAlwaysValid`）与 `LocalIndexLookUp`；扩展 `rebuild_range`（`plan_cache_rebuild.rs:91`）并拆分实现 `build_range_for_index_scan`（`plan_cache_rebuild.rs:435`，对应 Go `:401`，`VS1-39` 已登记，仅留 `updateRange` 给 `IndexJoin` 保持 `D10.1` 注释桩），同时因 `GroupRangesByCols` 在 2A 落地而**顺带解除 `build_range_for_table_scan` 的 `:406` 桩**；补齐 `PhysicalPlan::instance_cacheable`、`plan_cache_heap_size.rs`（`:50` `inner_plans` / `:63` `seal_plan` / `:123` `HeapSizer` / `:443` `plan_structure_hash`）。
7. **D09.12 & D09.14（仅第 2C 部分；另含 6 个 Cascades `EXC.B3` 函数/方法）**：
   - 第 2C 部分：`pkg/planner/core/partidx/check_constraint.go` 全 7 个函数（`CheckConstraints`、`exactMatch`、`canBeImpliedFromExprs`、`implCompareExpr`、`implIsNotNull`、`AlwaysMeetConstraints`、`checkIsNullRejected`，tracker 归 `D09.12`）与 `DataSource.{CheckPartialIndexes (:720), CheckPartialIndexByFilters (:779)}`（tracker 归 `D09.14` 且已在 `VS1-23` 计入，对应 Rust `crates/planner/src/core/stats.rs:209` 与 `crates/planner/src/logicalop/logical_datasource.rs:460`）。
   - Cascades 例外 `EXC.B3`（不移植）：`LogicalIndexScan.{Init (:53), ExplainInfo (:61), BuildKeyInfo (:96), MatchIndexProp (:171), GetPKIsHandleCol (:191)}` 与同文件仅被 `MatchIndexProp` 调用的辅助函数 `matchIndicesProp`（`:198`，全仓 grep 仅 `logical_index_scan.go:182` 一处调用；共 6 个 `D09.14` 条目，仅 Cascades `Convert2Gathers` 使用）。

### 10.3 现有 Rust `blocked-by` 边界桩变更清单

| 现有桩位置 | 当前阻塞 | 处理子部分 | 变更后状态 |
|---|---|---|---|
| `crates/planner/src/core/find_best_task.rs:1043` | `D10.2` (`GroupRangesByCols`) | **2A** | **移除桩**，按 §11.8 (`M-MAPORDER`) 实现确定分组并返回 `PropMatchedNeedMergeSort` |
| `crates/planner/src/core/plan_cache_rebuild.rs:406` | `D10.2` (`GroupRangesByCols` in TableScan) | **2A** | **移除桩**，当范围变化时调用 `group_ranges_by_cols` 重建 `ts.grouped_ranges` |
| `crates/planner/src/core/plan_cache_rebuild.rs:435` | `D10.1` (`buildRangeForIndexScan`) | **2A** | **拆分移除索引扫描桩**，实现 `build_range_for_index_scan`（`updateRange` 属 IndexJoin 仍留 `blocked-by:D10.1`） |
| `crates/planner/src/core/find_best_task.rs:1875` | `D10.2` (`convertToIndexScan`) | **2A → 2B** | 2A 移除单读边界，仅在通过 `InvalidTask` 前置校验且 `!path.is_single_scan` 时暂留 `D10.2`；2B **彻底移除桩** |
| `crates/planner/src/core/task_base.rs:548` | `D10.1` (`convertToRootTaskImpl` IndexReader) | **2A** | **移除桩**，构造 `PhysicalIndexReader` |
| `crates/planner/src/core/common_plans.rs:59` | `D10.1` (`IsPointGetWithPKOrUniqueKeyByAutoCommit`) | **2A** | **移除桩**，补齐 `Projection` / `TableReader` / `IndexReader` / `IndexScan` 判断（`BatchPointGet` 等仍走 `_ => Ok(false)`） |
| `crates/planner/src/core/physical_plans.rs:214` | `D10.5` (`PhysicalTableReader::get_avg_row_size`) | **2A**（或由 D12.1 2A 先解除） | 若尚未被 D12.1 2A 解除，则随 2A `get_tbl_stats` 一并解除 |
| `crates/planner/src/core/physical_plans.rs:1422` | `D10.1` (`detachRootTableScanPlan`) | **2B** | **移除桩**，改为 N1 纯 `Arc` 递归替换叶子节点（§11.3） |
| `crates/planner/src/core/task.rs:150` | `D10.1` (`finishIndexPlan` IndexLookUp) | **2B** | **移除桩**，按 §11.4 更新 `table_plan` 统计并保留 `origin_stats.stats_version` |
| `crates/planner/src/core/task_base.rs:542` | `D10.1` (`convertToRootTaskImpl` IndexLookUp) | **2B** | **移除桩**，调用 `build_index_look_up_task` |
| `crates/planner/src/core/find_best_task.rs:1190` | `D10.2` (`appendExtraHandleCol`) | **2B** | **移除桩**，实现 `PhysicalTableScan::append_extra_handle_col` |
| `crates/planner/src/core/planbuilder.rs:621` | `D10.2` (`checkAutoForceIndexLookUpPushDown`) | **2B** | **移除桩**，实现 `check_index_lookup_push_down_supported`（仅 `INDEX_LOOKUP_PUSHDOWN` hint 自身仍等待 `D13.3`） |
| `crates/planner/src/core/stats.rs:217-218`（及 `crates/planner/src/logicalop/logical_datasource.rs:460`） | `D10.2` (`CheckPartialIndexes`) | **2C** | 2A/2B 期间保留并注明前置为 `D09.2` `build_simple_expr`；2C **彻底移除桩** |
| `crates/executor/src/builder.rs:230-245` | —（新增临时桩 / `ErrUnknownPlan`） | **2A / 2B** → D12.1 2B/2C 移除 | `PhysicalKind::IndexReader` / `PhysicalKind::IndexLookUpReader` → `("D12.1", ...)`（防止落入 `D12.0` 兜底）；`PhysicalKind::IndexScan` / `PhysicalKind::LocalIndexLookUp` → 返回 `ErrUnknownPlan`（同 `TableScan` `:233`） |

---

## 11. 内部设计与架构不变式（N1 / A3 / 协议逐字节等价）

### 11.1 `PhysicalKind` 与结构体表示

1. `crates/planner/src/base.rs:66` `PhysicalKind` 新增（2A 加 `IndexReader`、`IndexScan`；2B 加 `IndexLookUpReader`、`LocalIndexLookUp`）：
   - `PhysicalIndexScan`（嵌 `sp: PhysicalSchemaProducer`，不携带仅 EXPLAIN 统计状态用的 `used_stats_info`，与 `PhysicalTableScan` 对齐 — 见 [D10.1 spec](../D10.1-physical-plans/spec.md) §3；其余字段：`table`、`index`、`idx_cols`、`idx_col_lens`、`access_condition`、`ranges`、`columns`、`db_name`、`table_as_name`、`range_info`、`physical_table_id`、`is_partition`、`keep_order`、`desc`、`by_items`、`double_read`、`need_common_handle`、`data_source_schema: Option<Schema>`、`gen_exprs: HashMap<TableItemID, ExprRef>`（对应 Go `map[model.TableItemID]expression.Expression`，`tidb_model::TableItemID` 已移植于 `crates/model/src/types_gen.rs:8162`）、`tbl_col_hists`、`pk_is_handle_col`、`const_cols_by_cond`、`prop`、`grouped_ranges`、`group_by_col_idxs`、`not_always_valid`）。
   - `PhysicalIndexReader`（嵌 `sp: PhysicalSchemaProducer`；`index_plan: Option<PhysicalPlanRef>`、`index_plans: Vec<PhysicalPlanRef>`、`output_columns: Vec<Arc<Column>>`、`plan_part_info: Option<Arc<PhysPlanPartInfo>>`）。
   - `PushedDownLimit`（`offset: u64`、`count: u64`）与 `PhysicalIndexLookUpReader`（嵌 `sp: PhysicalSchemaProducer`；`index_look_up_push_down: bool`、`index_plan: Option<PhysicalPlanRef>`、`table_plan: Option<PhysicalPlanRef>`、`index_plans: Vec<PhysicalPlanRef>`、`index_plans_un_nature_orders: HashMap<isize, isize>`、`table_plans: Vec<PhysicalPlanRef>`、`paging: AtomicBool`（见 §11.5）、`extra_handle_col: Option<Arc<Column>>`、`pushed_limit: Option<PushedDownLimit>`、`common_handle_cols: Vec<Arc<Column>>`、`plan_part_info: Option<Arc<PhysPlanPartInfo>>`、`expected_cnt: u64`、`keep_order: bool`）。
   - `PhysicalLocalIndexLookUp`（嵌 `sp: PhysicalSchemaProducer`；`index_handle_offsets: Vec<u32>`）。
2. **`PhysicalIndexScan.add_selection_condition_for_global_index` 与 `add_pushed_down_selection`**：全局索引分支需要 `PartitionPruning`（`D09.12`），当 `is.index.global == true` 时显式返回 `Err(blocked("D09.12", ...))`（目前分区表在 `planbuilder` / `PartitionProcessorBoundary` / `convert_candidates` 已提前拦截）；非全局索引照 Go 直接原样返回 `conditions`。`add_pushed_down_selection` 计算 `root_task_conds` 不空时的选择率时，遵循 `PhysicalTableScan::add_pushed_down_selection`（`crates/planner/src/core/find_best_task.rs:2374`）：遇 `blocked_by(&e).is_some()` 直接传播错误，仅对非边界错误按 Go 回退到 `SelectionFactor`。同理，`NeedExtraOutputCol` 在 `table.partition.is_none()` 时直接返回 `false`，有分区且需要物理表 ID 时进入 Go 判断逻辑。

### 11.2 `PhysicalIndexReader` 的 Schema 共享与 `ResolveIndices`（N1 §3.3）

1. **`SetSchema`**（`physical_plans.go:382-394`）：Go 忽略入参，先展平 `p.IndexPlans = flattenListPushDownPlan(p.indexPlan)`；当 `p.indexPlan` 为 `PhysicalHashAgg` / `PhysicalStreamAgg` / `PhysicalProjection` 时取 `p.indexPlan.Schema()`，否则取 `p.IndexPlans[0].(*PhysicalIndexScan).dataSourceSchema`；并设 `p.OutputColumns = p.schema.Clone().Columns`。Rust（N1 §3.3）持有副本，`set_schema` 复制该 schema 到 `self.sp`，同时初始化 `output_columns`。
2. **`ResolveIndices`**（`resolve_indices.go:403-427`）：
   - 先调 `self.sp.resolve_indices(ctx)`；
   - **清空 `self.index_plans`**（防 `Arc` 引用计数 > 1 时触发 `make_mut_plan` 写时复制，同 `PhysicalTableReader` 写法），经 `make_mut_plan(index_plan).resolve_indices(ctx)` 解析子树，再用 `flatten_list_push_down_plan` 重建 `self.index_plans`；
   - 按 `index_plan.schema()` 解析 `self.output_columns` 每一列，失败时按 Go 逻辑调用 `col.resolve_indices_by_virtual_expr(&*ctx.eval_ctx(), index_plan.schema())` 回退；
   - **仅当 `self.index_plan` 与 `self.sp.schema` 在 Go 中同指同一 `*Schema` 对象时**（当前对应 `PhysicalKind::Projection`，以及后续的 `HashAgg` / `StreamAgg`；注意若 `index_plan` 为 `Limit` / `TopN` / `Selection` / `IndexScan`，Go 的 `SetSchema` 指向 `is.dataSourceSchema` 而非 `index_plan.Schema()`，且 `PhysicalLimit.ResolveIndices` 会改写 `limit.schema` 的 `Index` 字段，因此不可对这些算子回抄）调用 `copy_resolved_indices`。

### 11.3 `PhysicalIndexLookUpReader`、`PhysicalLocalIndexLookUp` 与 `detachRootTableScanPlan` 的纯 `Arc` 设计（N1 §3.4）

1. **下推树改写（替代 `detachRootTableScanPlan` + `buildPushDownIndexLookUpPlan`）**：
   Go `detachRootTableScanPlan`（`physical_plans.go:3247`）循环向下走到唯一的叶子 `*PhysicalTableScan` 后执行 `rootParent.SetChildren()` 清空父节点的 children 并返回 `(root, rootParent)` 两个可变别名指针；在 `buildPushDownIndexLookUpPlan`（`:3146`）中先克隆 `tablePlan`、调用 `resetPlanIDRecursively`，从 `indexPlan.Schema()` 末尾逆序找首个 `ID >= 0 || ID == ExtraHandleID` 的列作为 `indexHandleOffsets`（common handle 为空），创建 `PhysicalLocalIndexLookUp`（内部调用 `setTableScanToTableRowIDScan(tableScan)`），若 `parentOfTableScan != nil` 则再挂载回 `parentOfTableScan` 并返回外层根 `tablePlan`。
   Rust `Arc` 树不允许在保留 `tablePlan` 根的同时把父节点 `rootParent` 当游离可变指针借出，按 N1 §3.4 改为纯递归所有权改写 `replace_leaf_table_scan_with_local_lookup`：
   - 先对传入的 `table_plan` 执行 `clone_plan()?` 与 `reset_plan_id_recursively(ctx, &mut cloned_table_plan)`；
   - 计算 `index_handle_offsets`；
   - 递归沿 `children[0]` 走到底层 `PhysicalTableScan`（如遇到分叉或非单链叶子，按 Go `intest.Assert(l == 1)` / `ok` 同等断言 panic）：把叶子 `TableRowIDScan` 设置 `is_child_of_index_look_up = true` 后，构造 `PhysicalLocalIndexLookUp::init(ctx, index_plan, leaf_table_scan, offset)`；回溯时用 `make_mut_plan` 原地（或按需）把每层父节点的 `children[0]` 替换为新子树返回。**结果计划的节点 ID、拓扑、schema 与 Go 100% 一致，零别名写时复制陷阱。**
2. **扁平化、下推 `tryPushDownLookUp` 与 `Arc` 写入顺序（关键）**：
   - `PhysicalIndexLookUpReader::init` 按 N1 `Arc` 所有权约束在 Go `initialize.go:254-263` 基础上调整必须先写的顺序：先 `sp.set_schema(table_plan.schema().clone_schema())` 与 `set_stats(table_plan.stats_info())`；再运行 `try_push_down_look_up`（其内部 `reset_row_count_as_zero_recursively(&mut table_plan)` 也在扁平化之前完成）；**随后必须在 `flatten_list_push_down_plan` 之前**对 `table_plan` 调用 `set_table_scan_to_table_row_id_scan(&mut table_plan)`；最后生成 `table_plans = flatten_list_push_down_plan(&table_plan)` 与 `(index_plans, index_plans_un_nature_orders) = flatten_tree_push_down_plan(&index_plan)`。若按 Go 原顺序把 `set_table_scan_to_table_row_id_scan` 放在 `flatten_list_push_down_plan` 之后，因 `table_plans[0]` 已持有一份 `Arc`，`make_mut_plan` 会写时复制出两份 `is_child_of_index_look_up` 不一致的 `TableScan`！
   - **关键约束**：下推模式下 `index_plan` 是包含双子节点 `PhysicalLocalIndexLookUp` 的树而非单链！凡在构造后需要改写 `index_plan`（如 `ResolveIndices`、`propagate_probe_parents`、`rebuild_range`）之处，都必须先清空 `index_plans`（与 `index_plans_un_nature_orders`），改完后用 **`flatten_tree_push_down_plan`**（而非 `flatten_list_push_down_plan`）重建，以保持后序扁平列表及 `IndexPlansUnNatureOrders` 映射正确。
3. **`PhysicalIndexLookUpReader.ResolveIndices`**（`resolve_indices.go:430-458`）：
   - 按 Go 顺序（参照 `crates/planner/src/core/resolve_indices.rs:142-143` `PhysicalTableReader::resolve_indices` 与 `resolve_indices_for_virtual_column(result: &mut [Arc<Column>], schema: &Schema)` 签名）：先克隆 `schema = self.sp.schema().clone()`，调用 `resolve_indices_for_virtual_column(&mut self.sp.schema_mut().columns, &schema)?`（注意：Go 通过共享 `*Schema` 令 `tablePlan.Schema().Columns` 与 `p.schema.Columns` 同指一处，传入 `tablePlan.Schema().Columns` 作为结果槽、以 `p.schema` 查找列；Rust 读者与 `table_plan` 各持一份 schema 副本，按 D10.9 spec §5 第 1 条把解析后的 `VirtualExpr` 下标保存在读者自身的 `self.sp.schema_mut()` 中），随后先清空 `table_plans` / 解析 `table_plan` / 重建 `table_plans` / 调用 `copy_resolved_indices(&mut self.sp.schema_mut().columns, table_plan.schema())` 把 `table_plan.schema()` 的 `Index` 抄回读者 schema（即使 `table_plan` 解析出错也照样执行 `copy_resolved_indices`，匹配 Go 出错前已写入共享列的效果），再清空 `index_plans` / 解析 `index_plan` / 用 `flatten_tree_push_down_plan` 重建 `index_plans` 与 `index_plans_un_nature_orders`，最后对照 `table_plan.schema()` 解析 `extra_handle_col`、对照 `table_plans[0].schema()` 解析 `common_handle_cols`。
   - **`PhysicalLocalIndexLookUp.ResolveIndices`**：核对 Go `physical_schema_producer.go` 与 `base_physical_plan.go:173`，该算子继承默认实现，遍历并递归调用所有子节点（`index_subplan` 与克隆后的 `TableRowIDScan`）的 `ResolveIndices`；Rust 中在 `impl PhysicalPlan for PhysicalLocalIndexLookUp` 显式分派 `self.sp.resolve_indices(ctx)`（确保 `children` 经 `make_mut_plan` 就地递归解析，已在 `crates/planner/src/physicalop/base_physical_plan.rs:327` 实现）。
   - **虚拟生成列与 `convertToRootTaskImpl`（回应 D10.1 spec §7 第 1 条）**：Go `task_base.go:355-374` 仅在 `t.tablePlan != nil` 时执行 `ExpandVirtualColumn`（单读 `PhysicalIndexReader` 不经过此分支，第 2A 部分不受影响；第 2B 部分 `PhysicalIndexLookUpReader` 经过 `task_base.rs:517-532` 既有检查——无额外膨胀列时与 Go 完全一致，一旦有膨胀列即保持 `blocked-by:D10.1` 安全回退，与 D09.1/D10.1/D10.9 边界口径一致）。
4. **`sinkIntoIndexLookUp` 与 `Arc` 写时复制陷阱**（`task.go:745-799`）：
   Rust 实现 `sink_into_index_look_up` 时，在改写 `reader.table_plan`（设置新 `stats`）之前**必须先清空 `reader.table_plans`**，待更新完 `ts.set_stats(...)` 后再用 `flatten_list_push_down_plan` 重建 `table_plans`，防止因 `table_plans[0]` 仍持有 `Arc` 引用而触发 `make_mut_plan` 克隆出两份不一致的 `TableRowIDScan`。

### 11.4 `CopTask.finish_index_plan` 与 `StatsVersion`（`task.go:67-83`）

Go `CopTask.finishIndexPlan` 与 `PhysicalLimit.sinkIntoIndexLookUp`（`task.go:788-792`）将 `t.indexPlan.StatsInfo()`（或 `Limit.StatsInfo()`）赋给 `ts` 后，原地写回 `ts.StatsInfo().StatsVersion = originStats.StatsVersion`。因为在 `derive_limit_stats`（`crates/planner/src/util/misc.rs:311`）中 `StatsInfo.stats_version` 设为 `0`，如果此前曾把 `Limit`/`TopN` 下推到 `indexPlan`，Go 的指针共享写法会同时把 `indexPlan` 顶层 `Limit` 的 `StatsVersion` 改为表统计版本；不过核对 `CollectPlanStatsVersion`（`physical_plans.go:2768`）与代价模型确认：全仓只有 `PhysicalIndexScan` 和 `PhysicalTableScan` 的 `StatsVersion` 被读取（且 `CollectPlanStatsVersion` 对 `PhysicalIndexLookUpReader` 仅遍历 `indexPlan`）。Rust 采用 `Arc::make_mut` 或克隆新建 `StatsInfo`（保留 `origin_stats.stats_version`）写给 `ts`（`make_mut_plan(table_plan)`），不反向篡改共享 `StatsInfo`。

### 11.5 `PhysicalIndexLookUpReader.Paging` 的 `AtomicBool` 与 A3 `sealed` 不变式

`PhysicalIndexLookUpReader.GetPlanCostVer2`（`plan_cost_ver2.go:337-341`）中存在在 `&self` 代价计算阶段直接置 `p.Paging = true` 的副作用。为兼容 `&self` 共享读并在实例计划缓存（A3 spec §3.6）下保证并发安全：
- `paging` 使用 `std::sync::atomic::AtomicBool`（标注 `// interior: (c)`，与 `BasePhysicalPlan::cost` 同一规则）。
- **当 `self.sp.base.is_sealed()` 为真时，禁止修改 `paging`**（只读计算 0.6 折扣并按 Go 返回代价）；仅当未封存且 `enable_paging && expected_cnt > 0 && expected_cnt <= paging::THRESHOLD` 时执行 `self.paging.store(true, Ordering::Relaxed)`。对于未封存节点上的 `CostFlagRecalculate`（`EnablePaging` 从开切关后重算），照 Go 行为不将 `Paging` 重置为 `false`（详见 §12 第 2 条）。

### 11.6 内存估算（`MemoryUsage` vs A3 `HeapSizer`）

按 64 位 Go `unsafe.Sizeof` 实测（§12）：
- 常量：`EMPTY_PHYSICAL_INDEX_SCAN_SIZE = 560`、`PUSHED_DOWN_LIMIT_SIZE = 16`、`PHYSICAL_LOCAL_INDEX_LOOKUP_STRUCT_SIZE = 24`（`unsafe.Sizeof(PhysicalLocalIndexLookUp{}) - unsafe.Sizeof(physicalSchemaProducer{})` = `216 - 192`）。
- 逐条对照 Go 公式实现 `MemoryUsage`（确保 `tidb_plan_cache_max_plan_size` 准入判定与 Go 完全一致），注意保留以下已实测的 Go 实现特征：
  - `PhysicalIndexReader.MemoryUsage`（`physical_plans.go:428-445`）：第 435 行 Go 调用了 `p.indexPlan.MemoryUsage()` 但丢弃返回值，只累加了 `IndexPlans` 切片；Rust 照 Go 数值逻辑实现；
  - `PhysicalIndexLookUpReader.MemoryUsage`（`:595-624`）：不含结构体自身基大小、布尔乘 3、`IndexPlansUnNatureOrders` 用 `len * SIZE_OF_INT`、累加 `index_plan` 与 `table_plan`（不加扁平切片）；
  - `PhysicalLocalIndexLookUp.MemoryUsage`（`:3222-3230`）：使用 `len(IndexHandleOffsets) * SIZE_OF_INT32`（而非 `cap`）。
- **`plan_cache_heap_size.rs`（A3 §3.5 实际内存会计）**：与 Go `MemoryUsage` 区分开——`inner_plans` 改为返回 `Vec<&PhysicalPlanRef>`（覆盖 `TableReader.table_plan`、`IndexReader.index_plan`、`IndexLookUpReader.{index_plan, table_plan}`），使 `seal_plan` 与 `plan_structure_hash`（追加采样 `x.ranges`、`paging`、`pushed_limit` 等可变状态）完整覆盖两个子树；`HeapSizer::node` 对 4 个新算子按 Rust 真实容器容量与 `Arc` 地址去重计数，不受 Go 漏项影响。

### 11.7 `ToPB` Wire 字节精确性与 `checkCoverIndex`

1. **禁止对 `PhysicalIndexScan` 与 `PhysicalLocalIndexLookUp` 调用 `new_executor`（关键）**：
   `crates/planner/src/core/plan_to_pb.rs:46` 的 `new_executor(tp, executor_id)` 总是调用 `e.set_executor_id(executor_id)`（哪怕为空串也会写出 proto2 字段 12 `ExecutorId`）。实测 Go `PhysicalIndexScan.ToPB`（`plan_to_pb.go:529`）与 `PhysicalLocalIndexLookUp.ToPB`（`physical_plans.go:3208`）均返回不设 `ExecutorId` 的 `&tipb.Executor`（`ExecutorId == nil`；`LocalIndexLookUp` 的 `KeepOrder` 亦为 `nil`），同时因 gogoproto 对非 nullable 标量字段编码了 `FineGrainedShuffleStreamCount = 0` 与 `FineGrainedShuffleBatchSize = 0`。Rust 必须直接构造 `tipb::Executor::default()`，仅设置 `tp`、`idx_scan` / `index_lookup`、`fine_grained_shuffle_stream_count(0)`、`fine_grained_shuffle_batch_size(0)`，**不设 `executor_id` 与 `keep_order`**，确保编码字节与 Go 实测完全一致。
2. **`checkCoverIndex`**（`plan_to_pb.go:351`）：严格照 Go：`!idx.Unique` 返回 `false`；所有区间 `len(LowVal) == len(IdxCols)` 且 `LowVal` / `HighVal` 全不含 `NULL` 时返回 `true`（无论该区间是单点还是范围，见 §12 第 4 条）。

### 11.8 其他映射规则与驱动/缓存集成核对

- **`M-MAPORDER`（`GroupRangesByCols`）**：Go `find_best_task.go:1085-1102` 遍历 `map[string][]*ranger.Range` 返回结果顺序随机；Rust 使用辅助 `HashMap<Vec<u8>, usize>`（映射到目标下标）+ `Vec<Vec<Range>>` 按 key 首次在输入 `ranges` 中出现的顺序分组（`L2 [M-MAPORDER]`），保证 100% 确定且等价于 Go 的其中一种合法输出。
- **浮点 `min` 与 `NaN`（`AdjustRowCountForIndexScanByLimit`、`PhysicalIndexLookUpReader.GetPlanCostVer2`）**：必须使用 `tidb_base::gostd::math::min`（不可用 Rust 原生 `f64::min`，因原生 `NaN.min(x) == x` 而 Go `builtin min(NaN, x) == NaN`）。
- **`M-FMA` 核对**：核对 tracker `data/go_fma_sites.tsv`，`AdjustRowCountForIndexScanByLimit`（`pkg/planner/cardinality/cross_estimation.go:110`）存在 **1 处**已登记 FMA 位置：`rowCount += (path.CountAfterAccess - rowCount) * orderRatio`（在 `arm64`、`ppc64le`、`s390x`、`riscv64`、`loong64` 上融合为 `FMADDD`/`FMADD`，`amd64` `GOAMD64=v1` 不融合）。实现时必须标注 `// [M-FMA] cross_estimation.go:110` 并按目标平台提供显式 `M-FMA` helper（注意既有 `crates/planner/src/cardinality/mod.rs:34` 的 `madd` 漏了 `target_arch = "s390x"`，须使用包含 `s390x` 的完整目标集合，对齐 `crates/planner/src/core/task.rs:297`）。其余待移植文件（`find_best_task.go`、`plan_cost_ver2.go`、`plan_cost_ver1.go`、`plan_to_pb.go`、`explain.go`、`access_object.go`、`resolve_indices.go`、`physical_plans.go`、`initialize.go`、`plan_cache_rebuild.go`、`plan_cacheable_checker.go`、`planbuilder.go`、`partidx/check_constraint.go`）中本任务涉及的函数均为 **0 处** FMA 位置（`calcPagingCost` 2 处已随 VS1-27 完成）。
- **PostOptimize 与驱动（`optimizer.rs`）**：
  - `rule_eliminate_projection.go` / `rule_inject_extra_projection.go` 在 Go 中只改写 `PhysicalTableReader`（TiFlash），不改 `IndexReader` / `IndexLookUpReader`，无需改动这两个规则；
  - `optimizer.rs:772` **`propagate_probe_parents`** 与 `:799` **`has_probe_parents_below`** 须补齐 `PhysicalIndexReader`（递归 `index_plan` 并用 `flatten_list_push_down_plan` 重建）与 `PhysicalIndexLookUpReader`（递归 `index_plan` 与 `table_plan` 并分别用 `flatten_tree_push_down_plan` / `flatten_list_push_down_plan` 重建；可参照 `optimizer.rs:544` `rewrite_table_plan` 模式提炼读写 helper），沿用零拷贝捷径；
  - `optimizer.rs:1073` **`check_skip_reuse_chunk_for_overlong_type`** 与 `:1228` **`estimate_reusable_chunk_rows_for_overlong_type`** 补齐 `PhysicalKind::IndexReader | PhysicalKind::IndexLookUpReader` 分支（同 `TableReader` 逻辑）。
- **`plan_cache_rebuild.rs` 与 `plan_cacheable_checker.rs`**：
  - `is_physical_plan_cacheable` 补 `IndexReader`（检查 `index_plan`）、`IndexLookUpReader`（检查 `index_plan` 与 `table_plan`）、`IndexScan`（在 2C 前 `NotAlwaysValid` 恒为 `false`，仍写入完整 Go 逻辑）、`LocalIndexLookUp`（默认递归子节点）；
  - `PhysicalIndexScan::instance_cacheable` 检查 `self.gen_exprs.is_empty() && children_instance_cacheable(self)`；
  - `rebuild_range` 补 `IndexScan`（调用 `build_range_for_index_scan`）、`IndexReader`（类似 `TableReader` 经 `rebuild_chain_bottom` 递归链底端并重建 `index_plans`）、`IndexLookUpReader`（经 `rebuild_chain_bottom` 沿 `children[0]` 穿过 `LocalIndexLookUp` 仅到达叶子 `IndexScan`——绝不改写 `children[1]` 的克隆 `TableRowIDScan`——变化时用 `flatten_tree_push_down_plan` 重建 `index_plans` 与 `index_plans_un_nature_orders`，见 Go `rebuildRange(x.IndexPlans[0])`）。在 `build_range_for_index_scan` 与解桩后的 `build_range_for_table_scan` 中，先以 `same_ranges` 判定 `ranges` 是否变化：若与缓存相同直接返回 `Ok(None)`（不重算 `grouped_ranges`、不复制节点），保持命中同参数零拷贝（A3 spec §3.2）。

---

## 12. 在 Go v8.5.8 上实测核验的 Go 行为与边界

以下行为均已在本地 Go TiDB `v8.5.8`（`go1.25.12` arm64 单元测试或 `tidb-server --store=unistore` SQL 会话）上实测复现，并对照检查 `EXPLAIN`、wire `tipb` 字节与可观测性（慢日志 / 系统表 / warnings / `StmtCtx`）：

1. **`GroupRangesByCols` map 遍历序不确定**：
   - **实测**：输入 3 组前缀点 range，在 Go `GroupRangesByCols` 上连续执行 40 次产生 3 种分组排列（`[1,2,3]`: 27 次、`[2,3,1]`: 6 次、`[3,1,2]`: 7 次）。
   - **可见性核查**：核对 `explain.go:81-156`、live `tidb-server` `EXPLAIN` 输出、`plan_to_pb.go:501` 与 `distsql.go:311`——`EXPLAIN` 仅打印 `p.Ranges`（不打印 `GroupedRanges`），`ToPB` 不读取 `GroupedRanges`，执行器对各分组并行发出请求后按 `ByItems` 归并排序。属于 `M-MAPORDER` 覆盖范围（用户不可见），按首次出现序固定；不开 `upstream-pitfall`。
2. **`PhysicalIndexLookUpReader.GetPlanCostVer2` 的 `Paging` 粘性副作用**：
   - **实测**：对 `EnablePaging=true` 的 `SELECT ... LIMIT 10` 算一遍代价后 `Paging` 变为 `true`；随后关掉 `EnablePaging = false` 并带 `CostFlagRecalculate` 重算，代价虽按无 0.6 折扣重算，但 `p.Paging` **保持 `true` 未被清零**（`after recalc with EnablePaging=false: Paging=true`）。
   - **可见性核查**：全仓唯一带 `CostFlagRecalculate` 调用点为 `EXPLAIN ANALYZE`（`common_plans.go:936`，同一会话中 `EnablePaging` 未变）及 `costver2` 测试；A3 `sealed` 计划禁止在计算代价时写入任何缓存状态（§11.5）。该行为不会造成安全、结果错误、数据丢失或无关数据泄露（不触发宪章第一条 2 的修正例外），按宪章第九条 2 与 `CLAUDE.md` §2.6：未封存节点照 Go 保留以确保 `govec_costver2` 完全一致，并在实现 PR 时于本仓开具英语 `upstream-pitfall`（标签 `go-bug-kept`）issue 及在已知差异中记录 Go `plan_cost_ver2.go:337-341`。
3. **`PhysicalIndexReader.MemoryUsage` 与 `PhysicalIndexLookUpReader.MemoryUsage` 计量缺陷**：
   - **实测**：对同一 `PhysicalIndexReader` 计算 `MemoryUsage`：`IndexPlans` 保留时为 `6935`，置 `nil` 后（保留 `indexPlan`）降为 `3323`（验证 Go `:435` 丢失了 `indexPlan.MemoryUsage()` 返回值，仅累加扁平切片）；64 位实测基大小分别为 `PhysicalIndexScan=560`、`PhysicalIndexReader=264`、`PhysicalIndexLookUpReader=360`、`PhysicalLocalIndexLookUp=216`（与 `physicalSchemaProducer` 差值 `24`）、`PushedDownLimit=16`。
   - **可见性核查**：`MemoryUsage()` 直接控制 `tidb_plan_cache_max_plan_size` 准入开关；为与 Go 准入边界逐条一致，`PhysicalPlan::memory_usage` 严格按 Go 公式，而 A3 实际配额按 `plan_cache_heap_size.rs` 的 `HeapSizer`（独立于 `MemoryUsage()`）精确计算（已由 A3 spec 确定）。
4. **`checkCoverIndex` 对复合唯一索引非单点非 NULL 范围返回 `true`**：
   - **实测**：2 列 `UNIQUE INDEX (a, b)` 在 `full2`（两列点且非 NULL）与 `nonPoint2`（`(a, b) > (1, 1) AND (a, b) < (2, 2)` 完整两列范围但非单点）上均产出 `idxExec.Unique = true`，仅 `prefix1`（单列）与 `withNull`（含 NULL）产出 `false`。
   - **可见性核查**：直接写入 `tipb.IndexScan.unique` wire 字节，Rust 按 L1 照 Go 移植，实现 wire 100% 一致。
5. **`AdjustRowCountForIndexScanByLimit` 出现 `0/0 -> NaN`**：
   - **实测**：Go 测试传 `dsStatsInfo.RowCount=0, path.CountAfterAccess=0` 时 `selectivity` 为 `NaN`，Go 内置 `min(NaN, 0)` 返回 `NaN`（`got=NaN`）；另在 live `tidb-server --store=unistore` 对清空并 `ANALYZE` 后的表跑 `SELECT b FROM t_zero ORDER BY b LIMIT 1`，观测到 unistore 空表 `ANALYZE` 仍落入 `stats:pseudo` 而显示 `estRows = 1.00`，只有真实 0 行非 pseudo 统计才会进入 `0/0`。Rust 采用 `gostd::math::min` 与 Go 语义一致。
6. **`PhysicalIndexScan.ToPB` 与 `PhysicalLocalIndexLookUp.ToPB` 省略 `ExecutorId`**：
   - **实测**：Go 编码 hex 分别为 `08011a42...880100900100` 与 `0814880100900100c201020802`，解码后 `ExecutorIdNil=true`、`FGS=(0,0)`、`LocalIndexLookUp.KeepOrderNil=true`（而其余 6 个算子均设 `ExecutorId`）。已在 §11.7 明确禁止调用 `new_executor`。

---

## 13. 测试、`govec` 扩展与端到端验收

1. **`govec_findbest`（tracker `govec/domains/findbest.go` 与 `crates/planner/tests/govec_findbest.rs`）**：
   - **第 2A 部分**允许先保留 `!is_single_scan` 的 `D10.2` 边界并下调 `EXPECTED_BLOCKED`；**第 2B 部分**后清空 `("D10.2", 528)` 与 `("D10.2 (Go: table)", 140)` 两项。
   - **必须扩展 `fbDump` / `dump`**：不再把 `IndexReader`、`IndexLookUp`、`IndexScan`、`LocalIndexLookUp` 折叠成 `{"other": true}`，精确比较：
     - `IndexReader`：`indexPlans` 扁平链、`idx`（内层树）、`outputCols`（列名）、`keepOrder`；
     - `IndexLookUp`：`idx`、`tbl`、`indexPlans`、`tablePlans`、`indexLookUpPushDown`、`indexPlansUnNatureOrders`、`paging`、`keepOrder`、`pushedLimit`、`extraHandle`、`commonHandleCols`；
     - `IndexScan`：`table`、`index`、`ranges`、`groupedRanges`（注：Go 自身非确定序，比较时按各组集合规范化对比或由生成器/测试明确约定）、`groupByColIdxs`、`access`、`keep`、`desc`、`doubleRead`；
     - `LocalIndexLookUp`：`indexHandleOffsets` 及两个子节点。
   - 目标：`amd64` 与 `arm64` 双架构回放 **0 差异**（`D10.7` 60、`D10.8` 7、`D13.3` 2 等其他块已有边界数保持锁定）。
2. **`govec_postprocess`（tracker `govec/domains/postprocess.go` 与 `crates/planner/tests/govec_postprocess.rs`）**：
   - 清空 `("plan D10.2", 11)` 与 `("missing D10.2", 12)`；
   - 扩展 `ppDump` / `dump` 与 `node_at` / `check_reader_schemas`：比较 `IndexScan.ToPB` 与 `LocalIndexLookUp.ToPB` 完整 protobuf 字节、`PhysicalIndexReader.OutputColumns`（`ResolveIndices` 后含虚拟列回退）与 `accessObject`、`PhysicalIndexLookUpReader.{ExtraHandleCol, CommonHandleCols}`、`VirtualExpr` 下标，以及 `IndexLookUpReader` 自身 schema 与 `tablePlan` schema 逐列 `(unique_id, index)` 一致性检查。
3. **`govec_costver2` 与单测**：
   - 在 `costver2` 增补 `PhysicalIndexScan`、`PhysicalIndexReader`、`PhysicalIndexLookUpReader`（含 `PushedLimit`、`IndexLookUpPushDown`、`EnablePaging` 开/关重算）、`getTblStats` 向量与 `AdjustRowCountForIndexScanByLimit` 边界单测（含 `0/0 -> NaN` 与 `M-FMA` 舍入测试）；
   - 扩展 `costing_shared_children_copies_no_node` 与 `best_plan_is_owned_alone_after_the_memo`（`govec_findbest.rs`）涵盖索引单读、回表双读与下推计划：更新 `owned_alone` 检查覆盖 `IndexReader`（`index_plan` + `index_plans` 各节点的 `strong_count <= 2`）与 `IndexLookUpReader`（`index_plan`/`index_plans` 与 `table_plan`/`table_plans`，含 `LocalIndexLookUp` 两个子节点），验证 `owned == true`、单读全 `physicalOptimize` `clone_node == 0`，以及所有计划（含双读与下推，其在 `findBestTask` 阶段因 `TaskMap` 共享及 `build_push_down_index_look_up_plan` 的 `clone_plan` 会合法触发复制）在**`memo` 释放后的 `ResolveIndices` 与 `postOptimize` 阶段 `clone_node == 0`**。
4. **A3 实例计划缓存与内存会计单测（`plan_cache_instance_tests.rs` / `crates/session/src/tests.rs`）**：
   - 覆盖 `SELECT k FROM sbtest1 WHERE k BETWEEN ? AND ?`（`IndexReader`）、`SELECT c FROM sbtest1 WHERE k BETWEEN ? AND ?`（`IndexLookUpReader`）、`GroupRangesByCols` 参数化重建、`LocalIndexLookUp` 下推计划命中 `found_in_plan_cache`：验证二次命中参数相同零拷贝、参数改变正确重建 `Ranges`/`GroupedRanges`/`IndexPlans`、`seal_plan` 后对共享节点算代价不写 `cost` 与 `paging`、`plan_structure_hash` 前后严格不变、`HeapSizer` 误差校验。
   - **第 2C 部分**：新增针对 `IS NOT NULL` 与比较约束、`PartIdxCondNotAlwaysValid` 缓存开关与回退用例（等 `D09.2` `build_simple_expr` 解锁后跑通）。
5. **路由器与 `tools/ab` 端到端**：
   - 2A/2B 在 D12.1 第 2B/2C 合入前，集成测试验证所有索引计划稳定在 `executor/builder.rs` 停在 `blocked-by:D12.1` 并无副作用回退 PoC；
   - 与 D12.1 第 2B/2C 合并后，在真实 TiKV 集群（`tools/ab` + sysbench/随机覆盖索引与回表查询）对比 `EXPLAIN` 计划、列元数据与执行结果 0 mismatch。

---

## 14. 决定点（按宪章第九条分类）

### 14.1 需要负责人决定（宪章第九条 1）

无。

说明：本任务第 2A/2B/2C 部分全部设计严格对齐 Go v8.5.8 规划结果、wire `tipb` 编码与准入数值边界；唯一涉及跨会话共享安全的变更（`Paging` 受 `sealed` 保护不回写）直接落实已获负责人批准的 [A3 spec](../exceptions/A3-plan-cache/spec.md) §3.6 不变式；`GroupRangesByCols` 取确定分组顺序属于宪法第九条 2 明确的 `M-MAPORDER` 范畴；将部分索引拆入第 2C 部分等待 `D09.2`、并在第 2A 阶段保留 `!is_single_scan` 边界以及在 `D12.1` 2B/2C 合入前保留执行器 `D12.1` 边界桩，均属于可追溯的临时分阶段边界，不引入任何长期用户可见差异。

### 14.2 维护者已定（宪章第九条 2）

1. **三部分拆分与推进顺序（2A → 2B；2C 等 `D09.2`）**：优先实施第 2A 部分（`IndexReader` 与 `getTblStats`）以即刻解锁 D12.1 第 2B 部分，紧接着第 2B 部分（`IndexLookUpReader` + `LocalIndexLookUp`）解锁 D12.1 第 2C 部分；第 2C 部分（部分索引）显式排在 `D09.2` `build_simple_expr` 之后，避免为了一个尚未就绪的表达式解析入口阻塞核心二级索引读写主链路。
2. **第 2A 阶段 `!is_single_scan` 严格停在 `blocked-by:D10.2`**：先运行 `InvalidTask` 前置校验（如 `CopSingleReadTaskType` 拒绝双读），通过校验后若 `!path.is_single_scan` 仍可构造双读任务则返回 `blocked-by:D10.2`，绝不跳过双读候选而在仅剩的候选里选表路径，确保 2A 合入后的新栈计划与 Go 100% 一致、边界清晰可回退。
3. **随部分跨块顺带移植既定函数并归口 tracker 计数**：按 §10.2 将 `D10.1`/`D10.5`/`D10.6`/`D10.9`/`D09.0`/`D13.1` 对应的索引算子、代价、基数、后处理与计划缓存函数随 2A/2B 同步移植（各自计入原属块，同时补登第 1 部分既已移植但漏在 `part_funcs.tsv` 的 14 个 `D10.2` 函数），并将 `GetPhysicalIndexScan4LogicalIndexScan` / `deriveStats4LogicalTableScan` / `deriveStats4LogicalIndexScan`（§10.1 3 个 `D10.2`）、`GetPhysicalIndexReader`（`D10.1`）以及 `LogicalIndexScan` 体系的 6 个 `D09.14` 函数/方法 + 1 个 `D10.6` 方法按全仓引用核查归入 `EXC.B`/`B3`。
4. **`detachRootTableScanPlan` + `buildPushDownIndexLookUpPlan` 与 `PhysicalIndexLookUpReader::init` 的纯 `Arc` 设计**：按 N1 §3.4 以递归重建单一路径 `Arc` 链取代 Go 返回父指针可变别名的写法（§11.3），要求 `PhysicalIndexLookUpReader::init` 在 `flatten_list_push_down_plan` 之前执行 `set_table_scan_to_table_row_id_scan`，并要求所有修改 `table_plan` 或下推 `index_plan` 的路径先清空再重建扁平列表（`flatten_tree_push_down_plan`），杜绝写时复制导致的 `TablePlans`/`IndexPlans` 失步。
5. **`PhysicalIndexLookUpReader.Paging` 使用 `AtomicBool` 与 `sealed` 保护**：非封存节点照 Go 保留粘性设置（匹配 `costver2`，并在实现 PR 时开具 `upstream-pitfall` / `go-bug-kept` issue 记录之），封存节点依 A3 §3.6 只读不写。
6. **`PhysicalIndexScan.ToPB` 与 `PhysicalLocalIndexLookUp.ToPB` 绕过 `new_executor`**：按 Go 实测 wire 字节显式省略 `ExecutorId`（及 `LocalIndexLookUp.KeepOrder`），保留 0 值 FGS 标量字段，实现 `ToPB` 逐字节等价。
7. **`GroupRangesByCols` 确定化（`M-MAPORDER`）、`gostd::math::min`（`NaN` 对齐）与 `M-FMA`**：按输入 range 首次出现顺序分组，不改变 `EXPLAIN`、`ToPB` 与归并语义；基数调整与 `GetPlanCostVer2` 统一用 `gostd::math::min` 保持与 Go `NaN` 一致；`AdjustRowCountForIndexScanByLimit` 按 `go_fma_sites.tsv` 显式对齐 FMA 行为（含 `s390x`）。
8. **`govec` 扩展与边界清零策略**：升级 `findbest` 与 `postprocess` 的 Go 生成器/Rust 回放器消除 `other: true` 盲区，分阶段直至彻底清零 `findbest`（528 + 140）与 `postprocess`（11 + 12）的 `D10.2` 边界，并扩展 `costver2` 与 `plan_cache_instance_tests`。

---

## 本文所用 ID（第 2 部分补充）

- D09.0 / D09.2 / D09.12 / D09.14 / D09.17：初始化与入口 / 表达式改写（`build_simple_expr`） / 分区与部分索引约束（`partidx`） / 逻辑算子 / DDL 与运维（`ADMIN CHECK`）
- D10.1 / D10.2 / D10.5 / D10.6 / D10.7 / D10.8 / D10.9：物理算子与 task / 本块（FindBestTask 与访问路径） / 代价模型 v2 / 基数估算 / IndexMerge / 点查与 BatchPointGet / 后处理（`ResolveIndices` / `ToPB` / `accessObject` / EXPLAIN）
- D12.1 / D13.1 / D13.3：读取类执行器 / 实例级计划缓存（A3） / Optimizer Hints
- A3 / N1 / N13 / EXC.B / EXC.B3 / M-MAPORDER / M-FMA：计划缓存设计 / 计划树 `Arc` 表示 / 共享元数据 / Cascades 优化器例外 / map 遍历顺序确定化 / 浮点乘加

