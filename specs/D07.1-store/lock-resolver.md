# D07.1 前置调查：lock resolver 对照表（client-go ↔ client-rust）

对应 issue #101。只做对照，不改代码；代码移植范围在本表评审后另行确认。

## 0. 对照基准

| 对象 | 版本 | 来源 |
|---|---|---|
| client-go | `c3b50791b9fb`（`[tidb-8.5] *: upgrade Go to 1.25.12 (#2037)`，tidb-8.5 分支） | tracker `govec/go.mod`：`github.com/tikv/client-go/v2 v2.0.8-0.20260803075849-c3b50791b9fb` |
| TiDB | `v1.1.0-beta.0.20260827064614-8b857efa2036`（v8.5.8） | tracker `govec/go.mod` |
| client-rust | `dulao5/client-rust@2fc142f5dbf6` | tidb-rs `Cargo.toml` 中 `tikv-client` 的 `rev` |

下文 `go:N` 指 client-go `txnkv/txnlock/lock_resolver.go` 第 N 行，`rs:N` 指 client-rust `src/transaction/lock.rs` 第 N 行，`req.rs:N` 指 client-rust `src/transaction/requests.rs` 第 N 行；其他文件写全路径。

结论列的标记：

- **一致**：行为相同（允许实现方式不同）。
- **差异**：两边都有实现，但可观察行为不同（结果、RPC 次数、等待时间、错误）。
- **缺失**：client-rust 没有对应实现。
- **仅 rs**：client-rust 有、client-go 8.5 没有。
- **8.5 无**：client-go master 有、8.5 分支没有，**不移植**。
- **X**：读路径用不到（GC、写事务专用），不在本次范围。

## 1. 先说结论

1. **核心状态判定一致。** `CheckTxnStatus` 的请求参数、TTL=0 特殊协议、`TxnNotFound` 重试、悲观锁 `primaryMismatch`、async commit 从 secondaries 恢复状态、fallback 到 `forceSyncCommit`，client-rust 都已按 client-go 实现，且有单元测试。正确性上没有发现会让读结果不同的缺口。
2. **读路径的主要差距是"读优化"整体缺失**（§3.1）。client-go 8.5 的所有快照读（Get / BatchGet）和 coprocessor 都走 `ForRead=true`：已提交或已回滚的事务，不等它的锁被清掉，而是把事务 ID 放进重试请求的 `Context.resolved_locks` / `committed_locks`，清锁放到后台做。client-rust 只在 max-ts 点查里用了 `resolved_locks`，`committed_locks` 完全没用；其他读都是"同步清锁，再重读"。结果相同，但遇锁时延迟更高、RPC 更多。
3. **遇到活跃锁时的等待策略不同**（§3.2）。client-go 按锁剩余 TTL（`msBeforeTxnExpired`）限制每次退避的最长睡眠；client-rust 只用固定的 `lock_backoff` 序列，不看 TTL。
4. **事务状态缓存的作用域不同**（§3.5）。client-go 每个 store 一个 2048 条的 FIFO 缓存，跨请求共享；client-rust 的缓存每次 `resolve_locks` 调用新建一个，调用结束就丢。
5. **shared lock**（§3.6）：v8.5.8 只有在 `tidb_foreign_key_check_in_shared_lock = ON`（默认 OFF）时才产生。client-go 的快照读遇到也报错（与 client-rust 的 `reject_shared_locks` 一致）；**但 TiDB 的 coprocessor 会把 `shared_lock_infos` 展开逐个解析**，client-rust 的 coprocessor 会直接报错。这是唯一一处"Go 能读、Rust 报错"的点。
6. **#101 列出的三项在 8.5 中不存在**：txn file 锁、`asyncResolveTaskPool`、lock hints（`LockHintsInRequest` / `backoffOnLockHintsInRequest`）。见 §4。

## 2. 调用方：谁用什么参数调 resolver

