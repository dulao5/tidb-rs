# D07.1：TiKV 故障场景下的读对照（#128 第 2 项）

2026-11-13。在 3 TiKV 集群上，一边持续读，一边逐个注入故障，比较 Go TiDB 与 tidb-rs。目的是在真实集群上验证 [client-go 对等对照表](client-go-parity.md) 的结论，尤其是 3 项 P0。工具与用法见 [`tools/ab/faults/`](../../tools/ab/faults/README.md)；入口 `tools/ab/faults/run.sh` 可以放进每周构建。

## 结论

1. **结果正确性**：
   - **静态数据**：所有场景、所有读法的每个结果都与故障前的快照一致，两边都是。这一项只说明故障切换时没有返回损坏或不完整的结果。
   - **有写入时（第二轮，见"写入方：过期读与漏读"）**：一个 Go TiDB 连接在整个运行期间持续提交（1PC 和带 secondary 的 async commit），两边读这些行。所有场景下，没有一次读看到比"读开始前已返回的提交"更旧的值，两边都是。所以本次的故障下**没有发现过期读或漏读**。
2. **P0-1（退避预算）在真实集群上复现，两次都复现**：
   - 故障：`kill -9` 一个 TiKV，约 31 s 后重启。
   - tidb-rs 的两种点查各报错 27–30 次（`1105: region unavailable: Leader of region N is not found` / `peer is not leader`）。
   - Go 一次都没报错，只是最长挂起 14–18 s 后成功。
   - tidb-rs 的范围扫描和聚合也没有报错：它们用调用方的 48 次退避。点查用的是 fork 写死的 10 次（约 1.5 s）。这正是对照表 P0-1 的判断。
3. **P0-3（4 MiB 接收上限）复现，已修复**（client-rust#10，tidb-rs 升到 `efe655f` 后复跑 5 / 5 成功，`expected.json` 的条目已删）：
   - 一个 cop 响应约 6 MiB（100 行 × 64 KiB）。tidb-rs 5 次全部报 `gRPC api error: status: OutOfRange, message: "Error, decoded message length too large"`，Go 全部成功。
   - 同一张表的小响应两边都正常。
4. **P0-2（2 s RPC 超时）没有复现**：
   - SIGSTOP 一个 TiKV 5 s：两边都在约 5 s 挂起后成功。tidb-rs 的超时请求换 peer 重试（`DeadlineExceeded` 不失效缓存），所以撑过了暂停。
   - 把 TiKV 的 CPU 限到 3% / 0.5%：两边都没有超时，p99 最多约 700 ms。
   - 结论：P0-2 只在**单个请求本身在 TiKV 上要跑超过 2 s** 时才会报错，例如大 BatchGet、负载很高时跨大 region 的 Scan。本次的负载造不出这种请求。对照表中 P0-2 的判断不变，但真实触发条件更窄，见下文"对对照表的更新"。
5. **ServerIsBusy 没有触发**：限 CPU 时 TiKV 没有返回 ServerIsBusy。本次的负载（每边 8 个线程）排不满 TiKV 的读池。对照表中 ServerIsBusy 2 ms 重试的问题，在这里**既未证实也未否定**。
6. **断网 10 s 后，tidb-rs 的 coprocessor 读多挂起约 3 s**：
   - 范围扫描和聚合的最长挂起是 13.3 s，Go 是 10.4 s。点查两边相同（10.4 s）。
   - **原因待查**，可能的解释有两个：
     - (a) 对照表 §5 的"缺 HTTP/2 keepalive"：死连接要靠请求超时发现，cop 的超时是 60 s；
     - (b) 断网期间 TCP 重传退避被拉长，恢复后连接要等下一次重传。
   - 验证方法：断网期间用 `ss -ti` 看 tidb-rs 到该 TiKV 的连接的重传计时器；在 fork 中加上 `http2_keep_alive_interval(10s)` 后重跑 `net_cut` 对比。确认之前，不作为"缺 keepalive"的证据写进对照表。
7. **两边都出现的错误**：本次三轮运行中 Go 一次错误都没有，所以没有两边共有的错误种类。工具会把这类错误单独列出（`BOTH:`，JSON 中的 `errors_on_both`），但不计入退出码，因为它们不是对等差异。
8. **其余场景两边相当**：region 分裂（48 个）与合并、leader 迁移（evict-leader 加 40 个 transfer-leader）、TiKV 限 CPU，两边都没有报错，p99 和最长挂起相近。

## 环境

