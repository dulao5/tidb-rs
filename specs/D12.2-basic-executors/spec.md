# D12.2 基础算子执行器（spec）

状态：第 1 部分（VS1-35：Selection、LIMIT、Projection、TableDual）评审中（2026-11-24）。块：D12.2（ROADMAP P4），crate `tidb-executor`（N0 rank 14）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（D12.2 一行）、§3.2（边界桩）；[D12.0 tasks](../D12.0-executor/tasks.md)（执行器框架，VS1-13）；[D12.1 spec](../D12.1-read-executors/spec.md)（读取类执行器，本 spec 的格式来源）；[D05.3 spec](../D05.3-session/spec.md) §2.4（`ResetContextOfStmt` 已由 VS1-14 提前移植）；[N3](../exceptions/N3-async-runtime/spec.md)（异步运行时）。

## 1. 范围

Go 范围：`pkg/executor` 的 `select.go`、`projection.go`、`unionexec/` 和 `builder.go` 中相应的 `buildXxx`，tracker 中 D12.2 共 71 个函数。

### 1.1 第 1 部分（VS1-35）

函数清单是 `specs/VS1-vertical-slice/functions.tsv`（与 tracker `data/part_funcs.tsv` 相同）中 VS1-35 的 35 个函数。

| Go | Rust（`crates/executor/src/`） | 函数 |
|---|---|---|
| `select.go` | `select.rs` | `LimitExec`（`Open`、`Next`、`Close`、`adjustRequiredRows`）、`SelectionExec`（`Open`、`open`、`Next`、`unBatchedNext`、`Close`）、`newSelectionExecutorContext`、`TableDualExec`（`Open`、`Next`）、`checkMaxExecutionTimeExceeded`、`globalPanicOnExceed`（`Action`、`GetPriority`）、`setOptionForTopSQL` |
| `projection.go` | `projection.rs`（新文件） | `ProjectionExec`（`Open`、`open`、`Next`、`Close`、`isUnparallelExec`、`unParallelExecute`、`parallelExecute`、`prepare`、`drainInputCh`、`drainOutputCh`）、`newProjectionExecutorContext`、`projectionInputFetcher.run`、`projectionWorker.run`、`readProjection`、`recoveryProjection` |
| `builder.go` | `builder.rs` | `buildLimit`、`buildSelection`、`buildProjection`、`buildTableDual` |

converge：`scripts/converge.py --block D12.2 --part specs/VS1-vertical-slice/functions.tsv --item VS1-35 --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv crates/executor/src/{select,projection,builder}.rs` 为 35/35，没有桩。

清单外顺带移植：`markChildrenUsedCols`（`builder.go`，tracker 中属 D12.1，`buildLimit` 要用，标注 `go: markChildrenUsedCols L1`）。`executorBuilder.build` 的分派（D12.0）增加这四种计划；`ExecStmt.handleNoDelay` 与 `isNoResultPlan`（D12.0）补上 Projection 的 `calculateNoDelay`（DO 语句）分支，原来写的是"D12.2"。

### 1.2 块内其余（`blocked-by:D12.2`）

MAX ONE ROW、UNION ALL（`unionexec/`）、TABLESAMPLE、内存表扫描（`TableScanExec`）、`Start` / `Stop`、`select.go` 的 `init`（安装 `globalPanicOnExceed`，见 §3），以及写路径的函数（`SELECT ... FOR UPDATE`、`SELECT INTO`、UPDATE / DELETE 的语句上下文，永久不移植）。`detach.go` 中 Projection、Selection 的 `Detach`（D12.0 的函数）仍是 `blocked-by:D12.2` 的桩：它们只在游标读（`TryDetach`）时用到，本部分不改。

## 2. 承载方式

1. **执行器不持有会话（N2）。** Go 的 `projectionExecutorContext` / `selectionExecutorContext` 从会话取语句的内存 tracker、求值上下文、`EnableVectorizedExpression` 和运行时统计集合，Rust 在建执行器时取一份（`M-CTX`），之后不读会话。`checkMaxExecutionTimeExceeded` 因此由调用方传入会话变量和进程开始时间（L2）；它的调用方都是加锁读（写路径 X），目前没有调用。
2. **并行投影（`M-GO`、`M-CH`、`M-WG`）。** Go 的 fetcher 与 worker 是 goroutine，通道是带缓冲的 channel，`finishCh` 是关闭即广播的 channel。Rust：
   - fetcher 与 worker 是 tokio task。`ExecCtx.pool`（新字段，N3 的 `ExecPool`）有值时在池上 spawn，否则在当前 runtime 上 `tokio::spawn`。会话目前不传池（`None`），所以都在语句所在的 runtime 上运行；单线程 runtime 上也能运行（单测）。
   - 输入、输出通道是容量与 Go 相同的 `tokio::sync::mpsc`（`numWorkers`、每个 worker 1）；`finishCh` 是 `watch::channel(false)`，关闭即发送 `true`，`readProjection` 用 `select!` 同时等它和数据通道（Go 的 `select` 是随机选择，Rust 偏向先看 finish，结果相同：关闭后不再处理数据）。
   - `projectionOutput` 由 Arc 共享：chunk 在 `parking_lot::Mutex` 中，`done` 是容量 1 的通道。
   - 子执行器：Go 的 fetcher 通过 `proj.Children(0)` 读子执行器，Rust 的子执行器只能有一个所有者，`prepare` 用 `take_all_children`（新的 `ExecutorMeta` 方法，`M-WG`）把它交给 fetcher task，fetcher 退出时把自己连同子执行器作为 task 的返回值交回；`Close` 等所有 task 结束（Go 的 `wg.Wait()`）后用 `set_all_children` 放回，再由 `BaseExecutorV2.Close` 关闭。
   - `Drop`：语句的 future 被取消、执行器没有 `Close` 就被丢弃时，发送 finish 并 abort 所有 task（`M-DEFER`），不会有 task 比执行器活得久。Go 没有这条路径（goroutine 不会被丢弃）。
