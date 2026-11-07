# N3+A1：并发、取消、错误边界与统一内存模型（基础设计 spec，草案）

状态：草案（2026-11-04 按 PR #2 评审修订：执行模型改为固定大小的任务池，见 3.1）。类型：实现例外（基础设计），合并 N3（含 A2）与 A1。本 spec 中的映射规则 ID（`M-xx`）见 [docs/porting/mappings.md](../../../docs/porting/mappings.md)。计划定稿：P1 之前（A1 的执行器部分在 P4 前复核）。

所属块：D00.3（sqlkiller、servermemorylimit、expensivequery、disk）、D01.7 / D01.10（chunk 与落盘容器）、D07.2（copr 迭代器）、D12.0（exec 框架）、D15.1（util/memory）；同时约束 D12 所有带 worker 的算子。

行数均取自 `go_funcs.tsv`（v8.5.8，非生成代码，按函数体累计）；位置均为 `pkg/` 下路径。

---

## 第一部分　N3：并发、取消与错误边界

### 1. 要解决的问题

Go 执行器的并发、取消、错误处理靠三样东西：goroutine + channel、多处轮询的 kill 信号、`panic`/`recover`。这三样在安全 Rust 里都没法逐行照搬：goroutine 能随便借用算子的字段，`recover` 把 OOM 和 kill 当成控制流，`Close` 还可能和 `Next` 并发调用。如果各块各自处理，会有几十种写法，L1 对照也做不下去。

v8.5.8 中的事实：

| 事实 | 位置 | 对 Rust 的影响 |
|---|---|---|
| 执行器接口是 Volcano 式 `Open/Next(ctx, chk)/Close`，注释明确写着 "`Close()` may be called ... with `Next()` at the same time" | `executor/internal/exec/executor.go` `Executor` 接口（L51–76） | Rust 中 `&mut self` 不允许并发调用 `close`；需要把"外部要求结束"和"所有者执行 Close"分开 |
| `exec.Next` 包装器：`defer recover()` 把 panic 转成 error；调用前后各检查一次 `HandleSQLKillerSignal`；同时记录运行时统计 | `exec/executor.go` `Next`（L437–463）、`Close`（L466–473） | 检查点位置要保留（L1），recover 要换载体 |
| 带 worker 的算子都是"fetcher goroutine + N 个 worker goroutine + 结果 channel + closeCh/finishCh"结构：投影 `prepare` 起 1+N 个 goroutine；HashAgg `prepare4ParallelExec` 起 fetcher、partial、final 三组；IndexLookUp `startIndexWorker/startTableWorker`；HashJoinV2 `startBuildAndProbe` | `executor/projection.go`（286 行）、`aggregate/agg_hash_executor.go`（688）、`executor/distsql.go`（1825）、`join/hash_join_v2.go`（1355）、`join/hash_join_v1.go`（1225）、`sortexec/sort.go`（659）+ `parallel_sort_worker.go`（193） | worker 生命周期跨越多次 `Next` 调用，不能用 `std::thread::scope` 借用算子字段 |
| 全包规模：`pkg/executor` 下（含写路径、analyze 等）有 57 处 `go` 语句、33 处 `RunWithRecover/WithRecovery`、135 处 `select {}`；`store/copr` 有 7 处 `go`、7 处 `select` | 统计自 v8.5.8 源码 | 映射规则必须机械化，能写进 plan 模板 |
| `WaitGroupWrapper.RunWithRecover(exec, recoverFn)`：**无论是否 panic 都调用 `recoverFn(r)`**，很多算子把它当"worker 退出钩子"用 | `util/wait_group_wrapper.go` L179 | Rust 包装器要保留"退出时必调、带可选 panic 负载"的语义 |
| worker panic 的典型处理：把 `util.GetRecoverError(r)` 作为一个结果送进结果 channel，由主 goroutine 的 `Next` 返回 | `hash_join_v2.go` `handleProbeWorkerPanic`/`handleJoinWorkerPanic`（L853–863），`projection.go` `recoveryProjection`（L484） | worker 错误就是"一条特殊结果"，Rust 中自然是 `Result<T>` 进 channel |
| Close 的顺序靠约定：`close(closeCh)` → `finished.Store(true)` → 清空各 channel → 只等待部分 WaitGroup（`waiterWg`，不等 `workerWg`）→ 注册 runtime stats → 释放磁盘 → 关闭子节点 | `HashJoinV2Exec.Close`（L670–710）、`IndexLookUpExecutor.Close`（L1204）、`BaseExecutorV2.Close`（exec/executor.go L312，逐个关闭子节点，返回第一个错误） | Go 允许 goroutine 在 Close 返回后才退出；Rust 必须在 Close/Drop 里 join，否则 worker 持有的 `Arc` 和磁盘文件会延迟释放 |
| kill 信号：`SQLKiller.Signal` 是一个 `uint32` 原子量，五种取值（`QueryInterrupted`、`MaxExecTimeExceeded`、`QueryMemoryExceeded`、`ServerMemoryExceeded`、`RunawayQueryExceeded`），`HandleSignal` 映射到错误 1317 / 3024 / 8175 / 8176 / runaway；顺带每秒最多检查一次连接是否存活 | `util/sqlkiller/sqlkiller.go`（96 行，全部） | 状态机 L1 移植；需要额外的"唤醒"能力（见 3.3） |
| `KILL [QUERY]`：`Server.Kill` → `killQuery` 发信号、`conn.cancelDispatch()` 取消 context、`SQLKiller.FinishResultSet()`；KILL CONNECTION 还设置读写 deadline | `server/server.go` `Kill`（L922）、`killQuery`（L963） | 三种手段（信号、context、强制结束结果集）都要有对应物 |
| `max_execution_time`：不是计时器，而是 expensivequery 的 100ms ticker 扫进程表，超时调用 `sm.Kill(id, true, true, false)` | `util/expensivequery/expensivequery.go` `Run`（ticker 100ms，L96 判断） | L1 照搬，超时粒度与 Go 相同（约 100ms） |
| 内存超限也走 kill：`PanicOnExceed.Action` 发 `QueryMemoryExceeded` 后 `panic(err)`；`Tracker.Consume` 在 bs>0 时检查会话根 tracker 的 killer，有信号就 `panic(err)` | `util/memory/action.go` L164–183，`tracker.go` `Consume`（L446–518）、`HandleKillSignal`（L522） | Go 把 OOM/kill 当 panic 用；Rust 必须改成 `Result`，这是本 spec 最大的一条映射规则 |
| `ExecStmt.Exec` 的 recover 只吞 8175/8176/1317/3024 四种错误，其余 panic 继续上抛到连接层；`recordSet.Next` 则吞掉所有 panic 并打 "execute sql panic"；连接层 `clientConn.Run` 再兜底，打 "connection running loop panic" 并把 `%v` 写回客户端 | `executor/adapter.go` L503–539、L157–165；`server/conn.go` `Run`（L1072 起） | 用户可见的报错文本取决于 panic 被哪一层接住，需要保持 |
| copr 迭代器：`tidb_distsql_scan_concurrency` 个 `copIteratorWorker` goroutine + 一个 sender；`sendRate` 令牌限流；`rateLimitAction` 作为内存超限动作挂在会话 tracker 上；KV 层通过 `KVVars.Killed = &SQLKiller.Signal` 轮询 kill | `store/copr/coprocessor.go`（1886 行）`open`（L870）、`worker.run`（L834）、L238–251；`distsql/context/context.go` L117 | I/O 部分天然是异步的（client-rust 是 tokio），要定好同步执行器和异步 KV 的边界 |
| 投影只在估算行数 ≥ `MaxChunkSize`、且非写语句时并行，否则 `numWorkers = 0` | `executor/builder.go` L2170–2182 | 点查、小查询不起 worker；线程池开销只影响分析型查询 |
| 并发度变量：`tidb_executor_concurrency`（默认 5）是各算子并发度的默认来源，旧的 `tidb_hash_join_concurrency` 等在设置时为 `ConcurrencyUnset(-1)` 以外的值才覆盖 | `sessionctx/variable/session.go` `Concurrency` 各 getter（L3244 起）；`tidb_vars.go` `DefExecutorConcurrency=5`、`DefDistSQLScanConcurrency=15` | getter 逻辑 L1 移植，worker 数量必须完全相同 |
| v8.5.8 `tidb_hash_join_version` 默认 `legacy`（v1） | `tidb_vars.go` L1675 | v1、v2 都在 D12.6 范围内；本 spec 的模式对两者都适用 |