- tiup playground v8.5.8：1 PD、3 TiKV、1 Go TiDB，tag `maya-faults`。与 tidb-rs、负载程序同在一台 10 核 Linux arm64 虚拟机上。
- 数据：sbtest1 共 10 万行，预先分成 8 个 region。
- tidb-rs：#130 分支，client-rust `42d2962`（含 #101 R1–R7、R12 及 #9）。PoC 的重试配置：region 退避 48 次（约 20.5 s），lock 退避 16 次。
- 读负载：每边每种读法 2 个线程（每边 8 个），随机 key，见 README：
  - 点查（max-ts）：自动提交点查，MaxTS，不取 TSO；
  - 点查（TSO）：`max(c)` 套点查，按语句的 start_ts；
  - 范围扫描：10 行；
  - 聚合：`count(*)`、`sum(k)`，约 2000 行。
- 每阶段 10 s。场景之间留半个阶段让集群恢复。

## 结果

"挂起"是同一个线程两次成功读之间的最长间隔。p99 是该场景全部读的 p99（包括故障前后正常的部分）。错误次数为 0 时不列出。

| 场景 | 读法 | Go TiDB：成功 / p99 / 最长挂起 / 错误 | tidb-rs：成功 / p99 / 最长挂起 / 错误 |
|---|---|---|---|
| 稳态（无故障，10 s） | 点查 max-ts | 4521 / 12.3 ms / 23 ms | 4592 / 12.3 ms / 20 ms |
| | 点查 TSO | 4379 / 12.6 ms / 20 ms | 4515 / 12.2 ms / 21 ms |
| | 范围 | 2270 / 20.0 ms / 29 ms | 2335 / 20.0 ms / 29 ms |
| | 聚合 | 3585 / 13.2 ms / 21 ms | 3738 / 13.0 ms / 18 ms |
| 分裂 48 个 region + 激进合并（30 s） | 点查 max-ts | 13426 / 13.0 ms / 46 ms | 13582 / 12.9 ms / 52 ms |
| | 点查 TSO | 12875 / 13.0 ms / 34 ms | 13281 / 13.0 ms / 51 ms |
| | 范围 | 6809 / 20.5 ms / 44 ms | 6898 / 21.0 ms / 61 ms |
| | 聚合 | 10686 / 13.8 ms / 49 ms | 10898 / 14.2 ms / 37 ms |
| leader 迁移（evict-leader 10 s + 40 个 transfer-leader） | 点查 max-ts | 9892 / 12.1 ms / 20 ms | 9868 / 12.1 ms / 24 ms |
| | 点查 TSO | 9548 / 11.7 ms / 19 ms | 9685 / 12.0 ms / 19 ms |
| | 范围 | 4898 / 20.2 ms / 26 ms | 5007 / 19.2 ms / 30 ms |
| | 聚合 | 7682 / 13.6 ms / 20 ms | 7954 / 12.9 ms / 21 ms |
| **kill -9 一个 TiKV，约 31 s 后重启**（第 1 次） | 点查 max-ts | 6362 / 12.4 ms / 18.1 s | 8288 / 12.4 ms / 7.5 s / **30 个错误**（22 × Leader not found，8 × not leader） |
| | 点查 TSO | 6106 / 12.1 ms / 14.2 s | 7093 / 13.0 ms / 9.6 s / **27 个错误**（19 + 8） |
| | 范围 | 3130 / 19.7 ms / 16.1 s | 3661 / 19.6 ms / 16.1 s |
| | 聚合 | 4926 / 13.4 ms / 14.2 s | 5352 / 13.4 ms / 14.2 s |
| kill -9（第 2 次，另一个 TiKV） | 点查 max-ts / TSO | 0 个错误 | **各 30 个错误**（22 + 8） |
| TiKV 限 CPU 到 3%（20 s） | 点查 max-ts | 4928 / 296.8 ms / 1.8 s | 4869 / 197.8 ms / 2.0 s |
| | 点查 TSO | 4671 / 395.4 ms / 1.6 s | 4725 / 17.8 ms / 3.7 s |
| | 范围 | 2457 / 699.2 ms / 1.8 s | 2492 / 698.7 ms / 1.9 s |
| | 聚合 | 3736 / 14.9 ms / 3.9 s | 3822 / 14.8 ms / 3.6 s |
| TiKV 限 CPU 到 0.5%（20 s，另一次运行） | 全部 | 0 个错误，p99 ≤ 20 ms | 0 个错误，p99 ≤ 20 ms |
| 断网 10 s（iptables 丢弃该 TiKV 端口的全部 TCP） | 点查 max-ts / TSO | 4948 / 4754，最长挂起 10.4 s | 5087 / 4884，最长挂起 10.4 s |
| | 范围 | 2510 / 19.1 ms / 10.4 s | **1576** / 19.8 ms / **13.3 s** |
| | 聚合 | 3798 / 12.4 ms / 10.4 s | **2493** / 13.8 ms / **13.3 s** |
| SIGSTOP 一个 TiKV 5 s | 全部 | 0 个错误，最长挂起约 5.0 s | 0 个错误，最长挂起约 5.0 s |
| 宽行表（100 行 × 64 KiB，一次约 6 MiB） | 大响应 | 5 / 5 成功，p99 265 ms | **0 / 5**，5 × `gRPC OutOfRange: decoded message length too large` |
| | 小响应 | 5 / 5 | 5 / 5 |