| 读路径 | client-go 8.5 / TiDB v8.5.8 | client-rust `2fc142f` | 结论 |
|---|---|---|---|
| 点查 `Get`（普通 start_ts） | `txnsnapshot/snapshot.go:786` `NewClientHelper(..., resolveLite=true)` → `ForRead=true, Lite=true`；ignore/access 进 `resolvedLocks`/`committedLocks` | `ResolveLock` plan（`src/request/plan.rs:1286`）→ `resolve_locks`（非 ForRead） | **差异**（§3.1、§3.3） |
| 点查 `Get`（`version == MaxUint64`，autocommit 点查） | `snapshot.go:900`：只被第一个锁阻塞，之后遇到的其他事务的锁直接放入 `resolvedLocks` | `transaction.rs:1157` `get_at_max_ts`：同样只被第一个锁阻塞；`resolve_locks_for_max_ts_read` 只把 `MinCommitTsPushed` 放入 `resolved_locks` | **差异**：Rust 未处理 canAccess；已提交/已回滚走同步清锁 |
| `BatchGet` | `snapshot.go:591` `resolveLite=false`，`ForRead=true` | `ResolveLock` plan → `resolve_locks` | **差异**（§3.1） |
| Scan（`txnsnapshot/scan.go:313`） | `ResolveLocks`（**非** ForRead，同步） | `ResolveLock` plan → `resolve_locks` | **一致** |
| coprocessor | TiDB `pkg/store/copr/coprocessor.go:924` `resolveLite=false`，`ForRead=true`；`:1824` 展开 `shared_lock_infos` | `src/transaction/coprocessor.rs:645` `resolve_locks`（非 ForRead），不发 `resolved_locks`/`committed_locks` | **差异**（§3.1、§3.6） |
| GC `BatchResolveLocks` | `go:275` | `rs:567` `cleanup_locks` | **X** |

## 3. 逐项对照

### 3.1 ForRead：canIgnore / canAccess 与后台清锁

| client-go 函数/分支 | client-rust 对应 | 结论 |
|---|---|---|
| `ResolveLocksForRead` / `ResolveLocksWithOpts`（`go:422`、`go:447`），`ResolveLocksOptions.ForRead` | `resolve_locks_impl(max_ts_read)`（`rs:134`），仅 max-ts 点查传 `true` | **缺失**（通用 ForRead） |
| `resolveLocks` 中 `!forRead` 时 TTL 未过期才 `continue`（`go:616`）；ForRead 时对活跃锁也做 canIgnore 判断 | 无 | **缺失** |
| canIgnore：`action == MinCommitTSPushed` 或已回滚 或 `commitTS > callerStartTS`（`go:624`） | 仅 `MinCommitTsPushed` 且 max-ts（`rs:188`） | **缺失**（已回滚、提交在读之后两种情况） |
| canAccess：已提交且 `commitTS <= callerStartTS`（`go:630`），由 `ClientHelper` 放入 `Context.committed_locks`（`txnsnapshot/client_helper.go`） | 无；`committed_locks` 字段从未赋值 | **缺失** |
| ForRead 时非悲观锁的 `resolveLock` 在 goroutine 里异步执行（`go:584`），用 `asyncResolveCtx` + `asyncResolveLockMaxBackoff = 40000` | 同步 `resolve_lock_with_retry`（`rs:203`），完成后才重读 | **差异**（延迟） |
| ForRead 时 async commit 的整笔清锁也异步执行（`resolveAsyncCommitLock(..., asyncResolveAll=forRead)`，`go:1092`） | 同步 | **差异**（延迟） |
| `LockResolver.Close()` 取消后台任务（`go:110`） | 无后台任务 | 随上项一起处理 |

影响：结果一致（TiKV 对 `resolved_locks` 中的事务跳过锁，对 `committed_locks` 中的事务按已提交读取，和清完锁再读等价）。差异在遇锁时多一次或多轮同步 `ResolveLock` RPC 才能重读。热点行被大事务长期持锁时，延迟差距明显。

