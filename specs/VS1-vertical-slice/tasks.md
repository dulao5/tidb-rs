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
| VS1-10 | planbuilder 骨架、preprocess（SELECT）、`planner.Optimize` / `optimize` | D09.0、D09.13 | depth | 0.17 | VS1-01 | govec preprocess 向量一致 | 未开始 |
| VS1-11 | `TryFastPlan`、`PointGetPlan` | D10.8 | depth | 0.07 | VS1-10、VS1-19 | 点查计划向量一致 | 未开始 |
| VS1-19 | `property`、`planner/util`、`baseimpl`、`base_physical_plan` | D10.10、D10.1 | depth | 0.11 | VS1-01 | Go 单测与 govec 通过 | 进行中（#223） |
| VS1-12 | 点查执行器、`NewRowDecoder`；table 读取部分 | D12.1、D06.4 | depth + breadth | 0.13 | VS1-13、VS1-18 | 行解码 / 默认值 govec；ab `Point_Get` | 未开始 |
| VS1-13 | `ExecStmt`、`recordSet`、`Compiler`、`executorBuilder.build`、`internal/exec` | D12.0 | depth | 0.15 | VS1-01 | 单测通过；MaxTS 点查判定与 Go 一致 | 评审中（#222） |
| VS1-14 | 会话 `ExecuteStmt` / `PrepareStmt` / `ExecutePreparedStmt`、`LazyTxn`、txnManager、sessiontxn 只读 provider；回退时的检查点与恢复（plan §4.6）；预处理语句四种组合与按 stmt id 路由（§4.7） | D05.3、D05.4 | depth + breadth | 0.24 | VS1-13 | N2 §5 断言；autocommit 读、TSO 预取、warning 与 Go 一致；每种回退原因的 `SessionVars` 快照前后相等，SHOW WARNINGS 只有 PoC 的 warning；四种组合与 RESET / LONG_DATA / FETCH / CLOSE 用例；ab 比较 PREPARE 列元数据与每次 EXECUTE 的列定义（含反证）；`RecordSet` 没有 `Drop`（#222）：语句中途被丢弃（取消、KILL、断连）时，会话在下一条语句之前补做未完成的 finish（`close_record_set` / `FinishExecuteStmt`），用例检查中途丢弃后的会话状态 | 未开始 |
| VS1-15 | `PlanCacheStmt`、`GetPlanFromPlanCache` 未命中路径、可缓存性检查、`PrepareExec` | D13.1、D12.12 | depth | 0.08 | VS1-14 | 可缓存性 govec；ab 预处理语句部分 | 未开始 |
| VS1-16 | 结果集与 `writeResultSet` / `writeChunks`、`column` / `dump`、`conn_stmt` 执行与二进制参数 | D03.1 | breadth | 0.13 | VS1-14 | dump govec；protocol-smoke 通过 | 未开始 |
| VS1-17 | D06.3 第 2 部分（T8 / T9）；D06.5 `loadInfoSchema` / `tryLoadSchemaDiffs` / validator，由 PoC domain 驱动 | D06.3、D06.5 | breadth | 0.10 | — | schema 同步测试在新 InfoSchema 上通过 | 未开始 |
| VS1-18 | snapshot `Get` / `BatchGet` / `SetOption`、错误转换 | D07.1 | breadth | 0.04 | — | 故障场景点查与 PoC 相同 | 未开始 |
| VS1-20 | C2a 集成与 point_select 调优 | glue | depth | 0.08 | Phase 1 其余各项 | ab `strict` 的 `Point_Get` 100%；point_select ≥ 0.9× Go（1 / 8 线程） | 未开始 |

## Phase 2：规划器主体

