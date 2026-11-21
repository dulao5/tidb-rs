# D10.5 代价模型（spec，第一条纵向切片部分）

状态：评审中（2026-11-21）。块：D10.5（代价模型），只做第一条纵向切片需要的部分。crate `tidb-planner`。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3；[D10.1 物理算子 spec](../D10.1-physical-plans/spec.md)；[N1 计划表示](../exceptions/N1-plan-representation/spec.md)；例外清单 [candidates.md](../exceptions/candidates.md) B5（代价模型 v1）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/`。

## 1. 范围

优化器在多个候选物理计划中按代价选择。第一条纵向切片的 SELECT 只会产生 TableReader、TableScan、Selection、Projection、Limit、TopN、Sort、TableDual 和点查，本任务移植这些算子在代价模型 v2（默认的 `tidb_cost_model_version=2`）下的代价计算：

| 内容 | Go 文件 | Rust 文件 |
|---|---|---|
| 入口 `GetPlanCost` / `getPlanCost`；Selection、Projection、TableScan、TableReader、Sort、TopN、点查的 `GetPlanCostVer2`；公式 `scanCostVer2`、`netCostVer2`、`filterCostVer2`、`orderCostVer2`、`doubleReadCostVer2` 和 `numFunctions`；全表扫描惩罚 `getTableScanPenalty`；范围个数 `getNumberOfRanges`；因子表 `defaultVer2Factors` 和按 task 类型选因子的 5 个函数，以及它们用到的 `isTemporaryTable`、`getTableInfo` | `planner/core/plan_cost_ver2.go` | `core/plan_cost_ver2.rs` |
| v2 用到的三个公共函数：`hasCostFlag`、`getCardinality`（估算行数；`CostFlagUseTrueCardinality` 时取运行时的实际行数）、`getOperatorActRows`，以及模型版本常量 | `planner/core/plan_cost_ver1.go` | `core/plan_cost_ver1.rs` |

Limit、TableDual、NominalSort 在 Go 中没有自己的 `GetPlanCostVer2`，用 `BasePhysicalPlan` 的默认实现（子节点代价之和，叶子为 0），已由 VS1-19 移植。

### 1.1 converge（`--part specs/VS1-vertical-slice/functions.tsv --item VS1-30`）

清单 32 个函数：**matched 20/20**（missing 0、extra 0），12 个标为不移植（§2）。另外移植了清单外的 7 个函数（§2）。`--fma-sites`：本部分移植的函数中没有登记的 FMA 位置（§5）。

## 2. 函数清单的增减

**不移植**（`functions.tsv` 的 `not_ported` 列写明原因，tracker 的 govec 豁免表同样登记）：

| 函数 | 原因 |
|---|---|
| `plan_cost_detail.go` 的 11 个函数（`setPointGetPlanCostDetail`、`setPhysicalTableReaderCostDetail`、`setPhysicalTableOrIndexScanCostDetail` 等写代价明细的函数，以及 hash join 明细结构的 `desc` 方法） | 它们只被代价模型 v1 的 `GetPlanCostVer1` / `GetCost` 调用（`plan_cost_ver1.go` 第 236、318、419、444、911、1121、1169 行）。代价模型 v1 是例外 B5（非默认路径，不移植），移植了也没有调用者 |
| `BatchPointGetPlan.GetPlanCostVer2` | 批量点查（D10.8）不在第一条纵向切片中，Rust 没有这个类型 |

**增加**（清单外，v2 的代码直接调用，一起移植）：

| 函数 | 所属 | 为什么 |
|---|---|---|
| `GetPlanCost` | D10.5 | `getPlanCost` 的导出包装，FindBestTask 和 EXPLAIN 调用 |
| `numFunctions`、`isTemporaryTable`、`getTableInfo` | D10.5 | `filterCostVer2` 和选因子的函数调用 |
| `hasCostFlag`、`getCardinality`、`getOperatorActRows` | EXC.B（tracker 按文件把整个 `plan_cost_ver1.go` 划进例外 B） | 虽然在 v1 的文件里，却是 v1 和 v2 共用的函数，v2 的每个算子都调用。converge 把它们列为"属于 EXC.B"（info，不是错误） |

**其余**：块内其余函数（index reader、index lookup / merge、join、聚合、union、MPP、CTE 的代价，`aggCostVer2`、`hashBuildCostVer2` 等它们专用的公式，以及只有 Go 测试调用的 `costVer2Factors.tolist`）属于切片外的算子，Rust 中还没有这些算子类型，留到算子移植时一起做（converge 报为 `blocked-by:D10.5`，govec 豁免表登记）。

## 3. 承载方式

| Go | Rust | 说明 |
|---|---|---|
| `p.GetPlanCostVer2(taskType, option)`，通过 `p.SCtx()` 读会话变量 | `PhysicalPlan::get_plan_cost_ver2(&self, ctx, task_type, option)`；`get_plan_cost_ver1` 同样增加 `ctx` | 节点不保存上下文（`M-CTX`），上下文作为参数传入。两个 trait 方法此前没有调用者（只有 `util_test.rs`），本任务一起改 |
| 各算子的 `GetPlanCostVer2` 方法 | `core/plan_cost_ver2.rs` 中的固有方法；算子的 `impl PhysicalPlan` 中用一行分派转到它（`glue: dispatch`，与 `ExplainInfo` 的写法相同） | 一型一译：Go 方法只在 `plan_cost_ver2.rs` 中翻译一次 |
| 代价缓存 `PlanCostInit` / `PlanCostVer2`，在算子上原地写 | 嵌入的 `BasePhysicalPlan` 的 `Mutex<PlanCostCache>`，经 `&self` 写（VS1-26 的说明，#223 评审） | FindBestTask 中一个子计划被多个候选和 `taskMap` 共享，Go 在共享节点上只算一次。Go 在乘上算子的代价系数之前先置 `PlanCostInit`，中间没有人读，Rust 一次写入最终值 |
| `PointGetPlan` 自己的 `planCostInit` / `planCost` / `planCostVer2` 字段 | 删除，改用嵌入的 `BasePhysicalPlan` 的缓存 | trait 方法是 `&self`，原来的普通字段写不了；这三个字段此前没有使用者 |
| `costVer2Factors` 结构和包变量 `defaultVer2Factors` | `CostVer2Factors`（字段改为 snake_case）和 `LazyLock` 静态量 `defaultVer2Factors` | 选因子的函数返回 `&'static CostVer2Factor` |
| Go 内置 `min` / `max`（浮点） | `gostd::math::min` / `max` | 与 `math.Min` / `Max` 相同的 NaN 和 ±0 规则；Rust 的 `f64::max` 遇到 NaN 返回另一个数 |
| `math.Log2` | `gostd::math::log2` | Go 自己的实现（arm64 上 `log10.go:36` 融合成 FMA；amd64 上 `Log` 是汇编），不用 Rust 的 `log2` |
| `fmt.Sprintf` 写代价公式（trace） | `gofmt::sprintf`；因子按 `%v` 调它的 `String()` | 公式字符串与 Go 逐字节相同（govec 比较） |
| `uint64` 的 `p.Count+p.Offset`（TopN） | `wrapping_add` | Go 溢出回绕 |