### 2. 方案比较

**2.1 执行模型（A2）**

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| a. 全异步，共用网络运行时 | `async fn next()`，worker 是 tokio task，与网络、KV 共用一个运行时 | PoC 现状；与 client-rust 同一运行时 | CPU 密集的算子会占住网络线程；没有让出点时大查询会饿死其他连接。**否决**（2026-11-06 注：方案 d 修订后也只有一个运行时，区别在于 d 有按时间片让出的检查点、有界队列和停顿检测，正是这里列出的缺点的解法） |
| b. rayon | worker 作为 rayon job | 数据并行方便 | Go 的 worker 大量阻塞在 channel 上，固定大小的 rayon 池会被耗尽甚至死锁；不适合长生命周期的流水线 worker。**否决作为 worker 载体**（可在 N6/A5 中用于算子内部的纯计算并行，另议） |
| c. 同步 `Next` + 弹性线程池 | 每个 goroutine 一个 OS 线程，channel 用 crossbeam，KV 用 flume 桥接到 tokio | 写法与 Go 的阻塞式 channel/select 最像 | goroutine 是 M:N 的轻量任务，1:1 的 OS 线程运行模型与它最不像：64 个并发分析型会话、每条查询 30–60 个 worker 就是数千个线程，每个 chunk 跨 2–3 次线程切换；需要自写弹性线程池和同步/异步桥接。**否决**（初稿曾推荐，2026-11-04 评审后撤回） |
| d. async `Next` + 专用的固定大小任务池（参照 TiKV） | 执行器在一个与网络分开、线程数等于核数的任务池上运行；goroutine 映射为 task；检查点处协作式让出 | 运行模型与 goroutine 一致（M:N、廉价）；线程数固定；KV 请求直接 await，没有桥接；有 TiKV 的成熟做法可循（多级队列、准入控制） | 执行器中禁止任何阻塞（用 lint 强制）；future 被 drop 即取消，显式 Close 的顺序要靠 3.5 的规则保证。**采用** |

**2.2 OOM/kill 的错误边界**

| 方案 | 做法 | 评价 |
|---|---|---|
| a. 照搬 panic | `consume` 超限时 `panic_any(Error)`，在 `exec::next` 和 worker 边界 `catch_unwind` | 形状最像 Go，但把 unwind 当控制流：需要全代码 `UnwindSafe`、`panic=abort` 构建不可用、锁被毒化；**否决** |
| b. 显式 Result | 会失败的点（kill 检查、内存检查点）返回 `Result`，调用方 `?`；`catch_unwind` 只放在线程边界，只处理真正的 bug | 每个"Go 里会 panic 的 Consume 调用点"对应一个 `?`，规则机械，可以写进 plan 模板。**采用** |

### 3. 建议

#### 3.1 执行模型：async `Next` + 固定大小的任务池（方案 d，参照 TiKV）

> 2026-11-06 修订（PR #2 补充评审三，Seastar / SEDA 复查）：去掉独立的网络运行时，语句在接收它的线程上跑完（第 2、9 条）；checkpoint 改为按时间片让出（第 5 条）；新增 3.6 节，把运行时边界上的三个队列（Q1 语句准入、Q2 发往 TiKV 的在途窗口、Q3 阻塞池）和过载行为写成契约；新增 3.7 节，后台任务遵守同一纪律。原则：Seastar 决定代码在哪里跑（不交接、按时间片协作让出、全部 `Send` 以保留拓扑可换），SEDA 决定在哪里排队（只在资源边界、有界、可观测、过载在入口处理）；Go 已有的限制按 L1 移植，Go 没有的新行为默认关闭、作为功能例外。
>
> 2026-11-04 修订：评审指出方案 c（每个 goroutine 一个 OS 线程）"写法最像 Go、运行模型最不像 Go"：goroutine 是 M:N 调度的轻量任务，Rust 里最接近它的是工作窃取线程池上的 task，不是 1:1 的 OS 线程；高并发分析型负载下方案 c 会产生数千个线程。项目负责人同意改为参照 TiKV 的线程池模型。

TiKV 的做法（`release-8.5` 分支）可以直接作为模板：

| 要素 | TiKV 的做法 | 位置 |
|---|---|---|
| 固定大小、按核数确定的工作窃取池，与网络线程分开 | unified read pool（底层 yatp），`max-thread-count`，不弹性增长 | `src/read_pool.rs` |
| 任务是 future，CPU 密集的工作协作式让出 | coprocessor 按时间片 `yatp::task::future::reschedule().await` | `src/coprocessor/endpoint.rs` |
| 多级反馈队列 | 按任务累计 CPU 时间降级，大扫描不会饿死点查 | `src/read_pool.rs`（`Extras::new_multilevel`） |
| 准入控制 | `max_tasks_per_worker`，超过时报 busy | `src/read_pool.rs` |

1. **算子接口**：`Executor` trait 的 `open` / `next` / `close` 是 async，方法与 Go 的 `Executor` 接口一一对应；trait 对象上的调用每次 `Next` 一个装箱 future。一次 `Next` 产出一个 chunk（默认最多 1024 行），一次小分配可以忽略；TiKV 的 batch executor 同样按 batch 动态分派。`Executor: Send`，不要求 `Sync`。
2. **只有一个运行时，语句不交接**（2026-11-06 修订）：进程只有一个 tokio 运行时，网络、语句执行、KV 客户端都在上面，不再有"网络运行时 → ExecPool"的交接。连接 task 就是语句 task：规划和根执行器都在接收这条连接的 task 里跑完，只有计划里带并行 worker（Go 起 goroutine 的地方）时才 `pool.spawn`。PoC 就是零交接的，这是它点查能到 Go 1.0–1.2 倍的原因之一；原稿的两次跨运行时交接对点查是纯开销。"CPU 工作卡住网络"由第 5 条的按时间片让出解决。
   - `ExecPool` trait 保留，实现暴露两种拓扑：**Steal**（tokio `multi_thread`，工作窃取，默认）和 **NoSteal**（N 个 tokio `current_thread` 运行时各占一个线程，accept 后挑一个运行时，连接之后留在那个线程；设计参照 pingora-runtime 的 NoSteal，不引 pingora 依赖，约百行）。数据库连接比代理连接更长、负载更不均，所以 NoSteal 只作可选项。两种拓扑跑同一套测试（第 5 节），这是"调度器可替换"的机器证明。
   - 所有执行器代码和 future 必须 `Send`、不依赖线程局部状态。这是同时保留两种拓扑的前提：`!Send` 的 `Rc` future 会把代码永远锁在单线程模式。行和 chunk 的处理路径上不 clone `Arc`（worker 按 `&ExecCtx` 借用）作为编码约定写进 plan 模板，由基准兜底；clippy 无法机械判断哪段代码属于这条路径，不做 lint。
   - 多级队列仍是计划中的升级：若"大扫描并发时的点查 p99"基准不达标，换成 yatp，并按宪章登记为来自 TiKV 的 L3 和 ADR。`tikv-client` 依赖 tokio，所以第一步用 tokio。
