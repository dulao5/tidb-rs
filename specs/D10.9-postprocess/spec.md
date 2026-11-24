# D10.9 后处理：ResolveIndices、ToPB、access object（spec，VS1-31）

状态：评审中（2026-11-24）。块：D10.9（后处理：resolve indices、plan → pb、EXPLAIN 输出、plan 编码），本任务只做第一条纵向切片的算子需要的前三项中的部分；EXPLAIN 文本是 VS1-38，plan 编码和克隆不在本任务。crate `tidb-planner`。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3；[D10.1 物理算子](../D10.1-physical-plans/spec.md)（§5 第 2 条把 schema 共享的核对留给本任务）；[N1 计划表示](../exceptions/N1-plan-representation/spec.md)（§3.4 物理计划是 `Arc` 树）；[A3 plan cache](../exceptions/A3-plan-cache/spec.md)（§3.1 计划进缓存之后不再写）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/`。

## 1. 范围

物理优化选出最终计划以后，还要做三件事：

1. **ResolveIndices**：把每个算子的表达式里引用的列，换成"子节点输出的第几列"（`Column.Index`）。执行器按这个下标取值。
2. **ToPB**：把 TableReader 下推到 TiKV / TiFlash 的那一段计划编成 `tipb.Executor`（coprocessor 的 DAG 请求）。TiKV 是一个列表（从扫描开始逐个算子），TiFlash 是一棵树（每个算子带子节点和 executor id）。
3. **access object**：算子访问了哪张表、哪个分区、哪个索引，用于 EXPLAIN 的 "access object" 列和二进制计划。

本任务移植切片中这些算子（TableReader、TableScan、Selection、Projection、Limit、TopN、Sort、NominalSort，以及点查）的这三部分：

| 内容 | Go 文件 | Rust 文件 |
|---|---|---|
| 14 个 `ResolveIndices` 相关函数 | `planner/core/resolve_indices.go` | `core/resolve_indices.rs` |
| 6 个算子的 `ToPB`、`FindColumnInfoByID` | `planner/core/plan_to_pb.go` | `core/plan_to_pb.rs` |
| access object 的 4 种类型及其方法、TableScan / 点查 / TableReader 的 access object | `planner/core/access_object.go` | `core/access_object.rs`；接口 `base.AccessObject` 在 `base.rs` |
| TableScan 的 `ExplainID`（ToPB 的 executor id 用到） | `planner/core/explain.go` | `core/explain.rs` |

`ResolveIndices` 在 `physicalOptimize` 的最后调用（`core/optimizer.rs`），这时 FindBestTask 的 memo 已经丢掉，计划由调用方独占，`make_mut_plan` 原地写、不复制节点（`govec_findbest.rs` 的 `best_plan_is_owned_alone_after_the_memo` 断言了这一点）；之后计划才进 plan cache，满足 A3 §3.1。

不在本任务：EXPLAIN 文本（`ExplainInfo` 等，VS1-38）；plan 编码（`encode.go`、`flat_plan.go`）、计划克隆（`plan_clone_*.go`）；切片之外的算子（join、聚合、index reader、窗口、MPP……）的这三部分。执行器怎样用 `ToPB` 拼 DAG 请求（`constructDAGReq`）是执行器的任务（VS1-34），那里仍是 `blocked-by:D10.9` 的桩，本任务不动执行器。

### 1.1 converge（`--part specs/VS1-vertical-slice/functions.tsv --item VS1-31`）

| 文件 | 已移植 | 桩 | 合计 |
|---|--:|--:|--:|
| `resolve_indices.go` | 14 | 0 | 14 |
| `plan_to_pb.go` | 7 | 0 | 7 |
| `access_object.go` | 15 | 0 | 15 |
| `explain.go` | 1 | 0 | 1 |

37 个函数全部有 `// go:` 标注（matched 37/37，missing 0、extra 0）。其中 L2 的函数（`PhysicalTableScan.ToPB`、`PhysicalTableScan.AccessObject`、`PhysicalTableReader.accessObject`、`getDynamicAccessPartition`）各有一个分支是边界（§4），理由见 §5。块 D10.9 其余 170 个函数不在本任务。

