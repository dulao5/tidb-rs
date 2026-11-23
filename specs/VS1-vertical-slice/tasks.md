# VS1：tasks

状态截至 2026-11-14，全部未开始。每项的内容、函数范围、依赖和验收见 [plan.md](plan.md) §5，函数清单见 [functions.tsv](functions.tsv)。

**标签**：`depth` 计入深度份额，`breadth` 是 #49 中的 P2 块，只是按 VS1 的需要排在前面。

**周**：项目一周的全部产能，约等于 15k 行 Go 函数体（plan §5 的口径）。

**通用完成条件**：每一项都要满足以下四条，下表不再重复。
- 所含函数 converge 100%（有 `// go:` 标注，或者登记为 `blocked-by:<块>`）；
- `stubs.tsv` 已登记；
- Lucky 评审通过；
- 每周构建默认模式不退化。

## Phase 0：前置

| # | 任务 | 标签 | 周 | 依赖 | 完成条件 | 状态 |
|---|---|---|--:|---|---|---|
| VS1-00 | N1 修订 PR（#149 的后续），同步改 `mappings.md` 的 M-PLAN / M-SELF，新增 M-FT | depth | 0.03 | — | 负责人确认并合入 | 未开始 |
| VS1-01 | 建立 `tidb-planner`、`tidb-executor`、`tidb-session` crate；arena、`PlanCtx`、摘出分派的宏、`make_mut_plan` / `map_children`、async `Executor` trait；plan 模板 | depth | 0.10 | VS1-00 | `check_layers.py` 通过；试点中的固定写法测试移过来后通过 | 未开始 |
| VS1-02 | 路由器与开关 `--new-stack=off\|vs1\|strict`、`Blocked(block)` 错误、`tidbrs_route_total`。plan §4.3–4.7 的要求必须在开工前确认（#205）：第 0 步的会话条件（显式事务、`autocommit=0`、`tidb_snapshot` / `tidb_read_staleness` / `tx_read_ts` / `tidb_txn_mode`、同步白名单之外的变量偏离默认值、游标）；InfoSchema 版本相等；同步白名单；`LastStmtInfo`；stmt id 归属表 | depth | 0.12 | VS1-01 | `off` 模式下 ab / sysbench 无变化；第 0 步每个条件一个用例；§4.5 每个字段两种顺序的测试 | 未开始 |
| VS1-03 | tracker 支持块的部分（part）与 `blocked-by`；ab / 每周构建加 `strict` 一轮和"新栈"报告节 | depth | 0.05 | — | 每周报告出现新栈覆盖面 | 未开始 |
| VS1-04 | parser AST `Rc<WithClause>` → `Arc`；`Session: Send` 静态断言 | depth | 0.02 | — | parser 测试通过 | 未开始 |

## Phase 1：点查纵向（C2a，目标 12-11 至 12-14）

