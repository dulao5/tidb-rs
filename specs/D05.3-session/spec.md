# D05.3 会话执行主流程（spec）

状态：第 1 部分（VS1-14，会话执行主流程）已合入（#235）；第 2 部分（VS1-14b，预处理语句）已合入，见 §6；会话的表达式上下文（tasks P1-1）与 EXECUTE 经 `Compile` 进入优化器评审中（2026-11-22），见 §8、§9。块：D05.3（`pkg/session`），crate `tidb-session`（N0 rank 15）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §4.1、§4.3、§4.5–§4.7，VS1-14；[D05.4 spec](../D05.4-sessiontxn/spec.md)（sessiontxn，#219）；[D12.0 tasks](../D12.0-executor/tasks.md)（`ExecStmt`，#222）；[N2](../exceptions/N2-session-context/spec.md)；[N11](../exceptions/N11-cluster-readts/spec.md)；ADR 0010（会话与协议无关）。

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
| `ExecutePreparedStmt` | 照 Go（L1）：按 id 找语句，找不到时记日志并返回 `ErrStmtNotFound`（8111）；找到后把 `PlanCacheStmt` 包成 EXECUTE 节点（`ast.ExecuteStmt`，无名字、带语句和参数）交给 `ExecuteStmt` | 参数是路由器已求值的常量值（Go 是 `[]expression.Expression`，本 spec §6.1）；EXECUTE 节点本身没有 SQL 文本（Go 在这里新建的 `ast.ExecuteStmt` 的 `Text()` 同样为空），但其中的预处理语句保留着 PREPARE 解析的原文：Go 的每个 AST 节点都带原文，Rust 用 `PlanCacheStmt.stmt_text`，计划缓存未命中时随节点包装交给规划器，表达式列名由此取得（[D13.1 spec §3.5](../D13.1-plancache/spec.md)，#361）。现在停在编译器里规划器的入口（`blocked-by:D09.13`），见 [D13.1 spec §4.1](../D13.1-plancache/spec.md) |
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

## 8. 会话的表达式上下文（sessionexpr，tasks P1-1）

### 8.1 做法

Go 的 `pkg/expression/sessionexpr/sessionctx.go`（57 个函数）：会话建立时创建一个 `ExprContext`，之后表达式的构建和求值每次都通过它**实时**读会话（`sctx.GetSessionVars()`、`StmtCtx`、InfoSchema 等）。移植在 `crates/session/src/sessionexpr.rs`（文件头 `go-file: pkg/expression/sessionexpr/sessionctx.go`；函数在 tracker 中属于 D00.1，由 D00.1 的 P1 收尾移交给本块）。

Rust 中表达式上下文是 `Arc<dyn ExprContext>`（`Send + Sync`，可以交给执行器的 worker），而 `SessionVars` 由会话独占（`&mut`），上下文不能持有会话本身。所以照 N2 §3.1 的三层，上下文持有一个"会话视图"（`SessionView`），在会话交出上下文时（`GetExprCtx` / `GetPlanCtx`，每条语句一次）从会话生成：

| 层 | 内容 | 与 Go 的关系 |
|---|---|---|
| 标量会话变量的快照 | `sql_mode`、当前库、字符集与排序规则、`default_collation_for_utf8mb4`、`block_encryption_mode`、`default_week_format`、`div_precision_increment`、`max_allowed_packet`、`tidb_redact_log`、`tidb_sysdate_is_now`、`tidb_enable_noop_functions`、`windowing_use_high_precision`、`group_concat_max_len`、连接 id、`timestamp` 变量、预处理器给出的只读用户变量 | Go 每次读当前值。差别只在"语句执行中途改了这些变量"时可见（N2 §3.2）：新栈上没有这种语句（`SET` 走 PoC；`SET_VAR` hint 会在规划中写变量，但 hint 内容是解析器的边界 D02.2，非空 hint 列表直接回退） |
| 与会话共享的单元 | EXECUTE 的参数值（`SessionVars.PlanCacheParams`）、计划列 id 计数器（`PlanColumnID`）、`RAND()` 的随机数生成器（`Rng`）、用户变量（`UserVars`） | 与 Go 相同：上下文读写的就是会话的这几个值。参数值在 `Optimize` 里由 plan cache 的未命中路径写入（`SetParameterValuesIntoSCtx`），之后构建 `?` 时读取，所以必须共享。实现：`SessionVars` 的这几个字段改为 `Arc` 共享的单元（`SharedField`、`AtomicI64Field::share`），会话照旧直接读写；克隆 `SessionState`（回退检查点）时复制当前值，不共享 |
| 语句上下文 | 语句的 warning 汇（共享）、plan cache tracker（共享）、stale read TSO 的 provider（共享），以及语句 id、类型上下文、错误上下文（复制） | 上下文不持有 `StatementContext` 本身：规划器通过 `stmt_ctx_mut()` 写语句上下文的普通字段，要求它没有被共享。warning 计数照 Go 的 `StmtCtx.WarningCount()`（`SHOW WARNINGS` 执行中为 0） |

`getStmtTimestamp`：先看 stale read TSO（有则用它的时间，只求值一次）；否则看 `timestamp` 变量，设了就用它；都没有用当前时间。Go 的 `timestamp` 变量在没设时由 `GetSession` 钩子返回语句缓存的"现在"；Rust 的钩子本来就返回当前时间（D05.2 已登记的 L2），这里一致。

### 8.2 函数