3. **每个 goroutine 对应一个 task**（`M-GO`）：`go worker.run()` → `pool.spawn(async move { worker.run().await })`。数量严格等于 Go 的数量（由同一个 `Concurrency` getter 决定），`tidb_executor_concurrency` 等变量含义不变；物理并行度由池大小决定，这与 goroutine 和 GOMAXPROCS 的关系相同。
4. **channel 映射**（`M-CH`、`M-WG`）：有缓冲 channel → 容量相同的异步有界 channel；无缓冲 → 容量为 0 的 rendezvous（无法直接表达时用容量 1 加确认，在 plan 模板里固定一种写法）；`close(ch)` → drop 发送端；`select` → `select!`；`sync.WaitGroup` → 保存 `JoinHandle`，关闭时逐个 await。只能使用**取消安全**的 channel 原语（在 `select!` 中被放弃的分支不会丢数据），选型在试点中确定并写进 plan 模板。
5. **检查点：kill / 内存检查按 Go 的位置，让出按时间片**（`M-KILL`，2026-11-06 修订）：`ctx.checkpoint().await` 放在 Go 的 `HandleSQLKillerSignal` 同一位置（`exec::next` 前后等），以及任何不经过 `Next` 就连续处理数据的循环（hash build、sort、merge、聚合 final 合并的内循环）。每次调用都做 kill 检查和内存检查（第二部分 3.3，都是一次原子读）；**是否让出 CPU 按时间片决定**：本次 poll 运行超过 quantum（默认 1ms）才让出。Go 的检查点是 chunk 级的（`internal/exec/executor.go:448`、`:462`，每次 `exec.Next` 前后各一次，一次 `Next` 可能要几百微秒），所以 `ctx.checkpoint()` **每次调用都看时钟**（约 30ns）；2026-11-07 按 PR #8 评审修订，原稿"每 N 次看一次"会让 400µs 一次的 `Next` 每次 poll 跑到约 12.8ms。真正逐行调用的内层循环（hash build、sort 等）用显式的 `CheckpointStride::new(n)`，每 n 次看一次时钟，这才是 TiKV `RescheduleChecker` 的模式（`components/tidb_query_common/src/storage/scanner.rs`：每 32 个 key 看一次时钟，超过 1ms `reschedule().await`）。Rust 没有 Go sysmon 的抢占，这个原语加 3.6 的停顿检测就是抢占的替代品。checkpoint 同时记录本片 CPU 时间，归到语句和 resource group（注意：当前按 poll 的墙钟时间累计，CPU 超额订阅时会把被抢占的时间也算进去；接 Top SQL、资源管控之前要改成线程 CPU 时间或做校正，PR #9 评审）；按组的调度份额以后加，钩子现在留（`ExecCtx` 字段，见 foundation-interfaces）。调用接口不变。
6. **KV 访问没有同步/异步边界**：coprocessor 迭代器的 worker（数量 = `tidb_distsql_scan_concurrency`，`sendRate` 按 L1 映射为同容量的信号量，它限制的是已收到、未被消费的响应，管的是内存）就是同一个运行时上的 task，直接 await `tikv-client` 的请求。另有服务器级的在途窗口 Q2（3.6），限制的是发往每个 store 的在途 RPC。需要确认 client-rust 的请求 future 被 drop 时会取消请求（待试点验证）。每个 cop 请求都带上 TiKV 调度依赖的字段：priority、resource group name、override priority、task_id（语句级，Go `request_builder.go` 的 `TaskID`）、busy_threshold（与 Go 相同的取值）、deadline（`max_execution_time`）。
7. **禁止阻塞**（`M-BLOCK`、`M-LOCK`）：执行器代码中不得阻塞线程。落盘 I/O 走 `spawn_blocking` 或单独的阻塞池；await 期间不得持有锁；执行器 crate 中不得 `std::thread::spawn` 或 `block_on`。这些用 clippy 强制（见 [mappings.md](../../../docs/porting/mappings.md)）。
8. **准入**：见 3.6 的 Q1。Go 已有 `token-limit`（默认 1000，满了排队等待，不拒绝），按 L1 移植；按 resource group 分份额、过载时拒绝是功能例外，默认关闭。
9. **会话与语句**（2026-11-06 修订）：连接 task 持有 `Session`，语句在同一个 task 里执行，不再 move 到另一个运行时；但 `Session: Send` 的要求保留（Steal 拓扑下 task 会在线程间迁移，见 N2）。结果直接从语句 task 写网络；`FinishResultSet` 的情形（卡在网络写）由 KILL 触发写 future 的取消解决。
10. **调度与 Go 的差异**：Go 没有优先级；换成 yatp 后，混合负载下点查会比 Go 更快。这不是用户可见的语义，作为实现例外声明。

#### 3.2 Worker 的写法（L2 映射规则）

- worker 需要的共享状态放进 `Arc<…Shared>`（只读部分 + 原子量 + `Mutex`），worker 闭包 `move` 进去，满足 `'static`。Go 中 worker 直接读 `e.xxx` 字段的地方，改成读 `shared.xxx`；这是唯一的结构性改动，统一登记为 L2。
- `RunWithRecover(exec, recoverFn)` → `pool.spawn_with_recover(name, fut, on_exit: FnOnce(Option<PanicError>))`：future 外包一层 `catch_unwind`，`on_exit` 无论是否 panic 都调用，保持 Go 的"退出钩子"语义。
- worker 读取 `SessionVars` 的地方（Go 里常见 `e.Ctx().GetSessionVars()...`）只能读 N2 提供的 `Send + Sync` 语句快照（见第 6 节）。

#### 3.3 取消：`SqlKiller` + 唤醒

1. `SQLKiller` 按 L1 移植（信号取值、`SendKillSignal` 的 CAS、`HandleSignal` 的错误映射、连接存活检查、`FinishResultSet`/`Reset`），放在 `Arc` 里，KILL、内存控制器、expensivequery 都能拿到。
2. **增加一个唤醒通道（`M-GOCTX`，Go 里由 `ctx.Done()`、`closeCh` 分担）**：`SendKillSignal` 成功后同时触发一个 `CancelToken`（tokio `CancellationToken`）。所有等待都经过 `ctx.send` / `ctx.recv`，内部是 `select!{ data, token.cancelled() }`；直接调用 channel 的 `send` / `recv` 被 lint 禁止。这样被 kill 的语句不会卡在 channel 上等到下一个检查点，关闭时也不会因为某个 worker 卡在没人读的 channel 上而 join 不返回。
3. **检查点保持 Go 的位置**：`exec::next` 前后（`HandleSQLKillerSignal`）、`HandleKillSignal` 调用点、内存检查点（第二部分 3.3）、KV 层读取 `Killed`。不新增检查点，除非某个长循环在 Go 中依赖 `Consume` 的 panic 来中断（这种情况在该块 plan 中登记）。
4. 每个算子自己的 `closeCh/finishCh` 继续存在（关闭语义不同于 kill：比如 Limit 提前结束），映射为算子私有的 `CancelToken`，并作为语句级 token 的子 token。
5. `max_execution_time`（`M-TICK`）：Go 靠 expensivequery 每 100ms 扫一遍进程表。Rust 改为每条语句一个 deadline 定时器，到期调用与 Go 相同的 `Server::kill(id, query=true, max_execution_time=true, …)`。判断逻辑、日志按 L1 移植；比扫表精确，也没有 O(会话数) 的扫描。Go 约 100ms 的粒度不是用户契约。SQLKiller 每秒检查一次连接存活、`servermemorylimit` 每 100ms 读堆用量（第二部分）同理改为定时器。

#### 3.4 错误边界

1. **Go 中"当作控制流"的 panic 一律改成 `Result`**：`Tracker::consume`、`HandleKillSignal`、`PanicOnExceed::action` 都返回 `Result`。调用点加 `?`。Go 的函数签名没有 error 而内部会因 Consume panic 时，Rust 签名加上 `Result`，统一登记为 L2（"panic→Result"）。
2. **`catch_unwind` 只在 task 边界**（`M-PANIC`）：ExecPool 任务入口（转换成 `GetRecoverError` 等价的错误，经 `on_exit` 送入结果 channel）、语句入口（对应 `ExecStmt.Exec` / `recordSet.Next` 的 recover，保持 "execute sql panic" 日志和 `%v` 形式的报错文本）、连接循环（对应 `clientConn.Run`）。算子内部不写 `catch_unwind`。这些 panic 只来自 bug（越界、`unwrap`），release 构建保持 `panic = "unwind"`。
4. **锁一律用 `parking_lot`**（`M-LOCK`，无毒化）。std `Mutex` 被 panic 的 task 毒化后，同一语句的其他 worker 会全部报 `PoisonError`，用户看到的就不是 Go 的报错文本。
3. 由于 OOM/kill 不再是 panic，`ExecStmt.Exec` 中"只吞四种错误，其余继续 panic"的分支在 Rust 中退化为：四种错误按普通错误返回；真正的 panic 照 Go 的层次继续上抛到连接层。报错文本与 Go 相同。

#### 3.5 Close 与 Drop

