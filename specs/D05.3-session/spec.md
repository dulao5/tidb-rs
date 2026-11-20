# D05.3 会话执行主流程（spec）

状态：第 1 部分（VS1-14，会话执行主流程）已合入（#235）；第 2 部分（VS1-14b，预处理语句）评审中（2026-11-15），见 §6。块：D05.3（`pkg/session`），crate `tidb-session`（N0 rank 15）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §4.1、§4.3、§4.5–§4.7，VS1-14；[D05.4 spec](../D05.4-sessiontxn/spec.md)（sessiontxn，#219）；[D12.0 tasks](../D12.0-executor/tasks.md)（`ExecStmt`，#222）；[N2](../exceptions/N2-session-context/spec.md)；[N11](../exceptions/N11-cluster-readts/spec.md)；ADR 0010（会话与协议无关）。

## 0. 分两个 PR

VS1-14 太大，拆成两部分，按顺序合入：

| 部分 | 工作项 | 内容 |
|---|---|---|
| 第 1 部分（#235，已合入） | `VS1-14` | `ExecuteStmt` / `runStmt` / `finishStmt`、`LazyTxn` 的只读部分、`txnManager`、sessiontxn provider 的接入、`ResetContextOfStmt`（从 VS1-35 提前）、回退时的检查点与恢复（plan §4.6）、中途丢弃的结果集的补做（#222）、KILL 标志到 coprocessor 读（D07.2 的注） |
| 第 2 部分（§6） | `VS1-14b` | `PrepareStmt` / `ExecutePreparedStmt` / `DropPreparedStmt` / `resolvePreparedStmt` / `GetSessionPlanCache`，预处理语句四种组合与按 stmt id 路由（plan §4.7）的新栈一侧，列元数据的 ab 检查 |

`specs/VS1-vertical-slice/functions.tsv` 里第 2 部分的 5 个函数改记在 `VS1-14b` 名下，converge 按部分分别计算。

**路由器在进新栈之前检查会话空闲**（#235 评审）：`is_idle()` 不成立时不进新栈，交给 PoC，原因 `not_idle`，记为路由器的 bug（日志 + `tidbrs_route_total{reason="not_idle"}`）；只有新栈能执行的预处理语句报 1235。这样不会出现新栈写了状态、恢复又失败的情况。另外，执行器打开之后的回退（plan §4.3 不允许；`newstack.rs` 在结果集编码完成前会这样做，VS1-16 去掉）由路由器检测，记日志与 `reason="bug_blocked_after_open"`。

**第 1 部分中预处理语句一律走 PoC**（Lucky，VS1-14 评审要求）：路由器在 COM_STMT_PREPARE 时不再调用新栈，stmt id 直接归 PoC，原因记为 `prepared`（`tidbrs_route_total{engine="poc",reason="prepared"}`），`fallback` 与 `strict` 模式相同（`strict` 也不报 1235：这不是新栈碰到了桩，而是本部分不提供）。会话的 `prepare_stmt` / `execute_prepared_stmt` / `drop_prepared_stmt` 仍是 `blocked-by:D05.3` 的桩（第 2 部分）。§4.7 的路由表、列元数据检查（VS1-02 已写好）保留不动，第 2 部分打开。**第 2 部分已经取代这一段**：原因 `prepared` 与只在测试中打开 §4.7 的开关都已删除（§6.2）。

## 1. 范围（第 1 部分）

### 1.1 Go 函数

VS1-14 在 D05.3 中的 129 个函数去掉第 2 部分的 5 个，剩 124 个；D05.4 的 41 个已由 #219 移植（本 PR 补齐 6 个写路径函数的"不移植"登记，见 §1.3）；另从 VS1-35 提前 `ResetContextOfStmt` 与 `isWeakConsistencyRead`（D12.2，见 §2.4）。

| Go | Rust（`crates/session/src/`） | 内容 |
|---|---|---|
| `session.go` | `session.rs` | `ExecuteStmt`、`runStmt`、`PrepareTxnCtx`、`PrepareTSFuture`、`GetPreparedTxnFuture`、`Txn`、`CommitTxn` / `doCommit` / `doCommitWithRetry`（只读）、`RollbackTxn`、`rollbackOnError`、`RefreshTxnCtx`、`onTxnManagerStmtStartOrRetry`、`setRequestSource`、`execStmtResult`、`errIsNoisy`、`isEndTxnStmt`、`logStmt` / `logGeneralQuery` / `sqlForLog`、各取值方法（`GetSessionVars`、`GetStore`、`GetClient`、`GetInfoSchema`、`GetDomainInfoSchema`、`AffectedRows`、`LastMessage`、`Status`、`String`、`Value` / `SetValue` / `ClearValue`、`SetConnectionID` / `SetClientCapability` / `SetCommandValue`、`GetSQLLabel`、`isInternal`）、`Close`、`Parse` / `ParseSQL` |
| `txn.go` | `txn.rs` | `LazyTxn` 的三种状态（invalid / pending / valid）、`txnFuture.wait`、`changeToPending` / `changePendingToValid` / `changeToInvalid`、`Wait`、`Commit` / `Rollback`（只读事务）、`Valid`、`String`、`GetOption`、`Size`、`reset` / `cleanup`、事务信息（`init`、`updateState`、`resetTxnInfo`、`onStmtStart`、`onStmtEnd`） |
| `txnmanager.go` | `txnmanager.rs` | `txnManager` 全部：`EnterNewTxn` / `newProviderWithRequest`（optimistic、悲观 RR / RC / serializable、staleness）、`OnStmtStart`、`OnStmtEnd`、`OnTxnEnd`、`ActivateTxn`、`GetStmtReadTS`、`AdviseWarmup`、`AdviseOptimizeWithPlan`、事件记录 |
| `tidb.go` | `tidb.rs` | `finishStmt`、`autoCommitAfterStmt`、`checkStmtLimit`、`GetHistory`、`recordAbortTxnDuration`、`Parse` |
| `metrics/metrics.go` | `metrics.rs` | 指标变量（D14.1 的桩） |
| `executor/select.go` | `crates/executor/src/select.rs` | `ResetContextOfStmt`、`isWeakConsistencyRead`（§2.4） |