57 个中 53 个移植（47 个 L1，6 个 L2），4 个是边界桩。L2 的 6 个：`NewExprContext`（每条语句按视图新建，而不是每个会话一个）、`NewEvalContext`（见 §8.3）、`EvalContext.Sctx`（返回视图）、`EvalContext.TypeCtx`（不做 Go 只在测试构建里做的时区断言：会话还没执行语句时，语句上下文的时区是零值的 UTC，D05.2 的替身，与 Rust 已有的 `assert_location_with_session_vars` 相同）、`sqlExecutorProp` / `sequenceOperatorProp`（返回边界错误，见下）。

`GetExprCtx` 由原来的桩（静态默认上下文）改为返回这个上下文；`GetPlanCtx` 的表达式上下文也是它（原来是"只共用 plan cache tracker 的静态上下文"，D09.13 spec §7）。会话原来的 `get_plan_ctx` / `get_ranger_ctx` 两个桩删除：规划上下文由 `SessionContext::get_plan_ctx` 给出，ranger 上下文由规划上下文生成（`PlanCtx::ranger_ctx`）。

`PlanCtxExtended.SetReadonlyUserVarMap` / `Reset`（`pkg/planner/plannersession/context.go`，D09.13）一并移植：会话保存预处理器找到的只读用户变量，`IsReadonlyUserVar` 读它，`ResetContextOfStmt` 结束时清空（原来写着"N2：每次优化新建 PlanCtx"而跳过）。执行器框架的 `SessionContext` 加了这两个方法，默认实现是 `EmptyPlanContextExtended` 的（什么也不保存），供不是会话的测试替身使用。

### 8.3 可选属性（`NewEvalContext`）

Go 注册 9 个 provider。这里 9 个**全部注册**，因为 Go 的代码会按"属性是否存在"走不同的分支（例如 `expression_rewriter.go:1759` 的用户变量赋值在没有会话变量属性时返回普通错误，`UNCOMPRESS` 没有它时跳过内存统计，`CURRENT_USER()` / `GET_LOCK()` 取不到属性时是返回给客户端的普通错误）。会话还做不到的那几个，在**取用时**返回边界错误（`blocked-by`），语句回退到 PoC，而不是给出与 Go 不同的结果：

| provider | 做法 |
|---|---|
| InfoSchema | 会话的 InfoSchema（事务的，否则 domain 的）与 domain 的 |
| 存储 | 会话的存储 |
| DDL owner | 答 false：tidb-rs 不参加 DDL owner 选举（DDL 由 Go TiDB 执行），本实例不会是 owner |
| 权限检查 | 就是这个上下文；没有权限管理器（D04.1），按 Go 中管理器为 nil 的分支，检查一律通过 |
| 当前用户（`currentUserProp`） | 取用时 `blocked-by:D04.1`：Rust 的 `SessionVars` 没有 `User` / `ActiveRoles`（VS1 plan §3.2，PoC 单用户） |
| 会话变量（`NewSessionVarsProvider`） | 取用时 `blocked-by:D05.3`：tasks P1-2，内置函数如何拿到 `SessionVars` 还没定 |
| advisory lock | 取用时 `blocked-by:D05.3`：会话的锁表没有移植 |
| SQL 执行器（`GetRestrictedSQLExecutor`） | 取用时 `blocked-by:D00.3`（受限 SQL 执行器未移植） |
| sequence（`util.GetSequenceByName`） | 取用时 `blocked-by:D06.4`（Rust InfoSchema 没有 sequence）；`sequenceOperator` 的 3 个方法是桩 |

实现：前三个"取用时报边界错误"的属性用一个通用的边界 provider（`expropt::BlockedPropProvider`，tidb-rs 的胶水）：它报告自己的属性键（所以属性集合是满的），`getPropProvider` 取到它时返回它记下的 `blocked` 错误。

### 8.4 测试

`crates/session/src/tests.rs`：`session_eval_context_basic`、`session_eval_context_current_time`、`session_eval_context_privileges_and_optional_props`（9 个属性都存在，5 个取用时是边界错误）、`session_build_context_shares_the_session_state`、`restore_writes_back_into_the_shared_cells`（见 §8.5）（对应 Go `sessionctx_test.go` 的 5 个测试，按 §8.1 的视图改写：标量变量在建视图前设置；共享的单元在建视图后修改也能读到）。govec 不适用：这些函数只是把会话的状态转交出去，没有可按输入输出对照的计算（tracker 的豁免表改为"已移植，Rust 单测"）。

### 8.5 回退恢复与共享单元

检查点（§3）复制这几个共享单元的**值**（用户变量也复制，原来只共享表），恢复时把值写回会话**原来的单元**，而不是换成新单元（`SessionState::restore_keeping_shared_cells`）。所以回退之前建好的表达式上下文（或其他持有这些单元的对象）在恢复之后读到的是恢复后的值，与会话一致。测试：`restore_writes_back_into_the_shared_cells`（参数值、计划列 id、随机数生成器、用户变量）。

### 8.6 已知差异

1. 标量会话变量是交出上下文时的快照，Go 每次实时读（§8.1）。等解析器能解析 hint 之后，`SET_VAR` hint 会在规划中写会话变量：那时快照要在 hint 生效**之后**取（或在写入后重建规划上下文），否则规划读到的是 hint 之前的值。在那之前非空 hint 列表直接回退，看不出差别。
2. 不做 Go 测试构建中的时区断言（§8.2）。
3. 会话做不到的 5 个可选属性取用时回退（§8.3），Go 中它们都有值。

## 9. EXECUTE 经 Compile 进入优化器

### 9.1 `Compile`