## 4. 边界与不携带的分支

- **代价模型 v1**：`tidb_cost_model_version=1` 时 Go 调 `p.GetPlanCostVer1`。v1 是例外 B5，不移植；`getPlanCost` 在这里返回 `blocked-by:EXC.B`（错误 1235，路由据此回退），不会悄悄用 `BasePhysicalPlan` 默认实现的"子节点之和"代替。单元测试 `cost_model_v1_is_a_boundary`。系统变量本身仍可设置为 1（例外清单 B5 说"设置时报错"，那是会话变量层的事，不在本任务）。
- **`GenPlanCostTrace` 钩子**：Go 的包变量，TiDB 中只有测试设置它，生产代码里恒为 nil，`getPlanCost` 从不调用。不携带。
- **类型分支中未移植的算子**：`getNumberOfRanges`、`getTaskScanFactorVer2`、`getTaskNetFactorVer2`、`getTableInfo` 的 type switch 中有 IndexReader、IndexLookUp、IndexMerge、IndexScan、ExchangeReceiver / ExchangeSender 的分支。这些算子在 Rust 中还没有类型，不可能出现这样的节点，所以这些分支不写（每处注释写明），等算子移植时补上。这不是"静默"：没有一条 Go 会走的路径在 Rust 中变成了别的结果。
- **TableScan 的向量索引分支**（`p.AnnIndexExtra != nil`）：TableScan 不携带 `AnnIndexExtra`（D10.1 spec §3，向量索引不在切片中），恒为 Go 的 nil。
- **`CostFlagUseTrueCardinality`**：`getCardinality` 读语句的 `RuntimeStatsColl`；它为 nil 时 Go 在 `getOperatorActRows` 中解引用 nil，Rust 同样 panic（在取探测次数之前，结果相同：没有 probe parent 的算子探测次数为 1）。只有 EXPLAIN ANALYZE 的 `true_card_cost` 用到，单元测试 `cardinality_estimated_and_true`。