| # | 任务 | 块 | 标签 | 周 | 依赖 | 完成条件 | 状态 |
|---|---|---|---|--:|---|---|---|
| VS1-26 | `taskMap` 小试点（spike） | N1 | depth | 0.05 | VS1-01 | 试点报告；不通过时先修订 N1 | 未开始 |
| VS1-21 | `buildSelect` 等 SELECT 构建，加 `resolveHavingAndOrderBy` | D09.1、D09.4 | depth | 0.18 | VS1-10、VS1-22、VS1-23 | 逻辑计划向量一致 | 未开始 |
| VS1-22 | 表达式改写（子查询之外） | D09.2 | depth | 0.14 | VS1-10 | 表达式向量一致 | 未开始 |
| VS1-23 | 逻辑算子公共部分，以及列裁剪、投影消除、谓词下推与简化、TopN / Limit 下推（VS1 的算子）；M-FT 审计 | D09.14、D09.7、D09.8、D09.11 | depth | 0.21 | VS1-01 | 规则级计划向量一致；审计表写进 plan | 未开始 |
| VS1-24 | `DoOptimize`、`postOptimize`、`InjectExtraProjection`、`ResultReorder`；hint 骨架 | D09.13、D13.3 | depth | 0.19 | VS1-21 | 物理计划向量一致 | 未开始 |
| VS1-25 | ranger 整块 | D10.4 | depth | 0.22 | — | ranger govec 一致 | 未开始 |
| VS1-27 | 物理算子与 task（VS1 算子）、`exhaustPhysicalPlans`（Sel / Proj / Sort / TopN / Limit） | D10.1、D10.3 | depth | 0.18 | VS1-19 | 随 VS1-28 验收 | 未开始 |
| VS1-28 | FindBestTask：DataSource / TableDual、skyline、表路径、访问路径、路径统计 | D10.2 | depth | 0.31 | VS1-26、VS1-27、VS1-29、VS1-30 | EXPLAIN 计划向量（去掉 estRows / cost）一致 | 未开始 |
| VS1-29 | pseudo 基数估算、统计结构读取部分 | D10.6、D11.1 | depth | 0.21 | — | pseudo selectivity govec 一致 | 未开始 |
| VS1-30 | 代价模型 v2（VS1 算子） | D10.5 | depth | 0.04 | VS1-19 | 代价 govec 一致 | 未开始 |
| VS1-31 | `ResolveIndices`、`ToPB`、access object（VS1 算子） | D10.9 | depth | 0.04 | VS1-27 | DAG 逐字节一致 | 未开始 |
| VS1-32 | 规划耗时的早期测量：VS1-24 / VS1-28 一能产出计划就用只含 VS1 语句的 lua 测每次 EXECUTE 的优化耗时，与 Go（plan cache 打开 / 关闭）对照 | glue | depth | 0.03 | VS1-24、VS1-28 | 报告写进 plan §10-3 | 未开始 |

## Phase 3：扫描执行与集成（C2b）

| # | 任务 | 块 | 标签 | 周 | 依赖 | 完成条件 | 状态 |
|---|---|---|---|--:|---|---|---|
| VS1-33 | coprocessor 客户端与 distsql（表路径）；P2 没赶上时用 glue 适配器 | D07.2、D07.3 | breadth | 0.20 | — | 故障场景 GREEN | 未开始 |
| VS1-34 | TableReader 执行器 | D12.1 | depth | 0.04 | VS1-13、VS1-31、VS1-33 | ab `TableReader` / `TableFullScan` / `TableRangeScan` | 未开始 |
| VS1-35 | Selection / Limit / Projection / TableDual 执行器 | D12.2 | depth | 0.07 | VS1-13 | 单测；ab 对应分类 | 未开始 |
| VS1-36 | Sort / TopN（内存中） | D12.5 | depth | 0.08 | VS1-13 | 单测；ab `Sort` / `TopN` | 未开始 |
| VS1-37 | C2b 集成与 read_only 调优；默认值评审 | glue | depth | 0.10 | Phase 3 其余各项 | ab `strict` 的 VS1 分类 100%；read_only ≥ 0.9× Go | 未开始 |
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