Go `pkg/executor/compiler.go` 的 `Compiler.Compile`（L45）现在照 Go 依次执行：`plannercore.IsReadOnly`（写入 `StmtCtx.IsReadOnly`）→ `Preprocess`（带 `WithPreprocessorReturn`、`InitTxnContextProvider`；之后会话保存只读用户变量）→ 取事务的 InfoSchema → EXECUTE 时 `GetPreparedStmt` → `planner.Optimize`（规划上下文来自会话的 `get_plan_ctx`，表达式上下文是 §8 的 sessionexpr）→ `CountStmtNode`（EXECUTE 时计它的预处理语句）→ `needLowerPriority` → `StmtCtx.SetPlan` → 生成 `ExecStmt` → `AdviseOptimizeWithPlanAndThenWarmUp`。原来的三个桩（`IsReadOnly`、`Preprocess`、`Optimize`，`blocked-by:D09.13` / `D09.0`）删除。

与 Go 的差异：

1. Go 的预处理器在 `ensureInfoSchema` 中向事务管理器要 InfoSchema；Rust 的预处理器从返回值里拿（PREPARE 也是这样交进去的），所以 `Compile` 把事务的 InfoSchema 放进返回值。会话没有事务 InfoSchema 时停在 `blocked-by:D05.4`（与原来相同）。
2. 优化器要 `&mut ast::Stmt`（Go 的规划器在共享的节点上写），Rust AST 不能 `Clone`。所以语句由会话**移交**给 `Compile`，规划器直接在它上面写，写完再冻结成 `Arc` 共享（§9.2）。EXECUTE 也是这样（原来逐字段复制一份给优化器，现在不再复制）。
3. 带 `USING @v` 的文本协议 EXECUTE（路由器从不交给新栈）在预处理之前就停在 `blocked-by:D09.2`，即 `buildExecute` 改写 `USING` 变量的位置；之后还要用户变量函数（D08.9）。Go 会先做预处理再报错，这里的报错位置提前。
4. Go 的 AST 节点自带原文（`ast.Node.Text()`），Rust AST 只存字节区间。`Compile` 把会话交给它的语句文本（`StmtCtx.OriginalSQL`）放进节点包装 `NodeW`（`set_text`），规划器给构建器设 `sql_text`，表达式列名（`SelectField.Text()`）从中取得（D09.13 spec §5.1）。这只是表示方式不同，列名与 Go 相同；以前没有设置，文本协议的表达式列名是空串（#361）。EXECUTE 的会话文本是预处理的 SQL（`ResetContextOfStmt`），EXECUTE 节点本身没有区间，不读它；预处理语句的列名用计划缓存给的原文（§6.1 表中 `ExecutePreparedStmt` 一行）。

### 9.2 普通语句：把语句移交给 `Compile`

原来只有 EXECUTE 能编译：会话把语句放进 `Arc`，同时交给事务管理器（`OnStmtStart`）和 `Compile`，规划器拿不到可写的一份，普通语句（`COM_QUERY` 的 SELECT）在预处理之前停在 `blocked-by:D05.3`。现在照下面的顺序做，文本协议的语句与 EXECUTE 走同一条路。

**做法**

1. 路由器把解析好的语句**整个交给**新会话（`NewStack::execute` 收 `ast::Stmt`，不再是 `Arc`），自己不留一份。新栈停在边界、要回退到 PoC 时，路由器按原文**重新解析一次**，再降级给 PoC（PoC 要 AST）。重新解析只发生在回退时；新栈执行的语句只解析一次。
2. 会话的 `ExecuteStmt` 收下语句。`Compile` 之前的各步只借用它：`ResetContextOfStmt`、事务内 / stale read 的校验、`OnStmtStart`、语句标签、预先加载用到的表。
3. `Compile` 收下语句（移交）。`IsReadOnly` 读它，预处理器和优化器在 `&mut ast::Stmt` 上写；`planner.Optimize` 之后把它冻结成 `Arc<ast::Stmt>`，放进 `ExecStmt` 的 `StmtNode`。
4. `Compile` 成功之后，会话把同一个 `Arc` 交给事务管理器，作为它的当前语句（`GetCurrentStmt`）。

**核对：`OnStmtStart` 到 `Compile` 结束之间，谁读语句**（Go v8.5.8，逐个看了实现）

| 位置 | 读语句的什么 | 什么时候读 |
|---|---|---|
| 事务管理器的 `OnStmtStart` | 保存节点；事件名用 `OriginalText()`（Rust 用语句种类，§5） | 调用当时 |
| provider 的 `OnStmtStart`：基础、乐观、悲观 RR、stale read | 不读（参数是 `_`，或只转给基础的） | — |
| provider 的 `OnStmtStart`：悲观 RC | `NeedSetRCCheckTSFlag` → `IsReadOnly(node)` | 调用当时 |
| `AdviseWarmup`（基础、RC） | 不读语句：只看事务是否已准备、`StmtCtx.IsStaleness`、`SnapshotTS` | — |
| `AdviseOptimizeWithPlan`（基础、乐观、RR、RC、stale read） | 不读语句：只看计划和会话变量 | — |
| `GetCurrentStmt` 的调用者 | 预处理器的 stale read 分支（换成 stale read provider 后再调一次 `OnStmtStart(GetCurrentStmt())`，这个 provider 不读节点）；`NewTxnInStmt`（INSERT / DELETE / DDL / GRANT / REVOKE 和用户管理等语句的执行器，在 `Compile` 之后） | — |