## 2. 函数清单的增减

**增加**（tracker `data/part_funcs.tsv` 与 `specs/VS1-vertical-slice/functions.tsv` 中加进 VS1-31）：

| 函数 | 所属 | 为什么 |
|---|---|---|
| `IndexAccess.ToPB`（`access_object.go`） | D10.9 | `ScanAccessObject.SetIntoPB` 调用；原清单漏了 |
| `PhysicalTableScan.ExplainID`（`explain.go`） | D10.9 | TiFlash 的 `ToPB` 用它做 executor id；Go 的 `ExplainID` 按扫描方式给出 `TableFullScan_6` 之类，不覆盖就会和 Go 的 DAG 字节不同 |

**不再移植**：`refine4NeighbourProj` 已随 VS1-24 移植（`InjectExtraProjection` 调用），本任务直接用。

另外修改了已移植的函数：`PointGetPlan.ResolveIndices`（D10.8）原来在有虚拟列时返回 `blocked-by:D10.9`，现在调用 `resolveIndicesForVirtualColumn`，按 Go 移植（L1）；`expandVirtualColumn`（D10.1）改用本任务的 `FindColumnInfoByID`（Go physical_plans.go 同样调用它）。

## 3. 承载方式

| Go | Rust | 说明 |
|---|---|---|
| `p.ResolveIndices() error`，算子用 `p.SCtx()` 取求值上下文 | trait 方法 `resolve_indices(&mut self, ctx: &PlanCtx)` | 节点不带会话上下文（`M-CTX`），`ctx` 是 Go 的 `p.SCtx()`。trait 的签名因此加了 `ctx` |
| 递归改子节点（`p.Children()[i].ResolveIndices()`） | `make_mut_plan(child).resolve_indices(ctx)` | 计划独占时原地写（见 §1） |
| `resolveIndicesForSort(p BasePhysicalPlan)` 按 `p.Self` 的类型取 ByItems | `resolve_indices_for_sort(&mut dyn PhysicalPlan, ctx)`，按 `kind()` 向下转型（`M-IFACE`） | 其他类型时返回 Go 的同一条错误 |
| TableReader 的 `TablePlans`（展开的 table plan 节点列表）与 `tablePlan` 指向同一批节点 | `table_plans` 存的是同一批 `Arc` | 原地改 `table_plan` 之前先清空 `table_plans`（否则引用计数大于 1，会写时复制），改完再用 `flattenListPushDownPlan` 重新展开，结果与 Go 相同（同 `optimizer.rs` 的 `rewrite_table_plan`） |
| `ToPB(ctx *base.BuildPBContext, storeType)` | 各算子的固有方法 `to_pb(&self, ctx: &BuildPBContext, store_type)`，trait 方法分派过去 | |
| `&tipb.Executor{Tp: ..., ExecutorId: &executorID}` | `new_executor`：总是设置 `tp`、`executor_id`（可能是空串）、两个 fine grained shuffle 字段 | Rust 的 tipb 是 rust-protobuf 2.8（proto2 有"是否设置"位），gogoproto 对 `nullable=false` 字段总是编码；Go 中指针字段（`ExecutorId`、`KeepOrder`、`IsFastScan`、`IsPartialSort`）也总是设置。不显式设置字节就会不同 |
| `expression.ExpressionsToPBList(evalCtx, exprs, client)` | `expressions_to_pb_list(eval_ctx, &EvalState::new(), exprs, client)` | 关联列的值在 tidb-rs 中放在求值状态里（D08.1 spec §3.2），这里用空的状态，见 §5 第 4 条 |
| `base.AccessObject` 接口 | trait `base::AccessObject`（`to_string`、`normalized_string`、`set_into_pb(Option<&mut ExplainOperator>)`） | Go 的 nil `*tipb.ExplainOperator` 是 `None`；`DynamicPartitionAccessObject` 只是 Stringer，`String()` 写成 `Display`（`M-NAME`） |
| `DynamicPartitionAccessObjects`（切片类型）、`OtherAccessObject`（字符串类型） | 元组结构体 `DynamicPartitionAccessObjects(Vec<_>)`、`OtherAccessObject(String)` | Go 的 nil 切片是空 `Vec` |
| `PhysicalTableReader.accessObject(sctx) base.AccessObject` | `access_object(&self, sctx) -> Result<Box<dyn AccessObject>, Error>` | 返回 `Result` 只为 §4 的分区边界 |
| `p.ExplainID()` 读 `p.SCtx()` 的 `IgnoreExplainIDSuffix` | `explain_id(None)` | `ToPB` 的参数是 `BuildPBContext`，拿不到语句上下文，按 nil 上下文格式化（与已移植的 `BasePhysicalPlan.ToPB` 相同），见 §5 第 7 条 |