1. **`close()` 保留且必须显式调用**，顺序逐行对应 Go：先取消（对应 `close(closeCh)`）→ drain channel → **await 全部 worker 的 `JoinHandle`**（比 Go 严格：Go 有时只等 `waiterWg`）→ **fetcher 归还子执行器** → 注册 runtime stats → 释放磁盘 → 关闭子节点（返回第一个错误）。显式 Close 有可见副作用（EXPLAIN ANALYZE 统计、慢日志），不能只靠 Drop。
   - 子执行器的所有权：Go 的 fetcher 在另一个 goroutine 里调用子节点的 `Next`（如 `projectionInputFetcher.run`），Rust 中子执行器被 move 进 fetcher task。fetcher 退出时把子执行器作为 `JoinHandle` 的返回值交回，父节点 Close 时取回后再关闭。这是所有带 fetcher 的算子共用的模板规则。
2. **Drop 是安全网**：未 Close 就被丢弃（错误路径、unwind）时，Drop 取消并 join worker、删除落盘文件、内存记账随缓冲区释放自动归零；但不注册 stats。
3. "Close 与 Next 并发"在 Rust 中不存在：外部（KILL、内存控制器、`FinishResultSet`）只发信号，Close 永远由持有者执行。
4. future 被 drop 即取消：语句 future 只在连接断开或 Drop 安全网的情形下被 drop；正常路径和 KILL 都走显式 Close。

#### 3.6 运行时边界、队列与过载（2026-11-06 新增）

> 实现位置（2026-11-07）：ExecPool 与两种拓扑、checkpoint 时间片、poll 停顿检测、Q1、Q2、Q3 和对应指标都在 crate `tidb-runtime`（N0 rank 1），设计说明见 [runtime-skeleton.md](runtime-skeleton.md)。

SEDA 的作者在 2010 年的回顾里否定了"每个阶段一个线程池、阶段间全部排队"，保留了有界队列、准入控制、按阶段观测和过载时在入口挡住。tidb-rs 只在三个资源边界排队，算子和 worker 类型不单独建池：

| 队列 | 位置 | 容量与满时行为（默认 = Go） | 新增能力（功能例外，默认关闭） |
|---|---|---|---|
| **Q1 语句准入** | 连接 task 开始执行一条语句前 | L1 移植 Go `token-limit`（`server/server.go` `getToken`，默认 1000），满了排队等待 | 按 resource group 分份额；排队超过上限时报新的明确错误码（不复用 1317） |
| **Q2 发往 TiKV 的在途窗口** | 每个 store 一个服务器级窗口，`ExecCtx` 可拿到的共享对象 | 默认无限；Go 的 `tidb_store_limit`（默认 0 即关闭，超限返回 client-go `ErrTokenLimit`）按 L1 保留在同一对象里 | 窗口加性增、乘性减（AIMD）；按 resource group 分份额 |
| **Q3 阻塞池** | `spawn_blocking`：只放真正阻塞的工作（落盘 I/O 等）。**按类别分别限额**（落盘、其他各一份），一类占满不会饿死另一类（D43）。SLEEP 和 GET_LOCK 不进 Q3：SLEEP 用 async 定时器，按 Go 的 sleep 循环语义检查 KILL；GET_LOCK 本来就是 async | 有界，满了排队 | — |

Q2 的规则：

1. **按在途 RPC 计数**：请求发出时占一个名额，响应到达即释放，不管上层是否已消费。否则 IndexLookUp 的索引侧响应占着名额、表侧请求发不出去，会死锁。结果缓冲多少仍由 Go 的 `sendRate` 管（3.1 第 6 条）。
2. **语句内上限不变**：Go 的 `tidb_distsql_scan_concurrency` 等仍是每个 copIterator 的上限，Q2 在它之上封顶。
3. **拥塞信号用被动信号**：每个 cop 响应的 ExecDetails 里有 TiKV 侧的调度等待时间（`ScanDetailV2.read_pool_schedule_wait_nanos`，kvproto 字段 13；TiKV `src/coprocessor/tracker.rs` 写入），AIMD 用它调窗口。不用 `estimated_wait_ms`：TiKV 只在请求带 `busy_threshold` 时才算它，而且是以拒绝该请求（`ServerIsBusy`）的方式返回（`src/read_pool.rs` `check_busy_threshold`），用它探测拥塞等于每次多一次重试。
4. **resource group**：Go 在 client-go 里已有按 resource group 的 RU 限流（resource control 拦截器）。那部分按 L1 移植时接在 Q2 上，Q2 的分份额不另写一套。
5. **为什么必须现在定**：cop wait 主要来自 SQL 节点的开环发送。一条 IndexLookUp 最多 75 个以上在途 cop 请求（`executor/distsql.go`、`store/copr/coprocessor.go`），几十条并发就是几千个任务压到一个 TiKV，TiKV 再公平也只能排队，而 SQL 节点看不到这个队列。默认无限就是 Go 的行为，所以策略可以以后再打开；但 Q2 必须是 `ExecCtx` 上的共享对象，不能是 copIterator 的局部变量，这一条碰接口。

**观测**：三个队列都导出深度和等待时间指标。语句级记录 Q1 等待、Q2 等待、TiKV 回传的 cop wait 与 process 时间、poll 停顿次数、按组 CPU，进 EXPLAIN ANALYZE 的执行信息和指标。**不加进慢日志**：`INFORMATION_SCHEMA.SLOW_QUERY` 的列和慢日志格式是兼容契约，有工具解析；Go 已有的 cop wait 字段照旧。

**poll 停顿检测**：单次 poll 超过阈值（默认 10ms）计数并打日志，覆盖语句和后台 task。它是 3.1 第 5 条时间片让出的兜底，用来发现漏放 checkpoint 的循环。

**盲区**（PR #10 评审）：只有经过 `ExecPool` 启动的 task 被插桩。tikv-client 自己 `tokio::spawn` 的 task（tonic / h2 的连接 task 等）也跑在我们的 worker 线程上，但不经过插桩，它们的停顿完全看不到。Lucky 用 bpftrace 抓到的 2 次 blocked 栈正是这类 task：在 `h2::proto::streams::Streams::poll_pending_open` 里等 h2 连接共享的 `std::sync::Mutex`（5.4ms、23.7ms），持锁线程被抢占后其他线程排队。补救办法二选一：用 tokio 的 `on_before_task_poll` / `on_after_task_poll` hook 覆盖所有 task（需要 `tokio_unstable`），或者在 nightly 用 bpftrace 做全量检查。

**后续**：
1. 在 Linux 上用 bpftrace 抓一次**我们的 connection task** 上 blocked 停顿的栈，确认是否也是 h2 的锁（目前只是推测）；
2. 在独立的客户端机器上重测过载，确认收尾时的 preempted 停顿是否只是 sysbench 收尾时的 CPU 突发。

#### 3.7 后台任务（2026-11-06 新增）

schema reload 的 diff、统计加载、plan cache 淘汰等后台任务，在 Go 里靠运行时抢占兜底；在单运行时的 Rust 里，它们和语句共用线程，会卡住语句。规则：后台任务和执行器一样遵守 `M-BLOCK`，长循环按 3.1 第 5 条放时间片让出点，否则放到 Q3。停顿检测覆盖后台任务。

### 4. 对 L1 移植的影响（示例：投影的并行执行）

Go（`executor/projection.go` `parallelExecute`，L231）：

```go
output, ok := <-e.outputCh
if !ok { return nil }
err := <-output.done
if err != nil { return err }
mSize := output.chk.MemoryUsage()
chk.SwapColumns(output.chk)
e.memTracker.Consume(output.chk.MemoryUsage() - mSize)
e.fetcher.outputCh <- output
```

Rust（示意）：

```rust
// go: executor/projection.go (*ProjectionExec).parallelExecute L1 [M-CH, M-MEM]
let Some(mut output) = ctx.recv(&mut self.output_rx).await? else { return Ok(()) }; // recv 同时监听 cancel，被 kill 时返回 1317/3024/8175…
ctx.recv(&mut output.done).await?.transpose()?;                                  // worker 错误（含 panic 转换）
chk.swap_columns(&mut output.chk);                                               // 列缓冲带记账句柄，交换时改挂到本算子
ctx.checkpoint().await?;                                                         // 对应 Go 的 Consume：执行超限动作，并让出
ctx.send(&self.fetcher_output_tx, output).await?;
```