结论：节点只在 `OnStmtStart` 调用的当时被读（种类和是否只读），这时规划器还没有写；`AdviseWarmup`、`AdviseOptimizeWithPlan` 不读语句。

**选择：`OnStmtStart` 借用节点，`Compile` 之后补交冻结的 `Arc`**

- `OnStmtStart` 收 `Option<&ast::Stmt>`，provider 在调用当时读完（RC 的 `IsReadOnly`）。sessiontxn 的接口（`StmtInfo`）不变，provider 看到的仍是语句本身，与 Go 一样。
- 不选"语句种类摘要"：摘要要把 RC 的 `IsReadOnly` 提前算好、改 sessiontxn 的 `StmtInfo` 接口，而且 `Compile` 之后的读者（`NewTxnInStmt`）仍要真正的节点，最后还是要补交 `Arc`。借用加补交只改会话自己的代码。
- `Compile` 之后的读者拿到的是规划器写过之后的节点，与 Go 一样（Go 共享指针，读到的也是写过的）。

**与 Go 的差异**

1. `OnStmtStart` 到 `Compile` 结束之间，事务管理器的当前语句是空的（Go 是节点）。现在没有读者：预处理器的 stale read 分支是边界桩（`blocked-by:D05.4`），路由器也不把 stale read 交给新栈。将来移植那个分支时，要把预处理器手里的节点交给 `OnStmtStart`，而不是取 `GetCurrentStmt`。
2. `Compile` 出错时事务管理器没有当前语句（Go 有）。出错之后的 `rollbackOnError` 不读它。
3. 回退时多解析一次（第 1 条）。只影响回退的语句。
4. Go 的 `ExecStmt.RebuildPlan` 会在 `a.StmtNode` 上再做一次预处理和优化。现在它是桩，唯一的调用方是乐观事务的重试（写路径），不影响只读路径。将来移植时，语句已冻结成 `Arc`，事务管理器也持有一份，`Arc::get_mut` 会失败，AST 又不能 `Clone`。两种做法：先让两处持有者都交回 `Arc`（事务管理器 `set_current_stmt(None)`，以及 `RebuildPlan` 所在的 `ExecStmt` 自己的 `stmt_node.take()`），再用 `Arc::try_unwrap` 拿回可写的语句，规划完再冻结、补交；或者按原文重新解析一份。到时按重试路径的代价选择。

### 9.3 停在哪里，怎样回退

| 语句 | 停在 | 说明 |
|---|---|---|
| 预处理的点查 `SELECT c FROM sbtest1 WHERE id=?` | 不再停：快速路径用参数值建点查计划（D10.8 spec §4a）；自动提交时 `Compile` 换成其中的点查、会话走点查短路径，否则 `Exec` 换成其中的点查；短路径在 MaxTS 时复用预处理语句里的点查执行器，普通路径新建执行器（§10） | VS1-20 之前停在 `getNameValuePairs`（`blocked-by:D13.1`） |
| 其他预处理 SELECT（如 `id > ?`） | 物理优化：统计信息推导（`blocked-by:D10.6`），之后依次是 FindBestTask（D10.2）、ResolveIndices（D10.9）、代价（D10.5） | 逻辑计划已用参数值建好；FindBestTask 正由另一项工作（VS1-28）移植 |
| 物理优化完成后的 Execute 计划 | 不再停（§10） | VS1-20 之前在 `Compile` 中停在 `blocked-by:D13.1` |
| 文本协议的点查 `SELECT c FROM sbtest1 WHERE id=10` | 不再停：语句移交给 `Compile`（§9.2），快速路径建点查计划，执行器建好、打开、读完 | 原来在预处理之前停在 `blocked-by:D05.3` |
| 文本协议的点查，常量超出 `BIGINT` 范围（如 `id=9223372036854775808`） | Go 的计划是 TableDual，新栈停在 `PhysicalTableDual`（`blocked-by:D10.1`） | 与预处理语句的同类参数相同 |
| 文本协议的点查，负数常量（`id=-1`） | 不走快速路径（`-1` 是一元负号表达式，不是常量），完整规划器的计划带 Projection，建执行器时停在 `blocked-by:D12.2` | 同上，执行器打开之前 |
| 其他文本协议 SELECT（范围、聚合、排序等） | 规划完成后，建执行器时停在最外层还没有移植的执行器：TableReader（`D12.1`）、Projection / Selection / Limit / TableDual（`D12.2`）、Sort / TopN（`D12.5`）；少数停在物理优化（`D10.1` / `D10.2` / `D10.8`）或取当前用户（`D04.1`）。都在执行器打开之前 | 原来都停在 `D05.3`；分布见 VS1 的集成记录 `c2a-integration.md` §7 |
| 没有选择数据库时的 `SELECT ... FROM t` | 不回退：预处理器照 Go 返回 1046 | 原来停在 `D05.3` 回退，PoC 也报 1046。现在两种模式下这个错误都由新栈返回，ab 统计里不再算作回退 |
| 路由器测试中的真实会话（替身存储，没有 coprocessor client） | fast plan 的下推检查，`blocked-by:D09.13`（规划上下文没有 store client） | 原来停在 `D05.3`，1235 / 回退不变 |

这些位置都在执行器打开之前：会话的 `rollbackOnError` 结束语句的事务，路由器恢复检查点后回退到 PoC，计数器（`executed_stmts`）不增加；panic 由路由器恢复成 1105（原有机制，不变）。默认的 `--new-stack=off` 不建新会话，不经过这里。

### 9.4 测试

