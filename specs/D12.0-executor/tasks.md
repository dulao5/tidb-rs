# D12.0 执行器框架（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D00.1）语句标签：`GetStmtLabel`、`WithStmtLabel`（`ast.GetStmtLabel`，由 adapter / compiler 设置和读取）

## 第 1 部分：执行器框架（VS1-13）

函数清单是 `specs/VS1-vertical-slice/functions.tsv` 中 VS1-13 的 123 个函数，`scripts/converge.py --block D12.0 --part specs/VS1-vertical-slice/functions.tsv --item VS1-13` 为 123/123（L1 68、L2 55）。代码在 `crates/executor/src/`。

- [x] `internal/exec/executor.go`：`Executor` trait（Go 接口；`M-SELF`，执行器嵌入 `BaseExecutorV2`）、`BaseExecutorV2` / `BaseExecutor`、`exec.Open` / `Next` / `Close`（`Next` 的 runtime stats 与两次 KILL 检查）
- [x] `adapter.go`：`ExecStmt`（`Exec`、`PointGet`、`buildExecutor`、`openExecutor`、`next`、`FinishExecuteStmt`、`CloseRecordSet` 等）、`recordSet`、`chunkRowRecordSet`、`colNames2ResultFields`、`IsFastPlan`、`formatSQL`
- [x] `compiler.go`：`Compiler.Compile`（preprocess / `planner.Optimize` 是边界桩，VS1-10）、`needLowerPriority`、`CountStmtNode`
- [x] `builder.go`：`newExecutorBuilder`、`executorBuilder.build`（各 `buildXxx` 随执行器移植，现为边界桩，报所属块）
- [x] `detach.go`、`internal/builder/builder_utils.go`、`internal/exec/indexusage.go`、`metrics/metrics.go`、`utils.go`

承载方式（N2 / N3，L2）：

1. **会话**：执行器不持有会话（N2）。`ExecStmt.Ctx`、`recordSet.stmt`、`executorBuilder.sctx` 不是字段，改为方法参数；会话一侧的接口是 `glue::SessionContext`（`GetSessionVars`、SQL killer、事务管理器的 `GetStmtReadTS` / `GetTxnInfoSchema`、`Txn(false)`、`SetProcessInfo`、`AdviseOptimizeWithPlanAndThenWarmUp`），由 VS1-14 的会话实现。执行器建好后通过 `ExecCtx`（`Arc<StatementContext>`）读语句状态。
2. **chunk 分配器**：不用 `vars.GetChunkAllocator()`（`M-POOL`）。
3. **panic**：Go 在 `Open` / `Next` / `Close`、`Exec`、`Compile` 中 recover；这里只在 task 边界 `catch_unwind`（`M-PANIC`）。
4. **类型断言**：`*PointGetExecutor` 用 `Executor::as_point_get`（`M-IFACE`）；`testutil.MockPhysicalPlan` 是测试中的 `MockPhysicalPlan`。
5. **点查执行器缓存**：Go 在 MaxTS 时把点查执行器存进 `PsStmt.PointGet.Executor`，下次 EXECUTE 复用。原来这里在本次执行结束（`recordSet.Finish`）后放回缓存；点查短路径接通时（VS1-20b，D05.3 spec §10）这段复用去掉了：它属于 plan cache（A3 例外 §1.1），`ExecStmt.PointGet` 每次新建执行器，用完即丢。
6. **暂用的类型**（`glue`）：`ResultField`（`resolve.ResultField`，D09.0）、`PlanCacheStmt`（只有 `PointGet.Executor`，D13.1）、`Plan` = 物理计划（VS1 只有物理计划）。

不在只读节点路径上、未移植的分支（都不是 VS1-13 的函数）：悲观 DML / SELECT FOR UPDATE 的加锁重试（`blocked-by:X`）、外键级联、慢日志 / 语句摘要 / Top SQL（D14.2）、plan replayer、审计插件、runaway（N10）、Analyze（委托 Go TiDB）、`Execute` 计划（D13.1）。