### 3.2 TTL 与等待（`txnExpireTime` / `msBeforeTxnExpired`）

| client-go | client-rust | 结论 |
|---|---|---|
| 对所有锁先查事务状态，状态已定就立即清锁，不看锁 TTL（`go:604`、`resolve` 闭包） | 同（`rs:156` 注释及循环） | **一致** |
| 活跃锁：`UntilExpired(txnID, ttl)` 取最小值作为 `ResolveLockResult.TTL`（`go:617`、`go:636`、`go:671`） | 只返回活跃锁列表 `live`，不算剩余时间 | **缺失** |
| 调用方用 `BackoffWithMaxSleepTxnLockFast(msBeforeExpired)`：每次睡眠不超过锁剩余 TTL（`snapshot.go:582`、copr `coprocessor.go:1843`） | `ResolveLock` plan / coprocessor 用固定 `lock_backoff.next_delay_duration()`（`plan.rs:1310`、`coprocessor.rs:653`） | **差异**：锁快过期时 Rust 可能睡得比需要的久；锁 TTL 很长时 Go 也是按退避上限睡，二者相近 |
| 出错时 `msBeforeTxnExpired.update(0)`（`go:611`） | 直接返回错误 | **一致**（调用方都把错误上抛） |

### 3.3 lite 模式

| client-go | client-rust | 结论 |
|---|---|---|
| `resolveLite = lite \|\| l.TxnSize < ResolveLockLiteThreshold`（默认 16，`config/client.go:193`）（`go:1208`） | 无；`new_resolve_lock_request` 不带 keys（`req.rs:192`），总是整 region 扫 | **缺失**（性能） |
| lite 且 `key == primary` 时跳过（CheckTxnStatus 已清过 primary）（`go:1210`） | 不跳过，总对 encountered key 所在 region 发 ResolveLock | **差异**（多一次 RPC） |
| lite 时 `Keys = [l.Key]`，且不记 `cleanRegions`（`go:1230`、`go:1267`） | 无 | **缺失** |

### 3.4 `getTxnStatusFromLock` / `getTxnStatus`（含 TxnNotFound）