- `execute_of_a_prepared_stmt_reaches_the_planner`：真实 PREPARE `id > ?` 后 EXECUTE，`Compile` 设置了 `IsReadOnly`，停在物理优化的边界；会话空闲、`executed_stmts` 为 0，参数值在会话里；恢复检查点后快照与之前逐字相同。
- `execute_reads_the_parameters_through_the_session_expression_context`：int、字符串、NULL 三种参数，每次 EXECUTE 都停在物理优化的边界；会话里的参数值、会话交给规划器的规划上下文读到的参数值、规划器的表达式改写器在这个上下文里为 `?` 建的常量（值与类型：BIGINT、VARCHAR，NULL 按 Go 的 `InferParamTypeFromUnderlyingValue` 为 VARCHAR）都与参数一致。
- `prepare_of_the_sbtest_point_select_builds_the_plan`：点查 EXECUTE 跑完（§10.3）。
- `text_protocol_point_select_runs_to_the_end`（§9.2）：真实会话上的文本协议点查：执行器建好并打开（`executed_stmts` 为 1），语句上下文里是点查计划，事务管理器的当前语句与 `ExecStmt` 的语句是同一个 `Arc`，读替身存储得到 0 行，关闭后会话空闲。
- 普通语句不再停在 `D05.3`，以下测试改到新的位置：`compile_error_rolls_back`、`dropped_result_is_finished_before_the_next_statement`（没有选择数据库，`Compile` 返回 Go 的 1046）；`every_fallback_point_restores_the_session` 的第二个回退点和 `statement_paths_are_blocked`（`SELECT 1` 规划完成，建 Projection 执行器时停在 `D12.2`，执行器打开之前）；路由器的 `phase0_session_falls_back_or_reports_1235`（选择数据库后停在 `D09.13`；fallback 下 PoC 用重新解析的语句回答；没有数据库时新栈直接返回 1046）。
- `expression_columns_are_named_by_their_text`（#361）：`SELECT 1`、`SELECT -42`、`SELECT 1, 2, 3`、`SELECT 1 + 1`、`SELECT 1.5`、`SELECT 1 LIMIT 1`、`SELECT id + 1 FROM sbtest1 WHERE id = 1`、`SELECT c, 1 FROM sbtest1 WHERE id = 1` 的列名，在文本协议、PREPARE、EXECUTE 未命中、EXECUTE 第二次（命中）四处都与 Go v8.5.8 发出的列定义相同。这些语句在建第一个执行器（投影，D12.2）时回退，结果集还拿不到，所以测试按会话的步骤走到 `Compile`，读 `ExecStmt` 的输出列名（结果列的名字就取自它的 `ColName`）。
- 编译器单测 `compile_plans_a_plain_statement`（原 `compile_plans_only_execute`）：替身会话没有事务的 InfoSchema，`SELECT 1` 停在 `D05.4`。

## 10. Execute 计划的两条路：点查短路径与普通路径（VS1-20、VS1-20b）

### 10.1 做法

预处理语句的 EXECUTE 在 `planner.Optimize` 之后得到一个 Execute 计划（`*plannercore.Execute`，里面是物理计划）。Go 的 `Compile`（`pkg/executor/compiler.go:133`）接着问 `IsSafeToReusePointGetExecutor`（`plan_cache.go:351`）。它只看三件事，**与 plan cache 是否打开无关**：

- 不是 stale read（`staleread.IsStmtStaleness`，即语句上下文的 `IsStaleness`）；
- 是自动提交事务（`IsAutoCommitTxn`：`autocommit` 打开且不在显式事务中）；
- 预处理语句记下的 schema 版本等于这次用的 InfoSchema 的版本。

三件都成立、且 Execute 里面是点查计划（`PointGetPlan`）时，走**点查短路径**；否则走**普通路径**。新栈两条路都照 Go 走（VS1-20b 补上短路径，之前总走普通路径）。

**点查短路径**（`IsSafeToReusePointGetExecutor` 移植为 L1，放在 planner 的 `plan_cache.rs`）：

1. `Compile` 把 `ExecStmt.PsStmt` 设为这条预处理语句，`ExecStmt.Plan` 换成 Execute 里面的点查计划（Execute 计划不再交给 `ExecStmt`）。语句上下文里的计划仍是 Execute 计划，与 Go 相同（Go 只换了 `stmt.Plan`）。`AdviseOptimizeWithPlanAndThenWarmUp` 收到点查计划，结果与普通路径相同（自动提交的主键点查用 MaxTS，不取 TSO）。
2. 会话看到 `PsStmt` 后调用 `ExecStmt.PointGet`（`session.go:2160`），然后立刻让事务变成 invalid（`txn.changeToInvalid()`）。不经过 `runStmt`，也不经过 `ExecStmt.Exec`，所以不调用 `inheritContextFromExecuteStmt`：`isPreparedStmt` 保持 false。
3. `ExecStmt.PointGet`：取读时间戳，把语句上下文的优先级设为 `HighPriority`。读时间戳是 MaxTS（自动提交的主键点查）时，照 Go 复用预处理语句里存着的点查执行器：取出来，用这次的点查计划 `Init`（表、handle、索引及其值、加锁标志、列、行解码器、执行器基础部分、索引使用统计，全部从这次的计划重新读，不留上一次的任何值）；没有存着的就新建一个，并在结果集结束（`recordSet.Finish`）时放回预处理语句。TableDual（不是点查执行器）和分区表的执行器不存。读时间戳不是 MaxTS 时既不取也不存。返回不包 `execStmtResult` 的结果集，关闭时只关闭执行器，不调用 `finishStmt`。
   - 与 Go 的写法不同的一处（N3）：Go 的执行器经会话拿到语句上下文，这里执行器自己持有语句上下文的 `Arc`（执行器基础部分的统计、运行时统计、索引使用统计）。会话在 `Finish` 之后还要写语句上下文（`FinishExecuteStmt`），要求只有它一个持有者，所以执行器放回预处理语句之前先丢掉这些（`release_stmt_state`），下一次 `Init` 再建。原来移植的复用代码没有这一步，在真实会话上从未走到；接通短路径后第二次 EXECUTE 就会在关闭时 panic（"statement context is shared"），会话测试发现并修正。
   - 执行器存在本会话自己的预处理语句里，不在实例级 plan cache 中共享；它只读计划。点查执行器自己的快照运行时统计（`stats`）照 Go 保留，不随复用清空（它不持有语句上下文，`Close` 再次登记）。
   - 复用时 `Init` 不重设 snapshot 的选项：优先级、资源组、replica read 等仍是执行器第一次建立时的值。Go 也是这样（`Init` 不碰 `e.snapshot`），不是缺陷。
   - 复用的执行器还留着上一次的 snapshot（Go 也是，MaxTS 下 snapshot 与事务无关）。client-rust 的 snapshot 会缓存读到的值，于是复用之后读不到别处新写入的值；存储驱动改为 MaxTS 的读每次新建 client-rust snapshot（D07.1 spec §3）。在 playground 上验证：Go TiDB 改了某一行之后，同一连接上的下一次 EXECUTE 读到新值（修正前一直是旧值）。