启动 worker（Go：`e.wg.Add(1); go e.workers[i].run(ctx)`）：

```rust
// go: (*ProjectionExec).prepare L1 [M-GO]
for w in workers {
    self.handles.push(ctx.pool.spawn_with_recover("projection-worker", w.run(), on_panic_send(out_tx.clone())));
}
```

分支、channel 容量、worker 数量、错误传递路径都和 Go 相同；变化只在载体（task、异步 channel、共享状态放进 `Arc`、panic→Result），都由全局映射规则覆盖，所以仍是 L1，标注只引用规则 ID。

### 5. 验收（N3）

| 项目 | 指标 |
|---|---|
| KILL 延迟 | TPC-H SF10 的 Q1、Q9、Q18、Q21 在 CPU 密集阶段执行 `KILL QUERY`，从发出到被杀会话收到 1317 的 p99 ≤ 100ms，且不高于同环境 Go 的实测值 |
| task 不泄漏 | 测试框架在每条语句结束后断言该语句在 ExecPool 上的 task 数为 0（对应 Go 测试中的 goleak）；进程线程数固定，不随负载增长 |
| 池指标 | 导出队列长度、运行中 task 数、入队等待时间（对应 yatp 的指标） |
| 资源释放 | KILL / 超时 / 内存超限 / worker panic 四种中止路径下，语句结束后该语句的内存记账为 0、落盘文件为 0 |
| 必须通过的 Go 测试 | `executor/test/jointest/hashjoin` `TestKillDuringBuild`、`TestKillDuringProbe`；`executor/test/executor` `TestQueryWithKill`；`executor` `TestKillTableReader`；`executor/test/simpletest` `TestKillStmt`；`session/test/variable` `TestMaxExecutionTime`；`sessionctx/variable` `TestMaxExecutionTime`；`util/sqlkiller` 全部；`server/tests/commontest` `TestClientDisconnectKillsAutocommitInsert`；各算子的 `*RandomFail` / `randomPanic` 类 failpoint 测试（`join` `TestHashJoinRandomFail`、`aggregate` `TestRandomFail`） |
| 并发度语义 | 对每个并发度变量，构造查询断言实际 worker 数与 Go 相同（读 runtime stats 中的 concurrency 字段，与 Go 的 EXPLAIN ANALYZE 对比） |
| 性能 | sysbench oltp_point_select / read_only QPS 不低于当前 PoC 基线（去掉交接后路径与 PoC 一样长）；TPC-H 22 条查询总耗时不高于 Go |
| 两种拓扑 | Steal 拓扑每个 PR 跑全部测试；NoSteal 拓扑 nightly 跑同一套测试。可抓出两类只在一种拓扑下暴露的 bug：`current_thread` 下 task 同步等待自己 spawn 的 task 会死锁；依赖线程局部状态的代码只在工作窃取下出错 |
| 停顿检测 | 测试和基准中打开停顿分类（`SliceConfig::classify_stalls` 或 `TIDB_RS_STALL_CLASSIFY=1`），在每次基准运行的**稳态窗口**内（去掉开头和结尾各 1 秒，`scripts/n3-bench.sh` 的 `STEADY_SKIP_START` / `STEADY_SKIP_END`），`on_cpu` 和 `blocked` 两类停顿（> 10ms）计数为 0，后台任务包括在内。`preempted`（被 OS 抢占）和 `off_cpu`（平台给不出切换次数时无法细分，如 macOS）只记录、不判定；窗口外（预热、所有客户端同时结束并断开时）的停顿单独报告。2026-11-07 按 PR #9、#10 评审修订：poll 按墙钟计时，CPU 超额订阅时被抢占也会算成停顿；Linux 上 320 线程过载的停顿集中在 sysbench 收尾的约 50ms 内（见 docs/bench/n3-runtime.md §4.1、§4.2）。确定的只有时间点，以及 bpftrace 抓到的 2 次 blocked 在 tonic 连接 task 的 h2 锁上；"收尾停顿来自连接关闭和 h2 锁排队、而非语句执行"仍是推测，待下面的 bpftrace 后续任务确认。macOS 上 CPU 超额订阅时，被抢占的停顿分布在整个稳态，不集中在收尾 |
| 队列指标 | Q1 / Q2 / Q3 的深度与等待时间指标存在且在基准中有读数 |
| cop 请求字段 | 用 mock TiKV 断言每个 cop 请求都带 priority、resource group name、override priority、task_id、busy_threshold、deadline，取值与 Go 对同一语句的请求相同 |
| 过载 | 负载升到饱和点的 2 倍、5 倍时，吞吐不低于峰值的 90%，已接纳语句的 p99 有上界且不高于 Go（默认配置，即 Go 行为；AIMD 打开时另测） |
| 高并发分析负载 | 64 个并发会话跑 TPC-H Q1、Q9：吞吐不低于 Go；同时运行的 sysbench 点查 p99 不高于 Go（这是多级队列要解决的场景，不达标时换 yatp，见 3.1 第 2 条） |

---

## 第二部分　A1：统一内存模型

### 1. 要解决的问题

Go 的内存统计是"估算 + 手工记账"：每个算子在合适的地方调用 `Consume(delta)`，delta 来自 `chk.MemoryUsage()` 之类的公式；放进 map、`[]byte` 的键、`Datum` 的开销则是按经验估算。结果是：统计值和真实占用有偏差，漏记、多记、Close 后未归零的 bug 反复出现（tracker 中有专门的 `TestMemoryAndDiskUsageAfterClose`、`TestPBMemoryLeak`），实例级限额只能靠 Go runtime 的 `HeapInuse` 和 GC 兜底。Rust 没有 GC，所有分配都有确定的所有者，可以做到"按实际分配记账"。

v8.5.8 中的事实：