| client-go 分支 | client-rust 对应 | 结论 |
|---|---|---|
| `l.TTL == 0` → `currentTS = MaxUint64`（`go:716`） | `rs:831` | **一致** |
| 否则 `currentTS = GetLowResolutionTimestamp`（本地缓存，无 RPC）（`go:723`） | 每次 `resolve_locks` 调一次 `pd_client.get_timestamp()`（PD RPC）（`rs:146`） | **差异**（多一次 PD RPC；数值上 Rust 更新） |
| `TxnNotFound` 且锁已过期 → `rollbackIfNotExist = true` 重试（`go:748`） | `rs:861`；过期判断前又调一次 `get_timestamp()` | **一致**（Rust 多一次 PD RPC） |
| `TxnNotFound`、锁未过期、悲观锁 → 返回 `TxnStatus{ttl: l.TTL}`（当作活跃）（`go:768`） | 返回 `Locked(lock_ttl, lock)`（`rs:868`） | **一致** |
| `TxnNotFound`、锁未过期、非悲观 → `bo.Backoff(BoTxnNotFound)` 后重试，共用调用方 backoffer 预算（`go:777`） | 新建 `OPTIMISTIC_BACKOFF` 退避（每次调用独立预算）（`rs:877`） | **差异**（预算来源不同；最终都返回错误） |
| failpoint `txnNotFoundRetTTL`、`txnExpireRetTTL`、`getTxnStatusDelay` | 无 | **X**（测试用） |
| `getTxnStatus` 先查缓存（`go:804`） | `check_txn_status` 先查缓存（`rs:708`） | **一致**（缓存作用域见 §3.5） |
| 请求字段：`PrimaryKey/LockTs/CallerStartTs/CurrentTs/RollbackIfNotExist/ForceSyncCommit/ResolvingPessimisticLock/VerifyIsPrimary=true`（`go:822`） | `new_check_txn_status_request`（`req.rs:631`）字段相同，另带 `is_txn_file` | **一致** |
| `ResolvingPessimisticLock = lockInfo.IsPessimistic()`，含 `SharedPessimisticLock`（`go:201`、`go:821`） | `lock_type == PessimisticLock`（`rs:853`） | **一致**（shared 已被提前拒绝，见 §3.6） |
| region error → `BoRegionMiss` 退避后重新定位（`go:851`） | `retry_multi_region(DEFAULT_REGION_BACKOFF)` | **一致** |
| `KeyError.TxnNotFound` → `txnNotFoundErr`；`PrimaryMismatch` 且 resolving 悲观锁 → `primaryMismatch`；其他 → 错误（`go:862`） | `rs:746`：`TxnNotFound` → `Error::TxnNotFound`；其他（含 `primary_mismatch`）→ `Error::KeyError`，由 `action_for_lock` 判断 | **一致** |
| 响应 → `TxnStatus{ttl, commitTS, action, primaryLock}`（`go:878`） | `TransactionStatus{kind, action, is_expired}`（`req.rs:696`） | **一致** |
| `IsRolledBack`：`ttl==0 && commitTS==0 && action ∈ {NoAction, LockNotExistRollback, TTLExpireRollback}`（`go:127`） | `is_determined`（`req.rs:742`）同一集合 | **一致** |
| TTL 是否过期：`oracle.IsExpired(txnID, status.ttl)`，本地低精度时钟（`go:537`） | `check_ttl(current)`，`check_txn_status` 里又调一次 `get_timestamp()`（`rs:763`） | **差异**（每次 CheckTxnStatus 多一次 PD RPC） |

### 3.5 已解析状态缓存与 cleaned region

| client-go | client-rust | 结论 |
|---|---|---|
| `LockResolver.mu.resolved`：每个 store 一个，跨请求共享（`go:61`） | `ResolveLocksContext`，`resolve_locks_impl` 每次调用新建（`rs:153`） | **差异**：同一事务的锁在不同读请求中会重复 `CheckTxnStatus` |
| 容量上限 `ResolvedCacheSize = 2048`，FIFO 淘汰（`go:44`、`go:258`） | 无上限（但只活一次调用） | **差异**（若改为共享缓存，必须同时加上限） |
| `StatusCacheable` = `IsStatusDetermined`（`go:172`） | `is_cacheable` = `is_determined`（`req.rs:762`） | **一致** |
| `saveResolved` 存未定状态时 panic；已有条目与新状态不同（`HasSameDeterminedStatus`）时 panic（`go:236`） | `save_resolved` 直接覆盖，不校验（`rs:425`） | **缺失**（一致性断言） |
| async commit 恢复后 `saveResolved`（`go:1082`） | `rs:549` | **一致** |
| `cleanTxns`：每次 `resolveLocks` 内 txn → region 集合（`go:521`） | `clean_regions`（`rs:155`），同作用域 | **一致** |
| `pessimisticCleanTxns` + `PessimisticRegionResolve`（`go:522`、`go:573`） | 无 | **X**：只有写路径（`transaction/prewrite.go:513`、`pessimistic.go:424/572`）设置 |

### 3.6 shared lock

