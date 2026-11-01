# N3+A1：并发、取消、错误边界与统一内存模型（基础设计 spec，草案）

状态：草案。类型：实现例外（基础设计），合并 N3（含 A2）与 A1。计划定稿：P1 之前（A1 的执行器部分在 P4 前复核）。

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
| a. 全异步 | `async fn next()`，worker 是 tokio task | 与 client-rust、网络层同一个运行时；PoC 现状 | trait 对象上的 async 递归每次 `Next` 都要 `Box<Pin<Future>>`；CPU 密集的算子（hash 构建、排序、聚合）会阻塞 tokio worker，要到处 `block_in_place`；future 在任意 `.await` 处被 drop 就等于取消，与 Go 显式的 Close 顺序不一致；Go 的阻塞式 channel/select 代码要改写成 async 形态，L1 对照变差。**否决** |
| b. rayon | worker 作为 rayon job | 数据并行很方便 | Go 的 worker 大量阻塞在 channel 上，rayon 线程被阻塞会耗尽池子甚至死锁（生产者和消费者都在同一个固定大小的池里）；rayon 不适合长生命周期的流水线 worker。**否决作为 worker 载体**（可在 N6/A5 中用于算子内部的纯计算并行，另议） |
| c. 同步算子 + 专用执行线程池 + 异步 I/O 边界 | `Next` 是同步函数；Go 的 goroutine 映射为执行线程池（ExecPool）上的任务，允许阻塞；channel 用 crossbeam（`select!` 对应 Go 的 `select`）；KV/coprocessor 请求仍是 tokio task，通过同时支持同步与异步两端的有界 channel（如 flume）把结果交给同步侧 | 代码形状与 Go 最接近（阻塞 recv、select、WaitGroup 都有直接对应物）；CPU 工作不占用 tokio 线程；Close 顺序可以逐行对应 | 一条语句的执行要从 tokio 连接任务切换到 ExecPool 线程（两次 channel 跳转，估计每语句数微秒）；需要自己写一个弹性线程池 |

**2.2 OOM/kill 的错误边界**

| 方案 | 做法 | 评价 |
|---|---|---|
| a. 照搬 panic | `consume` 超限时 `panic_any(Error)`，在 `exec::next` 和 worker 边界 `catch_unwind` | 形状最像 Go，但把 unwind 当控制流：需要全代码 `UnwindSafe`、`panic=abort` 构建不可用、锁被毒化；**否决** |
| b. 显式 Result | 会失败的点（kill 检查、内存检查点）返回 `Result`，调用方 `?`；`catch_unwind` 只放在线程边界，只处理真正的 bug | 每个"Go 里会 panic 的 Consume 调用点"对应一个 `?`，规则机械，可以写进 plan 模板。**采用** |

### 3. 建议

#### 3.1 执行模型：同步 `Next` + ExecPool（方案 c）

1. **算子接口**：`trait Executor: Send { fn open(&mut self, ctx) -> Result<()>; fn next(&mut self, ctx, chk: &mut Chunk) -> Result<()>; fn close(&mut self) -> Result<()>; ... }`，方法与 Go 的 `Executor` 接口一一对应。`Send` 是必须的：Go 的 fetcher goroutine 会在另一个 goroutine 里调用子节点的 `Next`（如 `projectionInputFetcher.run`），Rust 中子执行器要能移交给 worker 线程。不要求 `Sync`。
2. **ExecPool**：进程级弹性线程池，语义接近 tokio 的 blocking pool：有空闲线程就复用，没有就新建，空闲超时（如 60s）回收。**不设会导致死锁的硬上限**（Go 的 goroutine 也没有上限；流水线式 worker 互相等待，固定大小的池会死锁），只设一个很高的保护上限（如 4096），超过时语句报错而不是排队，并打指标。线程栈默认 2MiB，可配置。
3. **每个 goroutine 对应一个 ExecPool 任务**。数量严格等于 Go 的数量（由同一个 `Concurrency` getter 决定），所以 `tidb_executor_concurrency` 等变量含义不变。
4. **channel 映射**：Go 的有缓冲 channel → `crossbeam_channel::bounded(n)`（容量相同）；无缓冲 → `bounded(0)`；`close(ch)` → drop 发送端；`select` → `crossbeam::select!`；`sync.WaitGroup` → 保存 `JoinHandle` 并 join；`channel.Clear` → drain。
5. **同步/异步边界只有一处**：coprocessor（D07.2）和点查等 KV 访问。`copIteratorWorker` 映射为 tokio task（数量 = `tidb_distsql_scan_concurrency`，`sendRate` 映射为同容量的信号量），把响应送进 flume channel；执行器侧的 `copIterator::next` 在 ExecPool 线程上同步 `recv`。点查之类的单次请求在 ExecPool 线程上用 `Handle::block_on`。
6. **语句的根执行**放在 ExecPool 线程上；连接任务（tokio，N7 范围）通过有界 channel 收结果并写网络包。这样网络写阻塞不会占用执行线程，`FinishResultSet` 的情形（卡在网络写）由连接任务直接丢弃写 future 解决。

