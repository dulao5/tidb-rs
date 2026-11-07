# N3 运行时接入 PoC 的基准（PR B）

日期：2026-11-07。对象：PoC（main `de1fa1c`）接入 `tidb-runtime` 前后的对比，以 Go TiDB v8.5.8 作参考。对应 [N3 spec](../../specs/exceptions/N3-A1-concurrency-memory/spec.md) §5 和[设计说明](../../specs/exceptions/N3-A1-concurrency-memory/runtime-skeleton.md) §4。

**结论先行**

| 问题 | 结论 |
|---|---|
| 接入运行时是否让 PoC 变慢 | **没有**。点查、read_only、扫描并发下的点查、过载各项，接入后与接入前都在噪声范围内（中位数相差 −1.7%～+1.5%，3 次运行的波动区间互相重叠） |
| 停顿检测 | 点查、read_only、扫描并发下的点查：**0**。过载到 256、320 个并发语句时出现停顿（每次 15 秒 17～44 次，多为 10～16ms）。分类后本机全部是 off_cpu，`on_cpu` 为 0，分散在整个运行中；Linux 上集中在所有客户端同时断开的收尾时刻（§4.2）。N3 §5 改为只在稳态窗口内判定 `on_cpu`、`blocked` 为 0：本机 `on_cpu` 达标，`blocked` 只能在 Linux 上判定 |
| Q1 | 默认 1000 不会触发排队，平均等待约 144ns（`queue_wait_seconds` 的 sum / count）。`--token-limit 64` 时 320 个线程下 Q1 排队，p99 从 16.3ms 降到 9.5ms，吞吐降到 45k（峰值的 84%） |
| Q2、Q3 | PoC 中 **没有读数**：Q2 未接入（见 §5），PoC 没有阻塞型工作，Q3 没有使用 |
| 换 yatp 多级队列 | **现在不换**。一个全表扫描会话并发时，点查 p99 接入前后相同（1.31ms），且与 Go 的差距来自 PoC 的访问路径（见 §3），不是调度。等真实执行器移植后，用 N3 §5 的"64 个 TPC-H 会话 + 点查"场景再决定。那时的基准**必须包含没有下推的大排序和 hash join**（Lucky）：根节点上长时间运行的算子才是多级队列真正起作用的地方，本次的扫描测试把部分聚合下推了，测不到 |
| 时间片 | **保持 1ms**。没有证据要求调整：PoC 的检查点只在 chunk 级，各项测试中时间片没有引起可测的开销；停顿只出现在 CPU 超额订阅时 |

## 1. 环境

| 项 | 值 |
|---|---|
| 机器 | Apple M4 Pro，12 核，48 GB 内存，macOS 27.0.1 |
| 集群 | `tiup playground v8.5.8`，1 PD、1 TiKV、1 Go TiDB，tag `maya-n3`，端口偏移 24000（`scripts/playground.sh start -t maya-n3 -o 24000`）。同一台机器上另有一个 `tiup playground nightly`（其他 agent 的），基准期间它基本空闲（CPU < 3%），没有使用、也没有停止它 |
| 数据 | `scripts/sysbench-prepare.sh -P 28000 -t 1 -s 1000000 -r 4`：1 张表 100 万行，4 个 region |
| 客户端 | sysbench 1.0.20（`scripts/oltp_read_only_lat.lua`，自带直方图取 p95、p99）、mysql 客户端，与服务端在同一台机器 |
| 构建 | `cargo build --release -p tidb-server`，rustc 1.99.1，jemalloc。base = main `de1fa1c`；wired = 本 PR |
| 拓扑 | wired 默认 Steal（多线程工作窃取），12 个 worker |

**噪声说明**：sysbench、TiKV、tidb-rs 都在同一台 12 核机器上，高并发时负载约 15～18，机器处于超额订阅状态。所以绝对数值只在本机有意义，比较以同一时段内交替运行的 base / wired / Go 为准：每轮依次跑 base、wired、Go，共 3 轮，报中位数和最小～最大值。

## 2. 命令

```
scripts/n3-bench.sh base-r$rep  <base 的 tidb-server>       # 每轮
scripts/n3-bench.sh wired-r$rep <wired 的 tidb-server>
scripts/n3-bench.sh go-r$rep go                            # 同一 playground 的 Go TiDB
TESTS=overload OVERLOAD="64 128 320" scripts/n3-bench.sh wired_tl64-r$rep <wired> --token-limit 64
```