| client-go / TiDB | client-rust | 结论 |
|---|---|---|
| 产生条件：`tidb_foreign_key_check_in_shared_lock = ON`（默认 OFF，TiDB `sessionctx/variable/tidb_vars.go:1598`），外键检查用 `InShareMode` 加锁（`executor/adapter.go:1201`），prewrite 为 `Op_SharedLock`（client-go `transaction/2pc.go:555`） | — | v8.5.8 **会**产生，但默认关闭 |
| `resolveLocks` / `BatchResolveLocks` 遇到 `IsShared()` 报 "misuse" 错误（`go:605`、`go:291`） | `reject_shared_locks`（`rs:62`）在 `resolve_locks_impl`、`cleanup_locks` 入口报错 | **一致**（错误文字不同） |
| 快照读 Get/BatchGet/Scan：`ExtractLockFromKeyErr` 不展开 wrapper，直接进 `resolveLocks` → 报错 | 报错 | **一致** |
| TiDB coprocessor 展开 `GetSharedLockInfos()`，对每个成员锁分别解析（`pkg/store/copr/coprocessor.go:1824`） | `coprocessor.rs:645` 整个 wrapper 交给 `resolve_locks` → 报错 | **差异**：Go 能读、Rust 报错 |
| `IsPessimistic` 含 `SharedPessimisticLock`（`go:202`） | 被 `reject_shared_locks` 一并拒绝 | 展开后的成员锁不会是 shared 类型（`go:205` 注释），按普通锁处理即可 |

未验证：读请求是否真的会碰到 shared lock。`Op_Lock` 类锁本来不阻塞 SI 读，TiKV 对 `SharedLock` wrapper 的读冲突检查需要看 TiKV 8.5 代码确认。TiDB 的 coprocessor 专门处理了它，说明至少在某些情况下会返回。

### 3.7 async commit

| client-go | client-rust | 结论 |
|---|---|---|
| `expiredAsyncCommitLocks = primaryLock.UseAsyncCommit && !forceSyncCommit && ttlExpired`（`go:538`） | `Locked(..) if is_expired && use_async_commit`（`rs:496`） | **一致** |
| 同一事务已处理过就不再恢复（`cleanTxns` 中已有 → 直接返回）（`go:561`） | 恢复后状态已缓存 → 走 `from_status` 只清 encountered lock 所在 region（已在 `clean_regions` 中则跳过） | **一致**（RPC 数量相同或更少） |
| `checkAllSecondaries`：按 region 分组并发发 `CheckSecondaryLocks`（`go:1112`） | `check_all_secondaries`：`preserve_shard().retry_multi_region()`（`rs:781`） | **一致** |
| `checkSecondaries` 遇 region error：退避后重新分组、递归（`go:995`） | `retry_multi_region` 重新分片 | **一致** |
| `addKeys`：锁数 < 请求 key 数 → 用响应 `commit_ts`；多个 region 的 `commit_ts` 不同则报错；`commitTS < minCommitTs` 报错；`lock_version` 不符报错；非 async-commit 锁 → `nonAsyncCommitLock`（`go:929`） | `CollectSecondaryLocks::merge` + `resolved_commit_version`（`req.rs:821`、`req.rs:925`），检查项相同，另加"返回锁数多于 key 数"检查 | **一致**（Rust 更严） |
| commit_ts 取 `max(primary.minCommitTs, 所有 secondaries.minCommitTs)`（`go:1120`、`go:965`） | `resolved_commit_version(primary_min_commit_ts)` | **一致** |
| `nonAsyncCommitLock` → `resolve(l, forceSyncCommit=true)` 重查（`go:566`） | `fallback_2pc` → `get_txn_status_from_lock(..., true)`（`rs:528`） | **一致** |
| 状态已定时直接清 `secondaries + primary`（`go:1066`） | 无对应分支 | **一致**：进入该函数的前提是 `primaryLock != nil`（锁仍在），状态不可能已定，此分支实际走不到 |
| `resolveAsyncResolveData`：按 region 分组，`ResolveLock` 带 `Keys`，并发（`go:1026`、`go:1148`） | 对每个 key 的 region 发不带 keys 的整 region `ResolveLock`，串行，按 `clean_regions` 去重（`rs:201`） | **差异**（整 region 扫描、串行；结果相同） |
| `resolveRegionLocks` 遇 `KeyError` 只记日志并返回 nil（`go:1196`）；普通路径 `resolveLock` 遇 `KeyError` 则返回错误（`go:1262`） | async commit 整笔清锁与普通路径共用 `resolve_lock_with_retry`，都返回错误（`rs:365`） | **差异，决定照搬 Go（L1）**：只在 async commit 整笔清锁这一处吞掉 `KeyError`（记日志、返回成功），普通路径仍返回错误。这是有意与 Go 一致：锁没清掉时，调用方重读会再次遇到这个锁，事务状态已在缓存中，于是再次清锁或进入退避，不会读到错误的数据 |