4. 计划可能来自实例级 plan cache（多个会话共享），执行器只读它，不写（`crates/executor/clippy.toml` 禁止取得计划的可写引用）。

**普通路径**（三件中有一件不成立，例如显式事务、`autocommit=0`、schema 变了、stale read；或 Execute 里不是点查）：

1. `Compile` 把 Execute 计划交给 `ExecStmt`（`execute_plan` 字段，对应 Go `Plan` 字段为 Execute 的情形），语句上下文的计划也是这个 Execute 计划（Go 两处存的是同一个指针，这里共享一个 `Arc`）。`AdviseOptimizeWithPlanAndThenWarmUp` 收到 Execute 里面的物理计划：Go 每个事务上下文 provider 的 `AdviseOptimizeWithPlan` 都先把 Execute 拆开（`if execute, ok := plan.(*plannercore.Execute)`），所以结果相同。
2. `ExecStmt.Exec` 开头的 `inheritContextFromExecuteStmt`（L1）：`QueryString` 设为 EXECUTE 节点的文本（二进制协议的 EXECUTE 节点没有文本，Go 也是空串），`OutputNames` 取 Execute 计划的，`isPreparedStmt = true`，`Plan` 换成 Execute 里面的物理计划，语句上下文的计划也换成它。
3. 之后 `buildExecutor` 建的就是里面的计划（点查是 `buildPointGet`）。Go 的 `buildExecutor` 里 `ExecuteExec` 那一段只在计划仍是 Execute 时才走到；`Exec` 已经换掉，所以不会走到（Go 也一样），`ExecuteExec` 不需要移植。
4. 会话走完整的 `runStmt` → `Exec` → `finishStmt`。

`ExecStmt.IsReadOnly`（原为 `blocked-by:D09.13` 的桩）改为 L1：`planner.IsReadOnly(a.StmtNode, vars)`。点查执行器打开之后，`finishStmt` 会问它；原来它在执行器打开之后报 `blocked`，路由器只能报"打开之后的 blocked"（bug 路径）。

### 10.2 与 Go 的差异

VS1-20 时记下的三处可观察差别（新栈不走短路径，Lucky 评审 #301）随短路径补上而消失：慢日志与语句摘要的 `Prepared` 现在与 Go 一样为 false；语句上下文的请求优先级现在与 Go 一样为 `HighPriority`；自动提交的预处理点查现在与 Go 一样不经 `runStmt` / `finishStmt`，事务直接变成 invalid。还剩下这些：

点查执行器的复用也照 Go 做了（§10.1 短路径第 3 条；A3 例外 §1.1 最后一行要求照 Go 移植），不再是差别。

1. `ExecStmt.PointGet` 里的 tracing region 与 Top SQL 没有移植（D14.2），`StmtCtx.StmtType` 为空时用 `GetStmtLabel` 补上的那一步也没有移植（`GetStmtLabel` 在 D00.1）。这些原来就有，与本节无关。
2. **plan cache 的 warning 可能不同**：Go 命中缓存后重建计划失败时会加 `skip plan-cache: plan rebuild failed, ...` 之类的 warning（`RebuildPlan4CachedPlan`）。只出现在缓存命中之后参数让计划变化的情形（例如前一次 EXECUTE 缓存了点查，这次参数超出列的范围）；这类语句 Go 的计划是 TableDual，新栈停在 TableDual 的边界（D10.1），不在点查的范围内。
3. `QueryString` 写进会话的值表（`SessionContext::set_query_string`，新增的胶水方法）；现在没有读它的代码。短路径与 Go 一样不写它。

观察（不是差别）：`IsSafeToReusePointGetExecutor` 的第三个条件（预处理语句的 schema 版本等于当前版本）在正常流程中总是成立。EXECUTE 在 `Compile` 之前先经过 `planCachePreprocess`：schema 变了就用同一个 InfoSchema 重新 preprocess，并把预处理语句的版本改成这个 InfoSchema 的版本（第 4 步）；`Compile` 随后拿同一个 InfoSchema 比较，两者相同。Go 也是这个顺序。所以 schema 变化之后的 EXECUTE 仍走短路径（测试 `prepared_point_get_after_a_schema_change_takes_the_short_path`），只有重新 preprocess 失败（报错，语句不执行）时才走不到这里；同时 schema 变化会清空存着的点查执行器，下一次新建。