| 事实 | 位置 | 对 Rust 的影响 |
|---|---|---|
| `Tracker` 是一棵树（Impala 风格），`Consume` 沿父链逐级原子累加，并在每级比较硬限额与软限额（软 = 0.8×硬）；记录 `maxConsumed`；更新全局 `MemUsageTop1Tracker` | `util/memory/tracker.go`（583 行，50 个函数）`Consume` L446、`softScale` L117 | 树结构、标签、硬/软限额、`MaxConsumed` 全部 L1 保留（EXPLAIN ANALYZE、慢日志都读它） |
| 超限动作是一条按优先级排列的链：`ActionOnExceed` 接口（`Action/SetFallback/GetFallback/GetPriority/SetFinished/IsFinished`）；`reArrangeFallback` 按优先级降序合并；优先级常量 `DefPanicPriority < DefLogPriority < DefSpillPriority < DefCursorFetchSpillPriority < DefRateLimitPriority` | `util/memory/action.go`（73 行）、`tracker.go` `FallbackOldAndSetNewAction`（L238）、`reArrangeFallback`（L297） | "哪个动作先触发"是用户可见语义，L1 保留 |
| 语句开始时 `ResetContextOfStmt`：会话 tracker 限额设为 `tidb_mem_quota_query`，按 `tidb_mem_oom_action`（默认 `CANCEL`）挂 `PanicOnExceed` 或 `LogOnExceed`；语句 tracker 挂到会话 tracker 下；`tidb_enable_tmp_storage_on_oom` 时磁盘 tracker 挂到 `GlobalDiskUsageTracker` | `executor/select.go` `ResetContextOfStmt`（L922 起，约 L1000–1046） | L1 |
| 超限报错：8175 `ErrMemoryExceedForQuery`（"Your query has been cancelled due to exceeding the allowed memory limit for a single SQL query…[conn=%d]"）、8176 `ErrMemoryExceedForInstance`、8001 `ErrMemExceedThreshold`（LOG 动作的日志文本）；全局磁盘超限 "Out Of Quota For Local Temporary Space!" | `errno/errname.go` L961、L1079–1080；`executor/select.go` `globalPanicOnExceed` | 错误码与文本逐字保持 |
| 落盘动作：`SpillDiskAction`（RowContainer）、`SortAndSpillDiskAction`；第一次触发时 `go spillHelper.SpillToDisk()` **异步**落盘，之后的触发等待落盘完成，若仍超限则调用 fallback；HashAgg 用软限额动作 `AggSpillDiskAction`；HashJoinV2 的 `hashJoinSpillAction` 只设标志，由算子在 `isSpillNeeded` 处同步落盘 | `util/chunk/row_container.go`（448 行）`baseSpillDiskAction.action`（L437）；`aggregate/agg_spill.go`（251）；`join/hash_join_v2.go` L203–261、L749 | 两种落盘方式并存；RowContainer 因异步落盘需要 `RWMutex` |
| copr 的 `rateLimitAction`（`tidb_enable_rate_limit_action`，默认关）优先级最高，超限时降低发送速率 | `store/copr/coprocessor.go` L238–251 | L1 |
| 实例级：`servermemorylimit.Handle.Run` 每 100ms 读 `HeapInuse`，超过 `tidb_server_memory_limit` 时向 `MemUsageTop1Tracker`（且 ≥ `tidb_server_memory_limit_sess_min_size`，默认 128MB）发 `ServerMemoryExceeded`；60s 杀不掉就 `FinishResultSet`；之后 `runtime.GC()`；记录到 `MEMORY_USAGE_OPS_HISTORY` | `util/servermemorylimit/servermemorylimit.go`（176 行）`killSessIfNeeded` L102 | 判断逻辑 L1；"堆使用量"的来源必须换（无 Go runtime） |
| 与 Go runtime 绑定的部分：`tidb_server_memory_limit_gc_trigger`、`tidb_enable_gogc_tuner`、`tidb_gogc_tuner_*`、`util/gctuner`（31 个函数）、`memoryusagealarm` 写 goroutine/heap profile、`Tracker.Release` 的 GC finalizer | `util/gctuner/*`、`util/memoryusagealarm`（330 行，块 X）、`tracker.go` L550 | 没有对应物，需要特性层面的决定（待定 3） |
| Chunk：列存，`Column{length, nullBitmap, offsets []int64, data, elemBuf}`；`MemoryUsage()` = 各列 `Sizeof + cap(...)` 之和 | `util/chunk/column.go`（636 行）L73；`chunk.go`（518）L185 | 布局 L1（编解码、`Row` 访问依赖它）；`MemoryUsage` 改为读取实际容量 |
| 复用池：`chunk.Allocator`（会话级，`maxFreeChunks=64`、每类型 256 列），受 `tidb_enable_reuse_chunk` 控制 | `util/chunk/alloc.go`（202 行）、`pool.go`；`SessionVars.GetChunkAllocator`（session.go L1833） | 删除（`M-POOL`），变量按 Go 运行时专属变量处理（见 3.2） |
| 落盘容器：`RowContainer`、`SortedRowContainer`、`DataInDiskByRows`、`DataInDiskByChunks` | `row_container.go`、`row_in_disk.go`（284）、`chunk_in_disk.go`（289）、`row_container_reader.go`（81） | 落盘文件格式是内部的，可以重设计；对外语义（何时落盘、磁盘列）保持 |
| 用户可见的内存/磁盘数值：EXPLAIN ANALYZE 的 `memory`/`disk` 列 = 语句 tracker 树中 label 等于 plan ID 的节点的 `MaxConsumed`（`FormatBytes` 格式）；慢日志 `Mem_max`/`Disk_max`；statements_summary `AVG_MEM/MAX_MEM/MAX_DISK`；`PROCESSLIST.MEM/DISK`；`MEMORY_USAGE` 表 | `planner/core/common_plans.go` `getRuntimeInfo`（L1116，`SearchTrackerWithoutLock(p.ID())`）、`getRuntimeInfoStr` L1107–1111；`sessionctx/variable/slow_log.go` L176–178；`util/stmtsummary/reader.go` L290–293；`infoschema/tables.go` L865、L1665–1690 | 列、格式、归属算子保持；数值不可能与 Go 逐字节相同（见 3.5） |
| 规模：executor、util/chunk、distsql、store/copr 中共有约 235 处 `Consume(` 调用 | 统计自 v8.5.8 源码 | 每处都要有映射结论 |

### 2. 方案比较

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| a. 照搬估算 | 移植全部 `Consume(delta)` 和估算公式 | 数值最接近 Go | 把 Go 的偏差和漏记一起搬过来；Rust 的数据结构（N5、N6）与 Go 不同，公式本身就不成立；放弃了 A1 的收益 |
| b. 全局分配器钩子 | `#[global_allocator]` 按线程当前语句记账 | 覆盖率最高、对代码零侵入 | 线程间传递的数据（worker 产出、共享缓冲）归属错乱；分配路径里无法执行动作链（日志、锁、落盘都会递归分配）；无法区分算子，EXPLAIN ANALYZE 的按算子列做不出来 |
| c. 带记账的缓冲区 + 保留 Tracker 树和动作链 | 所有大块内存（chunk 列、行容器、哈希表、聚合状态、排序缓冲、copr 响应）都通过同一个记账分配器 `TrackedAlloc` 分配，分配/释放时自动计入所属 tracker；超限动作在检查点执行 | 记的是真实容量；Drop 自动归零；树、标签、动作链、报错都按 L1 保留 | 小对象、未改造的分配不计入（需设覆盖率指标）；数值与 Go 不同 |

### 3. 建议（方案 c）

#### 3.1 保留为 L1 的部分

- `Tracker` 树的 API 与语义：`NewTracker(label, limit)`、`AttachTo/Detach/ReplaceChild`、硬/软限额（0.8）、`MaxConsumed`、`SearchTrackerWithoutLock`、`FormatBytes`、`String()`、标签常量（`LabelForSQLText` 等）、`IsRootTrackerOfSess`、`MemUsageTop1Tracker`。
- 动作链：`ActionOnExceed` trait 与全部实现（`LogOnExceed`、`PanicOnExceed`、`globalPanicOnExceed`、各 spill action、`rateLimitAction`），优先级与 `reArrangeFallback` 逐行移植。
- `ResetContextOfStmt` 中的绑定逻辑；`servermemorylimit` 的选人、日志、60s 强制结束、ops history；所有错误码和文本。
- Chunk/Column 的逻辑布局与访问接口（`Row`、`Column` 的 null bitmap / offsets / data 语义、编码格式 `codec.go`）。

#### 3.2 例外替换的部分

| Go | Rust | 等级 |
|---|---|---|
| 算子中的 `Consume(±delta)` 和估算公式 | 删除 delta 计算；正向 Consume 的位置改为 `mem.checkpoint()?`；负向 Consume（释放）删除，由 Drop 完成 | 例外（A1），按调用点登记 |
| `Chunk.MemoryUsage()` 的公式 | 返回实际缓冲容量之和（与 Go 公式同口径：cap 之和 + 结构体大小） | L2 |
| `chunk.Allocator` / `pool.go` 复用池 | 删除（`M-POOL`：这是 Go 绕开 GC 的做法）。`tidb_enable_reuse_chunk` 按 Go 运行时专属变量处理：接受，设为非默认值时返回 warning（N8） | 例外（A1） |
| RowContainer 的异步落盘与 `RWMutex` | 落盘请求只置标志，由持有者在下一次 `Add`/检查点完成（写文件经 `spawn_blocking`，持有者 await 它）；不再需要读写锁 | 例外（A1），动作触发顺序不变 |
| 落盘文件格式（`row_in_disk.go`、`chunk_in_disk.go`） | 第一步按 Go 的格式移植；是否改格式随 N5/N6 再定 | 内部格式，暂按 L1 |
| `servermemorylimit` 读 `HeapInuse` | 读 jemalloc 统计（`stats.allocated`，或 `resident`，待定 4） | L3（同源：TiKV 也用 jemalloc 统计） |
| `Tracker.Release` 的 GC finalizer、gctuner、`runtime.GC()` | 无对应物 | 特性例外（待定 3） |

#### 3.3 记账与检查点（核心机制）