### 1.2 边界桩（临时，登记在 tracker `data/stubs.tsv`）

| 函数 | 块 | 原因 |
|---|---|---|
| `GetExprCtx`、`GetPlanCtx`、`GetRangerCtx` | D05.3 | 会话背后的 `ExprContext`（`sessionexpr`，D05.3 tasks P1-1，不在 VS1-14 的清单里）。#234 之后执行器通过 `SessionContext::get_expr_ctx` 取它：在 `sessionexpr` 移植之前，会话给出一个按 Go 默认值构造的静态 `ExprContext`（不反映会话变量），C2a 之前必须换成真正的；规划器拿的是 `PlanCtx`（N2），由 `Compile` 构造，`Compile` 现在还是 `blocked-by:D09.13` |
| `GetDistSQLCtx` | D07.3 | `DistSQLContext` 随 VS1-33 |
| `GetBuildPBCtx` | D10.9 | `BuildPBContext` 随 `ToPB` |
| `GetTableCtx` | D06.4 | `tblctx`，写路径之外只有 D06.4 第 2 部分用 |
| `GetDomain`、`getSnapshotInfoSchema` | D06.5 | domain；`tidb_snapshot` 的语句由路由器交给 PoC |
| `SetProcessInfo`、`ShowProcess`、`UpdateProcessInfo` | D00.3 | `util.ProcessInfo`（SHOW PROCESSLIST 仍由 PoC 回答）；会话只记下 SQL、命令、时间与 `max_execution_time`，供以后接上 |
| `GetSessionManager`、`SetSessionManager` | D03.1 | server 的会话管理器 |
| `SetCollation` | D02.2 | `charset.GetCharsetInfoByID` |
| `InitMetricsVars`、`init` | D14.1 | 会话指标 |
| `kv.Storage` / `oracle.Oracle` 的实现（PoC 接线处） | D07.1 | store driver 的 `Begin` / `GetOracle` 还没有移植；PoC 的 `NewSession` 用一个报 `blocked-by:D07.1` 的存储。当前 `Compile` 在取 TSO 之前就返回 `Blocked`，所以不影响行为 |
| `PrepareStmt` 等第 2 部分的 5 个 | — | 第 2 部分已移植，剩下的桩见 §6.5 |

### 1.3 不移植（写路径，永久）

tidb-rs 是只读节点（N11 D8）。`LazyTxn` 的 staging buffer 与加锁：`CacheTableInfo`、`GetTableInfo`、`KeysNeedToLock`、`LockKeys`、`LockKeysFunc`、`StartFairLocking`、`RetryFairLocking`、`CancelFairLocking`、`DoneFairLocking`、`IsInFairLockingMode`、`RollbackMemDBToCheckpoint`、`flushStmtBuf`、`cleanupStmtBuf`、`Mem`；`txnManager.SetOptionsBeforeCommit`。D05.4 中 #219 没有标注的 6 个（`SetTxnAssertionLevel`、`SetOptionsBeforeCommit`、`temporaryTableKVFilter.IsUnnecessaryKeyValue`、`OptimisticTxnContextProvider.onTxnActive`：写路径；`OnLocalTemporaryTableCreated`：临时表；`funcFuture.Wait`：已作为 RC 语句 future 的 `UseStartTs` 变体移植）同样写入 `functions.tsv` 的 `not_ported` 列。Go 测试工具 `GetRows4Test`、`ResultSetToStringSlice`（testkit 用）也不移植。

`runStmt` 中只与写有关的分支（pipelined DML 的错误处理、`IsExplainAnalyzeDML` 的提交、`hasFileTransInConn`）照 Go 保留判断，分支体是写路径时标注为不可达。

## 2. 承载方式

### 2.1 会话的结构（N2）

```text
Session
 ├── core: SessionCore          实现 executor 的 glue::SessionContext 与 sessiontxn 的 SessionCtx
 │     ├── vars: SessionVars
 │     ├── txn: LazyTxn          invalid / pending(txnFuture) / valid(kv.Transaction)
 │     ├── txn_manager: TxnManager（拥有当前 provider）
 │     ├── killer: Arc<SqlKiller>（N2 SessionShared；KILL、max_execution_time）
 │     ├── store: Arc<dyn Storage>、oracle: Arc<dyn Oracle>
 │     ├── domain_is: InfoSchema（VS1-17 之前是空的 v2）
 │     └── values: Go 的 session.mu.values
 └── running: Option<RunningStmt>  正在返回行的语句（ExecStmt + 补做所需的状态，§2.3）
```