#### 3.2 Worker 的写法（L2 映射规则）

- worker 需要的共享状态放进 `Arc<…Shared>`（只读部分 + 原子量 + `Mutex`），worker 闭包 `move` 进去，满足 `'static`。Go 中 worker 直接读 `e.xxx` 字段的地方，改成读 `shared.xxx`；这是唯一的结构性改动，统一登记为 L2。
- `RunWithRecover(exec, recoverFn)` → `pool.spawn_with_recover(name, exec, on_exit: FnOnce(Option<PanicError>))`：`on_exit` 无论是否 panic 都调用，保持 Go 的"退出钩子"语义。
- worker 读取 `SessionVars` 的地方（Go 里常见 `e.Ctx().GetSessionVars()...`）只能读 N2 提供的 `Send + Sync` 语句快照（见第 6 节）。

#### 3.3 取消：`SqlKiller` + 唤醒

1. `SQLKiller` 按 L1 移植（信号取值、`SendKillSignal` 的 CAS、`HandleSignal` 的错误映射、连接存活检查、`FinishResultSet`/`Reset`），放在 `Arc` 里，KILL、内存控制器、expensivequery 都能拿到。
2. **增加一个唤醒通道（L2，Go 里由 `ctx.Done()`、`closeCh` 分担）**：`SendKillSignal` 成功后同时触发一个 `CancelToken`（内部是一个只关闭、不发送的 channel + tokio `CancellationToken`）。所有阻塞等待都写成 `select!{ recv(data) , recv(cancel) }`，异步侧用 `token.cancelled()`。这样被 kill 的语句不会卡在 channel 上等到下一个检查点。
3. **检查点保持 Go 的位置**：`exec::next` 前后（`HandleSQLKillerSignal`）、`HandleKillSignal` 调用点、内存检查点（第二部分 3.3）、KV 层读取 `Killed`。不新增检查点，除非某个长循环在 Go 中依赖 `Consume` 的 panic 来中断（这种情况在该块 plan 中登记）。
4. 每个算子自己的 `closeCh/finishCh` 继续存在（关闭语义不同于 kill：比如 Limit 提前结束），映射为算子私有的 `CancelToken`，并作为语句级 token 的子 token。
5. `max_execution_time`：expensivequery 的 100ms 扫描循环 L1 移植，跑在一个后台线程上，超时调用 `Server::kill(id, query=true, max_execution_time=true, …)`。

#### 3.4 错误边界

1. **Go 中"当作控制流"的 panic 一律改成 `Result`**：`Tracker::consume`、`HandleKillSignal`、`PanicOnExceed::action` 都返回 `Result`。调用点加 `?`。Go 的函数签名没有 error 而内部会因 Consume panic 时，Rust 签名加上 `Result`，统一登记为 L2（"panic→Result"）。
2. **`catch_unwind` 只在线程边界**：ExecPool 任务入口（转换成 `GetRecoverError` 等价的错误，经 `on_exit` 送入结果 channel）、语句入口（对应 `ExecStmt.Exec` / `recordSet.Next` 的 recover，保持 "execute sql panic" 日志和 `%v` 形式的报错文本）、连接循环（对应 `clientConn.Run`）。算子内部不写 `catch_unwind`。这些 panic 只来自 bug（越界、`unwrap`），release 构建保持 `panic = "unwind"`。
3. 由于 OOM/kill 不再是 panic，`ExecStmt.Exec` 中"只吞四种错误，其余继续 panic"的分支在 Rust 中退化为：四种错误按普通错误返回；真正的 panic 照 Go 的层次继续上抛到连接层。报错文本与 Go 相同。

#### 3.5 Close 与 Drop