### 10.3 测试

- `prepare_of_the_sbtest_point_select_builds_the_plan`：替身存储上 PREPARE + EXECUTE 点查：连续两次 EXECUTE 都走点查短路径（VS1-20b）：`ExecStmt` 设了 `PsStmt`、计划是点查、没有 Execute 计划，结果集不包 `execStmtResult`，`IsPrepared` 为 false，语句上下文里仍是 Execute 计划，优先级为 High；每次 `executed_stmts` 加一，读替身存储得到 0 行，关闭后会话空闲，点查执行器（MaxTS）存在预处理语句里。测试表的 `id` 原来没有主键标志（`pk_is_handle` 而列没有 `PriKeyFlag`，快速路径找不到 handle），已补上；替身快照的 `Get` 原来是 `unimplemented!`，改为"没有这一行"。
- `prepared_point_get_outside_autocommit_takes_the_normal_path`（VS1-20b）：`autocommit = 0` 与 `BEGIN` 之后同样的 EXECUTE 走普通路径：结果集包 `execStmtResult`，`IsPrepared` 为 true，语句上下文与 `ExecStmt` 的计划都换成了点查（`inheritContextFromExecuteStmt`），优先级不是 High。
- `prepared_point_get_after_a_schema_change_takes_the_short_path`（VS1-20b）：PREPARE 之后 schema 版本从 1 变为 2，EXECUTE 先按新版本重新 preprocess 并记下版本 2（`planCachePreprocess` 第 4 步），`Compile` 再比较时两者相同，所以仍走短路径。Go 也是这个顺序：第三个条件只在 `Compile` 拿到的 InfoSchema 与 preprocess 用的不同时才不成立，正常流程中两者是同一个。
- `is_safe_to_reuse_point_get_executor_checks_three_conditions`（VS1-20b）：直接调用函数，stale read、显式事务、`autocommit = 0`、预处理语句的 schema 版本不同，各自返回 false。stale read 现在到不了 `Compile`（预处理器的 stale read 分支是边界桩），所以只在这里测。
- `point_get_executor_is_reused_with_the_current_plan`（执行器复用）：替身存储放两行（id 1、2），实例级 plan cache 打开；EXECUTE 依次用 1、2、3（不存在）、1、2：每次读到自己的那一行（3 为空），第一次之后计划都来自缓存，执行器只建一次、之后每次 `Init` 复用，缓存计划的结构哈希始终不变。
- `unique_index_point_get_is_not_reused_and_reads_its_own_row`：唯一索引点查读 TSO（不是 MaxTS），执行器不存不取；k = 7、8、9（不存在）、7 各自经索引读到自己的行，缓存计划的结构哈希不变（A3 §2.3 第 9a 行）。
- 执行器单测 `point_get_with_max_ts_caches_the_executor`、`point_get_with_a_tso_does_not_cache`、`point_get_does_not_cache_other_executors`（恢复）：MaxTS 时存并复用（`Init`），TSO 时不存不取，TableDual 不存；语句优先级为 High。
- `autocommit_read_prefetches_tso_and_ends_idle`、`dropped_result_is_finished_before_the_next_statement`：`IsReadOnly` 不再报错，自动提交的只读事务正常结束（原来按"IsReadOnly 的错误"回滚）。
- `PHYSICAL_BOUNDARIES` 去掉 `D13.1`（`Compile` 不再停），加上 `D12.1` / `D12.2`：`id > ?` 的计划现在能做完物理优化（FindBestTask 已合入），`Exec` 建表读执行器（VS1-34）或 Projection 执行器（参数为 NULL 时，VS1-35）时停在它们的边界，仍在执行器打开之前。

## 11. 全局变量的加载与路由器的配套（VS1-20）

### 11.1 `loadCommonGlobalVariablesIfNeeded`

原来是桩（只置"已加载"标志，`blocked-by:D06.5`）。于是新会话的 SetSession 钩子从不运行，`SessionVars` 中由钩子设置的字段停在结构体默认值（Go 的 `NewSessionVars` 也不设它们，靠这次加载）：例如 `tidb_enable_prepared_plan_cache` 为 false，每次 EXECUTE 都多一个 `skip prepared plan-cache: plan cache is disabled` warning（Go 没有）；`tidb_enable_plan_replayer_capture` 为 false（Go 默认 ON）。

现在照 Go 移植（L2）：第一次需要时置标志，取 domain 的会话变量缓存，会话里还没有设置的变量逐个 `SetSystemVarWithRelaxedValidation`（未知变量跳过），客户端带 `CLIENT_INTERACTIVE` 时用 `interactive_timeout` 设 `wait_timeout`。Go 的 bootstrap 分支（`sessionctx.Initing`）不适用。

差异：domain 的 sysvar 缓存（`GetSessionCache`）是 D06.5，这里用替身 `session_cache`：照 `rebuildSysVarCache` 的规则，每个不跳过初始化的变量一项，全局作用域的变量取会话的 `GlobalVarAccessor`（路由器的替身回答编译时的默认值，PoC 同样不读 `mysql.global_variables`），只有会话作用域的变量取默认值。等 D06.5 的 sysvar 缓存接上后换成它，新会话就能看到 Go TiDB 上的 `SET GLOBAL`（VS1-17 的 TODO）。