## 5. 浮点乘加（`M-FMA`）

- `plan_cost_ver2.go` 中没有登记的 FMA 位置：代价都经 `SumCostVer2` 相加，同一个表达式里没有"乘积再加"。本部分移植的 `plan_cost_ver1.go` 的三个函数中也没有。
- 但 v2 的公式调用 `math.Log2`（`scanCostVer2`、`orderCostVer2`），它在 arm64 上融合（`log10.go:36`），amd64 上 `Log` 是汇编，两边最后几位可能不同。Rust 用 `gostd::math::log2`（已按目标平台移植），每个目标回放自己的向量。两个目标的向量中有差别的记录登记在 tracker 的 `targetdiffs_test.go`（§7）。
- **`converge.py --fma-sites` 只检查已移植的函数**（#286）：此前脚本检查 Rust 文件 `go-file:` 中每个 Go 文件的全部登记位置，`plan_cost_ver1.rs`（只移植了 3 个公共函数）会报出 v1 公式中的 50 个位置，`exhaust_physical_plans.rs` 会报出未移植的 index join 中的 3 个位置。现在只检查本文件中有 `go:` 或 `go-part:` 标注的函数里的位置；部分移植的函数（`go-part:`、L2）照样检查，漏写 `[M-FMA]` 注释最容易出在这里（#285 评审）。`go:` 标注缺 `M-FMA` 时报错，只有 `go-part:` 时检查 `go-part:` 标注。
- **代价模型 v1 的 50 个 FMA 位置**（#285 评审要求列在这里）。v1 是例外 B5，目前不移植；如果将来撤销 B5，移植这些函数时按 `M-FMA` 处理：每个位置写 `// [M-FMA] plan_cost_ver1.go:<行>`，按目标平台分别写 `f64::mul_add` 或分开的乘和加，写法参考 `calcPagingCost`（`core/task.rs` 的 `paging_madd` / `paging_msub`：gc 先算哪个乘积、把哪个乘积融合进去，要看汇编）。"融合的目标"一列中 "arm64 等" 指 arm64、ppc64le、s390x、riscv64、loong64；amd64 的基线 GOAMD64=v1 都不融合，v3 只作参考。