| # | 任务 | 块 | 标签 | 周 | 依赖 | 完成条件 | 状态 |
|---|---|---|---|--:|---|---|---|
| VS1-10 | planbuilder 骨架、preprocess（SELECT）、`planner.Optimize` / `optimize` | D09.0、D09.13 | depth | 0.17 | VS1-01 | govec preprocess 向量一致 | 进行中（#224） |
| VS1-11 | `TryFastPlan`、`PointGetPlan` | D10.8 | depth | 0.07 | VS1-10、VS1-19 | 点查计划向量一致；D09.0 的 `EncodeUniqueIndexKey` / `EncodeUniqueIndexValuesForKey` 移植后（建议并入本项，D12.1 spec §5），测试点查 `Next` 的唯一索引路径：索引 key、`decode_handle_in_index_value`、全局索引的分区 ID、索引有行无时的不一致错误（#234 评审） | 评审中（D10.8 46/52 + 6 桩，D09.0 `EncodeUniqueIndexKey` / `EncodeUniqueIndexValuesForKey` 2/2，D00.3 `ReportLookupInconsistent` 1/1；govec `fastplan` 5370 条：`fast` 2199 条，`uidx` 3171 条其中 2596 条经点查执行器回放。`glue::PointGetPlan` 已换成 planner 的类型） |
| VS1-19 | `property`、`planner/util`、`baseimpl`、`base_physical_plan` | D10.10、D10.1 | depth | 0.11 | VS1-01 | Go 单测与 govec 通过 | 进行中（#223） |
| VS1-12 | 点查执行器、`NewRowDecoder`；table 读取部分 | D12.1、D06.4 | depth + breadth | 0.13 | VS1-13、VS1-18 | 行解码 / 默认值 govec；ab `Point_Get` | 评审中（D12.1 31/33 + 2 桩，含 `fillRowChecksum` / `shouldFillRowChecksum`，D06.4 54/57 + 3 桩；govec `pointget` 5850 条（含 1800 条 row checksum）。ab `Point_Get` 要等 VS1-11 / VS1-14，归 VS1-20。唯一索引点查的 key 编码已并入 VS1-11） |
| VS1-13 | `ExecStmt`、`recordSet`、`Compiler`、`executorBuilder.build`、`internal/exec` | D12.0 | depth | 0.15 | VS1-01 | 单测通过；MaxTS 点查判定与 Go 一致 | 评审中（#222） |
| VS1-14 | 会话 `ExecuteStmt` / `PrepareStmt` / `ExecutePreparedStmt`、`LazyTxn`、txnManager、sessiontxn 只读 provider；回退时的检查点与恢复（plan §4.6）；预处理语句四种组合与按 stmt id 路由（§4.7）。注（D07.2）：KILL 还到不了 coprocessor 读——`tidb_store::copr` 的 `Variables.killed` 现为 `None`（`poc-kv` `KvSnapshot::coprocessor`），本项要把会话的 kill 标志传进去，worker 的 `finished()` 才能看到（Go `vars.Killed`） | D05.3、D05.4 | depth + breadth | 0.24 | VS1-13 | N2 §5 断言；autocommit 读、TSO 预取、warning 与 Go 一致；每种回退原因的 `SessionVars` 快照前后相等，SHOW WARNINGS 只有 PoC 的 warning；四种组合与 RESET / LONG_DATA / FETCH / CLOSE 用例；ab 比较 PREPARE 列元数据与每次 EXECUTE 的列定义（含反证）；`RecordSet` 没有 `Drop`（#222）：语句中途被丢弃（取消、KILL、断连）时，会话在下一条语句之前补做未完成的 finish（`close_record_set` / `FinishExecuteStmt`），用例检查中途丢弃后的会话状态 | 第 1 部分已合入（#235）；第 2 部分 VS1-14b（预处理语句，§4.7）评审中：`PrepareStmt` / `ExecutePreparedStmt` / `DropPreparedStmt` / `resolvePreparedStmt` / `GetSessionPlanCache` 5/5（L2），PREPARE 两边准备、`Blocked` 时恢复会话并归 PoC（现在都停在 VS1-15 的预处理执行器 D12.12），RESET / LONG_DATA / FETCH / CLOSE 用例，ab 比较 PREPARE 与 EXECUTE 的列定义（含反证）。见 [D05.3 spec](../D05.3-session/spec.md) §6。EXECUTE 节点（`ast.ExecuteStmt`，解析器块 D02.2）已补：`ExecutePreparedStmt` 照 Go 把语句交给 `ExecuteStmt`（L1），`ResetContextOfStmt`、语句标签、`isEndTxnStmt`、`resolvePreparedStmt` 读预处理语句；EXECUTE 原来停在编译器里规划器的入口（D09.13），现在 `Compile` 已接入 `planner.Optimize`（会话的 sessionexpr，D05.3 tasks P1-1，见 [D05.3 spec](../D05.3-session/spec.md) §8、§9）：点查停在 fast plan 中点查条件的参数（D13.1），其他查询停在物理优化的边界，回退干净（会话与路由器测试） |
| VS1-15 | `PlanCacheStmt`、`GetPlanFromPlanCache` 未命中路径、可缓存性检查、`PrepareExec` | D13.1、D12.12 | depth | 0.08 | VS1-14 | 可缓存性 govec；ab 预处理语句部分 | 评审中：D13.1 24/44 移植 + 20 个桩（缓存本身归 A3，不缓存任何东西），D12.12 2/2；govec `plancache` 238 条一致、18 条边界桩；路由器的 CLOSE 改为事务结束时删除（#247 完成条件）。预处理点查在 `strict` 下由新栈执行还要等 D06.3、D09.1、D13.3、D03.1。见 [D13.1 spec](../D13.1-plancache/spec.md)。`GetPreparedStmt`、`buildExecute`（`Execute` 计划）、`OptimizeExecStmt` 已移植（EXECUTE 节点补上之后），走到未命中路径后停在 `OptimizeAstNode`（D13.3）；仍不缓存任何计划（A3 例外未确认） |
| VS1-14m | 读路径的元数据锁（MDL）：`tryLockMDLAndUpdateSchemaIfNecessary`、`RemoveLockDDLJobs`（未接上报）、autocommit 点查 MaxTS 经 sessiontxn 接入（#146 第一部分：三项都照 Go，缺口见 #252 `go-gap`） | D09.0、D05.3 | depth | 0.03 | VS1-14、VS1-15 | PREPARE 登记并在结束时清除、EXECUTE / autocommit 读不登记、主键点查 0 次 TSO 的测试；govec preprocess 原先停在 MDL 桩的记录与 Go 一致；**在新栈会话的登记上报给集群（D06.5 的 MDL 检查循环，桩 `report_metadata_locks`）之前，路由器不得把 DML 或显式事务交给新栈**（它们的登记跨语句存在，DDL 不会等它们） | 评审中：PREPARE 通过元数据锁，停在规划器 `buildSelect`（D09.1）；govec preprocess 92 条一致（原 83，MDL 桩 9 条全部一致）。见 [D05.4 spec §3a](../D05.4-sessiontxn/spec.md) |
| VS1-16 | 结果集与 `writeResultSet` / `writeChunks`、`column` / `dump`、`conn_stmt` 执行与二进制参数；去掉 `newstack.rs` 中执行之后的回退（结果集不能编码时返回 `blocked-by:D03.1`，plan §4.3 不允许，路由器记为 bug，#235） | D03.1 | breadth | 0.13 | VS1-14 | dump govec；protocol-smoke 通过 | 第 1 部分评审中：D03.1 本部分 45/45 移植、无桩（另 51 个属连接循环 / 游标 / D13.1，`functions.tsv` 写明）；govec `protocol` 3830 条一致；执行后不回退、打开后计数、连接循环 panic 恢复（[D03.1 spec](../D03.1-server/spec.md) §6）；protocol-smoke 本机无集群未跑 |
| VS1-17 | D06.3 第 2 部分（T8 / T9）；D06.5 `loadInfoSchema` / `tryLoadSchemaDiffs` / validator，由 PoC domain 驱动 | D06.3、D06.5 | breadth | 0.10 | — | schema 同步测试在新 InfoSchema 上通过 | 第 1 部分评审中：`InfoCache`、schema 加载（全量 + diff）、PoC domain 驱动、预处理查表；validator 等留给第 2 部分（VS1-17b，等 N11） |
| VS1-18 | snapshot `Get` / `BatchGet` / `SetOption`、错误转换 | D07.1 | breadth | 0.04 | — | 故障场景点查与 PoC 相同 | 评审中（#221） |
| VS1-20 | C2a 集成与 point_select 调优 | glue | depth | 0.08 | Phase 1 其余各项 | ab `strict` 的 `Point_Get` 100%；point_select ≥ 0.9× Go（1 / 8 线程）；ab 包含一个真正走到 `pseudoSelectivity` 唯一键查找的用例（UniqueID 与列 ID 的对应，D10.6 spec §5 第 1 条）；profile：记录每个点查计划的分配次数（每条语句重建 pseudo 表），据此决定是否缓存（D11.1 spec §3）；另记录 InfoSchema 查表的两处额外开销（D06.3 spec §11.2）：PREPARE 把 SQL 多解析一次、每条语句在规划前提前加载用到的表 | 评审中（集成记录 `c2a-integration.md`）：预处理点查在新栈上跑通，ab `strict` 的 `point_select / ps` 136/136 新栈执行且与 Go 一致；`Point_Get` 分类 0 不一致、168 条 1235（文本协议 D05.3、Projection、TableDual），尚未达到 100%；point_select 0.93× / 0.89× Go（1 / 8 线程），分配约 566 次 / 语句（Go 约 200）；`pseudoSelectivity` 用例要等 C2b 的规划器与执行器（集成记录 §4） |
| VS1-20b | 点查短路径：`Compile` 中按 `IsSafeToReusePointGetExecutor` 的条件（非 stale read、自动提交、schema 版本相同）设 `PsStmt`，会话走 `ExecStmt.PointGet`；**不缓存执行器**，每次新建（缓存是 plan cache 的 A3） | D12.0、D05.3、D13.1 | depth | 0.03 | VS1-20 | 自动提交的预处理点查与 Go 一样走短路径：`Prepared` 为 false、优先级为 High、不经 `runStmt` / `finishStmt`（D05.3 spec §10.2 第 1 条）；ab `strict` 的预处理点查保持全部一致；记录每条语句的分配次数相对 VS1-20 的变化 | 未开始 |