- Go 的 `GetTxnManager(sctx)` 把 txnManager 存在 `SessionVars.TxnManager` 里；这里由会话直接持有，provider 的每个方法传入会话（D05.4 spec §2）。`txnManager` 调用 provider 时要同时借用 manager 与会话：manager 先把 provider 取出，调用后放回（L2，注释写明）。
- `core` 与 `running` 分开，结果集的 `next` / `close` 可以同时借用 `&mut ExecStmt`（在 `running` 里）和会话（`core`），而且在 `await` 期间 `ExecStmt` 一直留在会话中：future 被取消时，补做所需的状态不会丢（§2.3）。
- `Session: Send`、`SessionCore: Send`、`ExecStmtResult: Send` 的静态断言；`SessionVars: !Sync` 用 `compile_fail` 文档测试固定（N2 §5 第 1 条）。`ExecCtx: Send + Sync` 已在 `tidb-executor`（#222）。

### 2.2 事务（`LazyTxn`、`txnFuture`）

- 三种状态照 Go。`txnFuture` 持有 sessiontxn 的 `TxnTsFuture`（`ConstantFuture` 或 TSO 请求），`wait` 等到 ts 后以 `Storage::begin([TxnScope, StartTS])` 得到 `kv::Transaction`。TSO 出错时 Go 不带 start ts 再 begin（client-go 自己取 TSO）；`tidb-kv` 的 `begin` 同样由实现决定（L1）。
- `kv::Transaction` 没有 `SetVars` / `IsPipelined`（只读路径不需要）：`LazyTxn` 记下 `KVVars`，`IsPipelined` 恒为 false（L2）。
- 只读事务的提交：`doCommit` 在 `IsReadOnly()` 时直接返回（Go 也是），所以 `CommitTxn` 只做 `changeToInvalid`、`SetInTxn(false)`、`OnTxnEnd`、`TxnCtx.Cleanup` 等状态清理；非只读事务的提交是 `blocked-by:X`（写路径，N11 D8），新栈上不会出现。
- 事务信息（`txninfo.TxnInfo`，`information_schema.tidb_trx` 用）照 Go 维护最小字段；`TxnDurationHistogram` 等指标是 D05.4 第 2 部分。语句 digest（`StmtCtx.SQLDigest`，D02.1 normalize）还没有，传空串，与 Go 中 digest 为空时一样直接返回。

### 2.3 中途丢弃的结果集（#222 留下的要求）

`recordSet` 没有 `Drop`：`finish` / `close` 是 async 且需要会话。Go 在协议层用 `defer rs.Close()` 保证 `finishStmt` 与 `ExecStmt.CloseRecordSet` 一定执行；Rust 中语句的 future 可能被取消（KILL、断连、客户端中断）。

做法（含 #235 评审的修改）：

- `runStmt` 返回行时，`ExecStmt` 留在会话的 `running` 里；`recordSet` 放在一个由结果与会话共享的槽中（`Arc<Mutex<Option<RecordSet>>>`，锁只在取出 / 放回时持有，不跨 `await`）。`ExecStmtResult` 的方法以 `&mut Session` 为参数，调用时把 `recordSet` 取出、用完放回；`close` 完成后清空 `running`。`next` 遇到的错误同时记入 `running`。
- **结果与语句绑定**：每条返回行的语句有一个序号，同时存在 `running` 和结果里。会话已经结束了那条语句（下一条 `ExecuteStmt` 会先结束没关闭的语句）之后，旧结果的 `next` / `fields` 返回错误，`finish` / `close` 什么也不做，不会动到新语句（Lucky：保留结果 A、执行 B、再关 A，以前会关掉 B 的语句）。
- 下一次 `ExecuteStmt`（以及路由器取检查点之前）调用 `finish_unfinished_stmt`：
  - 结果还在（保留了但没关）：槽里有 `recordSet`，照 Go 的 `execStmtResult.Close` 执行（`Finish`：关闭执行器、`finishStmt`；再 `ExecStmt.CloseRecordSet`）。
  - 结果已丢弃（或某个方法的 future 在取出期间被取消）：`recordSet` 随之丢弃，只补做会话一侧：`finishStmt(nil)`（autocommit 下 `CommitTxn`，`LazyTxn` 回到 invalid、`OnTxnEnd`），再 `CloseRecordSet(txnStartTS, lastErrs)`。执行器的 `Close` 不再执行（§4 第 6 条）；点查执行器不再放回缓存，下次重建。

### 2.4 `ResetContextOfStmt` 提前到本项

Go 的 `ExecuteStmt` 在 `Compile` 之前调用 `executor.ResetContextOfStmt`（`select.go`，D12.2），它安装新的 `StmtCtx`，并写入 `SysWarningCount` / `SysErrorCount`、`PrevAffectedRows`、`PrevLastInsertID`、`PrevFoundInPlanCache`、类型标志与错误级别等——正是 §4.5 / §4.6 关心的"上一条语句"的状态和 warning 计数。plan 把它排在 VS1-35（Phase 3），但 C2a 的点查就需要它，所以本项提前移植（`functions.tsv` 中 `ResetContextOfStmt`、`isWeakConsistencyRead` 改记 VS1-14），放在 `tidb-executor` 的 `select.rs`。