`scripts/n3-bench.sh`（本 PR 新增）的测试项：
- `ps`：oltp_point_select，一个事件就是一次点查；
- `ro`：oltp_read_only，一个事件是 10 次点查加 4 个范围查询；
- `scan`：一个会话循环执行 `SELECT COUNT(*), SUM(k) FROM sbtest1`（整表），同时 16 个线程跑点查；
- `overload`：点查，线程数 16→320 逐级增加。

每次 15 秒，`--db-ps-mode=auto`。原始数据见 [n3-runtime-data/](n3-runtime-data/)。

## 3. 结果

QPS 是每秒查询数（sysbench `queries/sec`）；延迟是每个事件的 p95、p99，单位 ms。

| test | threads | build | QPS median (min–max) | p95 ms | p99 ms | errors | n |
|---|--:|---|--:|--:|--:|--:|--:|
| ps | 8 | base | 31,547 (30,796–32,663) | 0.34 (0.32–0.35) | 0.40 (0.37–0.40) | 0 | 3 |
| ps | 8 | go | 32,027 (31,855–32,124) | 0.34 (0.33–0.34) | 0.40 (0.39–0.40) | 0 | 3 |
| ps | 8 | nosteal | 29,597 (29,597–29,597) | 0.35 (0.35–0.35) | 0.40 (0.40–0.40) | 0 | 1 |
| ps | 8 | wired | 32,367 (30,940–32,586) | 0.32 (0.32–0.34) | 0.37 (0.37–0.40) | 0 | 3 |
| ps | 32 | base | 41,099 (40,841–41,735) | 1.03 (1.01–1.03) | 1.21 (1.19–1.26) | 0 | 3 |
| ps | 32 | go | 49,084 (48,939–49,156) | 1.10 (1.07–1.10) | 1.48 (1.48–1.51) | 0 | 3 |
| ps | 32 | wired | 41,236 (41,236–41,965) | 1.01 (1.01–1.03) | 1.21 (1.19–1.21) | 0 | 3 |
| ps | 64 | base | 44,744 (44,100–45,530) | 1.96 (1.92–2.04) | 2.44 (2.34–2.70) | 0 | 3 |
| ps | 64 | go | 53,979 (53,886–54,066) | 1.81 (1.81–1.81) | 2.70 (2.64–2.70) | 0 | 3 |
| ps | 64 | nosteal | 45,297 (45,297–45,297) | 2.04 (2.04–2.04) | 2.39 (2.39–2.39) | 0 | 1 |
| ps | 64 | wired | 44,733 (44,720–45,547) | 1.92 (1.92–2.00) | 2.44 (2.34–2.54) | 0 | 3 |
| ro | 8 | base | 25,774 (24,295–25,848) | 4.82 (4.72–5.12) | 5.01 (4.91–5.54) | 0 | 3 |
| ro | 8 | go | 22,640 (22,630–22,678) | 5.54 (5.43–5.54) | 6.63 (6.63–6.63) | 0 | 3 |
| ro | 8 | nosteal | 24,298 (24,298–24,298) | 5.01 (5.01–5.01) | 5.32 (5.32–5.32) | 0 | 1 |
| ro | 8 | wired | 25,633 (24,610–25,764) | 4.82 (4.72–5.01) | 5.22 (5.01–5.32) | 0 | 3 |
| ro | 32 | base | 35,942 (35,784–36,013) | 14.19 (13.90–14.19) | 15.37 (15.06–15.68) | 0 | 3 |
| ro | 32 | go | 38,318 (37,954–38,582) | 14.76 (14.76–15.06) | 17.33 (17.33–18.40) | 0 | 3 |
| ro | 32 | wired | 36,206 (36,138–36,334) | 13.90 (13.90–13.90) | 15.06 (14.76–15.06) | 0 | 3 |
| ro | 64 | base | 39,175 (39,117–39,510) | 26.37 (26.37–26.37) | 28.57 (28.00–28.57) | 0 | 3 |
| ro | 64 | go | 41,744 (41,657–41,752) | 27.45 (26.90–27.45) | 33.52 (31.57–33.52) | 0 | 3 |
| ro | 64 | nosteal | 38,192 (38,192–38,192) | 26.37 (26.37–26.37) | 28.57 (28.57–28.57) | 0 | 1 |
| ro | 64 | wired | 39,770 (39,413–40,190) | 26.37 (26.37–26.37) | 28.00 (28.00–28.57) | 0 | 3 |
| scan | 16 | base | 33,202 (33,022–33,205) | 0.83 (0.81–0.83) | 1.31 (1.29–1.37) | 0 | 3 |
| scan | 16 | go | 38,886 (37,063–39,072) | 0.60 (0.60–0.64) | 0.75 (0.74–0.81) | 0 | 3 |
| scan | 16 | nosteal | 26,993 (26,993–26,993) | 1.12 (1.12–1.12) | 1.51 (1.51–1.51) | 0 | 1 |
| scan | 16 | wired | 33,530 (32,798–33,576) | 0.81 (0.80–0.83) | 1.31 (1.21–1.34) | 0 | 3 |
| overload | 16 | base | 38,025 (37,494–38,113) | 0.54 (0.54–0.57) | 0.61 (0.61–0.67) | 0 | 3 |
| overload | 16 | go | 40,084 (39,415–40,244) | 0.57 (0.56–0.58) | 0.68 (0.67–0.69) | 0 | 3 |
| overload | 16 | wired | 37,360 (37,182–37,740) | 0.57 (0.54–0.57) | 0.65 (0.63–0.67) | 0 | 3 |
| overload | 32 | base | 40,842 (40,713–41,563) | 1.01 (1.01–1.03) | 1.24 (1.21–1.24) | 0 | 3 |
| overload | 32 | go | 49,121 (49,011–49,159) | 1.10 (1.10–1.10) | 1.51 (1.51–1.51) | 0 | 3 |
| overload | 32 | wired | 41,237 (41,219–41,833) | 1.01 (1.01–1.03) | 1.21 (1.21–1.21) | 0 | 3 |
| overload | 64 | base | 44,029 (43,883–44,936) | 1.88 (1.88–1.92) | 2.30 (2.30–2.39) | 0 | 3 |
| overload | 64 | go | 53,572 (53,525–54,996) | 1.84 (1.84–1.84) | 2.81 (2.81–2.81) | 0 | 3 |
| overload | 64 | wired | 44,650 (44,143–45,099) | 1.92 (1.88–1.96) | 2.39 (2.34–2.49) | 0 | 3 |
| overload | 64 | wired_tl64 | 43,947 (43,580–44,319) | 1.88 (1.84–1.92) | 2.25 (2.25–2.39) | 0 | 3 |
| overload | 128 | base | 47,558 (47,318–48,845) | 3.87 (3.79–4.02) | 5.12 (5.01–5.43) | 0 | 3 |
| overload | 128 | go | 55,418 (55,286–55,793) | 3.23 (3.17–3.23) | 4.63 (4.45–4.72) | 0 | 3 |
| overload | 128 | wired | 47,671 (46,093–49,268) | 3.87 (3.64–4.11) | 5.32 (5.12–5.65) | 0 | 3 |
| overload | 128 | wired_tl64 | 45,898 (45,699–46,621) | 3.50 (3.43–3.50) | 4.11 (4.11–4.19) | 0 | 3 |
| overload | 256 | base | 52,682 (51,641–53,066) | 8.96 (8.60–9.14) | 12.58 (12.33–12.84) | 0 | 3 |
| overload | 256 | go | 59,274 (59,225–59,712) | 6.50 (6.50–6.63) | 8.60 (8.60–8.78) | 0 | 3 |
| overload | 256 | wired | 53,095 (52,760–53,353) | 9.32 (9.32–9.70) | 13.10 (13.10–13.36) | 0 | 3 |
| overload | 320 | base | 53,272 (52,919–53,577) | 11.85 (11.38–12.09) | 16.65 (15.99–16.98) | 0 | 3 |
| overload | 320 | go | 60,196 (59,813–60,208) | 8.43 (8.43–8.60) | 11.38 (10.94–11.38) | 0 | 3 |
| overload | 320 | wired | 54,012 (53,520–55,081) | 11.85 (11.85–11.85) | 16.32 (15.99–16.65) | 0 | 3 |
| overload | 320 | wired_tl64 | 45,111 (44,249–46,085) | 8.27 (8.27–8.43) | 9.51 (9.32–10.30) | 0 | 3 |