## 4. 边界桩

本任务新增 2 个函数内的边界（tracker `data/stubs.tsv` 各登记一行）、去掉 1 个：

| 位置 | 阻塞块 | 原因 |
|---|---|---|
| `PhysicalTableScan.ToPB` 的第一个分支：TiFlash、分区表、MPP / batch cop 时走 `partitionTableScanToPBForFlash` | D09.12 | 分区表在 plan builder 里就停在 `blocked-by:D09.12`，切片的计划里不会有分区表的扫描。Go 这个分支还要 `StmtCtx.UseDynamicPartitionPrune()`，而 `ToPB` 只拿到 `BuildPBContext`，读不到；所以两种裁剪模式下都返回 blocked，不退回到普通扫描 |
| `getDynamicAccessPartition`：分区表且使用 dynamic 裁剪时要调 `PartitionPruning` | D09.12 | 同上，分区裁剪还没有移植。非分区表或非 dynamic 模式时按 Go 返回 nil |
| （去掉）`PointGetPlan.ResolveIndices` 中虚拟列的 `blocked-by:D10.9` | — | 现在按 Go 调用 `resolveIndicesForVirtualColumn` |

桩棘轮：planner 区域 367 → 368（净增 1，即上面两行减去去掉的一行）。按 port-block §2 第 2 种情况：本任务新纳入 `base.AccessObject` 接口和 `ToPB` 入口，超出切片的分支（分区表）各写一个桩，都在 tracker 登记并说明不可达的理由。

## 5. 与 Go 的已知差异

1. **列对象共享**（resolve_indices.go:394–401、:835–840；initialize.go:256）：Go 中 TableReader 的 schema 就是 table plan 的 schema（同一个 `*Schema`、同一批 `*Column`），`PhysicalTableScan.ResolveIndicesItself` 写 `column.Index = i` 时，TableReader 的 schema 列也跟着变；TableReader 的 `resolveIndicesForVirtualColumn` 写 `col.VirtualExpr` 时，扫描的列也跟着变。Rust 每个节点有自己的 schema（D10.1 spec §5 第 2 条），TableReader 在 `Init` 时拿到 table plan schema 的一份副本。
   - **`Index`：与 Go 相同。** TableReader 解析完 table plan 以后，把 table plan schema 各列解析后的 `Index` 按位置抄回自己的 schema（同一位置的两列 unique id 相同时才抄；副本来自 `Init` 时的 table plan schema，所以每个位置都相同）。table plan 出错时也照样抄，对应 Go 中出错前已经写进共享列对象的值。之后 TableReader 的 schema 列与 table plan 的 schema 列逐列相同，与 Go 读到的一样（govec 回放对每条规划成功的记录检查这一点，见 §6）。之后的 postOptimize 即使换掉 table plan 的根（TiFlash 下注入投影、合并 Selection），Go 的 TableReader 仍指向原来那批列对象，Rust 的 TableReader 也保留抄好的值，两者仍相同。
   - **`VirtualExpr`：仍有差异。** 虚拟列表达式只在 TableReader 自己的副本上解析，table plan 中扫描的 schema 列保留未解析的表达式（Go 中扫描的列也是解析后的）。执行器读的是 TableReader 自己的 `VirtualExpr`，扫描的 `ToPB` 不读 `VirtualExpr`，所以结果相同；而且虚拟生成列在 SELECT 构建时就停在 `blocked-by:D09.1`，切片的计划里没有带 `VirtualExpr` 的列。