- 不移植的部分（各自是其他块的边界桩，在代码中标注）：会话级内存 / 磁盘 tracker 的重新挂接与 OOM 动作（`SessionVars.MemTracker` 未移植，A1）、`GlobalAnalyzeMemoryTracker`（ANALYZE 委托给 Go TiDB）、EXECUTE 的预处理语句的 SQL 摘要（`parser.NormalizeDigest` 未移植，为空）、Top SQL / pprof 标签（D14.2）、`IndexUsageCollector`（D11.2）、`CTEStorageMap` / `TblInfo2UnionScan`（这些执行器未移植，没有要清的东西）、`SetForcePlanCache` / `SetAlwaysWarnSkipCache`（plan cache，D13.1）、`ctx.GetPlanCtx().Reset()`（会话没有常驻的 `PlanCtx`，N2：每次规划新建）。
- EXECUTE 照 Go 处理：取预处理语句（`GetPreparedStmt`），按它（不是 EXECUTE 节点）设置语句上下文，`OriginalSQL` 取它的 SQL 文本；预处理语句是不带锁的 SELECT 时先算 EXECUTE 节点的 `isWeakConsistencyRead`（这一步会再取一次语句，所以放在持有语句锁之前，结果相同）。
- `isWeakConsistencyRead` 调 `plannercore.IsReadOnly`，其中的 `ast.IsReadOnly` 是 `blocked-by:D02.2` 的桩（EXECUTE 分支已移植）；只有 `tidb_read_consistency=weak` 时才会走到（Go 的 `&&` 短路照搬），而这个变量不在同步白名单里，路由器第 0 步已把这种会话交给 PoC。因此它返回 `Result<bool>`，错误向上传播（M-ERRVAL）。
- govec：`resetstmt` 领域（tracker），见 §5。

### 2.5 KILL 标志到 coprocessor 读（D07.2 的注）

Go 的 `SessionVars.KVVars = tikvstore.NewVariables(&vars.SQLKiller.Signal)`：coprocessor worker 的 `finished()` 读这个指针。这里：

- `SqlKiller.signal` 改为 `Arc<AtomicU32>`（字段访问不变，`Deref`），`SqlKiller::signal_handle()` 给出共享的句柄（Go 的 `&SQLKiller.Signal`）；
- 会话的 `copr_vars()` 生成 `tidb_store::copr::tikv::Variables { backoff_weight: KVVars.BackOffWeight, killed: Some(killer.signal_handle()) }`，以后 `GetDistSQLCtx`（VS1-33）用它；
- `poc-kv` 的 `KvSnapshot::coprocessor` 增加 `killed` 参数，调用方把语句的 kill 标志传进去；PoC 自己没有 KILL，传 `None`，与现在相同。

测试：KILL 之后 `copr_vars().killed` 读到信号，下一条语句的 `ResetContextOfStmt` 清零（`tests.rs`）；`tidb-store` 的 `copr_iter.rs` 新增 `kill_signal_stops_the_workers`：卡在锁上的 worker 在信号置位后停止访问存储。

## 3. 回退时的检查点与恢复（plan §4.6）

### 3.1 新栈这条路径实际写的字段（逐字段，依据本 PR 的 L1 代码）

| 写入者 | 字段 |
|---|---|
| `PrepareTxnCtx` → `EnterNewTxn` → provider `OnInitialize` | `TxnCtx`（整个替换：`InfoSchema`、`TxnScope`、`IsPessimistic`、`Isolation`、`EnableMDL`），`InTxn`（`WithBeginStmt` 时），会话的 `txn_manager`（provider、事件），`LazyTxn`（`EnterNewTxnDefault` 时 pending / valid） |
| `ResetContextOfStmt` | `StmtCtx`（新的 `Arc`），`SysWarningCount`、`SysErrorCount`，`PrevFoundInPlanCache` / `FoundInPlanCache`，`PrevFoundInBinding` / `FoundInBinding`，`DurationWaitTS`，`PlanCacheParams`（`Reset`），chunk 分配状态（`ExchangeChunkStatus`：`PrevStmtUseChunkAlloc`），`SetVarHintRestore` 恢复的系统变量，killer 的 `Reset`、`ConnID` |
| `ExecuteStmt` | `StartTime`、`DurationCompile`，`LazyTxn` 的事务信息，`RequestSourceType`，`txn_manager` 的语句（`stmtNode`）与事件 |
| `OnStmtStart` → provider | provider 的语句状态（`stmt_tso_requests` 等），RC 的语句 ts future |
| `Compile` / `Optimize` | `StmtCtx.IsReadOnly`、`StmtCtx` 的 plan、`PlanID` / `PlanColumnID` 计数、warnings；`AdviseWarmup` → `PrepareTSFuture`：`LazyTxn` 变为 pending（TSO 请求已发出） |
| 出错时 `rollbackOnError` → `RollbackTxn` | `LazyTxn` → invalid，`TxnCtx.Cleanup`，`CleanupTxnReadTSIfUsed`（`TxnReadTS`），`InTxn = false`，`OnTxnEnd`（provider 清空） |
| `runStmt` 之后（没有回退：执行器已打开） | `TxnCtx.StatementCount`、`LastFoundRows`、`LastQueryInfo`、`StmtCtx` 的各计数 |

