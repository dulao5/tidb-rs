# D05.4 事务上下文（spec）

状态：第 1 部分（只读路径中 VS1 需要的部分）进行中（2026-11-14）。块：D05.4（ROADMAP P2，#49），新建 crate `tidb-sessiontxn`（N0 rank 11，`layers.toml` 去掉 planned）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) VS1-14、§9；[N11](../exceptions/N11-cluster-readts/spec.md) §4.2、§4.4、§4.5 与 D8 / D11（#146）；[ADR 0005](../../docs/adr/0005-schema-sync.md)；[N2](../exceptions/N2-session-context/spec.md)。

## 1. 范围

Go 范围：`pkg/sessiontxn`（`interface.go`、`future.go`、`failpoint.go`、`internal`、`isolation`、`isolation/metrics`、`staleread`），另有 tracker 划给本块的 `pkg/session/txninfo`。共 157 个函数。

### 1.1 第 1 部分（本 PR，VS1-14 的 sessiontxn 部分）

| Go | Rust | 内容 |
|---|---|---|
| `interface.go`、`future.go` | `lib.rs` | `EnterNewTxnType`、`EnterNewTxnRequest`、`StmtErrorAction`、`ErrorAction` / `RetryReady` / `NoIdea`、`ConstantFuture`、`TxnContextProvider` / `TxnManager`（读取部分）、`AdviseOptimizeWithPlanAndThenWarmUp`、`NewTxn`、`NewTxnInStmt` |
| `internal/txn.go` | `internal.rs` | `CommitBeforeEnterNewTxn`、`GetSnapshotWithTS` |
| `isolation/base.go` | `isolation/base.rs` | `OnInitialize`（三种进入方式）、`ActivateTxn`、`prepareTxn` / `prepareTxnWithOracleTS` / `replaceTxnTsFuture`（TSO 预取：规划前 `AdviseWarmup` 发出、`ActivateTxn` 等待）、`GetStmtReadTS`、`GetSnapshotWithStmtReadTS` / `getSnapshotByTS`、`SetOptionsOnTxnActive`（读取相关选项）、`newOracleFuture`（含 `tidb_low_resolution_tso`）、`canReuseTxnWhenExplicitBegin`、各钩子 |
| `isolation/optimistic.go` | `isolation/optimistic.rs` | autocommit 读、点查 MaxTS（`AdviseOptimizeWithPlan`） |
| `isolation/repeatable_read.go` | `isolation/repeatable_read.rs` | 悲观 RR：读用事务 start ts；`notNeedGetLatestTSFromPD` |
| `isolation/readcommitted.go` | `isolation/readcommitted.rs` | 悲观 RC：每条语句一个 ts（预取、`getStmtTS`）、`NeedSetRCCheckTSFlag`、`planSkipGetTsoFromPD` |
| `isolation/serializable.go` | `isolation/serializable.rs` | 构造与错误钩子 |
| `staleread/util.go`、`provider.go` | `staleread/` | `IsStmtStaleness`；staleness provider 的骨架（状态、`GetStmtReadTS` 等取值方法；进入事务是桩） |

converge：D05.4 0/157 → 75/157。

### 1.2 永久豁免：写路径

tidb-rs 是只读节点；显式事务里的写语句和加锁读由会话拒绝（N11 D8，1792，D05.3 / VS1-14）。不移植：RR 的 for-update ts（`getForUpdateTs`、`updateForUpdateTS`）、悲观锁错误处理（`handleAfterPessimisticLockError`）、fair locking、`SetOptionsBeforeCommit`、`SetTxnAssertionLevel`、乐观重试（`onTxnActive` / `isOptimisticTxnRetryable` 设置的 `CouldRetry`）、`temporaryTableKVFilter`、RC 的写检查（`GetSnapshotWithStmtForUpdateTS` 的 RCCheckTS、`IsCheckTSInWriteStmtMode`）。`SetOptionsOnTxnActive` 中只用于提交的选项（commit hook、async commit / 1PC、disk full、`GuaranteeLinearizability`、后台 goroutine 钩子）同理。

### 1.3 第 2 部分

见 [tasks.md](tasks.md)。

## 2. 承载方式