| 行 | 函数 | 融合的目标 | 源码 |
|--:|---|---|---|
| 86 | `PhysicalProjection.GetCost` | arm64 等 | `return cpuCost + concurrencyCost` |
| 129 | `PhysicalIndexLookUpReader.GetCost` | arm64 等 | `cost += (numTblWorkers + 1) * sessVars.GetConcurrencyFactor()` |
| 181 | `PhysicalIndexLookUpReader.GetPlanCostVer1` | arm64 等 | `p.PlanCost += getCardinality(p.indexPlan, costFlag) * ts.getScanRowSize() * p.SCtx().GetSessionVars().GetScanFactor(ts.Table)` |
| 186 | `PhysicalIndexLookUpReader.GetPlanCostVer1` | arm64 等 | `p.PlanCost += getCardinality(p.indexPlan, costFlag) * rowSize * netFactor` |
| 193 | `PhysicalIndexLookUpReader.GetPlanCostVer1` | arm64 等 | `p.PlanCost += getCardinality(p.tablePlan, costFlag) * tblRowSize * netFactor` |
| 228 | `PhysicalIndexReader.GetPlanCostVer1` | arm64 等 | `p.PlanCost += rowCount * rowSize * netFactor` |
| 273 | `PhysicalTableReader.GetPlanCostVer1` | arm64 等 | `p.PlanCost += rowCount * rowSize * netFactor` |
| 306 | `PhysicalTableReader.GetPlanCostVer1` | arm64 等 | `p.PlanCost += getCardinality(p.tablePlan, costFlag) * rowSize * netFactor` |
| 349 | `PhysicalIndexMergeReader.GetPlanCostVer1` | arm64 等 | `p.PlanCost += getCardinality(tblScan, costFlag) * rowSize * netFactor // net I/O cost` |
| 368 | `PhysicalIndexMergeReader.GetPlanCostVer1` | arm64 等 | `p.PlanCost += getCardinality(partialScan, costFlag) * rowSize * netFactor // net I/O cost` |
| 459 | `PhysicalIndexJoin.GetCost` | amd64 v3、arm64 等 | `cpuCost += sessVars.GetCPUFactor() * outerCnt` |
| 468 | `PhysicalIndexJoin.GetCost` | amd64 v3、arm64 等 | `innerCPUCost += outerCnt * (math.Log2(batchSize) + 1) * sessVars.GetCPUFactor()` |
| 473 | `PhysicalIndexJoin.GetCost` | amd64 v3、arm64 等 | `innerCPUCost += outerCnt * cost.DistinctFactor * sessVars.GetCPUFactor()` |
| 476 | `PhysicalIndexJoin.GetCost` | amd64 v3、arm64 等 | `innerCPUCost += outerCnt * cost.DistinctFactor * innerCnt * sessVars.GetCPUFactor()` |
| 494 | `PhysicalIndexJoin.GetCost` | arm64 等 | `cpuCost += probeCost + (innerConcurrency+1.0)*sessVars.GetConcurrencyFactor()` |
| 508 | `PhysicalIndexJoin.GetCost` | arm64 等 | `return outerCost + innerPlanCost + cpuCost + memoryCost` |
| 545 | `PhysicalIndexHashJoin.GetCost` | amd64 v3、arm64 等 | `cpuCost += sessVars.GetCPUFactor() * outerCnt` |
| 554 | `PhysicalIndexHashJoin.GetCost` | amd64 v3、arm64 等 | `innerCPUCost += outerCnt * (math.Log2(batchSize) + 1) * sessVars.GetCPUFactor()` |
| 559 | `PhysicalIndexHashJoin.GetCost` | amd64 v3、arm64 等 | `innerCPUCost += outerCnt * cost.DistinctFactor * sessVars.GetCPUFactor()` |
| 585 | `PhysicalIndexHashJoin.GetCost` | arm64 等 | `probeCost = (numPairs - batchSize*innerCnt*(concurrency-1)) * sessVars.GetCPUFactor()` |
| 591 | `PhysicalIndexHashJoin.GetCost` | arm64 等 | `cpuCost += (concurrency + 1.0) * sessVars.GetConcurrencyFactor()` |
| 597 | `PhysicalIndexHashJoin.GetCost` | amd64 v3、arm64 等 | `return outerCost + innerPlanCost + cpuCost + memoryCost` |
| 634 | `PhysicalIndexMergeJoin.GetCost` | amd64 v3、arm64 等 | `cpuCost += sessVars.GetCPUFactor() * outerCnt` |
| 649 | `PhysicalIndexMergeJoin.GetCost` | amd64 v3、arm64 等 | `innerCPUCost += outerCnt * (sortFactor + 1.0) * sessVars.GetCPUFactor()` |
| 654 | `PhysicalIndexMergeJoin.GetCost` | arm64 等 | `innerCPUCost += outerCnt * cost.DistinctFactor * sessVars.GetCPUFactor()` |
| 677 | `PhysicalIndexMergeJoin.GetCost` | arm64 等 | `probeCost = (numPairs - batchSize*avgProbeCnt*(innerConcurrency-1)) * sessVars.GetCPUFactor()` |
| 681 | `PhysicalIndexMergeJoin.GetCost` | arm64 等 | `cpuCost += probeCost + (innerConcurrency+1.0)*sessVars.GetConcurrencyFactor()` |
| 688 | `PhysicalIndexMergeJoin.GetCost` | amd64 v3、arm64 等 | `return outerCost + innerPlanCost + cpuCost + memoryCost` |
| 722 | `PhysicalApply.GetCost` | amd64 v3、arm64 等 | `cpuCost += lCount * sessVars.GetCPUFactor()` |
| 726 | `PhysicalApply.GetCost` | arm64 等 | `cpuCost += lCount * rCount * sessVars.GetCPUFactor()` |
| 732 | `PhysicalApply.GetCost` | amd64 v3、arm64 等 | `cpuCost += lCount * rCount * sessVars.GetCPUFactor() * 0.5` |
| 734 | `PhysicalApply.GetCost` | arm64 等 | `cpuCost += lCount * rCount * sessVars.GetCPUFactor()` |
| 741 | `PhysicalApply.GetCost` | amd64 v3、arm64 等 | `return cpuCost + lCost + lCount*rCost` |
| 802 | `PhysicalMergeJoin.GetCost` | amd64 v3、arm64 等 | `cpuCost += outerCnt * sessVars.GetCPUFactor()` |
| 809 | `PhysicalMergeJoin.GetCost` | arm64 等 | `return cpuCost + memoryCost` |
| 888 | `PhysicalHashJoin.GetCost` | amd64 v3、arm64 等 | `probeCost += probeCnt * cpuFactor` |
| 890 | `PhysicalHashJoin.GetCost` | amd64 v3、arm64 等 | `diskCost += probeDiskCost` |
| 893 | `PhysicalHashJoin.GetCost` | amd64 v3、arm64 等 | `cpuCost += probeCost + float64(p.Concurrency+1)*concurrencyFactor` |
| 898 | `PhysicalHashJoin.GetCost` | amd64 v3、arm64 等 | `cpuCost += buildCnt * cpuFactor` |
| 902 | `PhysicalHashJoin.GetCost` | amd64 v3、arm64 等 | `diskCost += buildCnt * diskFactor * rowSize` |
| 951 | `PhysicalStreamAgg.GetCost` | amd64 v3、arm64 等 | `return cpuCost + memoryCost` |
| 983 | `PhysicalHashAgg.GetCost` | arm64 等 | `cpuCost += (con + 1) * sessVars.GetConcurrencyFactor()` |
| 991 | `PhysicalHashAgg.GetCost` | amd64 v3、arm64 等 | `memoryCost += inputRows * cost.DistinctFactor * sessVars.GetMemoryFactor() * float64(numDistinctFunc)` |
| 1040 | `PhysicalSort.GetCost` | arm64 等 | `return cpuCost + memoryCost + diskCost` |
| 1079 | `PhysicalTopN.GetCost` | amd64 v3、arm64 等 | `return cpuCost + memoryCost` |
| 1117 | `BatchPointGetPlan.GetCost` | amd64 v3、arm64 等 | `cost += rowCount * rowSize * networkFactor` |
| 1118 | `BatchPointGetPlan.GetCost` | amd64 v3、arm64 等 | `cost += rowCount * seekFactor` |
| 1165 | `PointGetPlan.GetCost` | amd64 v3、arm64 等 | `cost += rowSize * networkFactor` |
| 1211 | `PhysicalUnionAll.GetPlanCostVer1` | arm64 等 | `p.PlanCost = childMaxCost + float64(1+len(p.Children()))*p.SCtx().GetSessionVars().GetConcurrencyFactor()` |
| 1228 | `PhysicalExchangeReceiver.GetPlanCostVer1` | arm64 等 | `p.PlanCost += getCardinality(p.Children()[0], costFlag) * p.SCtx().GetSessionVars().GetNetworkFactor(nil)` |

