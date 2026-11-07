# N3 运行时骨架：`tidb-runtime`（设计说明）

状态：PR A（本 crate，2026-11-07 按 PR #8 评审修订）；PR B（接入 PoC 并测基准）待做。依据：[N3 spec](spec.md) 3.1、3.2、3.6、3.7、§5，决定 D32–D37，[foundation-interfaces.md](../foundation-interfaces.md)。

N3 中位于所有执行器、会话、store crate 之下的部分，集中在 crate `crates/runtime`（包名 `tidb-runtime`，N0 rank 1，不依赖任何 tidb-rs crate）。`ExecCtx` 本身仍在 sessionctx / executor 层定义，它的 `checkpoint()`、`pool`、`store_windows`、`cpu` 字段都委托给这里的类型。

## 1. API

| 模块 | 类型 / 函数 | 对应 |
|---|---|---|
| `pool` | `Runtime::new(RuntimeConfig)`、`Runtime::block_on`；`ExecPool`（`Clone + Send + Sync`）：`spawn_connection`、`spawn`、`spawn_with_recover`；`Topology::{Steal, NoSteal}`；`TaskExit::{Completed, Panicked, Cancelled}`、`PanicError` | N3 3.1 第 2、3 条，3.2；M-GO、M-PANIC |
| `slice` | `checkpoint(&hook).await`、`CheckpointStride`（逐行循环）、`CheckpointHook`、`SliceConfig`、`CpuRecorder`、`set_cpu_recorder`、`task_cpu_time`、`stall_count`、`StallCheck` | N3 3.1 第 5 条，3.6 停顿检测；M-KILL |
| `admission` | `TokenLimiter`（Go `util.TokenLimiter`）、`Admission`、`AdmissionConfig`、`Token` | N3 3.6 Q1，D34 |
| `store_window` | `StoreWindows`、`StoreWindowsConfig`、`InflightPermit`、`StoreTokenLimitError`、`observe_schedule_wait` | N3 3.6 Q2，D35 |
| `blocking` | `BlockingPool::spawn_blocking(kind, f)`，`BlockingKind::{Spill, Other}` 各自限额 | N3 3.6 Q3，3.7；M-BLOCK；D43 |
| `local` | `FastHistogram`（线程本地直方图，每秒最多 flush 一次）、`ShardedGauge` / `ShardedGaugeVec`（按线程分片的 gauge，导出时求和） | 热路径指标 |
| `metrics` | `tidb_rs_runtime_*` 指标；client-go 的 `tidb_tikvclient_get_store_limit_token_error` | N3 3.6 观测 |

要点：

- **每个 task 都经过 `ExecPool` 启动**（`spawn_connection` 用于连接，`spawn` 对应 Go 的 `go f()`）。启动时给 future 包一层：每次 poll 计时，用于 CPU 记账、停顿检测和时间片，并维护按名字的存活 task 数（N3 §5 "task 不泄漏"）。不经过 `ExecPool` 的 task 里，`checkpoint` 只执行钩子，不让出。
- **checkpoint**：每次调用都执行调用方的钩子（KILL、内存检查，`ExecCtx` 以 `SqlKiller` 和 `MemTracker` 实现 `CheckpointHook`），也每次看时钟（约 30ns；Go 的检查点是 chunk 级的，一次 `Next` 可能几百微秒），本次 poll 运行超过 1ms 才 `yield_now`。逐行调用的内层循环用 `CheckpointStride::new(n)`，每 n 次看一次时钟。
- **`spawn_with_recover` 的 `on_exit`** 放在 drop guard 里，task 无论怎样结束都恰好调用一次：正常结束 `Completed`、panic `Panicked`、被 `abort`、运行时关闭或启动前就被丢弃时 `Cancelled`。Go 的 goroutine 不能被外部取消，所以 `Cancelled` 是 Rust 独有的；调用方在 `on_exit` 中做 `wg.Done()`、关闭 channel 时，等待方不会挂住。
- **热路径指标**：每次 poll、spawn、取得队列许可都要记录指标。直方图走线程本地缓冲（TiKV 的做法），gauge 按线程分片，按任务名的指标句柄每个线程只解析一次。8 线程的微基准（`cargo bench -p tidb-runtime --bench overhead`）：共享直方图 2.69µs → 16.8ns；共享 gauge inc+dec 248ns → 3.7ns；每次 poll 的插桩开销约 16ns（ExecPool 7.16ms 对 tokio 7.00ms，每线程 1 万次 poll）。
- **CPU 记账**：每次 poll 的时长就是 task 的 CPU 时间（task 不允许阻塞）。`set_cpu_recorder` 把语句的 `CpuRecorder` 挂到当前 task，之后 spawn 的 worker 自动继承。按 resource group 汇总以后在这个钩子上加。
- **NoSteal**：N 个 current-thread 运行时各占一个线程。连接按轮转放到某个线程上，它 spawn 的 task 留在同一线程（识别当前线程所属的运行时用一个运行时内部的线程局部标记，执行器代码仍不允许使用线程局部状态）。