1. **`close()` 保留且必须显式调用**，顺序逐行对应 Go：先取消（对应 `close(closeCh)`）→ drain channel → **join 全部 worker**（比 Go 严格：Go 有时只等 `waiterWg`）→ 注册 runtime stats → 释放磁盘 → 关闭子节点（返回第一个错误）。显式 Close 有可见副作用（EXPLAIN ANALYZE 统计、慢日志），不能只靠 Drop。
2. **Drop 是安全网**：未 Close 就被丢弃（错误路径、unwind）时，Drop 取消并 join worker、删除落盘文件、内存记账随缓冲区释放自动归零；但不注册 stats。
3. "Close 与 Next 并发"在 Rust 中不存在：外部（KILL、内存控制器、`FinishResultSet`）只发信号，Close 永远由持有者线程执行。

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
// go: executor/projection.go (*ProjectionExec).parallelExecute L2 (channel→crossbeam; Consume→自动记账+检查点)
let Some(mut output) = ctx.recv(&self.output_rx)? else { return Ok(()) }; // recv 同时监听 cancel，被 kill 时返回 1317/3024/8175…
ctx.recv(&output.done)?.transpose()?;                                     // worker 错误（含 panic 转换）
chk.swap_columns(&mut output.chk);                                        // 列缓冲带记账句柄，交换时改挂到本算子
self.mem.checkpoint()?;                                                   // 对应 Go 的 Consume：执行超限动作
ctx.send(&self.fetcher_output_tx, output)?;
```

启动 worker（Go：`e.wg.Add(1); go e.workers[i].run(ctx)`）：

```rust
// go: (*ProjectionExec).prepare L2
for w in workers {
    self.handles.push(ctx.pool.spawn_with_recover("projection-worker", move || w.run(), on_panic_send(out_tx.clone())));
}
```

分支、channel 容量、worker 数量、错误传递路径都和 Go 相同；变化只在载体（channel 类型、共享状态放进 `Arc`、panic→Result），统一登记为 L2，并把映射表写进 plan 模板。

### 5. 验收（N3）

| 项目 | 指标 |
|---|---|
| KILL 延迟 | TPC-H SF10 的 Q1、Q9、Q18、Q21 在 CPU 密集阶段执行 `KILL QUERY`，从发出到被杀会话收到 1317 的 p99 ≤ 100ms，且不高于同环境 Go 的实测值 |
| 线程不泄漏 | 测试框架在每条语句结束后断言该语句的 ExecPool 任务数为 0（对应 Go 测试中的 goleak）；sysbench 1 小时后线程数回到基线 ±5% |
| 资源释放 | KILL / 超时 / 内存超限 / worker panic 四种中止路径下，语句结束后该语句的内存记账为 0、落盘文件为 0 |
| 必须通过的 Go 测试 | `executor/test/jointest/hashjoin` `TestKillDuringBuild`、`TestKillDuringProbe`；`executor/test/executor` `TestQueryWithKill`；`executor` `TestKillTableReader`；`executor/test/simpletest` `TestKillStmt`；`session/test/variable` `TestMaxExecutionTime`；`sessionctx/variable` `TestMaxExecutionTime`；`util/sqlkiller` 全部；`server/tests/commontest` `TestClientDisconnectKillsAutocommitInsert`；各算子的 `*RandomFail` / `randomPanic` 类 failpoint 测试（`join` `TestHashJoinRandomFail`、`aggregate` `TestRandomFail`） |
| 并发度语义 | 对每个并发度变量，构造查询断言实际 worker 数与 Go 相同（读 runtime stats 中的 concurrency 字段，与 Go 的 EXPLAIN ANALYZE 对比） |
| 性能 | sysbench oltp_point_select / read_only QPS 不低于当前 PoC 基线（衡量根执行切换到 ExecPool 的开销）；TPC-H 22 条查询总耗时不高于 Go |

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
| 复用池：`chunk.Allocator`（会话级，`maxFreeChunks=64`、每类型 256 列），受 `tidb_enable_reuse_chunk` 控制 | `util/chunk/alloc.go`（202 行）、`pool.go`；`SessionVars.GetChunkAllocator`（session.go L1833） | 由统一缓冲区的复用池取代，变量含义保持 |
| 落盘容器：`RowContainer`、`SortedRowContainer`、`DataInDiskByRows`、`DataInDiskByChunks` | `row_container.go`、`row_in_disk.go`（284）、`chunk_in_disk.go`（289）、`row_container_reader.go`（81） | 落盘文件格式是内部的，可以重设计；对外语义（何时落盘、磁盘列）保持 |
| 用户可见的内存/磁盘数值：EXPLAIN ANALYZE 的 `memory`/`disk` 列 = 语句 tracker 树中 label 等于 plan ID 的节点的 `MaxConsumed`（`FormatBytes` 格式）；慢日志 `Mem_max`/`Disk_max`；statements_summary `AVG_MEM/MAX_MEM/MAX_DISK`；`PROCESSLIST.MEM/DISK`；`MEMORY_USAGE` 表 | `planner/core/common_plans.go` `getRuntimeInfo`（L1116，`SearchTrackerWithoutLock(p.ID())`）、`getRuntimeInfoStr` L1107–1111；`sessionctx/variable/slow_log.go` L176–178；`util/stmtsummary/reader.go` L290–293；`infoschema/tables.go` L865、L1665–1690 | 列、格式、归属算子保持；数值不可能与 Go 逐字节相同（见 3.5） |
| 规模：executor、util/chunk、distsql、store/copr 中共有约 235 处 `Consume(` 调用 | 统计自 v8.5.8 源码 | 每处都要有映射结论 |

### 2. 方案比较

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| a. 照搬估算 | 移植全部 `Consume(delta)` 和估算公式 | 数值最接近 Go | 把 Go 的偏差和漏记一起搬过来；Rust 的数据结构（N5、N6）与 Go 不同，公式本身就不成立；放弃了 A1 的收益 |
| b. 全局分配器钩子 | `#[global_allocator]` 按线程当前语句记账 | 覆盖率最高、对代码零侵入 | 线程间传递的数据（worker 产出、共享缓冲）归属错乱；分配路径里无法执行动作链（日志、锁、落盘都会递归分配）；无法区分算子，EXPLAIN ANALYZE 的按算子列做不出来 |
| c. 带记账的缓冲区 + 保留 Tracker 树和动作链 | 所有大块内存（chunk 列、行容器、哈希表、聚合状态、排序缓冲、copr 响应）都从同一种"记账缓冲区"分配，分配/释放时自动计入所属 tracker；超限动作在检查点执行 | 记的是真实容量；Drop 自动归零；树、标签、动作链、报错都按 L1 保留；落盘可以按缓冲区页整体计算 | 小对象、未改造的分配不计入（需设覆盖率指标）；数值与 Go 不同 |

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
| `chunk.Allocator` / `pool.go` 复用池 | 统一缓冲区的页池（语句级 + 会话级缓存），`tidb_enable_reuse_chunk` 控制是否在会话级缓存 | 例外（A1） |
| RowContainer 的异步落盘与 `RWMutex` | 落盘请求只置标志，由持有者在下一次 `Add`/检查点同步完成；不再需要读写锁 | 例外（A1），动作触发顺序不变 |
| 落盘文件格式（`row_in_disk.go`、`chunk_in_disk.go`） | 以缓冲页为单位顺序写出，可选压缩 | 例外（A1），格式内部 |
| `servermemorylimit` 读 `HeapInuse` | 读 jemalloc 统计（`stats.allocated`，或 `resident`，待定 4） | L3（同源：TiKV 也用 jemalloc 统计） |
| `Tracker.Release` 的 GC finalizer、gctuner、`runtime.GC()` | 无对应物 | 特性例外（待定 3） |

#### 3.3 记账与检查点（核心机制）

1. **记账缓冲区 `TrackedBuf` / 页池**：所有大块内存通过一个实现 `allocator_api2::Allocator` 的 `TrackedAlloc` 分配（`Vec<u8, TrackedAlloc>`、`hashbrown::HashMap<_, _, _, TrackedAlloc>`、bump arena 都能用）。`TrackedAlloc` 持有一个 tracker 叶子节点的句柄，分配/释放时把**实际字节数**记进去。
2. **热路径只做原子加**：分配路径中不执行动作、不打日志、不加锁。每个线程按句柄批量累计，满 64KiB（或释放）时才沿父链传播；越过硬/软限额时只在对应节点上置"已超限"标志。批量误差上界 = 64KiB × 该语句线程数，在验收中计入。
3. **检查点执行动作链**：`mem.checkpoint() -> Result<()>` 检查标志，按 Go `Consume` 的顺序执行：先 `HandleSignal`（kill），再硬限额动作链，再软限额动作链。动作若是 `PanicOnExceed` 就发 `QueryMemoryExceeded` 信号并返回 8175；若是 spill 就置落盘标志；若是 LOG 就打 8001 文本的日志。检查点放在 Go 中正向 `Consume` 的同一位置，再加上 `exec::next` 前后——所以"在哪一步报错"与 Go 相同，只是报错依据的是实际字节数。
4. **归属**：缓冲区记在分配它的 tracker 上；`Chunk::swap_columns`、把 chunk 交给父算子等"所有权转移"时改挂到接收方 tracker（一次原子减一次原子加），对应 Go 在 `SwapColumns` 后 `Consume(delta)` 的惯例，使 EXPLAIN ANALYZE 的按算子数值归属与 Go 一致。
5. **磁盘**：`DiskTracker` 同一棵树，落盘文件写入/删除时记账；文件句柄由 RAII 持有，Drop 删除。

#### 3.4 统一缓冲区与可计算预算

- 统一的最小单位是**页**（如 64KiB，大列单独分配），chunk 列、行容器、哈希表的变长键区、聚合状态 arena、排序缓冲、copr 响应缓冲都从页池取页。
- 一个算子的内存 = 它持有的页数 × 页大小 + 定长结构，可以在 `open` 时按并发度、`tidb_max_chunk_size`、字段类型算出下限（"可计算预算"）。第一阶段只用于观测（写入 runtime stats），不改变 Go 的触发规则。
- 落盘以页或页组为单位，因此"释放多少内存"在落盘前就能算出来；spill action 的 `hasEnoughDataToSpill` 判断可以用精确值。
- 与 N5：从 coprocessor 响应零拷贝解码时，chunk 列借用响应页（`Arc` 共享）。共享页只记账一次，记在 copr/reader 的 tracker 上，最后一个引用释放时归零。这会让一部分内存从 Go 的"消费算子"挪到"读取算子"名下，属于 3.5 中声明的差异。
- 与 N6：哈希表、聚合、排序的结构由 N6 决定，但必须使用 `TrackedAlloc` 和页池；它们的落盘必须通过本 spec 的动作链触发（优先级沿用 Go 的 `DefSpillPriority`）。

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
| 记账覆盖率 | TPC-H SF10 每条查询：峰值时 tracker 根节点 / 该语句线程的 jemalloc 已分配增量 ≥ 0.9 |
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
| N5（值表示与零拷贝） | 共享响应页的记账规则见 3.4；chunk 列的缓冲统一来自页池。 |
| N6（hash agg / join / sort 数据结构） | 必须用 `TrackedAlloc` 和页池；落盘由本 spec 的动作链触发；并发结构（partial/final worker、build/probe worker）沿用本 spec 的 ExecPool 与映射规则。 |
| N7（结果集编码与网络层） | 根执行在 ExecPool、网络写在 tokio 的分工，以及 `FinishResultSet` 的实现，需要 N7 配合。 |

## 第四部分　待定（需要项目负责人决定）

1. **同步 `Next` + ExecPool + 异步 I/O 边界**（第一部分 3.1）是否确认？这是对 PoC 全异步执行的推翻，影响 D07、D12 全部代码形态。
2. **ExecPool 上限策略**：建议"弹性、无死锁硬上限、超高保护上限时报错"。是否接受报错（Go 没有对应行为），还是改为排队（有死锁风险）？
3. **Go runtime 专属变量与列**（`tidb_server_memory_limit_gc_trigger`、`tidb_enable_gogc_tuner`、`tidb_gogc_tuner_*`、`tidb_memory_usage_alarm_*` 的 profile 输出、`MEMORY_USAGE.GC_LAST/GC_TOTAL`）：宪章第五条不允许"接受后静默忽略"。建议作为特性例外：读取返回 Go 默认值；设置为默认值时接受；设置为其他值时接受并返回 warning "has no effect in tidb-rs"；GC 列返回 NULL/0。也可以选择设置非默认值时报错。
4. **实例内存的度量**：`tidb_server_memory_limit` 对比 jemalloc 的 `allocated` 还是 `resident`？前者接近 Go 的 `HeapInuse` 语义，后者更接近 OOM 风险。默认值"总内存 80%"是否保持。
5. **数值类测试的处理规则**（"数值不对照"改写为性质测试、允许重标定配额）是否接受？这会让一批 Go 单测不能原样移植。
6. **优化器 arena、plan cache 等是否纳入语句 tracker**：Go 不记；纳入后 `Mem_max` 会变大，可能更早触发配额。建议只观测、不计入限额。
7. **RowContainer 改为同步落盘**：Go 异步落盘时，读取方可以和落盘并发；改为同步后落盘期间该算子停顿。性能是否需要另设指标？
8. **点查快速路径**：点查是否允许不经 ExecPool、直接在 tokio 任务内以异步方式执行（会让点查执行器成为唯一的异步算子，偏离"同步 Next"的统一规则）？建议先不做，以 sysbench 实测决定。

## 附：我不确定的地方

- 第 1 节的 `go`/`select`/`Consume` 计数是对整个包的 grep，包含写路径、analyze、import 等不在只读范围内的代码，只读范围内的实际数量会更少。
- "Close 只等 `waiterWg` 不等 `workerWg`" 是从 `HashJoinV2Exec.Close` 读出来的；其他算子是否在 Close 后仍有 goroutine 运行，要在各块 plan 中逐个确认。
- KILL 延迟、RSS 70%、开销 2% 这些门槛是初拟值，需要先在 Go 上测基线再定。
