# D05.4 事务上下文（tasks）

spec：[spec.md](spec.md)。状态截至 2026-11-14：第 1 部分进行中（本 PR）。

## 第 1 部分：只读路径（VS1-14 的 sessiontxn 部分）

- [x] T1 crate `tidb-sessiontxn`（rank 11）；`interface.go`、`future.go`、`internal`
- [x] T2 `isolation/base.go`：进入事务、激活、TSO 预取、读 ts、快照
- [x] T3 optimistic（autocommit、点查 MaxTS）、悲观 RR（读用 start ts）、悲观 RC（每条语句一个 ts）、serializable
- [x] T4 `staleread`：`IsStmtStaleness`、provider 骨架
- [x] T5 N11：`ReadTsSource`（§4.4）、`tidb_rc_read_check_ts` 拒绝（D11）
- [x] T6 模拟会话的场景测试；tracker govec 领域 `sessiontxn`
- [ ] T7 由 VS1-14 接入：会话实现 `SessionCtx` / `LazyTxn` / `TxnManager`，规划器实现 `PlanInfo` / `StmtInfo`，store 实现 `Oracle`；准入时的 lease 检查使用 `ReadTsSource`；显式只读事务的 1792（D05.3）

## 第 2 部分（`blocked-by:D05.4`，块完成前移植或改为永久豁免）

tracker 豁免理由写 "D05.4 part 2"。

- stale read：`staleread/processor.go` 全部、`CalculateAsOfTsExpr`、`CalculateTsWithReadStaleness`、`GetExternalTimestamp`、`StalenessTxnContextProvider` 的 `OnInitialize` / `activateStaleTxn` / `enterNewStaleTxnWithReplaceProvider` / `ActivateTxn` / `GetSnapshotWithStmtReadTS` / `OnStmtStart` / `OnStmtRetry` / `OnStmtErrorForNextAction` / `OnPessimisticStmt*` / `OnLocalTemporaryTableCreated`（需要 domain 的历史 InfoSchema，D06.5）
- `tidb_rc_read_check_ts` 的读检查（client-rust 支持 `RcCheckTs` 之后，N11 D11 (a)）
- `failpoint.go`、`staleread/failpoint.go`（测试钩子）、`isolation/metrics`（D14.1 指标）
- `pkg/session/txninfo`（事务摘要与 `TxnInfo.ToDatum`：`information_schema.trx_summary` / `TIDB_TRX`，D06.6 / D14.x 的读取者）