边界桩（`blocked-by`）：`ExecStmt.IsReadOnly`（D09.13）、`RebuildPlan`（D09.0）、`GetTextToLog` / `getLazyStmtText` 在 `tidb_redact_log=ON` 时（D02.1，规范化 SQL）、`updatePrevStmt`（D14.2，`SessionVars.PrevStmt`）、`recordSet.TryDetach` 的静态结果集（D12.12）、`executorBuilder.build` 的各执行器（D12.1 / D12.2 / D12.5）、detach.go 的各执行器方法（D12.1 / D12.2）、`ConstructDAGReq` 与 `ToPB`（D10.9）、`IndexUsageReporter` 的统计信息（D11.2）、`BaseExecutor.GetSysSession` / `ReleaseSysSession`（D06.5）、`UpdateDeltaForTableID`（X）、`encodePassword`（D04.1）、指标向量（D14.1）、Top SQL（D14.2）、`Compile` 中的 `IsReadOnly` / `Preprocess` / `Optimize` / `GetPreparedStmt`、`StatsCount`（D09.13 / D09.0 / D13.1 / D10.1）。

MaxTS 点查的判定：`ExecStmt.PointGet` 中 `useMaxTS := startTs == math.MaxUint64`，Go 只有 MaxTS 时才缓存和复用点查执行器（这里不复用，见上面第 5 条），语句提为高优先级。单测 `adapter::tests::point_get_*` 覆盖 MaxTS / TSO / 非点查三种情况：都新建执行器，预处理语句里不留执行器。判定本身（`IsPointGetWithPKOrUniqueKeyByAutoCommit`，VS1-19；optimistic provider 的 `AdviseOptimizeWithPlan`，VS1-14）不在本部分。

## 计划回放采集检查（加载全局变量之后的后续修正）

会话加载全局变量之后，`tidb_enable_plan_replayer_capture` 与 Go 一样默认 ON。Go 在语句结束时（`FinishExecuteStmt` → `checkPlanReplayerCapture`，`adapter.go:1713`）检查 domain 的计划回放句柄里有没有与本语句 SQL digest、plan digest 匹配的采集任务（任务由句柄从 `mysql.plan_replayer_task` 加载），有就发一个 dump 任务。

- [x] 移植 `checkPlanReplayerCapture`、`checkPlanReplayerCaptureTask`、`checkPlanReplayerContinuesCapture`、`checkPlanReplayerContinuesCaptureValidStmt`（`crates/executor/src/adapter.rs`）。句柄的接口是 `glue::PlanReplayerHandle`（只有 `GetTasks`），会话通过 `SessionContext::get_plan_replayer_handle` 给出；新栈没有句柄（采集任务的加载和 dump worker 都没有移植），所以返回 None，检查什么也不做，语句照常执行，与 Go 没有句柄或没有任务时相同。
- 位置的差异：Go 在语句执行完之后检查；新栈改在 `Exec` 构建执行器之前、`PointGet` 读开始时间戳之前检查。原因是路由器规则：新栈产生副作用（执行器打开、结果发出、会话状态改变）之后再遇到边界就不能回退。没有句柄或没有任务时两个位置的结果相同；有任务时语句在还能回退的地方停下。
- 边界：有任务时要用 SQL digest 和 plan digest 去匹配，两者都还没有计算（`parser.NormalizeDigest` 与计划的规范化，D02.1），所以在算 digest 的地方返回 `blocked-by:D02.1`（`plan_replayer_task_key`）。连续采集（`tidb_enable_plan_replayer_continuous_capture`，默认 OFF）同样只在有句柄时才走到这里。`sendPlanReplayerDumpTask` 目前走不到。
- 句柄未移植：`get_plan_replayer_handle` 返回 None 登记为桩（`blocked-by:X`，计划回放句柄未移植）；`sendPlanReplayerDumpTask` 返回边界（`blocked-by:X`），不会 panic。用户可见的限制写进 `docs/compatibility.md`：tidb-rs 节点不执行采集任务。
- 测试：`adapter::tests::plan_replayer_capture_stops_only_with_a_task`（没有句柄、句柄没有任务：照常执行；模拟一个采集任务：在构建执行器之前、读时间戳之前报边界；受限 SQL、开关关闭不检查）；会话侧 `plan_replayer_capture_records_the_table_stats_and_goes_on`（开关打开、没有任务的非点查 EXECUTE 记录表统计信息后继续，规划完成，停在构建表读取执行器 D12.1）。
- 桩：去掉规划器里"开关打开就停"的桩（D02.2），新增 digest 一处（D02.1）。棘轮：规划器 371 → 370，执行器 51 → 55（digest 一处是同一个缺口从"所有语句"缩小到"有采集任务的语句"的细化；句柄与 dump 任务两处是如实登记未移植的部分）。