## 6. 与 Go 的已知差异

1. **`PointGetPlan` 的 `accessCols == nil`**：Go 用 nil 区分"快速路径建的点查"（代价为 0）。Rust 的 `Vec` 没有 nil，空列表当作 nil。FindBestTask 设置的是表或索引的列，不会是空列表（L2）。
2. **`CostVer2.GetCost` 遇到 NaN**：Go 的 `max(c.cost, 0)` 保留 NaN；VS1-19 移植时用了 `f64::max`，NaN 变成 0。govec 的公式记录（行宽为 +Inf、因子为 0）发现了这一点，本任务改为 `gostd::math::max`。
3. 其余与 Go 相同：govec 比较了每个节点的缓存（是否已算、代价、trace 公式、各因子的代价），以及改变代价系数后再算一次时缓存是否生效。

## 7. 验证

- **govec**（tracker `govec/domains/costver2.go`，2,900 条记录，amd64 / arm64 两份；回放 `crates/planner/tests/govec_costver2.rs`）：
  - `plan` 2,000 条：一棵计划树，TableScan 之上可有 cop 端的 Selection / TopN / Limit / Projection，外面可有 TableReader，再往上是 root 端的 Selection / Projection / Sort / TopN / Limit；十分之一是单独的点查。随机组合：存储类型（TiKV / TiFlash）、降序、临时表、IndexLookUp 的子节点、`rangeInfo`、整数主键（含 unsigned）和各种范围（全范围、非空全范围、点范围）、分区裁剪条件、表的列类型（行宽）、统计信息（pseudo、修改行数、分析过的行数、是否放进 `StatsInfo.HistColl`）、各节点的行数（含 0、小于 1、大于 1e7）、表达式类型（函数 / 列 / 常量）、TopN 的 count 与 offset（含溢出）、task 类型、代价标志（`CostFlagRecalculate`、`CostFlagTrace`），以及会话变量：7 个算子代价系数、projection 并发度、distsql 扫描并发度、`tidb_enforce_mpp`、内存限额与 `tidb_enable_tmp_storage_on_oom`（Sort 落盘）、`tidb_opt_prefer_range_scan`、`USE/FORCE INDEX`。输出 `GetPlanCost` 的结果、`getNumberOfRanges`、每个节点的缓存，以及把代价系数加倍后再算一次的结果（不带 `CostFlagRecalculate` 时缓存生效，结果不变）。
  - `factor` 400 条：5 个选因子的函数 × task 类型 × 计划形状（TableScan、TableReader、其上的 Selection、点查；临时表、降序、TiFlash）。
  - `formula` 500 条：`scanCostVer2`、`netCostVer2`、`filterCostVer2`、`orderCostVer2`、`doubleReadCostVer2`，含 0、+Inf 和 trace。
  - **arm64：2,900 条全部与 Go 相同**。amd64：见 tracker PR（CI 产物），两个目标之间有差别的记录登记在 `targetdiffs_test.go`。