`wired_tl64` = wired 加 `--token-limit 64`（Q1 限制在 64 个并发语句）。

**扫描期间完成的全表扫描数**（每次 15 秒）：base 121 / 124 / 122，wired 122 / 123 / 118，Go 715 / 727 / 731。

Go 快约 6 倍的原因不在运行时：Go 的计划是 `IndexReader(HashAgg cop[tikv] ← IndexFullScan k_1)`，只读窄的二级索引 `k_1`，并把部分聚合下推。PoC 也把部分聚合下推了（`crates/sql/src/plan.rs`：`TableReader[cop: scan -> partial agg]`），但它只有 TableReader，要扫整张表的行，包括很宽的 `c`、`pad` 列。PoC 没有 EXPLAIN，这一点由代码确认（`crates/distsql/src/dag.rs` 只生成 `TableScan`）。这是规划器和访问路径的差距，忠实移植的规划器（P3）会补上，不算在运行时头上。同理，点查在 32 线程以上比 Go 低 15%～20%，接入前后相同，属于 PoC 执行路径本身。

### 判定（N3 §5）

| 项 | 判定 |
|---|---|
| 点查、read_only 的 QPS 不低于 PoC 基线 | **通过**：wired / base 在 ps 为 1.03 / 1.00 / 1.00，ro 为 0.99 / 1.01 / 1.02（分别对应 8 / 32 / 64 线程） |
| 大扫描并发下点查的 p99 | 接入前后相同（1.31ms），Go 为 0.75ms，差距见上 |
| 过载 2×、5× | 饱和点取 64 个线程：之后 QPS 的增长不到线程数增长的 1/10，延迟随线程数线性增加。2×（128）和 5×（320）时，吞吐为 64 线程时的 107%、121%，没有崩塌；p99 有上界（5.3ms、16.3ms），与 base 相同。但 **比 Go 高**（Go 为 4.6ms、11.4ms）：base 同样如此，属于 PoC 执行路径。打开 `--token-limit 64` 后 p99 降到 9.5ms，低于 Go，但吞吐只有峰值的 84%，低于 90% 的目标 |
| 停顿（修订后：稳态窗口内 `on_cpu`、`blocked` 为 0） | 稳态 `on_cpu` 为 0（macOS、Linux）；稳态 `blocked`：macOS 无法判定；Linux 待用修正后的窗口重测（PR #10 第二轮评审发现，旧脚本的窗口从 sysbench 进程启动起算，在 Lucky 的机器上几乎全落在建连阶段，那次的"稳态为 0"不成立）。Linux 收尾时有 blocked 和 preempted，单独报告。见 §4.2 |
| 队列指标有读数 | Q1 有；Q2、Q3 没有，见 §5 |