1. **`TrackedAlloc`**（`M-MEM`）：所有大块内存通过一个实现 `allocator_api2::Allocator` 的 `TrackedAlloc` 分配，它把字节数记到 tracker 后转发给 jemalloc，自己不管理内存（`Vec<u8, TrackedAlloc>`、`hashbrown::HashMap<_, _, _, TrackedAlloc>`、bump arena 都能用）。`TrackedAlloc` 持有一个 tracker 叶子节点的句柄，分配/释放时把**实际字节数**记进去。
2. **热路径只做原子加**：分配路径中不执行动作、不打日志、不加锁。每个线程按句柄批量累计，满 64KiB（或释放）时才沿父链传播；越过硬/软限额时只在对应节点上置"已超限"标志。批量误差上界 = 64KiB × 该语句线程数，在验收中计入。
3. **检查点执行动作链**：`mem.checkpoint() -> Result<()>` 检查标志，按 Go `Consume` 的顺序执行：先 `HandleSignal`（kill），再硬限额动作链，再软限额动作链。动作若是 `PanicOnExceed` 就发 `QueryMemoryExceeded` 信号并返回 8175；若是 spill 就置落盘标志；若是 LOG 就打 8001 文本的日志。检查点放在 Go 中正向 `Consume` 的同一位置，再加上 `exec::next` 前后——所以"在哪一步报错"与 Go 相同，只是报错依据的是实际字节数。
   - **实现与偏离**（2026-11-07，D15.1 及其评审）：不设"已超限"标志，检查点按当时的消耗重新沿父链判断；另设"自上次检查点后有增长"标志（`record` 在 `bs > 0` 时沿路径置位，`checkpoint` 读出并清零），没有增长就不执行动作链，对应 Go 只在 `Consume` 的 `bs > 0` 时执行。否则一个超限但正在释放内存的算子（如刚落盘完）会在每个 chunk 的检查点都执行动作链，直接沿 fallback 升级到取消查询，而 Go 要等下一次增长才升级。
   - **偏离：两次检查点之间的瞬时峰值看不到。** 分配后在下一个检查点之前又释放的内存，Go 的 `Consume` 会在峰值时执行动作，这里看不到。只涉及 `TrackedAlloc` 记账的部分（Go 本来就不记这部分，按估算记），作为 A1 的偏离接受。
4. **归属**：缓冲区记在分配它的 tracker 上；`Chunk::swap_columns`、把 chunk 交给父算子等"所有权转移"时改挂到接收方 tracker（一次原子减一次原子加），对应 Go 在 `SwapColumns` 后 `Consume(delta)` 的惯例，使 EXPLAIN ANALYZE 的按算子数值归属与 Go 一致。
5. **磁盘**：`DiskTracker` 同一棵树，落盘文件写入/删除时记账；文件句柄由 RAII 持有，Drop 删除。

#### 3.4 页池与可计算预算：推后

> 2026-11-04 修订：初稿让所有大块内存走 64KiB 页池、算子内存按页数计算。这是 Umbra/Velox 一类系统的做法；TiKV 没有页池，用的是 jemalloc 加粗粒度的显式记账（`MemoryQuota`）。A1 的核心收益（按实际分配记账、Drop 归零、动作链 L1）只需要 `TrackedAlloc`，不需要页池，而页池有写出"第二个分配器"的风险。

- **第一步只做 `TrackedAlloc`**。
- 页池和"可计算预算"推到 N5/N6 定稿时，由实测决定要不要做。
- 与 N5：从 coprocessor 响应零拷贝解码时，chunk 列借用响应缓冲（`Arc` 共享）。共享缓冲只记账一次，记在 copr/reader 的 tracker 上，最后一个引用释放时归零。这会让一部分内存从 Go 的"消费算子"挪到"读取算子"名下，属于 3.5 中声明的差异。
- 与 N6：哈希表、聚合、排序的结构由 N6 决定，但必须使用 `TrackedAlloc`；它们的落盘必须通过本 spec 的动作链触发（优先级沿用 Go 的 `DefSpillPriority`）。`Vec<T, A>` 这类带分配器参数的容器在生态里穿透性差（很多 crate 只接受 `Vec<T>`），N6 定稿前要验证 sort、hash 的实现能否全程带着 `TrackedAlloc`，否则达不到第 5 节的记账覆盖率。

#### 3.5 用户可见语义的兼容契约

**必须与 Go 相同**（对照测试逐项比较）：

- 变量：`tidb_mem_quota_query`、`tidb_mem_oom_action`、`tidb_server_memory_limit`、`tidb_server_memory_limit_sess_min_size`、`tidb_enable_tmp_storage_on_oom`、`tidb_enable_rate_limit_action`、`tidb_enable_reuse_chunk`、`tidb_mem_quota_apply_cache`、`tidb_max_chunk_size` 的取值范围、默认值、校验、作用域、报错。
- 触发规则：硬限额、软限额 0.8、动作优先级与 fallback 顺序；CANCEL 返回 8175（含 `[conn=%d]`），实例超限返回 8176，LOG 只记日志；磁盘配额文本。
- 结构：EXPLAIN ANALYZE 的 `memory`/`disk` 列存在、格式（`FormatBytes`）、"哪些算子有值、哪些是 N/A"；慢日志、statements_summary、PROCESSLIST、MEMORY_USAGE 的列名和类型。

**不承诺与 Go 相同**（声明为差异）：

- 所有内存/磁盘**数值**。Rust 记的是实际分配，Go 记的是估算；两者口径不同，不做数值对照。
- 因此"刚好在某个配额下触发/不触发落盘或取消"的边界行为可能不同：同一查询在同一 `tidb_mem_quota_query` 下，Rust 可能早于或晚于 Go 触发。
- `MEMORY_USAGE.GC_LAST/GC_TOTAL` 等 GC 列（待定 3）。

**替代的契约**（用于验收）：

- 单调与归零：每个 tracker 在语句结束后为 0；`MaxConsumed` ≥ 任一时刻的当前值。
- 精度：限额执行误差 ≤ `max(5%×quota, 批量误差上界 + 一个 chunk)`——Go 做不到这一点，这是 A1 的收益。
- 量级：同一查询的总内存与 Go 相比在 [0.3×, 1.5×] 内（不作为门禁，只作为异常报警）。

**运维指引**：混合集群里，同一条 SQL、同一个 `tidb_mem_quota_query`，可能在 Go 节点能跑而在 tidb-rs 被 8175 取消，或者反过来。发布时附一份说明，用实测的比值带宽（目前假设 [0.3×, 1.5×]）告诉用户配额该怎么设，带宽随 TPC-H 实测更新。

**测试处理规则**：断言具体字节数的 Go 测试（如 `util/chunk` `TestChunkMemoryUsage`、`TestListMemoryUsage`、`join` `TestConcurrentMapMemoryUsage`）标记为"数值不对照"，改写为上面的性质测试，在块 plan 中逐个列出；依赖"配额刚好卡在 Go 的数值上"的测试（如用某个具体 `tidb_mem_quota_query` 值来逼出落盘）允许重新标定配额，要写明原值、新值和理由。

### 4. 对 L1 移植的影响（示例）

Go（`executor/projection.go` `unParallelExecute`，L214）：

```go
mSize := e.childResult.MemoryUsage()
err := exec.Next(ctx, e.Children(0), e.childResult)
e.memTracker.Consume(e.childResult.MemoryUsage() - mSize)
if err != nil { return err }
```

Rust（示意）：

```rust
// go: executor/projection.go (*ProjectionExec).unParallelExecute A1 (Consume(delta) → checkpoint)
let err = exec::next(ctx, self.child_mut(0), &mut self.child_result);
self.mem.checkpoint()?;   // child_result 的列缓冲已由 TrackedAlloc 记在 self.mem 下
err?;
```

注意 Go 中 `Consume` 先于 `if err != nil` 执行，Rust 保持同样的顺序：检查点的错误（8175）优先于子节点错误返回——这与 Go 中 Consume panic 先于返回 err 的效果一致。

`Tracker` 本身（Go `tracker.go` `Consume`）：

```rust
// go: util/memory/tracker.go (*Tracker).Consume A1：拆成两半
//   热路径 record(bytes)：沿父链原子累加、更新 max、置超限标志（分配器调用，不失败）
//   checkpoint()：按 Go 原顺序 Top1 更新 → HandleSignal → 硬限额 tryAction → 软限额 tryAction，返回 Result
```

### 5. 验收（A1）