2. **出错时留下的值**（resolve_indices.go:494、:713）：Go 的 `p.Conditions[i], err = expr.ResolveIndices(...)` 在出错时把条件写成 nil，TopN 的 `item.Expr` 同样；Rust 出错时不写，保留原表达式。出错时整条语句失败，计划不再使用。
3. **`ResolveIndices` 的 ctx 参数**：见 §3。`PhysicalSelection.ResolveIndices` 的回退（`ResolveIndicesByVirtualExpr`）用 `ctx.eval_ctx()`，对应 Go 的 `p.SCtx().GetExprCtx().GetEvalCtx()`。
4. **关联列**（plan_to_pb.go:170、:192、:248、:278）：Go 的 `ExpressionsToPBList` 对关联列读 `*col.Data`；tidb-rs 把关联列的值放在求值状态里，`ToPB` 传空的 `EvalState`。关联列只出现在 Apply 下，切片不建 Apply。
5. **`SortByItemToPB` 返回 nil**（plan_to_pb.go:219、:222、:244、:743）：Go 把 nil 追加进 `OrderBy` / `PartitionBy` / `ByItems`，之后编码 DAG 时空指针 panic；Rust 在转换处 panic。下推检查已经保证这些表达式能转换，正常不会发生。
6. **`PhysicalTableScan.ToPB`**（plan_to_pb.go:266–311）：分区分支见 §4；TiFlash 扫描的 telemetry 计数器（:304、:306）没有移植；`AnnIndexExtra`（:285，向量索引）、`runtimeFilterList`（:291，TiFlash runtime filter）、`maxWaitTimeMs`（:295）这些字段 Rust 不携带（D10.1 spec §3），分别相当于 Go 的 nil、空列表、0。`SetPBColumnsDefaultValue` 出错时 Go 同时返回 executor 和错误，Rust 只返回错误（调用方都先看错误）。
7. **executor id**（explain.go:159–166；baseimpl/plan.go:93–100）：Go 在 `IgnoreExplainIDSuffix`（`EXPLAIN FORMAT='brief'`）时 executor id 不带 `_<id>` 后缀，并且 `ExplainID` 的结果第一次计算后就被缓存（`MemoizeStr`）。Rust 的 `ToPB` 总是带后缀。只影响 `EXPLAIN ANALYZE FORMAT='brief'` 下 TiFlash 运行时统计与算子的对应；只读节点的 EXPLAIN 是 VS1-38。
8. **`PhysicalTableReader.accessObject` 与 `getDynamicAccessPartition`**（access_object.go:380–419、:421–473）：返回 `Result`（§3）；`TableScanAndPartitionInfos`（MPP）Rust 不携带，只走 Go 的 `len(...) == 0` 分支。
9. **`PointGetPlan.ToPB`**：Go 返回 `nil, nil`。Rust 中 `PointGetPlan` 已有的固有方法 `to_pb()` 返回 `Ok(None)`（D10.8），trait 的 `to_pb` 仍是 `BasePhysicalPlan.ToPB` 的默认实现；执行器不会对点查调用 `ToPB`。govec 的根节点 `ToPB` 记录因此跳过下面有点查的子树（生成器注释说明）。

## 6. 验证

