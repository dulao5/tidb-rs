# D07.1 store driver（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D00.1）`ValidateSnapshotReadTS`：需要 `kv.Storage.GetOracle`（oracle 客户端）
- [ ] P1-2 （D05.2）savepoint：`TransactionContext.AddSavepoint`、`TransactionContext.DeleteSavepoint`、`TransactionContext.GetCurrentSavepoint`、`TransactionContext.ReleaseSavepoint`、`TransactionContext.RestoreBySavepoint`、`TransactionContext.RollbackToSavepoint`：恢复事务 membuffer 的 staging

## 第 1 部分：点查读取（VS1-18）

函数清单是 `specs/VS1-vertical-slice/functions.tsv` 中 VS1-18 的 40 个函数，`scripts/converge.py --block D07.1 --part specs/VS1-vertical-slice/functions.tsv --item VS1-18` 为 40/40。代码在 `crates/store/src/driver/`，client-go 的部分经 `driver/tikv.rs` 边界接到 client-rust（ADR 0008）。

- [x] `tikvSnapshot.Get` / `BatchGet` / `SetOption`、`NewSnapshot`、`getTiKVIsolationLevel` / `getTiKVPriority`、`toTiKVKeys`
- [x] `ToTiDBErr`、`extractKeyErr`、`newWriteConflictError`、`prettyWriteKey`、`prettyLockNotFoundKey`、`genKeyExistsError`
- [x] `backoff.Backoffer`（client-rust 的 `Backoff::time_budget`）
- [x] `BufferBatchGetter`（client-go 的 `transaction.BufferBatchGetter` 一并按 L3 移植）
- [x] `tikvStore` 的 `Name` / `Describe` / `GetClient` / `GetCodec` / `GetSnapshot` / `CurrentVersion`
- 边界桩：`tikvSnapshot.Iter` / `IterReverse`（`blocked-by:D07.1`，第 2 部分的 scanner，T11）；`ExtractKeyExistsErrFromHandle` / `FromIndex`（`blocked-by:D06.4`，写路径）

与 Go 的差异（都来自 client-rust 没有对应能力，L3）：

1. **错误对应**：client-rust 的错误按 client-go 的错误身份分类（`driver/error.rs` 的 `client_go_error`）：退避预算用完时按睡得最久的那类错误（client-go 的 `longestSleepCfg`）；没有在预算内重试就返回的 region 错误和 `LeaderNotFound` 记为 `ErrRegionUnavailable`，清锁失败记为 `ErrResolveLockTimeout`，与 PoC（`poc-kv` error.rs）的分类相同。其余错误保留原文（Go 的 `errors.Trace`）。
2. **快照选项**：priority、not-fill-cache、replica read、sample step、task ID、staleness、store labels、resource group tag / name、request source、load-based replica read threshold 只记录，不发送（client-rust 没有对应的请求字段）；MaxTS 点查由 client-rust 自己提为高优先级。`tikv_client_read_timeout` 和快照 ts 生效。
3. **RC check-ts**：隔离级别为 `RCCheckTS` 时读请求报错，不当作 SI 读（N11 §4.4、宪章第五条）；RC 按 SI 发送（解析锁而不是忽略锁，结果相同）。
4. **commit ts**：client-rust 不返回 commit ts，`WithReturnCommitTS` 照 client-go 的 `checkCommitTSRequired` 报错。
5. **其他**：读后不做 GC safe point 检查（与 copr 的适配器相同）；同一个快照上的读串行执行；`Backoffer.GetBackoffTimes` / `GetBackoffSleepMS` 返回空表（client-rust 不提供按类型的计数）。
6. **预算**：每次读 `getMaxBackoff`（20 s）× `tidb_backoff_weight`（默认 2）= 40 s，与 PoC 的 `tidb_retry_options` 相同。

故障场景对照：`TIDB_RS_POINT_GET=d07.1` 让 PoC（`poc-kv`）的点查和 BatchGet 改走本部分的快照，`tools/ab/faults/run.sh` 即可在同一套场景下比较两种实现，结果见 PR。
