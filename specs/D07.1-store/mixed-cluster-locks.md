# D07.1：真实锁下的读对比（#101 第 3 步）

2026-11-13。验证 client-rust fork 的 R1–R4（[lock-resolver.md](lock-resolver.md) §5）在真实 TiKV 集群上的行为与 Go TiDB 一致。工具与用法见 [`tools/ab/locks/`](../../tools/ab/locks/README.md)。

## 结论

1. **结果一致**：5 种锁加 FK shared lock，每种用三种读法，共 18 组对比。Go TiDB 与 tidb-rs 的读结果和错误全部相同，也都符合预期（下表"预期"列）。三种读法是：
   - **点查（max-ts）**：自动提交的纯点查，两边都按 MaxTS 读，不取 TSO。
   - **点查（TSO）**：聚合套点查，两边都按语句的 start_ts 读；tidb-rs 侧是 `snapshot(start_ts)` + client-rust `Transaction::get`，即 R1 修改的路径。
   - **范围扫描**：走 coprocessor。
2. **§3.6 在真实集群上复核**：`tidb_foreign_key_check_in_shared_lock = ON` 的外键检查确实在父表行上留下 shared lock wrapper（ScanLock 返回 `SharedLock[<ts>:PessimisticLock]`），两边读父表都不受影响，立即返回原值。
3. **R2（后台清锁）**：tidb-rs 第一次读之后，已判定事务（已过期、已提交 primary、async commit）的锁在 300 ms 内被清掉（scanlocks 2→0 或 1→0），第二次读不再发 CheckTxnStatus / ResolveLock。活锁和悲观锁按预期保留。
4. **R1 的效果**：R1 之前（client-rust `2fc142f`），遇到活锁时，点查（TSO）要 **8383 ms、12 次 CheckTxnStatus**，范围扫描要 **6880 ms、11 次**，都是一直等到 TTL 过期才回滚。R1–R4 之后（`ee7f1e2`）两者都是 **4 ms、1 次**，和 Go 一样：CheckTxnStatus 推高 min_commit_ts 后直接忽略这个锁。
5. **一处时间上的差别（不影响结果）**：async commit 已过期时，只有**点查（max-ts）**出现差别：Go 用 54–107 ms、4–5 次 CheckTxnStatus，tidb-rs 用 4 ms、1 次。同一种锁的点查（TSO）和范围扫描，Go 都是 4–5 ms、1 次。

   依据代码的解释（未用日志直接验证）：Go 的 `getTxnStatusFromLock` 用 `GetLowResolutionTimestamp()` 作为 CheckTxnStatus 的 `current_ts`（client-go `txnlock/lock_resolver.go:723`），这个低精度时间戳在每次 `GetTimestamp()` 取到 TSO 时刷新（`oracle/oracles/pd.go:220`），平时由后台定期刷新。点查（TSO）和范围扫描这条语句刚取过 start_ts，低精度时间戳是新的，TiKV 立即判定锁已过期；max-ts 点查不取 TSO，低精度时间戳可能早于锁过期的时刻，TiKV 便回答"未过期"，Go 退避重试，直到后台刷新。tidb-rs 每次解析锁都取一个新的 TSO，所以没有这个现象。R7 改用低精度时间戳时，要照 Go 在每次取 TSO 时刷新它，并把这种情况（max-ts 点查不取 TSO）写进 R7 的测试。
6. **没测到的**：
   - 批量点查：tidb-rs 的 SQL 层还不支持对主键用 `IN` / `OR`（返回 1235），BatchGet 的锁处理目前只由 client-rust 的单测覆盖（`snapshot_reads_share_lock_hints`）。
   - "提交在读之后 → 忽略"：需要一个在读事务 start_ts 之后才提交的锁，必须由显式事务先取 start_ts 再读，tidb-rs 只支持自动提交，目前只有 client-rust 单测覆盖（`test_resolve_locks_for_read_classifies`）。
   - 同一事务内跨语句复用 hint 集合：同样需要显式事务，目前只有 client-rust 单测覆盖（`snapshot_reads_share_lock_hints`）。
   - tidb-rs 的 SQL 层支持后，把这几项加回 `mixed_locks.py`。

## 环境

- tiup playground v8.5.8（1 PD、1 TiKV、Go TiDB），与 tidb-rs 同在一台 Linux arm64 虚拟机。sbtest1 共 10 万行，分 8 个 region。
- tidb-rs：主测用 #124 分支（client-rust `ee7f1e2`，含 R1–R4）；对照组用 client-rust `2fc142f`（R1 之前）。
- 确认 tidb-rs 的两种点查确实走不同路径：干净的行上，点查（max-ts）不取 TSO（`pd_client_cmd_handle_cmds_duration_seconds_count{type="tso"}` 增量 0），点查（TSO）取 1 次。tidb-rs 中有聚合的计划不走 max-ts（`crates/poc/session/src/lib.rs` 的 `max_ts_point_get` 要求 `plan.agg.is_none()`）；Go 的 MaxTS 优化也只用于纯 PointGet（`sessiontxn/isolation/optimistic.go:110-121`）。
- 造锁：`lockgen`，client-go `c3b5079`（与 v8.5.8 相同），通过 `CommitterProbe` 逐步执行两阶段提交。

## 结果（client-rust `ee7f1e2`）

"RPC"列是语句执行期间加上之后 300 ms 内的计数（CTS = CheckTxnStatus，RL = ResolveLock，CSL = CheckSecondaryLocks）。"锁数"是 tidb-rs 读之前 → 第一次读后 → 第二次读后。

