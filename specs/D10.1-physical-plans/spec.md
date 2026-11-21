# D10.1 物理算子与 task、D10.3 物理实现枚举（spec，VS1-27）

状态：评审中（2026-11-21）。块：D10.1（物理算子定义与 task）、D10.3（物理实现枚举），只做第一条纵向切片需要的部分。crate `tidb-planner`。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3、§11；[N1 计划表示](../exceptions/N1-plan-representation/spec.md)（§3.4 物理计划是 `Arc` 树，§4 摘出分派）；[D09.14 逻辑算子](../D09.14-logical-operators/spec.md)。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/`。

## 1. 范围

第一条纵向切片的 SELECT 只会产生这几种物理算子：TableReader、TableScan、Selection、Projection、Limit、TopN、Sort（以及排序属性用的 NominalSort）、TableDual，再加上点查（D10.8，已移植）。本任务移植：

| 内容 | Go 文件 | Rust 文件 |
|---|---|---|
| 上面这些算子的结构和方法（克隆、关联列、内存估算等），以及它们用到的文件内辅助函数 | `planner/core/physical_plans.go` | `core/physical_plans.rs` |
| 三种 task：`RootTask`、`CopTask`、`MppTask`，`ConvertToRootTask` | `planner/core/task_base.go` | `core/task_base.rs` |
| 这些算子的 `Attach2Task`（把算子挂到子 task 上，必要时把 cop task 收成 root task），以及 cop task 的辅助函数 | `planner/core/task.go` | `core/task.rs` |
| Selection / Projection / Sort / TopN / Limit 的 `exhaustPhysicalPlans`：在给定的物理属性下列出所有候选物理算子，以及每个候选对子节点要求的属性 | `planner/core/exhaust_physical_plans.go`，`operator/logicalop/*.go` 中的 `ExhaustPhysicalPlans` | `core/exhaust_physical_plans.rs`，`logicalop/*.rs` |
| `tidb_decode_key` 等内置函数背后的 key 编解码（全部是桩，见 §4） | `planner/core/expression_codec_fn.go` | `core/expression_codec_fn.rs` |

不在本任务：join、聚合、窗口、union、MPP、IndexReader / IndexLookUp / IndexMerge 的 task 处理（VS1 plan §3 D10.1 / D10.3 行）。`FindBestTask`（怎样在候选中选、怎样给 DataSource 建 cop task）是 D10.2（VS1-28）；代价公式是 D10.5（VS1-30）；EXPLAIN 文本、`ResolveIndices`、`ToPB` 是 D10.9（VS1-31）。

### 1.1 converge（`--part specs/VS1-vertical-slice/functions.tsv --item VS1-27`）

| 块 | 已移植 | 桩 | 例外 | 合计 |
|---|--:|--:|--:|--:|
| D10.1 物理算子与 task | 93 | 16 | 1 | 110 |
| D10.3 物理实现枚举 | 22 | 0 | 0 | 22 |

132 个函数都有交代（missing 0、extra 0）。L2 的函数和理由见 §5。

## 2. 函数清单的增减

**增加**（其他条目或其他块的函数，本任务直接用到，一起移植并计入各自的块）：

| 函数 | 所属 | 为什么 |
|---|---|---|
| 8 个算子的 `Init`、`NominalSort.Init`、`PhysicalTableReader.adjustReadReqType`，`flattenListPushDownPlan` 等 4 个展开函数（`initialize.go`） | D09.0（VS1-10） | VS1-10 把它们留成桩，等算子类型（本任务）。现在 VS1-10 的部分是 86/97 |
| `enforceProperty`（`plan.go`） | D09.0（VS1-10） | 同上：在 task 上补一个 Sort，要 `base.Task` 和 `PhysicalSort` |
| `BasePhysicalPlan.Attach2Task` | D10.1（VS1-19） | 同上 |
| `GetPropByOrderByItems`、`GetPropByOrderByItemsContainScalarFunc`（`find_best_task.go`） | D10.2（VS1-28） | 两个纯函数，TopN / Sort 的枚举和 TopN 的 `Attach2Task` 调用 |
| `LogicalProjection.TryToGetChildProp`、`tryTransformSortItemPtrs` | D09.14（VS1-23） | VS1-23 留成了桩；Projection 的枚举要用它们把排序属性换成子节点的列 |
| `ExtractCorrelatedCols4PhysicalPlan`（`util/coreusage/correlated_misc.go`） | D10.10 | TableReader 的 `ExtractCorrelatedCols` 调用 |
| `setMppOrBatchCopForTableScan`、`PhysPlanPartInfo.MemoryUsage` | D10.1 | TableReader 的 `Init`、`MemoryUsage` 调用 |
| `isPhysicalPlanCacheable` 的 TableReader / TableScan 分支（`plan_cacheable_checker.go`） | D13.1（VS1-15） | 原来等 reader 类型的边界，现在按 Go 移植；TableScan 的 `isFullScan`（D10.9）只在 index merge 下才读，新栈没有 index merge，仍是桩 |

**例外**：`GetPhysicalTableReader` 只给 cascades 优化器的 `TiKVSingleGather` 用（`planner/cascades/implementation_rules.go`），例外 B。

## 3. 承载方式

| Go | Rust | 说明 |
|---|---|---|
| 物理算子嵌入 `BasePhysicalPlan` 或 `physicalSchemaProducer` | 字段 `base` 或 `sp`；`physical_plan_boilerplate!(Kind)` / `physical_plan_boilerplate!(Kind, sp)` 生成每个算子的访问器（`sp` 形式同时生成 `schema` / `set_schema`） | `PhysicalKind` 增加 `NominalSort`：Go 中 NominalSort 的 TP 也是 `Sort`，但它是另一个类型 |
| `X{...}.Init(ctx, stats, offset, props...)` | `X { .., ..Default::default() }.init(ctx, stats, offset, props)`，返回算子本身，进树时放进 `Arc` | 上下文是参数（`M-CTX`），plan id 在 Go 分配的位置分配 |
| `base.Task` 接口，`RootTask` / `CopTask` / `MppTask` 指针 | `enum Task { Root, Cop, Mpp }`，按值 | `copy()` 是 Go 的 `Copy()`（注意 `RootTask.Copy` 不复制 `isEmpty`）；`Clone` 是"同一个 task"（Go 直接返回 `tasks[0]` 的地方）。task 里的计划是 `Arc`，拷贝 task 时共享，与 Go 共享指针相同 |
| `base.InvalidTask` | `Task::invalid_task()` | 空的 RootTask |
| `p.Attach2Task(tasks...)` | 分派函数 `core::task::attach2_task(ctx, p, &tasks)`，按 `p.kind()` 分到各算子（`M-IFACE`） | `p` 按值传 `Arc`：`exhaustPhysicalPlans` 每次都新建候选，引用计数为 1，`make_mut_plan` 原地写，与 Go 原地写 `p` 对应 |
| `attachPlan2Task(p, t)` | 子计划从 task 中移出放到 `p` 下 | 子计划的引用计数不变（单元测试 `attach_plan2_task_moves_the_child`） |
| `ConvertToRootTask`、`Attach2Task` 不返回错误 | 返回 `Result` | 只有边界桩（§4）会出错：错误 1235，路由据此回退 |
| `lp.ExhaustPhysicalPlans(prop)` | `LogicalPlan` 的 trait 方法 + 分派函数 `base::exhaust_physical_plans(ctx, p, prop)`（N1 §4 摘出分派） | 返回 `(Vec<PhysicalPlanRef>, bool)`，Go 的 nil 切片是空 `Vec`。Go 的 `exhaustPhysicalPlans4LogicalXxx(lp)` 先做类型断言，这里由算子把自己传进去 |
| 候选共享逻辑算子的表达式切片和 schema（`Exprs: p.Exprs`，`proj.SetSchema(p.Schema())`） | 表达式 `Arc` 共享，schema 复制 | 见 §5 第 3 条 |
| `util.GetPushDownCtx(p.SCtx())` | `PlanCtx::push_down_ctx()` | 规划上下文还没有 store client 时是 `blocked-by:D09.13`（VS1-23 已有） |
| `Offset + Count`（`uint64`） | `wrapping_add` | Go 的无符号加法溢出时回绕，Rust 的 `+` 在 debug 下会 panic |
| `Clone(newCtx)` | `PhysicalPlan::clone_plan()`，深拷贝 | 与 Go 一样，`PhysicalSelection` 的克隆不复制 `fromDataSource`，`PhysicalTableReader` 的克隆不复制 `PlanPartInfo`（单元测试 `clones_follow_go`） |
| `unsafe.Sizeof(PhysicalTableScan{})` | 常量 640（64 位 Go 实测） | `PhysPlanPartInfo` 为 96 |

不携带的字段（只在切片之外用到）：TableReader 的 `TableScanAndPartitionInfos`（MPP）；TableScan 的 `SampleInfo`（TABLESAMPLE）、`usedStatsInfo`（EXPLAIN 中的统计状态）、`runtimeFilterList` / `maxWaitTimeMs`（TiFlash runtime filter）、`AnnIndexExtra`（向量索引）。

**浮点乘加（`M-FMA`）**：`calcPagingCost` 的 task.go:583（`seekCnt*seek + float64(expectCnt)*cpu`，gc 先算 `seekCnt*seek`，再把第二个乘积融合进去）和 task.go:588（`pagingCst *= sel` 与 `pagingCst - seek` 融合成 `FNMSUBD`）在 arm64、ppc64le、s390x、riscv64、loong64 上融合，amd64（GOAMD64=v1）不融合。Rust 按目标平台写（`paging_madd` / `paging_msub`）。tracker 的 FMA 登记表此前不含 `planner/core`，本任务把这个包加进去（56 个位置，其中 50 个在代价模型 v1，D10.5 移植时要处理）。`exhaust_physical_plans.go` 中另有 3 个位置（`calcChildExpectedCnt`、`constructIndexJoin`、`shouldPruneIndexJoinByScanRatio`），都在没有移植的 index join 部分。

## 4. 边界桩

Go 函数 28 个，落在 13 个 Rust 桩函数上（tracker `data/stubs.tsv` 新增 13 行、删除 5 行：VS1-10 / VS1-19 / VS1-23 为本任务留的桩已解除；每行都有说明）：

| 桩 | 阻塞块 | 原因 |
|---|---|---|
| `tidbCodecFuncHelper` 的 10 个方法（`expression_codec_fn.go`） | D08.9 | 它们只服务 `tidb_decode_key`、`tidb_encode_record_key`、`tidb_encode_index_key` 三个内置函数，这三个函数类还没有移植（expression crate 中登记为 D08.9 的边界），没有调用者。移植时要用 info schema 的 `TableByName`、权限检查和分区查找，随那三个函数一起做 |
| `MppTask.ConvertToRootTaskImpl` | D10.1 | MPP 的 ExchangeSender 和 TiFlash reader 不在切片中 |
| `PhysicalTableScan.ResolveCorrelatedColumns` | D10.1 | 要用 `SubstituteCorCol2Constant`（`tasks.md` P1-1）：关联列的值在 tidb-rs 中放在执行器的求值状态里（D08.1 spec §3.2），只有 Apply 和 index join 执行器调用，都不在切片中。P1-1 仍未完成 |
| `detachRootTableScanPlan` | D10.1 | 只有 index lookup 下推调用；它返回指向树中父节点的指针，在共享的 `Arc` 树上要另行设计 |
| `PhysicalTableReader.LoadTableStats` | D10.2 | `loadTableStats`（`stats.go`）是 FindBestTask 的同步统计加载 |
| `PhysicalTableReader.GetAvgRowSize` | D10.5 | `getTblStats` 在代价模型文件中 |
| `computePartialCursorOffset` | D08.11 | 要用 `aggregation.NeedCount` / `NeedValue`，只有聚合下推调用 |
| 12 个算子的 `ExplainInfo` / `ExplainNormalizedInfo`（`explain.go`，放在 `core/explain.rs`） | D10.9 | EXPLAIN 文本。与 `PointGetPlan.ExplainInfo` 一样，调用时以 `blocked` 错误 panic（trait 方法返回 `String`） |

另外，在 Go 会走、而切片做不到的分支上，函数内部直接返回 `blocked-by:D10.1`（不留空值或默认值）：cop task 的 index merge、index lookup、index read 收成 root task；index lookup 的 `finishIndexPlan`；Limit / TopN 下推到 index merge；table plan 中有 join（TiFlash）；`enforceProperty` 的 MPP 交换；带虚拟生成列的 TableScan 收成 root task（§5 第 1 条）。

## 5. 与 Go 的已知差异

1. **虚拟生成列的展开**：`convertToRootTaskImpl` 用 `ExpandVirtualColumn` 原地改 TableScan 的 `Columns` 和 schema。task 是浅拷贝，这个 TableScan 被 `taskMap` 中的原 task 以及其他候选共享，所以 Go 的改动对它们都可见：同一个 cop task 第二次收成 root task 时列已经展开过，不再加额外的 Projection。Rust 的 `Arc` 树写时复制，做不到"改一处、处处可见"。没有虚拟列时这次展开得到的结果与原来相同，Rust 在副本上算一遍、不改共享的节点；真正多出列时返回 `blocked-by:D10.1`（回退）。sysbench 的表没有生成列。是否需要模拟、怎样模拟（例如把展开提前到建 cop task 时），留给 FindBestTask（VS1-28）和 N1 一起决定（待定问题 1）。
2. **schema 共享**：Go 中下推的 Limit 和它的子节点共享同一个 `*Schema`（注释 "Don't use clone()"），TableReader 的 schema 就是 table plan 的 schema，枚举出的 Projection / Limit 共享逻辑算子的 schema。Rust 每个节点有自己的 schema（N1 §3.3），在这些地方复制。之后通过共享指针写 schema 的只有两处：上面第 1 条（已是边界）和 D10.9 的 `resolveIndicesForVirtualColumn`（同样只在有虚拟列时写）。VS1-31 移植 `ResolveIndices` 时要再核对一次。M-FT 审计的赋值和方法调用（`.RetType =`、`SetFlag` 等）在这几个文件中只出现在聚合和 join 部分，不在本任务移植的函数里。
3. **`PhysicalTableScan` 的 TP**：Go 的 `TP()` 按扫描方式返回 `TableFullScan` / `TableRangeScan` / `TableRowIDScan` / `MemTableScan`（`explain.go`，D10.9）。这里没有覆盖，扫描的 TP 是 `TableScan`，`ExplainID` 跟着不同。只影响 EXPLAIN 文本和优化器 trace，两者都属于 D10.9（VS1-31）。
4. **`expandVirtualColumn` 找不到列**：Go 追加 `FindColumnInfoByID` 的结果，找不到时是 nil；`Vec<Arc<ColumnInfo>>` 没有 nil，这里 panic。虚拟列只依赖本表的列，不会找不到（L2）。
5. **`sinkIntoIndexLookUp`**：`PhysicalIndexLookUpReader` 没有移植，root 计划不会是它（收成 root task 时已经在 index lookup 处返回边界），所以总是走 Go 的 `return false`（L2）。
6. **`handleRootTaskConds` 的选择率**：Go 在 `Selectivity` 出任何错时都用 `SelectionFactor`（0.8）。Rust 对边界桩（`blocked`）直接返回错误，不悄悄换成 0.8。
7. **`SafeClone`**：Go 用 `%v` 格式化 recover 到的值；Rust 取 panic 消息字符串。
8. **`PointGetPlan.Attach2Task`**：Go 返回 nil task。`Task` 没有 nil，分派函数在这一分支 panic（与 Go 调用方第一次使用 nil task 时一样）；点查自己就是一个 task，不会被挂到别的 task 上。

## 6. 验证

- **govec**（tracker `govec/domains/physops.go`，3,104 条记录，amd64 / arm64 两份；回放 `crates/planner/tests/govec_physops.rs`）：
  - `exhaust` 1,500 条：Limit / TopN / Sort / Selection / Projection，子节点是 DataSource（只有 TiKV 路径、另有 TiFlash 路径、缓存表）、TableDual 或 DataSource 上的 Selection；要求的属性（task 类型、排序项、期望行数、`RejectSort`、Projection 的部分有序属性）和会话变量（MPP、`tidb_limit_push_down_threshold`、部分有序索引、Projection 下推）随机组合。比较每个候选的类型、字段、行数和它对子节点要求的属性，以及 `LIMIT_TO_COP` 不适用时的 warning 和被清掉的 hint 标志。**全部与 Go 相同（2,350 个候选）**。
  - `expand`（`ExpandVirtualColumn`，带多级虚拟列和额外的 handle / 物理表 ID 列）、`match_items`、`paging`（`calcPagingCost` / `extractRows`，含非默认的 seek / cpu 因子，钉住上面的 FMA 写法）、`avg_row_size`（`getAvgRowSize` 按类型宽度估算）、`read_req`：各 400 条（`read_req` 4 条），全部相同。
  - 只有 `Attach2Task`、task 的收拢、算子的克隆和内存估算等要有 FindBestTask 建出的物理计划才能比较的函数没有向量，由单元测试覆盖，并由 FindBestTask（VS1-28）的 EXPLAIN 计划向量验收（VS1 tasks 表中本任务"随 VS1-28 验收"）。
- **单元测试**（`core/physical_plans_tests.rs`，18 个）：`flattenTreePushDownPlan` 的 Go 注释示例；cop task 收成 TableReader（schema、行数、原 task 不变），index 路径的边界；Limit 下推（offset + count、PARTITION BY 时不加根 Limit）、TopN 下推（下推的 TopN、子属性），没有列的 ORDER BY 不下推；Selection / Projection / Sort / NominalSort 的挂接，空 root task 的 `isEmpty`；`enforceProperty`；Limit / Sort / TopN 的枚举（与 govec 相同的形状）；`SafeClone` 恢复 panic；克隆的字段；`CollectPlanStatsVersion`。

## 7. 待定问题

1. 虚拟生成列（§5 第 1 条）：Go 原地展开共享的 TableScan，跨 task 可见。要不要、怎样在 `Arc` 树上模拟，待 FindBestTask（VS1-28）确定 cop task 的构造后，与 N1 一起决定。
2. FMA 登记表加入 `planner/core` 后，`converge.py --fma-sites` 会对部分移植的文件报告未移植函数中的融合位置（目前是 `exhaust_physical_plans.go` 的 3 个 index join 位置）。要么移植时再补注释，要么让脚本只检查已移植函数中的位置；本任务没有改脚本。

## 本文所用 ID

- VS1-10：执行入口与语句编译、`initialize.go`（D09.0 的一部分）
- VS1-15：plan cache 的可缓存检查
- VS1-19：规划器基础（`BasePhysicalPlan` 等）
- VS1-23：逻辑算子与基本逻辑规则
- VS1-27：本任务，物理算子、task 与物理实现枚举
- VS1-28：`FindBestTask`、DataSource 的访问路径
- VS1-30：代价模型 v2
- VS1-31：`ResolveIndices`、`ToPB`、access object
- D08.1：表达式核心（求值状态，spec §3.2）
- D08.9：控制流、信息、杂项内置函数
- D08.11：聚合函数描述
- D09.0：执行入口（`initialize.go`、`plan.go` 所在的块）
- D09.13：优化器驱动
- D09.14：逻辑算子
- D10.1：物理算子定义与 task
- D10.2：`FindBestTask` 与访问路径
- D10.3：物理实现枚举
- D10.5：代价模型
- D10.9：后处理（EXPLAIN、`ResolveIndices`、plan 编码）
- D10.10：物理属性与优化器工具
- D13.1：plan cache
- N1：计划表示的设计（§3.3 schema 归属，§3.4 `Arc` 树，§4 摘出分派）
- M-CTX / M-IFACE / M-PLAN / M-SELF / M-FT / M-FMA / M-F2I：映射规则（上下文参数 / 接口与类型断言 / 计划树的写法 / 嵌入与 self / FieldType 共享 / 浮点乘加 / 浮点转整数）
- L1 / L2：来源标注的等级（L1 逐行对应；L2 有理由的改写）
- 例外 B：只有 cascades 优化器用的函数，不移植