3. **panic（`M-PANIC`）。** fetcher 与 worker 的 `defer recover()` 是 `catch_unwind`：panic 变成 `Err` 写进输出的 `done`，与 Go 的 `recoveryProjection` 相同，并且 task 照样把子执行器交回。memory tracker 超限时 Go 的动作是 panic，Rust 的 `Consume` 返回 `Err`，按 panic 同样处理（`util.GetRecoverError` 的结果就是这个错误）。`globalPanicOnExceed.Action` 的 `panic(msg)` 同样是返回 `Err(msg)`。
4. **类型断言（`M-IFACE`）。** `ExecStmt.handleNoDelay` 中 `toCheck.(*ProjectionExec)` 是新的 trait 方法 `Executor::as_projection`（默认 `None`，`ProjectionExec` 返回自己）。
5. **计划不可变。** 执行器只读计划（`PhysicalLimit`、`PhysicalSelection`、`PhysicalProjection`、`PhysicalTableDual`），每次执行的状态（`cursor`、`childResult`、`selected`、`inputRow` 等）在执行器中。`executorBuilder.build` 对子计划的递归调用用 `Box::pin`（D12.1 spec §2 第 3 条）。
6. **行迭代器（`M-ROW`）。** `chunk.Iterator4Chunk` 的当前行是行号（`Option<usize>`，`None` 为 `End()`）。

## 3. 不移植或停在边界桩的分支

| 位置 | 情况 | 处理 |
|---|---|---|
| `ProjectionExec.Open`、`SelectionExec.Open`（projection.go:109、select.go:712） | failpoint `mockProjectionExecBaseExecutorOpenReturnedError` 等 | 不移植（测试钩子） |
| `unParallelExecute`、`parallelExecute`、`prepare`、fetcher、worker（projection.go:219、249、283、411、472） | failpoint `ConsumeRandomPanic` | 不移植；panic 路径由单测中会 panic 的子执行器覆盖 |
| `projectionInputFetcher.run`、`projectionWorker.run`（projection.go:382、451） | `trace.StartRegion` | 不移植（Go runtime trace） |
| `LimitExec.Open` / `Close`（select.go:550、568） | opentracing span | 不移植；`Close` 超过 1 ms 的日志保留 |
| `recoveryProjection`（projection.go:488） | 日志带 `zap.Stack` | 日志只有错误，没有栈（`catch_unwind` 拿不到 panic 处的栈） |
| `select.go:init`（select.go:133） | 把 `globalPanicOnExceed` 装到进程级 tracker 上 | 不移植（属 D12.2 其余部分）：进程级内存 tracker 还没有移植（内存控制，A1）。动作本身已移植并有单测 |
| `setOptionForTopSQL`（select.go:1304） | 给快照设置 resource group tagger、pipelined 事务的选项 | 什么都不设置：Top SQL 是 D14.2，ROADMAP §3 决定不记录；tidb-rs 没有 pipelined 事务。点查执行器的 `Open` 现在调用它（D12.1 spec §3 原来写的是"不调用"） |
| `buildPointGet`（point_get.go:62） | 分区被裁剪为空 | 原来是 `blocked-by:D12.2` 的桩，现在返回 0 行的 `TableDualExec`，与 Go 相同（目前到不了，D12.1 spec §3） |

## 4. 与 Go 的其他差异