| 锁（预期） | 读法 | Go TiDB：结果 / 耗时 / RPC | tidb-rs：结果 / 耗时 / RPC | 锁数 | tidb-rs 第二次读 RPC |
|---|---|---|---|---|---|
| 2PC 只 prewrite，TTL 已过期（OLD，回滚） | 点查（max-ts） | OLD / 5 ms / CTS 1 | OLD / 5 ms / CTS 1、RL 1 | 2→0→0 | 无 |
| | 点查（TSO） | OLD / 9 ms / CTS 1、RL 1 | OLD / 5 ms / CTS 1、RL 1 | 2→0→0 | 无 |
| | 范围 | OLD / 5 ms / CTS 1 | OLD / 5 ms / CTS 1、RL 1 | 2→0→0 | 无 |
| 只提交 primary，secondary 留锁（NEW，提交） | 点查（max-ts） | NEW / 4 ms / CTS 1、RL 1 | NEW / 3 ms / CTS 1、RL 1 | 1→0→0 | 无 |
| | 点查（TSO） | NEW / 4 ms / CTS 1、RL 1 | NEW / 4 ms / CTS 1、RL 1 | 1→0→0 | 无 |
| | 范围 | NEW / 4 ms / CTS 1、RL 1 | NEW / 4 ms / CTS 1、RL 1 | 1→0→0 | 无 |
| 2PC 只 prewrite，TTL 6 s 未过期（OLD，忽略） | 点查（max-ts） | OLD / 4 ms / CTS 1 | OLD / 3 ms / CTS 1 | 2→2→2 | CTS 1 |
| | 点查（TSO） | OLD / 5 ms / CTS 1 | OLD / 4 ms / CTS 1 | 2→2→2 | CTS 1 |
| | 范围 | OLD / 4 ms / CTS 1 | OLD / 4 ms / CTS 1 | 2→2→2 | CTS 1 |
| 悲观事务持锁（OLD，不阻塞） | 点查（max-ts） | OLD / 3 ms / 无 | OLD / 3 ms / 无 | 2→2→2 | 无 |
| | 点查（TSO） | OLD / 3 ms / 无 | OLD / 3 ms / 无 | 2→2→2 | 无 |
| | 范围 | OLD / 4 ms / 无 | OLD / 3 ms / 无 | 2→2→2 | 无 |
| async commit 全部 prewrite，TTL 已过期（NEW，从 secondaries 恢复为提交） | 点查（max-ts） | NEW / 67 ms / CTS 4、RL 1、CSL 1 | NEW / 4 ms / CTS 1、RL 1、CSL 1 | 2→0→0 | 无 |
| | 点查（TSO） | NEW / 5 ms / CTS 1、RL 1、CSL 1 | NEW / 4 ms / CTS 1、RL 1、CSL 1 | 2→0→0 | 无 |
| | 范围 | NEW / 5 ms / CTS 1、RL 1、CSL 1 | NEW / 5 ms / CTS 1、RL 1、CSL 1 | 2→0→0 | 无 |
| FK shared lock `SharedLock[…PessimisticLock]`（原值） | 点查（max-ts） | 原值 / 6 ms | 原值 / 4 ms | — | — |
| | 点查（TSO） | 原值 / 4 ms | 原值 / 3 ms | — | — |
| | 范围 | 原值 / 4 ms | 原值 / 3 ms | — | — |

说明：

- tidb-rs 的 ResolveLock 多数在语句返回之后才发出（后台，R2），只是在 300 ms 内完成，所以计入了上表。语句执行期间的同步计数见 `--json` 输出的 `rpcs` 字段：例如"只提交 primary"的三种读法，同步部分都只有 CTS 1。
- 活锁情况下，tidb-rs 第二次读又发了一次 CheckTxnStatus。这是新语句、新快照，hint 集合不跨快照，Go 也是这样（Go 每种只读一次，所以表中没有它的第二次读）。
- "只提交 primary"的点查（TSO）：锁的事务提交时间戳早于读的 start_ts，R1 把它归为 `canAccess`，经 `committed_locks` 读穿，结果为 NEW。这一项实际验证了 `committed_locks`。

## 对照：R1 之前（client-rust `2fc142f`）

结果同样 18/18 一致，只有活锁一种情况在时间和 RPC 上差得多：

| 锁 | 读法 | tidb-rs `2fc142f` | tidb-rs `ee7f1e2` | Go TiDB |
|---|---|---|---|---|
| 2PC 只 prewrite，TTL 6 s 未过期 | 点查（TSO） | OLD / **8383 ms** / CTS 12、RL 1，等到 TTL 过期才回滚，锁 2→0 | OLD / 4 ms / CTS 1，锁保留 | OLD / 4 ms / CTS 1 |
| | 范围（coprocessor） | OLD / **6880 ms** / CTS 11、RL 1，同上 | OLD / 4 ms / CTS 1，锁保留 | OLD / 4 ms / CTS 1 |

点查（max-ts）在 R1 之前就和 Go 一致：那条路径（`get_at_max_ts`）原来就处理了 `MinCommitTsPushed`。

## 复现

```sh
scripts/playground.sh start && scripts/sysbench-prepare.sh -t 1 -s 100000 -r 8
cargo run --release -p poc-server -- -P 27001 --pd 127.0.0.1:25379 --auto-tls=false &
(cd tools/ab/locks && go build -o /tmp/lockgen ./lockgen)
tools/ab/locks/mixed_locks.py --lockgen /tmp/lockgen --json /tmp/locks.json
```