回退只发生在执行器打开之前（plan §4.3 第 3 步），所以最后一行不在恢复范围内，但检查点仍整体保存 `SessionVars` 的标量字段与非标量状态（VS1-02 的做法，成本见 `checkpoint.rs`），不按上表裁剪：裁剪只会增加漏字段的风险，而开销已经很小（约 0.3 µs）。上表用于测试：每一行至少有一个用例证明它被恢复。

### 3.2 会话侧的状态

`SessionVars` 之外，新会话还有 `LazyTxn`、`txn_manager`、`running`、killer 的信号。路由器第 0 步保证新栈只在 autocommit、不在事务中时执行，此时上一条语句结束后（Go 的 `CommitTxn` / `RollbackTxn` → `OnTxnEnd`）会话是"空闲"的：`LazyTxn` 既不 valid 也不 pending、`InTxn` 为 false、没有 `running`。（`Compile` 之前就出错的语句会留下一个 lazy 的 provider，Go 也一样；下一条语句的 `PrepareTxnCtx` 会换掉它，所以不算"不空闲"。）检查点记录这一点；不空闲时检查点标记为不可恢复，`restore` 返回错误，路由器不回退而报 1105，作为 `bug` 处理。恢复时把三者重置为空闲：丢弃 pending 的 TSO future、清掉 provider（等同 Go 的 `RollbackTxn`）。killer 的信号不恢复：KILL 是外部事件，回退后 PoC 也应看到。

### 3.3 TSO future

plan §4.6 第 2 条：新栈已发出的 TSO 请求交给 PoC。PoC 的 `TsFuture` 是 `poc-kv` 的 `JoinHandle`，而新会话的 oracle 现在是 `blocked-by:D07.1` 的桩（§1.2），而且 `Optimize` 在 `AdviseTxnWarmup` 之前（`TryFastPlan` / `buildLogicalPlan`）就返回 `Blocked`：目前没有可以交接的请求。本 PR 按 plan 的退路"丢弃"，`LazyTxn::take_pending_future` 留出接口；新会话接上真正的 oracle（C2a 集成，VS1-20）时再做交接，那时 `Optimize` 也开始走到 warmup。

### 3.4 测试

- **每种回退原因的快照测试**：`Blocked` 发生在路径的不同位置——`Compile` 之前（`ResetContextOfStmt` 中的 `blocked`）、`Compile` 中（规划器的桩，`D09.13`）、warmup 之后（TSO 已发出，`LazyTxn` pending）、`OnStmtStart` 之后的 provider 状态。每个位置比较回退前后的 `state_snapshot()`，并检查 SHOW WARNINGS 只剩 PoC 的 warning（路由器测试）。
- **与新会话相等**（Lucky）：在全新的会话上执行一条回退的语句，之后的快照等于全新会话的快照，逐项断言事务（`InTxn`、`TxnCtx`、`LazyTxn` 状态、provider）、autocommit、snapshot（`tidb_snapshot`、`TxnReadTS`）和上一条语句的状态（`SysWarningCount`、`PrevAffectedRows`、`LastFoundRows`、`LastQueryInfo`、`FoundInPlanCache`）。
- **随机交替测试**：随机序列"走新栈（以会话 API 模拟成功语句）/ 回退 / 走 PoC（以 `LastStmtInfo` 同步模拟）"，每次回退之后快照等于回退前；序列结束时与只走 PoC 的同一序列的快照相等（§4.5 的同步字段）。与 Go 的 SQL 级对照要等新栈能跑语句（C2a，VS1-20）。
- **中途丢弃**：结果集在第一个 chunk 之后丢弃，下一条语句之前会话补做 finish，`LazyTxn` invalid、`LastQueryInfo` 已写、`running` 为空。由于执行器还没有移植（`build` 是桩），这个测试用 `tidb-executor` 的测试替身（`testutil::MockPhysicalPlan`，Go 的 `testutil.MockPhysicalPlan`，在 `test-util` feature 之后）。

## 4. 与 Go 的差异（均已在代码中标注）

1. `ExecuteStmt` 不注册 Top SQL、plan replayer 捕获、`DebugTraceReceivedCommand`（D14.2 / D13.x / 优化器 trace），不做资源组 hint 的回退检查（N10）。`validateStatementInTxn`（显式只读事务里的写 → 1792，N11 D8）与 `validateStatementReadOnlyInStaleness`：路由器只把 autocommit 下的 SELECT 交给新栈，两者对 SELECT 都直接返回；移植为只覆盖 SELECT 的 L2，其他语句是 `blocked-by:D12.12`（第 2 部分的 BEGIN / COMMIT 随 D12.12）。
2. `loadCommonGlobalVariablesIfNeeded`：会话变量由路由器从 PoC 同步（plan §4.1），全局值来自编译期默认值（VS1-17 接上 domain 的 sysvar 缓存）；这里是 `glue`，只置位"已加载"。
3. `SetProcessInfo` 只记下参数（§1.2）。
4. `doCommitWithRetry` 不重试（只读事务的提交不会失败；`CouldRetry` 恒为 false，D05.4 spec §4）。
5. `ExecuteStmt` 的两个 `defer`（`txn.onStmtEnd`、`OnStmtEnd`）用 drop guard 实现（M-DEFER），语句的 future 被取消时也执行。
6. 结果集被**丢弃**（不是保留不关）时，执行器的 `Close` 不执行：执行器的 runtime stats、内存 tracker 的释放记录等 `Close` 里做的事被跳过（资源随所有权释放）。会话一侧的结束（`finishStmt`、`CloseRecordSet`、`LastQueryInfo`）照常补做（§2.3）。
7. `txnManager` 不预留两个 optimistic provider 轮换（`reservedOptimisticProviders`：Go 为减少分配；Rust 每个事务新建，M-POOL）。