说明：

- kill -9 时 tidb-rs 点查的最长挂起比 Go 短（7.5–9.6 s 对 14–18 s），是因为它更早放弃并报错，不是恢复更快。
- 限 CPU 那一次的影响取决于被限的 store 上有多少 leader：每次都选当时 leader 最多的 store，但前面的 kill 场景之后 leader 分布会变。0.5% 那次几乎没有影响，说明那个 store 上的 leader 已经很少。
- 宽行场景不是故障，是对照表 P0-3 的复现用例，放在同一套工具里，便于修复后回归。

## 写入方：过期读与漏读（第二轮）

第二轮运行在 tidb-rs `efe655f`（含 P0-3 修复）上，加入写入方后跑全部场景，做法见 [README 的 Writes 一节](../../tools/ab/faults/README.md#writes)：

- `sbtest.counters` 8 行，每行一个 region。
- 一个 Go TiDB 连接循环自动提交，值取自一个递增序列。奇数步只改一行（1PC）；偶数步一条语句改不同 region 的两行（async commit，带 secondary 锁）。
- 两边的 max-ts 点查、TSO 点查和全表扫描读这些行。在某次提交返回之后才开始的读，看到的值必须不小于那次提交的值，也不能大于已经尝试写过的最大值。
- 扫描还必须完整地看到每个两行提交：某行的值来自两行的那一步时，它的伙伴行必须不小于这个值。这一项查的是快照看到了已提交的 primary，却漏掉还在清理的 secondary。

| 场景 | 写入方：成功提交 / 错误 | 读这些行：错误结果（两边） | 读这些行的错误（Go / tidb-rs） |
|---|---|---|---|
| 稳态 | 2091 / 0 | 0 | 0 / 0 |
| 分裂与合并 | 6224 / 0 | 0 | 0 / 0 |
| leader 迁移（含这 8 个 region） | 4100 / 0 | 0 | 0 / 0 |
| kill -9 | 2818 / 0（最长挂起 12.5 s） | 0 | 0 / 13（点查各 6 次 `Leader of region N is not found`，扫描 1 次） |
| 限 CPU、断网、SIGSTOP、宽行 | 无错误 | 0 | 0 / 0 |

- 所有场景两边都**没有错误结果**，包括 kill -9 和 leader 迁移：两种场景下提交路径上都有锁。
- kill -9 时 tidb-rs 对这些行的报错是 P0-1：
  - 点查同上，是写死的 10 次退避；
  - 扫描那 1 次是推断，日志里没有直接证据：coprocessor 读本身用 48 次退避，最长挂起只有 13.6 s，所以错误应该来自清锁。coprocessor 读遇到写入方的 async commit 锁后，清锁的 CheckTxnStatus 发往被杀 TiKV 上的 primary，而清锁用的是写死的 `DEFAULT_REGION_BACKOFF`。P0-1a 修复后复跑可以确认。
  - 这些已经列进 `expected.json`，由 P0-1a 修复。
- 同一轮里旧表的结果与第一轮一致：kill -9 时 tidb-rs 点查报错 9–10 次（P0-1），宽行大响应 5 / 5 成功（P0-3 已修复）。

加上两行配对检查后（#133 评审），在同一版本上复跑了 kill -9 和 leader 迁移：

- 写入方分别成功提交 4615 次和 4027 次，没有错误。
- 扫描分别读了 Go 1717 / tidb-rs 1780 次、Go 1455 / tidb-rs 1535 次。
- 两边都**没有错误结果**，配对检查也没有违反。
- kill -9 下 tidb-rs 的报错仍然只有 P0-1 的点查。这一次扫描没有报错。

这套检查是 P0 修复的验收测试：每个 P0 修完后复跑，错误结果必须仍为 0。

## P0-1a 修复后的复跑（tidb-rs `2ca139e`）

client-rust#11 让点查（TSO、max-ts）和清锁改用调用方的预算。复跑 kill -9，tidb-rs 开 debug 日志，结果如下：

| 读法 | Go TiDB：成功 / 错误 / 最长挂起 | tidb-rs：成功 / 错误 / 最长挂起 |
|---|---|---|
| 点查 max-ts | 9808 / 0 / 11.7 s | 9593 / **0** / 11.7 s |
| 点查 TSO | 9150 / 0 / 11.8 s | 9104 / **0** / 10.7 s |
| 范围 | 4926 / 0 / 12.3 s | 5040 / 0 / 11.7 s |
| 聚合 | 7673 / 0 / 12.3 s | 7729 / 0 / 11.7 s |
| 写入行：点查 max-ts | 3159 / 0 / 13.7 s | 3117 / **0** / 13.8 s |
| 写入行：点查 TSO | 3012 / 0 / 18.8 s | 2978 / **0** / 16.3 s |
| 写入行：扫描 | 1565 / 0 / 16.2 s | 1566 / **0** / 18.8 s |

写入方成功提交 3782 次，没有错误，最长挂起 16.3 s。两边都没有错误结果，配对检查也没有违反。

- **P0-1 的报错消失了**：修复前 tidb-rs 每种点查报错 9–30 次，现在是 0 次。最长挂起与 Go 相当，最多相差约 2.5 s，两边方向都有。
- **扫描那 1 个错误的调用路径**（Lucky 的问题）：这一轮没有再出现，所以没能直接抓到。debug 日志中可以看到的是：
  - 断开期间，coprocessor 读遇到写入方的锁 455 次，全部清锁成功；
  - 计数行所在的 region（2057、2065 等）的 `LeaderNotFound` 在 plan 层被重试，并最终成功。
  - 但日志里的请求没有标出是 Get 还是 CheckTxnStatus，所以"清锁的 CheckTxnStatus"仍然是推断。要直接确认，需要在 fork 的重试日志里加上请求名。这个改动很小，P0-1b 会顺带加上。
- **预算仍然是层层相乘的**（Lucky 在 #11 上的意见）：一次读的 region 重试、它的清锁和锁等待各自有一份预算，store 宕机时总时长可能远超 Go 的 20 s。P0-1b 改为每次读一份共享的时间预算。

## P0-1b 之后：共享的 20 s 时间预算（tidb-rs `de641f8`）

client-rust#12 之后，tidb-rs 的 region 重试和锁等待都用 `Backoff::time_budget(20_000)`：
- 每次读一份预算，它的 region 重试、清锁和锁等待共用；
- 每种错误一条曲线；
- ServerIsBusy 不计入预算。

复跑了 kill -9、限 CPU、SIGSTOP，tidb-rs 开 debug 日志，重试日志带请求名：

| 场景 | 读法 | Go TiDB：错误 / 最长挂起 | tidb-rs：错误 / 最长挂起 |
|---|---|---|---|
| kill -9 | 点查 max-ts / TSO | 0 / 13.2 s、12.2 s | 0 / 13.3 s、13.3 s |
| | 范围 / 聚合 | 0 / 13.2 s、13.2 s | 0 / 13.7 s、14.0 s |
| | 写入行：点查 max-ts / TSO / 扫描 | 0 / 0.06 s | 0 / **7.1 s** |
| 限 CPU 3% | 全部 | 0 / ≤ 6.4 s | 0 / ≤ 6.4 s |
| SIGSTOP 5 s | 全部 | 0 / 5.0 s | 0 / 5.0 s |

写入方分别成功提交 9905、2107、1889 次，没有错误。所有场景两边都没有错误结果，配对检查也没有违反。

- **最长挂起都在预算之内**：最长 14.0 s，低于"20 s 加越过预算的那一次睡眠"。与 Go 相差不超过约 1 s。
- **写入行多挂起 7 s，原因在 tidb-rs 的 domain，不在 client-rust**（#136 用 debug 日志查过）：
  - 写入行的 8 个 region，这一轮没有一个请求发往被杀的 store（store 1）：store 7 上 7307 次，store 2 上 889 次。
  - 07:28:39.03–46.11 这 7.08 s 里，这些 region **一个请求都没有**，而且整个 tidb-rs 几乎停住：没有新的快照、TSO 或 coprocessor 请求。所以读并不是卡在重试上，而是在发出请求之前就被挡住了。
  - 原因：
    - 存 schema 版本的 meta region（132）的 leader 在被杀的 store 上，schema 同步循环读不到它，不再推进；
    - 5 s 后（`max_staleness`，poc/domain/src/registry.rs:33）watchdog 注销了这个节点（日志 `deregistered from etcd why="not keeping up with the cluster"`，07:28:39.136，poc/domain/src/sync.rs:615）；
    - 注销后，语句不再走 synced 模式，每条语句都要从 TiKV 读 schema 版本键来校验（`check_at`，meta/src/infoschema.rs:249），而这个键也在 region 132 上；
    - 于是**所有语句**都要等 region 132 选出新 leader（07:28:46.109，kill 后约 11.7 s），46.21 重新注册。
  - client-rust 在 region 132 上的行为与 Go 相当：
    - 断开期间共 92 次尝试，每次都是连接被拒（立即失败），然后失效缓存、从 PD 重新加载；
    - PD 在选举结束前一直返回旧 leader（store 1），所以重新加载拿回的仍是旧 leader，这一点 Go 也一样；
    - 新 leader 在 PD 上出现后立即切过去。数据 region 上两边的最长挂起相同（tidb-rs 13.3 s，Go 13.2 s）。
  - **结论**：这不是 client-rust 失效路径的缺陷。它是 tidb-rs domain 的可用性问题：meta region 的一次故障切换，被放大成全节点所有语句停顿。Go 在 schema lease（45 s）内照常用缓存的 schema 执行语句，不会因为一次 reload 慢了 5 s 就去逐条读 meta。建议在 domain（D06.2）里按 Go 的 SchemaValidator 处理：lease 内继续 admit；`max_staleness` 不应短于 lease。
- **扫描那个错误的来源已确认**：带请求名的重试日志里，断开期间有 8 次 `retry_after_routing_error kv_check_txn_status: GrpcAPI(...)`，即清锁的 CheckTxnStatus 发往了被杀的 store 并重试。P0-1a 之前，这条路径用的是写死的 `DEFAULT_REGION_BACKOFF`，10 次后报 `Leader of region N is not found`。这与之前的推断一致。
- **ServerIsBusy**：限 CPU 时 TiKV 仍然没有返回 ServerIsBusy。所以"排除在预算外"这一点只有 client-rust 的单测覆盖（60 次 ServerIsBusy 后读成功）。

## 对对照表的更新（client-go-parity.md §9）

| P0 | 本次结果 | 写回对照表的内容 |
|---|---|---|
| P0-1 | **复现**（kill -9，两次）：只有点查报错，cop 读没有，与"点查写死 10 次退避"的判断一致 | 触发条件写实为"3 TiKV 集群 kill -9 一个 TiKV：tidb-rs 点查约 30 次 `Leader of region N is not found` / `not leader`，Go 0 次（最长挂起 14–18 s）"；复现命令 `tools/ab/faults/run.sh -s kill_tikv` |
| P0-2 | **未复现**：超时后换 peer 重试，5 s 的暂停能撑过去 | 触发条件收窄为"单个请求在 TiKV 上处理超过 2 s"（大 BatchGet、负载高时跨大 region 的 Scan）；SIGSTOP 和限 CPU 都不会触发。复现方法改为构造大 BatchGet（几千个宽行 key）或在 TiKV 侧注入处理延迟 |
| P0-3 | **复现**：约 6 MiB 的 cop 响应 5 / 5 失败 | 复现命令 `tools/ab/faults/run.sh -s wide_rows` |

## 已知不一致与周跑

`tools/ab/faults/expected.json` 列出已知的不一致，即 tidb-rs 报错而 Go 不报错的情况。这样修复之前周跑保持绿色，出现**新的**错误种类或任何错误结果时退出码为 1。修好一项后删掉对应的条目，回归就会被这套工具守住。

目前**已经清空**：
- P0-3（宽行 OutOfRange）的条目在 client-rust#10 后删掉；
- P0-1（kill_tikv 下点查和清锁的 region 错误）的条目在 client-rust#11 后删掉。

场景没跑成时退出码为 2，不会报绿：没有免密 sudo 时跳过 `slow_tikv`（cgroup v2）和 `net_cut`（iptables），或者某个场景注入故障失败。每一项都打印为 `INCOMPLETE:`，并记在 JSON 的 `incomplete` 中。

## 没做到的

- **写入方的覆盖范围**：只有一个写入方，而且是 Go TiDB。
  - 只验证"读不回退"，不验证事务之间的可串行化。
  - tidb-rs 写入时的故障行为（提交路径）不在本次范围内。
  - 读开始的时间取在客户端，提交返回的时间也取在客户端，两者之间的网络时间会让检查略偏宽松：只会漏报，不会误报。
- **ServerIsBusy**：限 CPU 没有触发。需要更重的负载（排满读池），或 TiKV 的 failpoint 构建。
- **网络中断的变体**：只做了"一个 TiKV 的全部 TCP 被丢弃"；没有做单向丢包、延迟抖动（`tc netem`）。
- **PD 故障**：不在本次要求内。