### 3.8 悲观锁

| client-go | client-rust | 结论 |
|---|---|---|
| `primaryMismatch` 且是悲观锁 → 当作 `TxnStatus{}`，随后 `resolvePessimisticLock`（`go:527`） | `rs:481` → `pessimistic_rollback_lock`，返回 `Done` | **一致** |
| `primaryMismatch` 但不是悲观锁 → 报错（`go:528`） | `Err(KeyError)` 原样上抛 | **一致** |
| 状态 ttl==0（已定或 `LockNotExistDoNothing` 等）且是悲观锁 → `resolvePessimisticLock`，只回滚这个 key（`go:571`） | `rs:502` | **一致** |
| `resolvePessimisticLock`：`key == primary` 时跳过；`forUpdateTS = LockForUpdateTS`，为 0 时取 `MaxUint64`；`Keys=[l.Key]`（`go:1279`） | `pessimistic_rollback_lock`（`rs:263`） | **一致** |
| 悲观锁的 `resolvePessimisticLock` 必须在 `getTxnStatus` 之后调用（tidb#45134）（`go:315`） | 在 `action_for_lock` 中先 `get_txn_status_from_lock` 再回滚 | **一致** |
| ForRead 下悲观锁的 canIgnore/canAccess 判定 | 无（见 §3.1） | 悲观锁本身不阻塞读，影响很小 |

### 3.9 ResolveLock 请求与 region / gRPC 错误

| client-go | client-rust | 结论 |
|---|---|---|
| `resolveLock`：定位 key → 已在 `cleanRegions` 则跳过 → 发 `ResolveLock`（提交时带 `CommitVersion`）（`go:1204`） | `resolve_lock_with_retry`（`rs:297`） | **一致** |
| region error → `BoRegionMiss` 退避后重试（`go:1251`） | `handle_region_error` + 退避（`rs:348`） | **一致** |
| `SendReq` 失败（gRPC）由 region request sender 内部重试 | 显式 `is_grpc_error` → 失效 region/store 缓存后退避重试（`rs:381`） | **一致**（实现位置不同） |
| `LeaderNotFound` 由 sender 处理 | `rs:326` 失效缓存后退避 | **一致** |
| `KeyError` → 报错（`go:1262`） | `rs:365` | **一致** |
| `MaxExecutionDurationMs = MaxWriteExecutionTime`、`RequestSource`、`ResourceGroupName` 等上下文字段 | 未设置 | **缺失**（与资源管控、慢日志相关，需单独评估） |

### 3.10 观测与其他

| client-go | client-rust | 结论 |
|---|---|---|
| `RecordResolvingLocks` / `UpdateResolvingLocks` / `ResolveLocksDone` / `Resolving()`（`go:461`–`go:668`），TiDB 用于 `information_schema.DATA_LOCK_WAITS`（`executor/infoschema_reader.go:3141`） | 无 | **缺失**（观测，可归入 N9 或单列例外） |
| `ResolveLockDetail.ResolveLockTime`（`go:512`），进慢日志 `Resolve_lock_time` | 无；tidb-rs `crates/execdetails` 已有字段但无来源 | **缺失**（观测） |
| `metrics.LockResolverCountWith*` | `metrics::lock_resolver(LockMetric::*)` | **一致**（`client-go-metrics` feature） |
| `testingKnobs.meetLock`、failpoint `tryResolveLock`/`resolveLock`/`resolveAsyncResolveData`/`resolveAsyncCommitLockReturn` | 无（另有 `before-cleanup-locks`） | **X**（测试用） |
| `GetTxnStatus`（公开 API，`go:701`） | `LockResolver::check_txn_status`（`rs:695`） | 读路径不用 |
| `BatchResolveLocks`（`go:275`） | `cleanup_locks`（`rs:567`）；悲观锁处理与 Go 不同（Go 对悲观锁调 `resolvePessimisticLock`，Rust 统一放入批量 `TxnInfo`） | **X**（GC） |