## 5. 测试与 govec

- `crates/session/src/tests.rs`（单测，能用 crate 内部的步骤）：模拟存储与 TSO（计数请求与等待），覆盖 autocommit 读的 TSO 预取（`AdviseWarmup` 发出、`GetStmtReadTS` 等待、语句结束后 `LazyTxn` invalid、`InTxn` false）、点查 MaxTS（不取 TSO）、`ExecuteStmt` 在 `Compile` 返回错误时的 `rollbackOnError`、`finishStmt` / `autoCommitAfterStmt`、§3.4 的全部用例、N2 断言。
- govec `resetstmt`（tracker，块 D12.2 中提前的两个函数）：随机的 `sql_mode`、语句种类（SELECT 带或不带 `SQL_NO_CACHE` / `HIGH_PRIORITY`、SET 运算、SHOW WARNINGS、INSERT、UPDATE、DELETE、EXPLAIN、其他）、上一条语句的状态（warning / note / error 数、affected rows、`LastInsertID`、语句种类、`FoundInPlanCache`），Go 的 `ResetContextOfStmt` 在 `mock.Context` 上执行，记录之后的 `SysWarningCount`、`SysErrorCount`、`PrevAffectedRows`、`PrevLastInsertID`、类型标志、错误级别、`InSelectStmt` 等语句种类标志、`Priority`、`NotFillCache`、`PrevFoundInPlanCache`、SHOW WARNINGS 继承的 warning 数。这是"warning 计数与 Go 一致"的函数级对照；SQL 级对照随 C2a。

### 5.1 结果

- converge（`scripts/converge.py --part … --item VS1-14`）：D05.3 104 个函数（124 个去掉 20 个不移植）中移植 88、边界桩 16；D05.4 36 个中移植 35、桩 1（另 5 个不移植）；D12.2 2/2。
- govec `resetstmt`：700 条（arm64 本机生成；amd64 来自 tracker CI 的 artifact），`crates/session/tests/govec_resetstmt.rs` 全部一致。
- 一个限制：`finishStmt` 要问 `ExecStmt.IsReadOnly`，它仍是 `blocked-by:D09.13` 的桩（`planner.IsReadOnly` → `ast.IsReadOnly`，D02.2）。这里把它的错误当作语句的错误（autocommit 事务回滚，会话照样回到空闲），所以在它移植之前，走到执行器的语句在 `Close` 时返回这个错误。现在没有语句能走到执行器（`Compile` 先返回 `Blocked`），只有测试会遇到；它应在 C2a 之前随 VS1-10 / VS1-11 的后续移植。

## 6. 第 2 部分：预处理语句（VS1-14b）

### 6.1 Go 函数

5 个函数都在 `session.go`，移植在 `crates/session/src/session.rs`：`DropPreparedStmt` 是 L1，其余 4 个 L2（converge `--item VS1-14b`：5/5，无桩）。"准备一条语句"本身——`executor.NewPrepareExec`（D12.12，预处理执行器）和它生成的 `PlanCacheStmt`（D13.1，plan cache 用的语句对象）——属于下一个工作项 VS1-15（预处理语句的规划），这里是边界桩（§6.5）。