已知限制（Lucky 评审 #303）：

1. **全局值是编译时的默认值**。集群上用 `SET GLOBAL`（在 Go TiDB 上）改过的变量，新会话看不到：路由器只同步白名单里的会话变量（§4.1），白名单之外、被改过全局值的变量在新会话里仍是默认值，**不会触发第 0 步**（第 0 步只看本会话里偏离默认值的变量），所以结果可能与 Go 不同而不报告。要等 D06.5 的 sysvar 缓存接上（VS1-17）才消除。
2. **`CLIENT_INTERACTIVE` 分支走不到**：握手的 capability 没有交给新会话（`set_client_capability` 没有调用方，capability 留在 PoC 的连接上），`client_capability` 恒为 0，`wait_timeout` 保持全局值。代码中有注释。

后果（Go 的真实行为，记在 VS1-20 的集成记录 §5 第 3 条）：计划回放采集开着，非点查语句在 `CollectColumnStatsUsage` 停在 `recordTableRuntimeStats` 的边界桩（D02.2），早于原来的物理优化边界。会话测试的 `PHYSICAL_BOUNDARIES` 因此加上 D02.2。

### 11.2 路由器

1. **会话打开时加载**：路由器建好新会话后立即调用一次加载（Go 在第一条语句时加载）。这样第 0 步的 `tidb_txn_mode` 比较的是加载后的值（默认值）。Go 的加载跳过已经设置的变量；路由器同步的变量在加载之后才写，结果相同。加载出错（钩子拒绝默认值，不应发生）时会话的变量只设了一部分，路由器记错误日志并把这个会话**固定在 PoC**：第 0 步增加条件 `global variables not loaded`，之后的候选语句都走 PoC（只归新栈的预处理语句按 plan §4.7 报 1235）。
2. **第 0 步的 `tx_read_ts`**：Go 的钩子对空值也放一个读时间戳为 0 的 `TxnReadTS`（`setTxnReadTS`），所以"有这个对象"不等于"设置了读时间戳"。改为读时间戳非 0 才走 PoC。
3. **`SHOW WARNINGS` / `SHOW ERRORS`**：VS1 plan §4.5 要求交给上一条语句的执行方回答，原来总由 PoC 回答（PoC 的语句从不留 warning）。现在上一条语句由新栈执行时，路由器用新会话语句上下文中的 warning 回答，行的内容照 Go 的 `ShowExec.fetchShowWarnings`（TiDB 错误取 MySQL 错误码和消息，其他错误为 `ErrUnknown` 和文本；`SHOW ERRORS` 只取 Error 级），列定义用 PoC 的（与 Go 相同）。之后上一条语句仍算新栈的（Go 的 `SHOW WARNINGS` 保留上一条语句的 warning），直到下一条由 PoC 执行的语句。`SHOW COUNT(*) WARNINGS` 和带 `LIMIT` 的形式 PoC 本来就不支持（降级失败），不变。

### 11.3 sessiontxn 的选项类型

`tidb_load_based_replica_read_threshold` 在会话里是纳秒的 `i64`（Go 的 `time.Duration`），sessiontxn 原样作为 `LoadBasedReplicaReadThreshold` 选项值交给事务和 snapshot；存储驱动照 Go 按 `time.Duration` 取（Rust 的 `Duration`），类型不符时 panic（Go 的类型断言）。加载全局变量后这个变量有默认值 1s，第一次就碰到。sessiontxn 的两处（`isolation/base.rs`、`internal.rs`）改为交 `Duration`。

### 11.4 测试

- 路由器：`show_warnings_go_to_the_engine_that_ran_the_previous_statement`（新栈语句之后的 SHOW WARNINGS / SHOW ERRORS 的行，连续两次相同；PoC 语句之后交还 PoC）、`global_variables_are_loaded_when_the_session_opens`（plan cache 开着，`tidb_txn_mode` 为默认值，`tx_read_ts` 为 0 时不算会话条件、为 7 时算）、`failed_global_variable_load_pins_the_session_to_the_poc`（全局值读不到时会话固定在 PoC，strict 下候选语句也由 PoC 执行）。
- 会话：`PHYSICAL_BOUNDARIES` 加 D02.2（§11.1）。
- 端到端：VS1-20 的 ab（集成记录 §2）。

## 本文新增部分（§8–§11）所用 ID

- D00.1：上下文类型（exprctx / sessionexpr 的原归属）；D00.3：受限 SQL 执行器；D02.2：解析器的 AST 与 hint
- D04.1：权限；D05.2：会话变量；D05.3：本块，会话（P1-1：sessionexpr；P1-2：会话变量 provider）；D05.4：sessiontxn
- D06.4：表与 sequence；D08.9：用户变量函数；D09.2：表达式改写器；D09.13：优化器驱动
- D10.2 / D10.5 / D10.6 / D10.9：FindBestTask / 代价 / 统计信息推导 / ResolveIndices；D12.12：预处理语句的执行器；D13.1：plan cache
- VS1-28：FindBestTask 的移植工作项；VS1-20：点查纵向集成；A3：plan cache 例外；N2：会话上下文的三层结构
- D10.1：物理算子（TableDual）；D10.8：点查快速路径；D12.0：执行器框架
- D06.5：domain（sysvar 缓存）；D11.x：统计信息；VS1-17：domain 的接入
- D12.1：TableReader 等读执行器；D12.2：Projection / Selection / Limit / TableDual 执行器；D12.5：Sort / TopN 执行器；VS1-37：普通语句的集成