- **代价计算不复制节点**（`taskMap` 试点 #287 的要求）：所有代价函数只经 `&self` 写嵌入的 `BasePhysicalPlan` 的缓存，代价代码中没有 `make_mut_plan`。`physicalop` 增加按线程计数的 `clone_node_calls()`（宏生成的每个 `clone_node` 计一次，只是诊断用的计数器）；测试 `costing_shared_children_copies_no_node`：Sort 和 TopN 两个候选共享同一个 TableReader（其下是 Selection 和 TableScan），分别算代价、再带 `CostFlagRecalculate` 重算一次，`clone_node` 调用 0 次，共享节点的引用计数不变、缓存都已原地写入；同一测试中对共享节点做一次 `make_mut_plan`，计数加 1，证明计数器有效。
- **单元测试**：`cost_model_v1_is_a_boundary`（v1 返回 `blocked-by:EXC.B`，不写缓存）、`cardinality_estimated_and_true`（估算行数为 0 时取 1；`CostFlagUseTrueCardinality` 取运行时统计的实际行数）；`util_test.rs` 中原有的两个缓存测试（共享子节点原地写缓存、不复制）改为传入上下文。

## 8. 待定问题

无。

## 本文所用 ID

- D10.1：物理算子与 task；D10.5：代价模型（本块）；D10.8：点查与批量点查；D10.9：EXPLAIN 文本、`ResolveIndices`、`ToPB`
- EXC.B：例外 B，废弃或非默认路径（cascades、代价模型 v1、静态分区）；B5 是其中的代价模型 v1
- VS1-19：规划器基础（`BasePhysicalPlan`、`CostVer2` 等）
- VS1-26：`taskMap` 试点（代价缓存经 `&self` 写在共享节点上）
- VS1-30：本任务，代价模型 v2（第一条纵向切片的算子）
- `M-CTX`、`M-LOCK`、`M-FMA`、`M-F2I`、`M-GONAME`：映射规则（`docs/porting/mappings.md`）
- #223、#285、#286：tidb-rs 的 PR / issue（规划器基础的评审、物理算子、converge 的 FMA 检查范围）