## Phase 2：规划器主体

| # | 任务 | 块 | 标签 | 周 | 依赖 | 完成条件 | 状态 |
|---|---|---|---|--:|---|---|---|
| VS1-26 | `taskMap` 小试点（spike） | N1 | depth | 0.05 | VS1-01 | 试点报告；不通过时先修订 N1；统计代价计算期间 `clone_node` 的调用次数（代价缓存经 `&self` 写在共享节点上，应为 0，#223） | 未开始 |
| VS1-21 | `buildSelect` 等 SELECT 构建，加 `resolveHavingAndOrderBy` | D09.1、D09.4 | depth | 0.18 | VS1-10、VS1-22、VS1-23 | 逻辑计划向量一致；分区表返回 `blocked-by:D09.12`（D06.4 第 1 部分的分区表只是标记，读它的 record prefix 会触发 `debug_assert`，#217 评审） | 评审中（spec `specs/D09.1-select-builder`；D09.1 converge 29/43 + 14 桩，D09.4 6/6，D10.2（访问路径，从 VS1-28 移来）9/9；govec `selectbuild` 1,743 条：1,472 个计划、174 个错误与 Go 相同，96 条停在桩，分区表 `blocked-by:D09.12`；functions.tsv 中访问路径的 9 行已从 VS1-28 移到本项） |
| VS1-22 | 表达式改写（子查询之外） | D09.2 | depth | 0.14 | VS1-10 | 表达式向量一致 | 评审中（#258；D09.2 converge 44/53 + 9 桩；govec `rewrite` 2,392 条，1,467 条一致、922 条停在桩（多数是未移植的函数族），1 条已知别名差异） |
| VS1-23 | 逻辑算子公共部分，以及列裁剪、投影消除、谓词下推与简化、TopN / Limit 下推（VS1 的算子）；M-FT 审计 | D09.14、D09.7、D09.8、D09.11 | depth | 0.21 | VS1-01 | 规则级计划向量一致；审计表写进 plan | 第 1 部分评审中（spec `specs/D09.14-logical-operators`；govec 6,630 步一致；M-FT 审计见 plan §11） |
| VS1-24 | `DoOptimize`、`postOptimize`、`InjectExtraProjection`、`ResultReorder`；hint 骨架 | D09.13、D13.3 | depth | 0.19 | VS1-21 | 物理计划向量一致；PREPARE（`GeneratePlanCacheStmtWithAST`）使用同一个规划上下文，恢复对 `PlanBuilder.Build` 的调用（现在是 `blocked-by:D09.13` 的桩） | 评审中（spec `specs/D09.13-optimizer/spec.md`；D09.13 converge 56/60 + 4 桩，D13.3 38/38；govec `optdriver` 2,470 条：逻辑优化 1,638 条、postOptimize 412 条与 Go 一致，104 条停在构建器已有的边界；PREPARE 已恢复构建。最终物理计划的向量随 VS1-28 验收：物理优化依次停在统计信息推导（VS1-29）、FindBestTask（VS1-28）、ResolveIndices（VS1-31）、代价（VS1-30）） |
| VS1-25 | ranger 整块 | D10.4 | depth | 0.22 | — | ranger govec 一致 | 评审中（D10.4 converge 95/95；govec `rangerbuild` 5,721 条记录、18,198 个结果一致，121 个停在 D08.9 边界；`cardinality` 的 `selectivity` 410 条 D10.4 记录已与 Go 一致） |
| VS1-27 | 物理算子与 task（VS1 算子）、`exhaustPhysicalPlans`（Sel / Proj / Sort / TopN / Limit） | D10.1、D10.3 | depth | 0.18 | VS1-19 | 随 VS1-28 验收 | 评审中（D10.1 93/110 + 16 桩 + 1 例外，D10.3 22/22；govec `physops` 3,104 条，其中 `exhaustPhysicalPlans` 1,500 条、2,350 个候选与 Go 一致；`Attach2Task` / task 收拢由单元测试覆盖，随 VS1-28 的计划向量验收。spec `specs/D10.1-physical-plans/spec.md`） |
| VS1-28 | FindBestTask：DataSource / TableDual、skyline、表路径、访问路径（`getPossibleAccessPaths`、`filterPathByIsolationRead` 等 9 个函数和句柄列栈的 `pushMap` / `popMap` 已随 VS1-21 移植，见 functions.tsv；本项解除其中 hint 以外的桩）、路径统计 | D10.2 | depth | 0.31 | VS1-26、VS1-27、VS1-29、VS1-30 | EXPLAIN 计划向量（去掉 estRows / cost）一致；`clone_node` 计数器与试点 E1/E2 两个单元测试（共享子计划的命中与回滚）进本块测试；遵守 N1 §3 第 9 条；移植 `PruneIndexesByWhereAndOrder`（纯计算，配 govec），使有两个及以上二级索引的表的候选路径与 Go 一致（D09.13 spec §3）；路由器测试：`tidb_cost_model_version=1` 时 SELECT 回退 | 评审中（第 1 部分：记忆化、TableDual / DataSource 表路径、skyline、路径统计、统计信息推导、索引裁剪；spec `specs/D10.2-find-best-task/spec.md`；D10.2 converge 73/89 + 12 桩 + 4 例外；govec `findbest` 2,208 条：计划 969 条与 Go 相同、0 条不同，索引路径 668 条停在第 2 部分的边界（其中 140 条 Go 最终选表路径），`prune` 420 条相同；sbtest1 上第一个纵向切片的 27 条语句全部与 Go 相同、Go 中没有索引读；第 2 部分：索引读、部分索引、index merge 候选） |
| VS1-29 | pseudo 基数估算、统计结构读取部分 | D10.6、D11.1 | depth | 0.21 | — | pseudo selectivity govec 一致 | 评审中（#236；D10.6 converge 38/54 + 16 桩，D11.1 85/85；govec `cardinality` 5,702 条，`selectivity` 442 条停在 D10.4 / D08.9 边界） |
| VS1-30 | 代价模型 v2（VS1 算子） | D10.5 | depth | 0.04 | VS1-19 | 代价 govec 一致；代价缓存经 `&self` 写在共享节点上，代价计算中不用 `make_mut_plan`，有测试证明计算期间 `clone_node` 为 0（N1 §3 第 9 条） | 评审中（[spec](../D10.5-cost-model/spec.md)；D10.5 converge 20/20，12 个不移植：代价明细只有代价模型 v1（例外 B5）调用、批量点查不在切片中；govec `costver2` 2,900 条与 Go 一致（amd64 / arm64，两目标差 20 条，`math.Log2`）；代价计算不复制节点（测试 `costing_shared_children_copies_no_node`：`clone_node` 为 0）；代价模型 v1 是 `blocked-by:EXC.B`；`converge --fma-sites` 只查已移植函数，#286） |
| VS1-31 | `ResolveIndices`、`ToPB`、access object（VS1 算子） | D10.9 | depth | 0.04 | VS1-27 | DAG 逐字节一致 | 未开始 |
| VS1-32 | 规划耗时的早期测量：VS1-24 / VS1-28 一能产出计划就用只含 VS1 语句的 lua 测每次 EXECUTE 的优化耗时，与 Go（plan cache 打开 / 关闭）对照 | glue | depth | 0.03 | VS1-24、VS1-28 | 报告写进 plan §10-3 | 未开始 |

