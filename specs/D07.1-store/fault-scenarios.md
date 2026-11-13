# D07.1：TiKV 故障场景下的读对照（#128 第 2 项）

2026-11-13。在 3 TiKV 集群上，一边持续读，一边逐个注入故障，比较 Go TiDB 与 tidb-rs。目的是在真实集群上验证 [client-go 对等对照表](client-go-parity.md) 的结论，尤其是 3 项 P0。工具与用法见 [`tools/ab/faults/`](../../tools/ab/faults/README.md)；入口 `tools/ab/faults/run.sh` 可以放进每周构建。

## 结论

1. **结果正确性**：所有场景、所有读法，两边都**没有错误结果**。每个结果都与故障前的快照对比过。
2. **P0-1（退避预算）在真实集群上复现，两次都复现**：
   - 故障：`kill -9` 一个 TiKV，约 31 s 后重启。
   - tidb-rs 的两种点查各报错 27–30 次（`1105: region unavailable: Leader of region N is not found` / `peer is not leader`）。
   - Go 一次都没报错，只是最长挂起 14–18 s 后成功。
   - tidb-rs 的范围扫描和聚合也没有报错：它们用调用方的 48 次退避。点查用的是 fork 写死的 10 次（约 1.5 s）。这正是对照表 P0-1 的判断。
3. **P0-3（4 MiB 接收上限）复现**：
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
7. **其余场景两边相当**：region 分裂（48 个）与合并、leader 迁移（evict-leader 加 40 个 transfer-leader）、TiKV 限 CPU，两边都没有报错，p99 和最长挂起相近。

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

## 对对照表的更新（client-go-parity.md §9）

| P0 | 本次结果 | 写回对照表的内容 |
|---|---|---|
| P0-1 | **复现**（kill -9，两次）：只有点查报错，cop 读没有，与"点查写死 10 次退避"的判断一致 | 触发条件写实为"3 TiKV 集群 kill -9 一个 TiKV：tidb-rs 点查约 30 次 `Leader of region N is not found` / `not leader`，Go 0 次（最长挂起 14–18 s）"；复现命令 `tools/ab/faults/run.sh -s kill_tikv` |
| P0-2 | **未复现**：超时后换 peer 重试，5 s 的暂停能撑过去 | 触发条件收窄为"单个请求在 TiKV 上处理超过 2 s"（大 BatchGet、负载高时跨大 region 的 Scan）；SIGSTOP 和限 CPU 都不会触发。复现方法改为构造大 BatchGet（几千个宽行 key）或在 TiKV 侧注入处理延迟 |
| P0-3 | **复现**：约 6 MiB 的 cop 响应 5 / 5 失败 | 复现命令 `tools/ab/faults/run.sh -s wide_rows` |

## 已知不一致与周跑

`tools/ab/faults/expected.json` 列出了已知的不一致，即 tidb-rs 报错而 Go 不报错的情况：kill_tikv 下点查的 region 错误（P0-1）和宽行的 OutOfRange（P0-3）。所以修复之前周跑保持绿色，出现**新的**错误种类或任何错误结果时退出码为 1。修好一项后删掉对应的条目，回归就会被这套工具守住。

## 没做到的

- **ServerIsBusy**：限 CPU 没有触发。需要更重的负载（排满读池），或 TiKV 的 failpoint 构建。
- **网络中断的变体**：只做了"一个 TiKV 的全部 TCP 被丢弃"；没有做单向丢包、延迟抖动（`tc netem`）。
- **PD 故障**：不在本次要求内。