1. **`ProjectionExec.Close` 不排空 `e.outputCh`**（projection.go:326–345）：Go 只排空 fetcher 与 worker 自己的通道，`e.outputCh`（全局输出）留在原处由 GC 回收；Rust 同样不排空，通道随执行器丢弃。所以没有差别，只是写明 Go 的内存 tracker 在这里同样不归零的那一部分（全局输出中尚未被 `Next` 取走的 chunk）在 Rust 中也不归零，与 Go 一致。
2. **tracker 超限的错误走 panic 路径**（见 §2 第 3 条）：Go 的 `Consume` 在动作中 panic，被 worker / fetcher 的 recover 接住；Rust 的 `Consume` 返回 `Err`，fetcher / worker 把它当作 panic 值写进输出，`Next` 返回的错误相同。
3. **`Drop` 路径**（§2 第 2 条）是 Rust 独有的：Go 的语句没有被"丢弃"的情况。
4. **`ExecCtx.pool` 为 `None`**（§2 第 2 条）：Go 的 goroutine 在 Go runtime 上调度，Rust 的 task 在语句所在的 runtime 上；会话接上 N3 的 `ExecPool` 后改为池上执行，行为不变。
5. **`checkMaxExecutionTimeExceeded` 的签名**（select.go:307）：见 §2 第 1 条。Go 的 `sctx == nil` 对应 `sess_vars` 为 `None`，进程信息为空或开始时间为零值对应 `process_time` 为 `None`。
6. **`isNoResultPlan`**（adapter.go）：Go 对 `*logicalop.LogicalProjection` 也检查 `CalculateNoDelay`，Rust 的计划都是物理计划，只检查 `PhysicalProjection`。

## 5. 待定与可能的上游问题

没有发现 Go 的问题。`LimitExec.adjustRequiredRows` 中 `int(e.end - e.cursor)` 在 `end` 接近 `MaxUint64`（`LIMIT 18446744073709551615`）时为负，`chk.SetRequiredRows` 把非正数改为 `MaxChunkSize`，结果仍然正确；Rust 用回绕运算照样计算（`M-INT`），govec 覆盖这些边界。

## 6. 测试

- **govec 领域 `basicexec`**（tracker `govec/domains/basicexec.go`，4532 条）：Go 的执行器由 `executor.NewMockExecutorBuilderForTest` 从物理计划构建并 Open-Next-Close，LIMIT 和 Selection 的子节点换成 Go `requiredRowsDataSource` 的对应物（每次 Next 返回 `min(requiredRows, 剩余行数)` 行）。
  - `limit`（317 条）：Go 的 `TestLimitRequiredRows`、空数据源、`LIMIT 0`、最大 count、内联投影（输出列是子节点列的子集或换序）、`MaxChunkSize` 为 1 和 7、300 条随机；比较每次 Next 的输出值和数据源每次返回的行数。
  - `selection`（318 条）：Go 的 `TestSelectionRequiredRows`、八种比较函数（`eq ne lt le gt ge nulleq isnull`）与 NULL、没有行通过、全部通过、多个条件、300 条随机；比较同上。
  - `adjust_required_rows`（3885 条）：cursor 在 begin 前、上、后，接近 `MaxUint64` 的 begin / end，requiredRows 越界；直接比较调整后的 requiredRows。
  - `table_dual`（12 条）：0 行、1 行（没有列时是一个虚拟行）、构建器对其他行数的报错。
  - 回放：`crates/executor/src/govec_basicexec_test.rs`（crate 内的测试模块，要用 `pub(crate)` 字段），同样由 `ExecutorBuilder` 构建再换子节点，全部一致。向量 `crates/executor/tests/vectors/v8.5.8/{amd64,arm64}/basicexec.jsonl.gz` 取自 tracker CI 的产物；两个目标的记录相同（只有整数运算）。
- **Go 单测的移植**（`executor_required_rows_test.rs`）：`TestLimitRequiredRows`、`TestSelectionRequiredRows`、`TestProjectionUnparallelRequiredRows`、`TestProjectionParallelRequiredRows`（Go 中 `t.Skip`，这里 `#[ignore]`，手动运行通过）。
- **其他单测**（`basic_executors_test.rs`）：
  - 并行投影：输出保持子节点的顺序，结束后内存 tracker 归零；读完之前 `Close`；子执行器 panic 变成错误且子执行器照样被关闭；子执行器报错；没有 `Close` 就丢弃时 task 停止；在单线程 runtime 上运行；构建器按估计行数（不足一个 chunk 为 0 个 worker）和加锁语句决定 worker 数。
  - `SelectionExec.unBatchedNext`（govec 豁免：只有不可向量化的条件才走，即用户变量赋值，D08.9）：每次 Next 只返回一行。
  - `checkMaxExecutionTimeExceeded`（govec 豁免：依赖时钟）：没有会话、不是 SELECT、没有进程信息、开始时间在未来、未超时、`max_execution_time = 0` 都不报错，超时报 `ErrMaxExecTimeExceeded`。
  - `globalPanicOnExceed`：按 tracker 标签返回的消息、优先级。
  - TableDual 的行数、LIMIT 的内联投影、`markChildrenUsedCols`、构建器的四种计划与 TableDual 的报错。
- **会话测试的改动**（`crates/session`）：原来用 `SELECT 1`（Projection over TableDual）和 TableDual 的 mock 计划作为"执行器构建器的边界"的例子，现在它们能执行；改用 `SELECT 1 FROM DUAL ORDER BY RAND()`（Sort，`blocked-by:D12.5`）和 Sort 的 mock 计划。参数测试中 `id > NULL` 被折叠为 TableDual，现在返回空结果集。
- ab：`SELECT 1`、`SELECT ... LIMIT`、带条件的子查询等不读表的语句现在由新栈执行；按 CLAUDE.md §3，端到端比较由维护者在合并后运行（`tools/ab`）。