Phase 2 的待定设计问题（来自 VS1-19 / VS1-10 的评审，#223 / #224）：

- **VS1-23**（已决定，见 spec D09.14 §3：嵌入，offset 统一为 `isize`）：`BaseLogicalPlan` 是否改为嵌入 `baseimpl::Plan`（与 `BasePhysicalPlan` 一致，Go 的 `BaseLogicalPlan` 嵌入 `baseimpl.Plan`）；同时统一 query block offset 的类型（物理侧 `baseimpl::Plan.qb_block` 为 `isize`，按 M-INT；逻辑侧 `BaseLogicalPlan.qb_offset` 为 `i32`）。
- **VS1-24**：`probeParents`（`propagateProbeParents` 设置）指向祖先节点，需要一种不形成 `Arc` 环的表示（计划树中不允许 `Weak`，N1 §3.4）。在此之前 `BasePhysicalPlan.probe_parents` 保留 Go 的类型，不被设置。 已决定（spec D09.13 §4.2）：probe parent 只保存父节点的类型、外侧子节点的 plan id 和统计信息，随 Apply / IndexJoin 一起改。
- **VS1-26**：代价缓存（`PlanCostInit` / `PlanCost` / `PlanCostVer2`）已改为经 `&self` 写在共享节点上（`Mutex<PlanCostCache>`，#223 评审），不再经 `make_mut_plan` 复制共享子节点。试点需统计代价计算期间 `clone_node` 的调用次数（应为 0），并确认 `taskMap` 中的共享与 Go 一致（N1 §7 第 3 条）。