## 4. 停顿

| 场景 | 每次运行的停顿数 |
|---|---|
| ps 8 / 32 / 64、ro 8 / 32 / 64、scan 16（Steal 和 NoSteal） | 0 |
| overload 64 / 128 / 256 / 320（单独运行，各 15 秒） | 1 / 1 / 17 / 37 |
| overload 64 / 128 / 320，`--token-limit 64` | 2 / 0 / 0（3 轮合计） |

一次完整过载扫描（16→320 个线程）中共 83 次停顿，76% 落在 10～16ms，其余在 16～32ms（`n3-runtime-data/diag-overload-sweep-metrics.txt`）。停顿全部来自 `connection` task，也就是语句 task。

停顿数随并发语句数增长，而在客户端负载相同的情况下，用 Q1 把并发语句限制在 64 后停顿消失。两种解释都符合这些数据，目前**还不能区分**：
1. **OS 抢占**：poll 计时用的是墙钟。机器负载约 18，12 核上同时有 TiKV、sysbench 和 tidb-rs 的线程，一个 worker 线程在 poll 中途被换下 10ms 以上，就会算成停顿。限制并发后 tidb-rs 的 CPU 占用下降，抢占随之减少。
2. **锁竞争**：PoC 或 tikv-client 的路径上有同步锁（比如 region cache），320 个语句同时运行时有人被阻塞（M-LOCK 问题）。

后续：停顿疑似发生时，采样线程 CPU 时间（`CLOCK_THREAD_CPUTIME_ID`）。CPU 时间远小于墙钟就是抢占，否则是真的在 CPU 上忙或在锁上自旋。另外，在独立的客户端机器上复测。

## 5. Q2、Q3 为什么没有读数

- **Q2**：PoC 的 coprocessor 请求由 tikv-client（fork `dulao5/client-rust`）内部按 region 拆分，在它自己的 worker task 中发出。PoC 拿不到每个 RPC 的 store，`CopResponse` 也不带执行详情（`schedule_wait`）。要接 Q2，需要在 fork 中加一个按 RPC 调用的钩子，提供 store id、addr，并在响应到达时回调。这是另一个仓库的改动，另提。PoC 没有 `tidb_store_limit` 系统变量。默认窗口不限、store limit 为 0，所以不接入时行为与 Go 默认相同。
- **Q3**：PoC 没有落盘、SLEEP 之类的阻塞工作，指标存在，读数为 0。