## 2. 哪些是 L1，哪些是 Rust 独有

| 部分 | 性质 | 默认值 = Go |
|---|---|---|
| `TokenLimiter`（`pkg/util/tokenlimiter.go` 整个文件，D00.3，宪章 4.4） | `go: … L2`：带缓冲的 token channel → 公平（FIFO）的信号量。Go 中阻塞在 channel 接收上的 goroutine 按 FIFO 唤醒，tokio `Semaphore` 同样 FIFO | `token-limit` 1000，满了排队，不拒绝 |
| Go `server.getToken` / `releaseToken` 的指标 | 由调用方记录，与 Go 相同（D03.1；`tidb_metrics::token_acquired` / `token_released`，等待时间按 Go 截断为整数微秒） | — |
| client-go `getStoreToken` / `releaseStoreToken`、`ErrTokenLimit.Error` | `go-dep: … L1`。Go 的"先读后加"不是原子的，会在竞争下略微超限，照样保留 | `tidb_store_limit` 0（关闭）；超限时**报错**，不等待 |
| Q2 窗口、AIMD、按组份额 | Rust 独有 | 窗口默认无限；AIMD、按组份额默认关闭，打开时报"不支持" |
| Q1 按组份额、超时拒绝 | Rust 独有（功能例外） | 关闭，打开时报"不支持" |
| `spawn_with_recover` | `go: pkg/util/misc.go WithRecovery L2`（task 上的对应物；多了取消路径）。`on_exit` 恰好调用一次，panic 记日志、计数，不外传 | — |
| 时间片、停顿检测、CPU 记账、ExecPool、拓扑、Q3 | Rust 独有（取代 Go 运行时的 sysmon 抢占和 goroutine） | 时间片 1ms，每次调用看钟；停顿阈值 10ms；Q3 落盘、其他各 256（合计 512，tokio 默认的阻塞线程数） |

## 3. 与 N3 spec、foundation-interfaces 的差异

1. `spawn_with_recover` 返回 `JoinHandle<Option<R>>`（panic 时为 `None`），foundation-interfaces 写的是 `JoinHandle<R>`：panic 后没有 `R` 可返回。`on_exit` 的参数是 `TaskExit`，不是 `Option<PanicError>`：多了 `Cancelled`。
2. `StoreWindows::acquire(store_id, addr)` 不接收 `&ExecCtx`：取消由丢弃 future 完成（信号量的等待是取消安全的），`ExecCtx` 只用来拿到 `StoreWindows` 本身。`addr` 用于 client-go 指标的标签。**调用方必须把这个等待和 `ExecCtx` 的取消放在同一个 `select!` 里**（M-CH，即经过 `ExecCtx` 的辅助函数），否则窗口打开后，被 KILL 的语句仍会在队列里等。`Admission::get` 与 Go 一致，不响应 KILL（Go 的 `getToken` 也不响应）。
3. `ExecPool` 是具体类型，不是 trait：泛型方法的 trait 不能做成 trait 对象，而以后换 yatp 时替换的是 `Runtime` 的内部实现，调用方的接口不变。
4. `checkpoint` 是自由函数 `checkpoint(&hook)`，`ExecCtx::checkpoint()` 在上层委托给它。
5. Q3 按类别分别限额（D43，取代 D20）：SLEEP、GET_LOCK 不进 Q3。