| Go | 移植的内容 | 与 Go 的差异 |
|---|---|---|
| `PrepareStmt` | 全部步骤照 Go：`TxnCtx.InfoSchema` 为空时取 domain 的；`loadCommonGlobalVariablesIfNeeded`；`PrepareTxnCtx`；以一个 `PREPARE` 节点调 `onTxnManagerStmtStartOrRetry`；`AdviseWarmup`（发出 TSO 请求）；`tidb_enable_cache_prepare_stmt` 打开时按 Go 的 key 查 dedup cache；`NewPrepareExec`；无论成败都 `rollbackOnError` | ① `StmtCtx.DetachMemDiskTracker`：会话没有内存 tracker（A1）；② Rust AST 没有 `PrepareStmt`，用语法里未移植产生式的节点（`Stmt::Unported`，kind `PreparedStmt#0`）代替，provider 只问节点是不是 SELECT / 事务语句，两者答案相同；③ dedup cache 命中后的 `rebuildFromPrepareCache` 需要 `PlanCacheStmt`，是 `blocked-by:D13.1`——只有 `NewPrepareExec` 成功才会写这个缓存，所以 VS1-15 之前走不到 |
| `ExecutePreparedStmt` | 照 Go（L1）：按 id 找语句，找不到时记日志并返回 `ErrStmtNotFound`（8111）；找到后把 `PlanCacheStmt` 包成 EXECUTE 节点（`ast.ExecuteStmt`，无名字、带语句和参数）交给 `ExecuteStmt` | 参数是路由器已求值的常量值（Go 是 `[]expression.Expression`，本 spec §6.1）；节点没有 SQL 文本（Go 的 `Text()` 同样为空）。现在停在编译器里规划器的入口（`blocked-by:D09.13`），见 [D13.1 spec §4.1](../D13.1-plancache/spec.md) |
| `DropPreparedStmt` | 照 Go（L1）：不存在时 `ErrStmtNotFound`；否则只把 id 记进 `RetryInfo.DroppedPreparedStmtIDs`，语句在**事务结束时**才由 `cleanRetryInfo` 删除 | 无。用户可见的效果与 Go 相同：`BEGIN; PREPARE → 1; CLOSE 1; EXECUTE 1` 成功，关闭的语句在事务结束前仍计入 `max_prepared_stmt_count`。`RetryInfo` 只移植了这一部分与 `Retrying`（恒为 false，见下），放在 `SessionState`，因此检查点自动包含它 |
| `cleanRetryInfo`（不在 VS1 函数清单里，随 `DropPreparedStmt` 移植） | 挂在 Go 的同样位置（`crates/session/src/session.rs`）：`SessCtx::do_commit_with_retry`（提交；autocommit 语句结束时 `finishStmt` → `autoCommitAfterStmt` → `CommitTxn` 也经过这里）、`SessCtx::rollback_txn_state_in`（`RollbackTxn`，包括 `rollbackOnError`、autocommit 出错时的回滚、`EnterNewTxn` 失败时的回滚）；会话关闭（`Session::close`）时不清理，对应 Go 的 `inCloseSession`（`Close` 改由 `WithdrawAllPreparedStmt` 减计数） | 同时删除 plan cache 中的计划：VS1-15 之前不会建 plan cache，没有可删的。**Go 的坑**（照搬，记在 [D13.1 tasks](../D13.1-plancache/tasks.md) 给 VS1-15）：只取第一条被关闭语句的 `preparedObj`，之后每次循环都用它算 cache key，所以一个事务里关闭多条语句时，删掉的总是第一条语句的计划 |
| `resolvePreparedStmt` | 非 `EXECUTE` 的语句返回自身；EXECUTE 用 `GetPreparedStmt` 取预处理语句 | 预处理语句的 AST 在 `PlanCacheStmt` 的锁里，所以返回语句的句柄（`ResolvedStmt::Prepared`）而不是 AST 的引用；Go 唯一的调用方 `shouldCheckConnectionAliveBeforeCommit` 只问语句是不是写，属于写路径，不移植 |
| `GetSessionPlanCache` | 两种 plan cache 都关时返回 `None` | 否则 Go 懒构造 `NewLRUPlanCache`（D13.1）：VS1-15 的 plan cache 查找才会用到，这里 `blocked-by:D13.1` |

### 6.2 PREPARE 的路由（plan §4.7）

- 第 1 部分的原因 `prepared` 和只在测试中打开 §4.7 的开关都删除了：候选语句（第 1、2 步通过、InfoSchema 版本相等）在 PREPARE 时两边都准备，四种组合照 plan §4.7 的表。
- 进新栈之前与 `COM_QUERY` 一样：先补做没关闭的结果集（§2.3），会话不空闲就不试（路由器的 bug，记日志，归属 PoC、原因 `not_idle`）；然后取检查点。新栈返回 `Blocked` 时恢复检查点——之后新会话与没见过这条 PREPARE 的会话相同（Lucky，#205：回退之后会话状态等于全新会话），归属 PoC、原因是那个块；恢复失败（bug）时 PREPARE 报 1105，PoC 不再准备。新栈返回按 Go 移植的错误时照旧返回它（§4.3 第 3 步）。
- **现在的实际效果**：真正的新会话在 `NewPrepareExec` 返回 `Blocked("D12.12")`，所以每条候选语句都归 PoC，原因 `D12.12`。`fallback` 模式下 EXECUTE 走 PoC，行为与第 1 部分相同；`strict` 模式下 EXECUTE 报 1235（`blocked-by:D12.12`），这是 plan §4.7 第 2 行的规定（`strict` 统计新栈能跑多少），与第 1 部分"`strict` 也不报 1235"不同。新会话的结果集与列定义的编码（`newstack.rs`，D03.1）仍是 VS1-16 的桩：VS1-15 之后 PREPARE 会停在那里，同样归 PoC。

### 6.3 其他协议命令（plan §4.7"按 stmt id 路由其他命令"）