## Phase 3：扫描执行与集成（C2b）

| # | 任务 | 块 | 标签 | 周 | 依赖 | 完成条件 | 状态 |
|---|---|---|---|--:|---|---|---|
| VS1-33 | coprocessor 客户端与 distsql（表路径）；P2 没赶上时用 glue 适配器 | D07.2、D07.3 | breadth | 0.20 | — | 故障场景 GREEN | 未开始 |
| VS1-34 | TableReader 执行器 | D12.1 | depth | 0.04 | VS1-13、VS1-31、VS1-33 | ab `TableReader` / `TableFullScan` / `TableRangeScan` | 未开始 |
| VS1-35 | Selection / Limit / Projection / TableDual 执行器（`ResetContextOfStmt`、`isWeakConsistencyRead` 已由 VS1-14 提前移植） | D12.2 | depth | 0.07 | VS1-13 | 单测；ab 对应分类 | 未开始 |
| VS1-36 | Sort / TopN（内存中） | D12.5 | depth | 0.08 | VS1-13 | 单测；ab `Sort` / `TopN` | 未开始 |
| VS1-37 | C2b 集成与 read_only 调优；默认值评审 | glue | depth | 0.10 | Phase 3 其余各项 | ab `strict` 的 VS1 分类 100%；read_only ≥ 0.9× Go；`Compile` 接入 `planner.Optimize` 时加真实会话 EXECUTE 读取参数的测试（需要 sessionexpr，D09.13 spec §7） | 未开始；其中"真实会话 EXECUTE 读取参数的测试"已随 `Compile` 接入优化器完成（`execute_reads_the_parameters_through_the_session_expression_context`：int / 字符串 / NULL，[D05.3 spec](../D05.3-session/spec.md) §9.4）；普通 SELECT 接入 `Compile` 的方案（语句移交给 `Compile`、写完后冻结成 `Arc`，先核对事务管理器不读规划器写的字段）见 D05.3 spec §9.2 |
| VS1-38 | （可选）EXPLAIN：只支持 VS1 算子 | D10.9、D09.16、D12.12 | depth | 0.05 | VS1-28 | ab 计划级对照 | 未开始 |
| VS1-39 | （有条件，plan §10-3）实例级 plan cache（VS1 形态）与 A3 plan cache spec | D13.1 | depth | 0.15 | VS1-15、VS1-28 | spec 经负责人确认；read_only 达标 | 未开始 |

## 合计

| | depth（周） | breadth（周） |
|---|--:|--:|
| Phase 0 | 0.32 | — |
| Phase 1 | 0.90 | 0.40 |
| Phase 2 | 1.80 | — |
| Phase 3 | 0.29 | 0.20 |
| 合计 | 3.31 | 0.60 |
| 可选 / 有条件 | 0.20 | — |