## 4. PR B：接入 PoC 并测基准

> 2026-11-07 状态：第 1、2、3、5 条已接入；第 4 条（Q2）**没有接入**，原因见下。基准结果见 [docs/bench/n3-runtime.md](../../../docs/bench/n3-runtime.md)。

接入：

1. `crates/server` 的 `main` 用 `tidb_runtime::Runtime` 代替 `#[tokio::main]`，每个连接 `spawn_connection`；拓扑由配置项选择（默认 Steal）。
2. 语句入口（PoC 会话处理一条命令的地方）：`Admission::get().await` 加 `tidb_metrics::token_acquired(wait)`，结束时 `token_released()`；`command_started` 不再记录 token。
3. PoC 执行器的行循环（`crates/executor`、`crates/distsql` 的解码循环）在 Go 检查点的对应位置调用 `checkpoint(&())`（KILL 钩子随 D05 接入）。
4. PoC 发往 TiKV 的 coprocessor 请求外面包 `StoreWindows::acquire`，响应到达即释放；响应的 `ScanDetailV2.read_pool_schedule_wait_nanos` 送进 `observe_schedule_wait`。**未接入**：PoC 的 coprocessor 请求由 tikv-client（fork `dulao5/client-rust`）内部按 region 拆分、在它自己的 worker task 中逐个发出，PoC 看不到每个 RPC 的 store，`CopResponse` 也不带 store id 和执行详情。要按 RPC 取许可，需要在 fork 中加一个按 RPC 调用的钩子（带 store id、addr，响应时回调 schedule wait），这是 fork 仓库的改动，另提。默认窗口不限、`tidb_store_limit` 为 0（PoC 也没有这个系统变量），所以不接入时行为与 Go 默认相同。
5. 打开停顿检测，基准期间记录 `tidb_rs_runtime_poll_stalls_total`。

基准（N3 §5），在 `scripts/playground.sh` 起的 v8.5.8 playground 上，用 `scripts/playground-join.sh` 接入 tidb-rs：

| 项目 | 命令 | 判定 |
|---|---|---|
| sysbench 点查、read_only | `scripts/sysbench-prepare.sh`，然后 `scripts/sysbench-run.sh compare -T "1 8 16 64"`（read_only）；点查加 `-x "--point_selects=1 --range_selects=off"` 或 oltp_point_select | QPS 不低于接入前的 PoC 基线 |
| 大扫描并发下的点查 p99 | 一个会话循环跑全表聚合（`SELECT COUNT(*), SUM(k) FROM sbtest1`），同时 `sysbench-run.sh run -T 16` 跑点查 | 点查 p99 不高于 Go；不达标时按 N3 3.1 第 2 条评估 yatp |
| 过载 | 先找到饱和的线程数 N，再跑 2N、5N | 吞吐不低于峰值的 90%，已接纳语句的 p99 有上界且不高于 Go |
| poll 密集的点查（PR #8 评审） | `sysbench-run.sh compare -T "64 128"`，点查 | 与关闭插桩（直接 `tokio::spawn`）的构建相比，QPS 差异在噪声内 |
| 停顿与队列指标 | 基准期间抓 `/metrics` | 停顿计数为 0；Q1、Q2、Q3 的深度和等待时间都有读数 |

两种拓扑都跑一遍（NoSteal 只看功能和数量级，不作为通过条件）。