| 项目 | 指标 |
|---|---|
| 必须通过的 Go 测试（行为类） | `util/memory`：`TestAttachTo`、`TestDetach`、`TestReplaceChild`、`TestMaxConsumed`、`TestOOMAction`、`TestOOMActionPriority`、`TestErrorCode`、`TestGlobalTracker`、`TestSetLabel*`、`TestToString`、`TestFormatBytesWithPrune`；`util/chunk`：`TestSpillAction`、`TestSpillActionDeadLock`、`TestSortedRowContainerSortSpillAction`、`TestInterruptedDuringSpilling`、`TestPanicWhenSpillToDisk`、`TestReadAfterSpillWithRowContainerReader`；执行器：`TestOOMActionPriority`、`TestSessionRootTrackerDetach`、`TestOOMPanicInHashJoinWhenFetchBuildRows`、`TestMemoryAndDiskUsageAfterClose`、`TestExplainAnalyzeMemory`、`TestExplainAnalyzeCTEMemoryAndDiskInfo`、各算子 `TestFallBackAction`、hash join / sort / topn / agg / CTE 的 spill 测试（`inner_join_spill_test.go`、`outer_join_spill_test.go`、`sort_spill_test.go`、`parallel_sort_spill_test.go`、`topn_spill_test.go`、`agg_spill_test.go`、`cte` `TestSpillToDisk`）；`TestGlobalMemoryControl`、`TestGlobalMemoryControl2`、`servermemorylimit` `TestMemoryUsageOpsHistory`；`sessionctx/variable` 的 `TestTiDBMemQuotaQuery`、`TestTiDBServerMemoryLimit*`；`session/test/variable` `TestCoprocessorOOMAction`；`distsql` `TestSelectMemTracker`（按"数值不对照"规则改写后通过） |
| 记账覆盖率（含 N6 的哈希、排序结构） | TPC-H SF10 每条查询：峰值时 tracker 根节点 / 该语句线程的 jemalloc 已分配增量 ≥ 0.9 |
| 限额精度 | `tidb_mem_quota_query = Q`、`tidb_mem_oom_action=CANCEL` 下跑 TPC-H 重查询（Q9、Q18、Q21），实际峰值分配 ≤ Q + 3.5 节的误差上界；开启落盘时查询成功且结果正确 |
| 内存占用 | 默认配置下 TPC-H SF10 22 条查询各自的进程 RSS 峰值 ≤ Go 的 70%；sysbench oltp_read_only 稳态 RSS ≤ Go 的 70% |
| 记账开销 | sysbench oltp_read_only、TPC-H：关闭记账（编译开关）与开启记账的 CPU 差 ≤ 2% |
| 归零 | 所有执行器测试结束时语句 tracker、会话 tracker、全局磁盘 tracker 均为 0；无残留落盘文件 |

---

## 第三部分　与其他基础 spec 的关系

| spec | 交互 |
|---|---|
| N1（arena 计划） | 执行器从 N1 的拥有型不可变物理计划（`Arc` 节点）构建，持有 `Arc` 不借用 arena，所以可以 `Send` 给 worker。tracker 标签继续用 plan ID（EXPLAIN ANALYZE 用它查找）。优化阶段的 arena 本身也应记在语句 tracker 下（Go 不记，属于新增观测，不参与限额，待定 6）。 |
| N2（会话 / 语句上下文拆分） | `SqlKiller`、会话根 tracker 必须放在"跨线程可见"的会话共享部分（KILL、内存控制器、expensivequery 从其他线程访问）；`StmtCtx` 持有语句 `MemTracker`/`DiskTracker`/`RuntimeStatsColl`，以 `Arc` 形式交给执行器。N2 需要提供一个 `ExecCtx`（`Send + Sync`、廉价 clone）：killer、取消 token、语句 tracker、并发度快照、只读变量快照、ExecPool 句柄。worker 只能用它，不能碰 `SessionVars`。 |
| N5（值表示与零拷贝）、N5a（列格式） | 共享响应缓冲的记账规则见 3.4；chunk 列的缓冲用 `TrackedAlloc` 分配。 |
| N6（hash agg / join / sort 数据结构） | 必须用 `TrackedAlloc`；落盘由本 spec 的动作链触发；并发结构（partial/final worker、build/probe worker）沿用本 spec 的 ExecPool 与映射规则。 |
| A2（算子内部的 worker 结构） | **刻意保留 Go 的形状**：HashAgg 的 fetcher → partial → final、HashJoin 的 build/probe worker、IndexLookUp 的 index/table worker 都是为 goroutine 设计的结构，Rust 引擎（DataFusion、Velox、TiKV 的 coprocessor）用的是分区/morsel 并行。移植阶段仍按 Go 的结构移植：P4 以它为形状，并发度变量和 EXPLAIN ANALYZE 的 concurrency 字段都暴露它。这是一条显式决定，不是 Rust 侧的设计；以后改成分区并行时，走新的例外 spec（暂记为 N9）。 |
| N7（结果集编码与网络层） | 语句和网络写在同一个 task（3.1 第 9 条），`FinishResultSet` 靠取消写 future 实现，需要 N7 配合。 |

## 第四部分　决定与待定

已决定（2026-11-04，项目负责人确认 PR #2 评审意见）：

1. **执行模型**：async `Next` + 专用的固定大小任务池（方案 d，3.1）。第一步用专用 tokio 运行时，是否换 yatp 由高并发分析负载下的点查 p99 基准决定。
2. **池的上限策略**：固定线程数；语句默认排队，排队上限可配置，超限报新的错误码并打指标（3.1 第 8 条）。（2026-11-06 由 3.6 的 Q1 细化：默认 = Go `token-limit`；拒绝是功能例外。）
3. **Go runtime 专属变量与列**（`tidb_server_memory_limit_gc_trigger`、`tidb_enable_gogc_tuner`、`tidb_gogc_tuner_*`、`tidb_enable_reuse_chunk`、`tidb_memory_usage_alarm_*` 的 profile 输出、`MEMORY_USAGE.GC_LAST/GC_TOTAL`）：读取返回 Go 默认值；设为默认值时接受；设为其他值时接受并返回 warning "has no effect in tidb-rs"；GC 列返回 NULL/0。完整清单在 N8 中维护。
4. **实例内存的度量**：限额判断用 jemalloc 的 `resident`（更接近 OOM 风险），归属用 `allocated`，两者都导出指标。
5. **数值类测试**：改写为性质测试、允许重新标定配额（3.5）。
6. **点查快速路径**：先不做；但把"是否经过 ExecPool"设计成调度上的选择，而不是另一套执行器，实测后切换不需要改算子代码。

待定：

7. **优化器 arena、plan cache 等是否纳入语句 tracker**：Go 不记；纳入后 `Mem_max` 会变大，可能更早触发配额。建议只观测、不计入限额。
8. **RowContainer 改为由持有者完成落盘**：Go 异步落盘时，读取方可以和落盘并发；改后落盘期间该算子停顿。性能是否需要另设指标？
9. **数字门槛**（KILL 延迟、RSS 70%、记账开销 2% 等）：定稿前先在 Go 上实测基线并回填。
10. **（2026-11-06）单运行时、语句不交接、Steal 默认 / NoSteal 可选**（3.1 第 2、9 条）。
11. **（2026-11-06）checkpoint 按时间片让出**（3.1 第 5 条）。
12. **（2026-11-06）Q1 / Q2 / Q3 与过载契约**（3.6）：默认全部等于 Go；Q1 按组份额与拒绝、Q2 的 AIMD 与按组份额是功能例外，默认关闭，由基准决定是否打开。
13. **（2026-11-06）新观测字段不进慢日志**（3.6）。

## 附：我不确定的地方

- 第 1 节的 `go`/`select`/`Consume` 计数是对整个包的 grep，包含写路径、analyze、import 等不在只读范围内的代码，只读范围内的实际数量会更少。
- "Close 只等 `waiterWg` 不等 `workerWg`" 是从 `HashJoinV2Exec.Close` 读出来的；其他算子是否在 Close 后仍有 goroutine 运行，要在各块 plan 中逐个确认。
- KILL 延迟、RSS 70%、开销 2% 这些门槛是初拟值，需要先在 Go 上测基线再定。
