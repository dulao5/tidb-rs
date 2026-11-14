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
5. **点查执行器缓存**：Go 让 `PsStmt.PointGet.Executor` 和本次执行共用一个指针；这里执行器在本次执行结束（`recordSet.Finish`）后放回缓存。只在 MaxTS 时读写缓存，所以行为相同。
6. **暂用的类型**（`glue`）：`ResultField`（`resolve.ResultField`，D09.0）、`PlanCacheStmt`（只有 `PointGet.Executor`，D13.1）、`Plan` = 物理计划（VS1 只有物理计划）。

不在只读节点路径上、未移植的分支（都不是 VS1-13 的函数）：悲观 DML / SELECT FOR UPDATE 的加锁重试（`blocked-by:X`）、外键级联、慢日志 / 语句摘要 / Top SQL（D14.2）、plan replayer、审计插件、runaway（N10）、Analyze（委托 Go TiDB）、`Execute` 计划（D13.1）。

边界桩（`blocked-by`）：`ExecStmt.IsReadOnly`（D09.13）、`RebuildPlan`（D09.0）、`GetTextToLog` / `getLazyStmtText` 在 `tidb_redact_log=ON` 时（D02.1，规范化 SQL）、`updatePrevStmt`（D14.2，`SessionVars.PrevStmt`）、`recordSet.TryDetach` 的静态结果集（D12.12）、`executorBuilder.build` 的各执行器（D12.1 / D12.2 / D12.5）、detach.go 的各执行器方法（D12.1 / D12.2）、`ConstructDAGReq` 与 `ToPB`（D10.9）、`IndexUsageReporter` 的统计信息（D11.2）、`BaseExecutor.GetSysSession` / `ReleaseSysSession`（D06.5）、`UpdateDeltaForTableID`（X）、`encodePassword`（D04.1）、指标向量（D14.1）、Top SQL（D14.2）、`Compile` 中的 `IsReadOnly` / `Preprocess` / `Optimize` / `GetPreparedStmt`、`StatsCount`（D09.13 / D09.0 / D13.1 / D10.1）。

MaxTS 点查的判定：`ExecStmt.PointGet` 中 `useMaxTS := startTs == math.MaxUint64`，只有 MaxTS 时才缓存和复用点查执行器，语句提为高优先级。单测 `adapter::tests::point_get_*` 覆盖 MaxTS / TSO / 非点查三种情况。判定本身（`IsPointGetWithPKOrUniqueKeyByAutoCommit`，VS1-19；optimistic provider 的 `AdviseOptimizeWithPlan`，VS1-14）不在本部分。
