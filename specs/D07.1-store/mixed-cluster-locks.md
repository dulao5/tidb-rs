# D07.1：真实锁下的读对比（#101 第 3 步）

2026-11-13。验证 client-rust fork 的 R1–R4（[lock-resolver.md](lock-resolver.md) §5）在真实 TiKV 集群上的行为与 Go TiDB 一致。工具与用法见 [`tools/ab/locks/`](../../tools/ab/locks/README.md)。

## 结论

1. **结果一致**：5 种锁加 FK shared lock，每种都用点查和范围扫描（coprocessor）测，共 12 组对比。Go TiDB 与 tidb-rs 的读结果和错误全部相同，也都符合预期（下表"预期"列）。
2. **§3.6 在真实集群上复核**：`tidb_foreign_key_check_in_shared_lock = ON` 的外键检查确实在父表行上留下 shared lock wrapper（ScanLock 返回 `SharedLock[<ts>:PessimisticLock]`），两边读父表都不受影响，立即返回原值。
3. **R2（后台清锁）**：tidb-rs 第一次读之后，已判定事务（已过期、已提交 primary、async commit）的锁在 300 ms 内被清掉（scanlocks 2→0 或 1→0），第二次读不再发 CheckTxnStatus / ResolveLock。活锁和悲观锁按预期保留。
4. **R1 的效果**：R1 之前（client-rust `2fc142f`），活锁加范围扫描要 **6438 ms、11 次 CheckTxnStatus**，一直等到 TTL 过期才回滚。R1–R4 之后（`ee7f1e2`）是 **4 ms、1 次**，和 Go 一样：CheckTxnStatus 推高 min_commit_ts 后直接忽略这个锁。
5. **一处时间上的差别（不影响结果）**：async commit 已过期的点查，Go 用 54–107 ms、4–5 次 CheckTxnStatus，tidb-rs 用 4 ms、1 次。推测原因（未验证）：Go 用本地低精度时钟判断锁是否过期（`oracle.IsExpired`），这个时钟落后，于是先把锁当活锁退避了几轮；tidb-rs 每次取新的 TSO，立刻判定过期。R7 改用低精度时钟后，tidb-rs 应当出现同样的退避，R7 的 PR 要专门复测这一项。
6. **没测到的**：批量点查。tidb-rs 的 SQL 层还不支持对主键用 `IN` / `OR`（返回 1235），BatchGet 的锁处理目前只由 client-rust 的单测覆盖（`snapshot_reads_share_lock_hints`）。SQL 层支持后，把 `batch` 读法加回 `mixed_locks.py`。

## 环境

- tiup playground v8.5.8（1 PD、1 TiKV、Go TiDB），与 tidb-rs 同在一台 Linux arm64 虚拟机。sbtest1 共 10 万行，分 8 个 region。
- tidb-rs：A 组用 #124 分支（client-rust `ee7f1e2`，含 R1–R4）；对照组用 client-rust `2fc142f`（R1 之前）。
- 造锁：`lockgen`，client-go `c3b5079`（与 v8.5.8 相同），通过 `CommitterProbe` 逐步执行两阶段提交。

## 结果（client-rust `ee7f1e2`）

"RPC"一列是语句执行期间加上之后 300 ms 内的计数（CTS = CheckTxnStatus，RL = ResolveLock，CSL = CheckSecondaryLocks）。"锁数"是 tidb-rs 读之前 → 第一次读后 → 第二次读后。

| 锁 | 预期 | 读法 | Go TiDB 结果 / 耗时 / RPC | tidb-rs 结果 / 耗时 / RPC | 锁数 | tidb-rs 第二次读 RPC |
|---|---|---|---|---|---|---|
| 2PC 只 prewrite，TTL 已过期 | OLD（回滚） | 点查 | OLD / 5 ms / CTS 1、RL 1 | OLD / 5 ms / CTS 1、RL 1 | 2→0→0 | 无 |
| | | 范围 | OLD / 5 ms / CTS 1 | OLD / 5 ms / CTS 1、RL 1 | 2→0→0 | 无 |
| 只提交 primary，secondary 留锁 | NEW（提交） | 点查 | NEW / 4 ms / CTS 1、RL 1 | NEW / 4 ms / CTS 1、RL 1（RL 在后台） | 1→0→0 | 无 |
| | | 范围 | NEW / 4 ms / CTS 1、RL 1 | NEW / 4 ms / CTS 1、RL 1 | 1→0→0 | 无 |
| 2PC 只 prewrite，TTL 6 s 未过期 | OLD（忽略） | 点查 | OLD / 4 ms / CTS 1 | OLD / 3 ms / CTS 1 | 2→2→2 | CTS 1 |
| | | 范围 | OLD / 5 ms / CTS 1 | OLD / 4 ms / CTS 1 | 2→2→2 | CTS 1 |
| 悲观事务持锁 | OLD（不阻塞） | 点查 | OLD / 3 ms / 无 | OLD / 3 ms / 无 | 2→2→2 | 无 |
| | | 范围 | OLD / 4 ms / 无 | OLD / 3 ms / 无 | 2→2→2 | 无 |
| async commit 全部 prewrite，TTL 已过期 | NEW（从 secondaries 恢复为提交） | 点查 | NEW / 107 ms / CTS 5、RL 1、CSL 1 | NEW / 4 ms / CTS 1、RL 1、CSL 1（RL 在后台） | 2→0→0 | 无 |
| | | 范围 | NEW / 4 ms / CTS 1、RL 1、CSL 1 | NEW / 4 ms / CTS 1、RL 1、CSL 1（RL 在后台） | 2→0→0 | 无 |
| FK shared lock（`SharedLock[…PessimisticLock]`） | 原值 | 点查 | 原值 / 5 ms | 原值 / 4 ms | — | — |
| | | 范围 | 原值 / 4 ms | 原值 / 2 ms | — | — |

说明：

- 活锁情况下，tidb-rs 第二次读又发了一次 CheckTxnStatus。这是新语句、新快照，hint 集合不跨快照，Go 也是这样（Go 的 A 组每种只读一次，所以表中没有它的第二次读）。
- "RL 在后台"：语句返回时还没有发出 ResolveLock，在之后 300 ms 内完成。这就是 R2 的行为。

## 对照：R1 之前（client-rust `2fc142f`）

结果同样全部一致，只有一处时间和 RPC 差得多：

| 锁 | 读法 | tidb-rs `2fc142f` | tidb-rs `ee7f1e2` | Go TiDB |
|---|---|---|---|---|
| 2PC 只 prewrite，TTL 6 s 未过期 | 范围（coprocessor） | OLD / **6438 ms** / CTS 11、RL 1，等到 TTL 过期才回滚，锁 2→0 | OLD / 4 ms / CTS 1，锁保留 | OLD / 5 ms / CTS 1 |

点查在 R1 之前就和 Go 一致：tidb-rs 的自动提交点查走 max-ts 路径（`crates/poc/session/src/lib.rs` 的 `max_ts_point_get`，读 `latest_snapshot()`），那条路径原来就处理了 `MinCommitTsPushed`。

## 复现

```sh
scripts/playground.sh start && scripts/sysbench-prepare.sh -t 1 -s 100000 -r 8
cargo run --release -p poc-server -- -P 27001 --pd 127.0.0.1:25379 --auto-tls=false &
(cd tools/ab/locks && go build -o /tmp/lockgen ./lockgen)
tools/ab/locks/mixed_locks.py --lockgen /tmp/lockgen --json /tmp/locks.json
```