| 命令 | 做法 |
|---|---|
| `COM_STMT_CLOSE` | 路由器按归属发：both / new 发给新栈（只登记），PoC 的副本与路由器的归属在事务结束时才删除，与 Go 的 `DropPreparedStmt` 相同（VS1-15 改，见 [D13.1 spec](../D13.1-plancache/spec.md) §4.2；`close_by_owner`、`closed_stmt_lives_until_txn_end`、`autocommit_close_then_poc_statement`） |
| `COM_STMT_SEND_LONG_DATA` | 长数据由协议层保存，EXECUTE 时一次性解码进参数（ADR 0010 §4.1），哪一方执行就把同样的参数交给哪一方，所以"发给归属的一方、both 时两方"天然成立，不需要路由（`long_data_params_reach_the_engine_that_runs`） |
| `COM_STMT_RESET` | 同上，由协议层清掉长数据（`raw_without_deprecate_eof` 新增的 RESET 用例） |
| `COM_STMT_FETCH` | 协议层回答 1235，与 PoC 相同（从不打开游标）；带游标标志的 EXECUTE 是第 0 步条件：both / poc 走 PoC，new 报 1235（`prepared_both`、`prepared_new_only`，以及协议层的 FETCH 用例） |

### 6.4 列元数据

- 路由器：both 归属的语句回退到 PoC 时，PoC 结果集的列定义必须与 PREPARE 发出的逐字段相同，否则不发送、报 1105 `VS1 column metadata mismatch`（VS1-02 已实现，反证用例 `prepared_fallback_metadata_mismatch`）。
- ab：每条预处理语句比较 B 的 PREPARE 列定义与 EXECUTE 的列定义。Go 本身对某些语句（选择列表里有 `?`）PREPARE 与 EXECUTE 的列定义就不同，所以只报告"B 有而 A 没有"的差异（按列号与字段比较），记为 `metadata` 不一致。反证：`tools/ab` 的单测 `execute_metadata_differs_from_prepare`。

### 6.5 边界桩（登记在 tracker `data/stubs.tsv`）

| 位置 | 块 | 原因 |
|---|---|---|
| `new_prepare_exec`（`executor.NewPrepareExec` + `Next`） | D12.12 | 预处理执行器与 `PlanCacheStmt`，VS1-15 |
| `PrepareStmt` 中 dedup cache 命中、`GetSessionPlanCache` 建 cache | D13.1 | `PlanCacheStmt` / `EXECUTE` / LRU plan cache，VS1-15 |
| `newstack.rs` 的列定义与二进制参数 | D03.1 | 第 1 部分已有，VS1-16 已移植（结果编码与二进制参数，[D03.1 spec](../D03.1-server/spec.md)），桩已删除 |

### 6.6 测试

- 会话：`statement_paths_are_blocked`（`PrepareStmt` 停在 D12.12，之后会话空闲）、`closed_prepared_stmt_lives_until_txn_end`（`BEGIN; PREPARE; CLOSE; EXECUTE` 找得到、计数仍在；提交、回滚、autocommit 语句结束三条路径各自删除并减计数；会话关闭不清理）、`real_session_prepared_lookup`（8111、`GetSessionPlanCache`）、`execute_of_a_prepared_stmt_reaches_the_planner`（EXECUTE 节点经 `ResetContextOfStmt` 按预处理的 SELECT 设置上下文，停在规划器后恢复检查点；`isEndTxnStmt` / `resolvePreparedStmt` 读预处理语句）。
- 路由器（四种组合用脚本化的新栈，因为真正的新会话还不能成功准备）：`prepared_both`、`prepared_poc_only`、`prepared_new_only`、`prepared_both_fail`、`prepared_fallback_metadata_mismatch`、`close_by_owner`、`execute_starts_the_statement_once`、`prepare_with_unequal_schema_versions_is_poc_owned`；真正的新会话：`real_prepare_is_blocked_and_restored`（两种模式，归属、与全新会话的快照相等、EXECUTE、CLOSE）、`real_execute_reaches_the_planner_and_falls_back`（两边都准备了的语句，EXECUTE 走到规划器后 `fallback` 由 PoC 回答、新会话快照不变，`strict` 报 1235 `blocked-by:D09.13`）；`long_data_params_reach_the_engine_that_runs`。
- 协议层：RESET 清掉长数据、FETCH 报 1235（`crates/mysql-protocol/tests/integration.rs`）。
- 第三种组合（PoC 不支持、新栈支持）在真正的栈上还构造不出来：新栈现在什么都准备不了。用例用脚本化的新栈加"PoC 准备失败"模拟；真正的语句要等 VS1-15 之后找。
- govec：本部分没有。5 个函数只是会话状态的转换（单测覆盖），没有可以按输入输出对照的计算；预处理语句的 govec（可缓存性检查等）属于 VS1-15。

## 7. 待定

1. **plan 的排期**：`ResetContextOfStmt` 本应在 C2a 之前（本 PR 已提前）。同样在 C2a 之前、但不在任何 Phase 1 工作项里的还有：新会话接上真正的存储与 oracle（`kv.Storage` 的 `Begin` / `GetOracle`，D07.1），以及 `planner.IsReadOnly` / `ast.IsReadOnly`（D02.2，§5.1）。建议记入 VS1-20 或 VS1-10 的后续。
2. **TSO 交接**（§3.3）随 VS1-20。
3. `StmtCtx.StmtType`（`GetStmtLabel`，#222 的待定 3）：`ExecuteStmt` 中 `setRequestSource` 需要语句标签；`GetStmtLabel` 是 D00.1 交给 D12.0 的函数，还没有移植。这里对 SELECT 用 `"Select"`（Go 对 `SelectStmt` 的标签），其他语句 `blocked-by:D12.0`。