## 4. client-go master 有、8.5 没有（不移植）

| 功能 | 引入 master 的提交 | 8.5 情况 | client-rust 现状 |
|---|---|---|---|
| txn file 锁（`Lock.IsTxnFile`，ResolveLock/CheckTxnStatus 带 `is_txn_file`，批量清锁按此分组） | `6125c3b` txnkv: Support resolve txn file locks (#2003) | 无 | **仅 rs**：请求里带 `is_txn_file`（8.5 的锁该字段恒为 false，无害），保持即可 |
| `asyncResolveTaskPool`（后台清锁任务池，限制 goroutine 数） | `2115057` txnkv: reduce mem / goroutine costs for resolve lock tasks (#1940) | 无（8.5 每个后台清锁起一个 goroutine） | 无 |
| lock hints（`LockHintsInRequest`、`backoffOnLockHintsInRequest`：TiKV 忽略 `resolved_locks`/`committed_locks` 时退避） | `5f38569` txnkv: back off when TiKV ignores lock hints (#2075) | 无 | 无 |

## 5. 建议的移植范围（待确认）

按对读结果和延迟的影响排序：

| # | 内容 | 依据 | 建议 |
|---|---|---|---|
| R1 | 通用 ForRead：canIgnore（含已回滚、提交在读之后）/ canAccess、`committed_locks`，用于 Get / BatchGet / coprocessor | §3.1 | 移植（L3：与 client-go 行为一致） |
| R2 | ForRead 下后台异步清锁（含 async commit 整笔清锁）及 `Close` 取消 | §3.1 | 随 R1 移植；8.5 没有任务池，按 8.5 每锁一个 task 的语义即可 |
| R3 | `msBeforeTxnExpired` 及按 TTL 限制退避睡眠 | §3.2 | 移植 |
| R4 | coprocessor 展开 `shared_lock_infos` | §3.6 | 移植（只在 coprocessor；快照读保持报错，与 Go 一致） |
| R5 | lite 模式（`TxnSize < 16`、Get 的 `lite=true`、primary 跳过） | §3.3 | 移植（性能，结果不变） |
| R6 | 跨请求共享的状态缓存（2048 FIFO）+ `HasSameDeterminedStatus` 断言 | §3.5 | 移植 |
| R7 | `current_ts` 用低精度本地时钟，去掉每次 CheckTxnStatus 后的额外 TSO | §3.4 | 移植（减少 PD RPC） |
| R8 | async commit 清锁按 region 分组带 keys、并发 | §3.7 | 可选（性能） |
| R9 | `resolveRegionLocks` 吞 `KeyError` | §3.7 | 已决定：照搬 Go（L1），只限 async commit 整笔清锁 |
| R10 | `Resolving()`（DATA_LOCK_WAITS）、`ResolveLockTime` | §3.10 | 归入观测相关块，不在本次 |
| R11 | `MaxExecutionDurationMs`、`RequestSource`、`ResourceGroupName` | §3.9 | 单独评估（与资源管控一起） |

测试：R1–R4 需要 #101 计划第 3 步的混合集群场景（真实锁：prewrite 后暂停、悲观事务持锁、async commit 中断、`tidb_foreign_key_check_in_shared_lock=ON` 的外键检查），比较 Go TiDB 与 tidb-rs 的读结果和错误；R5–R8 结果不变，用单元测试（mock KV client 计数 RPC）验证 RPC 次数与请求字段。