## 6. NoSteal（仅供参考，1 轮）

| 测试 | 线程 | Steal（wired 中位数） | NoSteal |
|---|--:|--:|--:|
| ps | 8 | 32,367 | 29,597 |
| ps | 64 | 44,733 | 45,297 |
| ro | 8 | 25,633 | 24,298 |
| ro | 64 | 39,770 | 38,192 |
| scan 期间的点查 | 16 | 33,530 | 26,993 |

功能正常，0 次停顿。扫描并发时点查低 20%：扫描语句占着它所在的线程，同一线程上的点查连接只能在它让出时运行。这与 N3 把 NoSteal 定为可选项的判断一致。

### 4.1 停顿分类（2026-11-07 补测，PR #9 评审之后）

`tidb-runtime` 加入了停顿分类（`crates/runtime/src/stall.rs`）。每次 poll 前读一次线程 CPU 时间（Linux 上再加 `getrusage(RUSAGE_THREAD)`），停顿时再读一次，并按下面的规则归类：
- CPU 时间 ≥ 墙钟时间的一半：`on_cpu`（缺检查点或重的同步计算）；
- 有自愿切换：`blocked`（阻塞的系统调用、锁上睡眠，仅 Linux）；
- 只有非自愿切换：`preempted`（仅 Linux）；
- macOS 没有按线程的切换次数：`off_cpu`。

生产环境默认关闭，测试和基准用 `TIDB_RS_STALL_CLASSIFY=1` 打开。

在本机、自己的 playground 上，过载 320 个线程跑 3 次（`TIDB_RS_STALL_CLASSIFY=1 TESTS=overload OVERLOAD=320 scripts/n3-bench.sh …`）：

| 运行 | QPS | p99 ms | on_cpu | off_cpu |
|---|--:|--:|--:|--:|
| 1 | 51,940 | 17.33 | 0 | 39 |
| 2 | 52,774 | 18.03 | 0 | 44 |
| 3 | 52,965 | 16.98 | 0 | 43 |

全部来自 `connection` task（语句 task）。`on_cpu` 为 0，所以没有需要追查的代码路径。同时跑了 64 线程点查和扫描并发下的点查：0 次停顿。打开分类后点查 QPS 为 44,694，与关闭时的 44,733 在噪声之内。数据见 `n3-runtime-data/stall-classify/`。

macOS 上 off_cpu 分不出被抢占还是阻塞。

### 4.2 稳态窗口与收尾（PR #10 评审之后）

**采样顺序已修正**：现在线程 CPU 和切换次数的两次采样包住 poll 的 `[start, end]`，after 样本只在停顿时读取。之前 `start` 在 before 样本之前，`start` 和 before 之间发生的切换看不到，Lucky 在 Linux 上实测有 3 次 preempted 因此被误判成 off_cpu。

**按时间窗口统计**：`scripts/n3-bench.sh` 在每次运行的开始前、sysbench 输出 "Threads started!"（`--time` 从这里开始计）之后 1 秒、结束前 1 秒、结束后各抓一次 `/metrics`。sysbench 从启动到 "Threads started!" 可能要十几秒（Lucky 的 Linux 机器上 320 线程约 11.8 秒，对 Go TiDB 也一样），所以窗口不能按进程启动时刻计算；最早的版本就是这样算的，在那台机器上窗口几乎全落在建连阶段。中间一段为稳态，前后两段（预热和收尾）单独报告，边距可用 `STEADY_SKIP_START` / `STEADY_SKIP_END` 配置。

本机（macOS）重测，320 线程过载 3 次，打开分类：

| 运行 | QPS | p99 ms | 稳态 on_cpu / blocked / off_cpu | 开头 1 秒 | 结尾 1 秒 + 收尾 |
|---|--:|--:|--:|--:|--:|
| 1 | 52,273 | 17.68 | 0 / 0 / 37 | 0 | off_cpu 4 |
| 2 | 51,103 | 16.65 | 0 / 0 / 29 | 0 | off_cpu 2 |
| 3 | 51,996 | 17.33 | 0 / 0 / 43 | 0 | off_cpu 1 |

64 线程点查和扫描并发下的点查：所有窗口均为 0。