- **会话、存储、规划器是 trait**（`sctx.rs`，glue）：Go 的 sessiontxn 持有 `sessionctx.Context`（会话，D05.3，rank 15）、调用 `kv.Storage.GetOracle()`（D07.1）、对规划器的值做类型断言（`planner/core`，rank 13）。它们都在本 crate 之上，所以写成 `SessionCtx`、`LazyTxn`、`Oracle`、`PlanInfo`（`plan_kind()` 枚举 + `IsPointGetWithPKOrUniqueKeyByAutoCommit`）、`StmtInfo`（`IsReadOnly`），由上层实现。每个方法对应一次 Go 调用。
- **会话作为参数传入**，不保存在 provider 中（N2：会话拥有 txnManager，txnManager 拥有 provider）。
- **provider 是一个结构体**：Go 用嵌入 `baseTxnContextProvider` 并设置函数字段的方式构造各隔离级别的 provider；这里是 `BaseTxnContextProvider` + `Kind` 枚举（各隔离级别的状态），函数字段改为 `match`（L2，同 M-SELF 的思路）。Go 的类型名保留为类型别名。
- **等待 TSO 的方法是 async**（N3）：`ActivateTxn`、`GetStmtReadTS`、`GetSnapshotWithStmtReadTS`、`OnInitialize`。TSO future 是 `BoxFuture`；RC 的语句 future 是枚举（`UseStartTs` 对应 Go 的 `funcFuture(p.getTxnStartTS)`、`Constant`、`Oracle`）。
- **`TransactionContext`** 加 `txn_scope`、`info_schema`（`TxnInfoSchema`：`Arc<dyn Any>`，因为 InfoSchema 的类型在更高的 crate）。

## 3. 读时间戳与 N11

- **TSO 预取**：照 Go，`AdviseWarmup`（规划前）发出 TSO 请求，`ActivateTxn`（首次需要读 ts）等待；点查在 `AdviseOptimizeWithPlan` 中改用 MaxTS，不取 TSO。
- **`ReadTsSource`**（N11 §4.4）：provider 给出本语句读 ts 的来源（`Tso`、`LowResolution`、`MaxTs`、`TxnStart`、`Stale`、`Snapshot`）。schema lease 检查（N11 §4.2、ADR 0005、#145）在语句准入时进行，只接受本语句自己的新 TSO，不接受事务的 start ts；它由会话 / domain 实现（VS1-14、VS1-17），本块只提供来源。
- **显式只读事务**（N11 D8，已确认）：`EnterNewTxnWithBeginStmt` 照 Go 进入（RR / optimistic 用一个 start ts，RC 每条语句一个 ts）。`READ ONLY` 的接受与写语句 1792 由会话负责（D05.3 / VS1-14）。
- **`tidb_rc_read_check_ts`**（N11 D11）：Go 在事务内的只读语句上设置 `RCCheckTS`，复用上一个 ts 并让 TiKV 检查写冲突。client-rust 支持之前，该语句报 1235（"not supported"），不能只复用 ts 而不检查（`go-exception: N11 D11`）。D11 在 #146 中尚待负责人确认，本 PR 按任务要求实现建议方案。

## 4. 与 Go 的差异（均已在代码中标注）

1. `GetTxnInfoSchema` 不返回 `SnapshotInfoschema`（该会话字段随 D05.2 / D06.5；`tidb_snapshot` 语句由 VS1 路由到 PoC），也不包 `SessionExtendedInfoSchema`（D06.3 第 2 部分 T9）。
2. `TransactionContext` 不记录 `CreateTime`（指标与慢日志，D14.x）；`History` 只用于写重试，恒为 nil。
3. `RetryInfo.Retrying`（写重试）恒为 false。
4. 临时表：`OnLocalTemporaryTableCreated` 与快照 interceptor 是 `blocked-by:D06.4` 的桩。
5. RR provider 不保存 for-update 状态（写路径）；它的 `GetStmtForUpdateTS` 是桩。

## 5. 测试

`crates/sessiontxn/tests/providers.rs`：模拟会话、存储、TSO，按 Go `isolation/*_test.go` 的场景覆盖 autocommit 读与 TSO 预取、点查 MaxTS、RR 显式事务一个 ts、RC 每条语句一个 ts（BEGIN 后与 lazy 事务两种）、`tidb_snapshot`、`autocommit=0` 与低精度 TSO、D11 的拒绝、staleness 骨架。Go 的这些测试依赖 testkit / store，不能直接移植；SQL 级对照随 VS1-14。