- **govec**（tracker `govec/domains/postprocess.go`，1,033 条记录，amd64 / arm64 两份，两份记录完全相同；回放 `crates/planner/tests/govec_postprocess.rs`）：
  - `plan` 620 条：语句按 `findbest` 的方式规划（`DoOptimize`，最后一步是 `ResolveIndices`），比较每个算子解析后的列下标（Selection 的条件、Projection 的表达式、Sort / TopN 的 ByItems、TopN / Limit 的分区列和前缀列、Limit 和 TableScan 的 schema 列、虚拟列表达式），每个 TableReader 的 DAG（`TablePlans` 逐个 `ToPB(TiKV)` 和 table plan 的 `ToPB(TiFlash)`，比较 protobuf 字节），根上 Selection / Projection / TopN / Limit / Sort 的 `ToPB(TiKV)`（含"不是 partial sort"和子节点是 reader 的错误），以及 TableScan、点查、TableReader 的 access object（`String`、`NormalizedString`、`SetIntoPB` 后的字节）。表有 int handle（sbtest1）、聚簇的 common handle（新表 tch）、隐藏行 ID（新表 tn）；下推的 Selection、Limit、TopN、Projection，keep order 和 desc 的扫描都有。**591 条与 Go 逐字节相同**，29 条在规划时就停在已有的边界（字符串函数 D08.6 4 条、虚拟生成列 D09.1 6 条、分区表 D09.12 6 条、剩下索引路径 D10.2 11 条、BatchPointGet D10.8 2 条），按块固定在 `EXPECTED_BLOCKED`。
  - `missing` 360 条：给计划中第 k 个算子加一个子节点没有的列（条件、表达式、ByItem 或 Limit 的 schema 列），再调它的 `ResolveIndices`，比较错误文本（`Can't find column ... in schema ...`、`Some columns of Limit_N cannot find the reference from its child(ren)`）。294 条相同，66 条停在上面同样的边界。
  - `access` 52 条：直接用字段构造 4 种 access object（多个动态分区对象、带错误的对象、聚簇索引、空表名等），全部相同。
  - 覆盖：本任务 37 个函数都列在某个 op 的 `GoFuncs` 中；块 D10.9 其余函数在豁免表中写明理由（`postprocess_exempt.go`）。
- **TableReader 的 schema**：govec 回放中每条规划成功的 `plan` 记录都检查每个 TableReader 的 schema 列（unique id、`Index`）与其 table plan 的 schema 列逐列相同（`check_reader_schemas`，§5 第 1 条）。去掉抄回的那一步时这项检查在 444 条记录上失败。
- **回退的会话测试**（`crates/session/src/tests.rs` 的 `range_and_order_by_limit_queries_still_fall_back`）：`physicalOptimize` 现在对每个计划都跑真正的 `ResolveIndices`，如果 Rust 构建的 schema 与 Go 有出入，语句会以普通错误（1105，找不到列）结束，而不是回退。测试在 sbtest 表上用文本协议和预处理语句（`id BETWEEN ? AND ?` 等，每条 EXECUTE 两次）执行范围查询和 `ORDER BY ... LIMIT`，断言结果是回退信号（边界桩的 `blocked` 错误，固定每条语句停在哪个块：TableReader 执行器 D12.1、Projection 执行器 D12.2、Sort / TopN 执行器 D12.5），且已执行语句数（`executed_stmts`）不变、会话回到空闲；`LIMIT ?` 在 PREPARE 时就回退（D13.1）。
- **单元测试**：`core/access_object/tests.rs`（4 个：各类型的 `String` / `NormalizedString` / `SetIntoPB`，nil 的 `ExplainOperator`）；`tests/util_test.rs` 的 `base_physical_plan_methods`（`ResolveIndices` 递归到每个子节点、计划独占时原地写）；`point_get_plan/tests.rs`。
- 未在本地集群上做端到端比较（tidb-rs 还没有 TableReader 执行器，VS1-34）。

## 7. 待定问题

无。

## 本文所用 ID

- VS1-24：优化器驱动（`DoOptimize`、`InjectExtraProjection` 等），`refine4NeighbourProj` 随它移植
- VS1-31：本任务，`ResolveIndices`、`ToPB`、access object
- VS1-34：TableReader 执行器
- VS1-38：EXPLAIN 输出
- D08.1：表达式核心（求值状态，spec §3.2）
- D08.6：字符串函数
- D09.1：SELECT 构建（虚拟生成列在这里是边界）
- D09.12：分区裁剪
- D10.1：物理算子与 task
- D10.2：FindBestTask 与访问路径
- D10.8：点查快速路径
- D10.9：后处理（resolve indices、plan → pb、EXPLAIN 输出、plan 编码）
- D12.1：TableReader 等读执行器
- D12.2：Projection / Selection / Limit / TableDual 执行器
- D12.5：Sort / TopN 执行器
- D13.1：plan cache（`LIMIT ?` 绑定 EXECUTE 的值，在这里是边界）
- N1：计划表示（物理计划是 `Arc` 树）
- A3：plan cache 的例外