**macOS 的时间分布与 Linux 不同**：server 日志中每次停顿都有时间戳（runtime 现在通过 `tracing` 打日志，见下）。停顿分散在整个 15 秒内，每秒 1～8 次，并**没有**集中在收尾（`n3-runtime-data/stall-window/win320-r*-stall-log.txt`）。所以本机的 off_cpu 停顿是稳态下的现象：机器负载约 15～18，CPU 一直处于超额订阅。macOS 没有按线程的切换次数，无法判定其中是否有 blocked。按修订后的 N3 §5，本机 `on_cpu` 为 0，达标；`blocked` 在 macOS 上无法判定，以 Linux 为准。

**Linux（Lucky 复现，10 核）**：

已确认：
- 6 次运行的 24 次停顿，全部发生在 sysbench 结束前 28～51ms 之内，即 320 个客户端同时结束、断开的时刻；15 秒稳态中为 0。
- 用 bpftrace 抓到的 2 次 blocked 栈，在 tonic 的 h2 连接 task 中（不是我们的 connection task），等待 h2 连接共享的 `std::sync::Mutex`（`Streams::poll_pending_open` → futex，5.4ms、23.7ms）。这类 task 由 tikv-client 自己 spawn，**不经过插桩**，是停顿检测的盲区（N3 3.6 已注明）。
- 把 tidb-rs 绑在独占的 4 个核上后，320、640 个并发语句都是 0 次停顿。
- 另有一次运行在我们的 connection task 上出现 5 次 blocked（各 1 次自愿切换、13～20ms），与同一次运行的抢占停顿挤在约 5ms 的窗口内，也在收尾时刻。
- 修正窗口后的 5 次重测（PR #10 第三轮复核）：稳态窗口内 `on_cpu`、`blocked` 全部为 0；停顿都落在两端。收尾段每次 0～8 次，其中一次含 1 次 blocked；**建连阶段偶尔也有**：5 次中有 1 次在开头段出现 1 次 blocked。

推测（待 bpftrace 后续确认，N3 3.6 已列为后续）：
- 我们 connection task 上的 blocked 与 tonic 连接 task 同因，即 h2 的锁；
- 收尾时的 preempted 只是 sysbench 收尾时的 CPU 突发。

结论（部分是推测，见上文）：本机过载下的停顿是持续的 CPU 超额订阅，`on_cpu` 为 0；Linux 上看到的停顿集中在所有客户端同时断开的收尾时刻，原因推测为连接关闭和 h2 锁排队，待 bpftrace 后续任务确认，稳态窗口内的计数待用修正后的脚本重测。N3 §5 改为只在稳态窗口内判定 `on_cpu`、`blocked` 为 0。

**日志**：runtime 之前用 `log` 宏，tidb-server 的 tracing subscriber 没有 log 桥接，停顿警告和 `spawn_with_recover` 的 panic 日志都丢了。现在 runtime 改用 `tracing`：tidb-server 本来就用 tracing，改宏比安装全局的 `LogTracer` 更直接。`crates/runtime/tests/logs.rs` 测试停顿和 panic 两条日志都能被 tracing subscriber 收到。本次重测中，server 日志里能看到全部 116 条停顿警告。

## 7. 行为不变（integrationtest）

`scripts/integrationtest.sh -- select subquery topn_push_down topn_pushdown window_function collation_agg_func collation_misc collation_pointget common_collation null_rejected clustered_index tpch agg_predicate_pushdown`（13 个文件，681 条语句，378 条读），base 和 wired 各跑一次：两边逐条语句的结果分类（match / mismatch / unsupported / 错误）**完全相同**（`statements.tsv` 的前 6 列逐行 diff 为空），汇总都是 match 29、mismatch 9、unsupported 193、b_error 3。原始汇总见 `n3-runtime-data/integrationtest/`。

**覆盖范围要说清楚**：681 条语句中，写入 291 条只在 Go TiDB 上执行；读 378 条中 193 条 tidb-rs 报"不支持"，其余大多是 Go 侧本身报错（汇总中 a_error 155，含非读语句）或被跳过。真正比对到结果的只有 match 29 + mismatch 9 + b_error 3，**共 41 条**。"逐条相同"指这 41 条结果相同，另外 unsupported 和报错的分类也相同。接入不改变语义，这个覆盖足以说明没有回退，但不能当作 681 条语句的正确性证明。

全部 263 个文件跑一遍约 2 小时，base、wired 两遍要 4 小时，所以只选了偏读取的这一组；wired 的完整运行跑到第 66 个文件时中止，没有发现崩溃或连接异常。
