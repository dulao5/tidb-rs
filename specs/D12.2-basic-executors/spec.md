# D12.2 基础算子执行器（spec）

状态：第 1 部分（VS1-35：Selection、LIMIT、Projection、TableDual）重新评审中（2026-11-24）：第一次移植（#338）合并后因三个回归被撤回（#345），回归的原因与修复见 §7。块：D12.2（ROADMAP P4），crate `tidb-executor`（N0 rank 14）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（D12.2 一行）、§3.2（边界桩）；[D12.0 tasks](../D12.0-executor/tasks.md)（执行器框架，VS1-13）；[D12.1 spec](../D12.1-read-executors/spec.md)（读取类执行器，本 spec 的格式来源）；[D05.3 spec](../D05.3-session/spec.md) §2.4（`ResetContextOfStmt` 已由 VS1-14 提前移植）；[N3](../exceptions/N3-async-runtime/spec.md)（异步运行时）。

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

清单外顺带移植：`markChildrenUsedCols`（`builder.go`，tracker 中属 D12.1，`buildLimit` 要用，标注 `go: markChildrenUsedCols L1`）。`executorBuilder.build` 的分派（D12.0）增加这四种计划；`ExecStmt.handleNoDelay` 与 `isNoResultPlan`（D12.0）补上 Projection 的 `calculateNoDelay`（DO 语句）分支，原来写的是"D12.2"。这两个分支已照 Go 移植，但目前不可达：tidb-rs 的 AST 还没有 DO 语句，planner 也没有地方把 `calculate_no_delay` 设为 true；DO 语句进入新栈之前没有测试。

### 1.2 块内其余（`blocked-by:D12.2`）

MAX ONE ROW、UNION ALL（`unionexec/`）、TABLESAMPLE、内存表扫描（`TableScanExec`）、`Start` / `Stop`、`select.go` 的 `init`（安装 `globalPanicOnExceed`，见 §3），以及写路径的函数（`SELECT ... FOR UPDATE`、`SELECT INTO`、UPDATE / DELETE 的语句上下文，永久不移植）。`detach.go` 中 Projection、Selection 的 `Detach`（D12.0 的函数）仍是 `blocked-by:D12.2` 的桩：它们只在游标读（`TryDetach`）时用到，本部分不改。

## 2. 承载方式

1. **执行器不持有会话（N2）。** Go 的 `projectionExecutorContext` / `selectionExecutorContext` 从会话取语句的内存 tracker、求值上下文、`EnableVectorizedExpression` 和运行时统计集合，Rust 在建执行器时取一份（`M-CTX`），之后不读会话。`checkMaxExecutionTimeExceeded` 因此由调用方传入会话变量和进程开始时间（L2）；它的调用方都是加锁读（写路径 X），目前没有调用。
2. **并行投影（`M-GO`、`M-CH`、`M-WG`）。** Go 的 fetcher 与 worker 是 goroutine，通道是带缓冲的 channel，`finishCh` 是关闭即广播的 channel。Rust：
   - fetcher 与 worker 是 tokio task。`ExecCtx.pool`（新字段，N3 的 `ExecPool`）有值时在池上 spawn，否则在当前 runtime 上 `tokio::spawn`。会话目前不传池（`None`），所以都在语句所在的 runtime 上运行；单线程 runtime 上也能运行（单测）。`tokio::spawn` 要求当前线程在 tokio runtime 中：在 runtime 之外（例如会话单测用的 `futures::executor::block_on`）走到并行路径会 panic。会话单测的行数估计都不到一个 chunk（0 个 worker，串行），不会走到；生产路径的语句总在 runtime 中执行。
   - 输入、输出通道是容量与 Go 相同的 `tokio::sync::mpsc`（`numWorkers`、每个 worker 1）；`finishCh` 是 `watch::channel(false)`，关闭即发送 `true`，`readProjection` 用 `select!` 同时等它和数据通道；`tokio::select!` 没有 `biased;`，两边都就绪时随机选择，与 Go 的 `select` 相同，关闭后不再处理数据。
   - `projectionOutput` 由 Arc 共享：chunk 在 `parking_lot::Mutex` 中，`done` 是容量 1 的通道。
   - 子执行器：Go 的 fetcher 通过 `proj.Children(0)` 读子执行器，Rust 的子执行器只能有一个所有者，`prepare` 用 `take_all_children`（新的 `ExecutorMeta` 方法，`M-WG`）把它交给 fetcher task，fetcher 退出时把自己连同子执行器作为 task 的返回值交回；`Close` 等所有 task 结束（Go 的 `wg.Wait()`）后用 `set_all_children` 放回，再由 `BaseExecutorV2.Close` 关闭。
   - `Drop`：语句的 future 被取消、执行器没有 `Close` 就被丢弃时，发送 finish 并 abort 所有 task（`M-DEFER`），不会有 task 比执行器活得久。Go 没有这条路径（goroutine 不会被丢弃）。`Close` 等 task 时，每个 handle 在它的 task 结束之后才从执行器中移走，所以 `Close` 的 future 中途被丢弃时，还在运行的 task 仍由 `Drop` abort。task 没有交回结果就结束（在 `catch_unwind` 之外 panic，或 runtime 正在关闭）时记一条 warn 日志，fetcher 手里的子执行器此时无法关闭；Go 的 goroutine 不会这样结束。
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
7. **预处理语句的列定义不沿用第一次 EXECUTE 的（待负责人裁决，#367，upstream-pitfall）**：Go 的连接把预处理语句第一次 EXECUTE 的列定义存进 `PointGet.ColumnInfos`，之后每次 EXECUTE 都沿用（`pkg/server/internal/resultset/resultset.go:108-131`，只在 schema 版本变化时清空，`pkg/planner/core/plan_cache.go:151`），即使后来的 EXECUTE 绑定的参数类型不同；`DumpBinaryRow` 按 `GetUint64` 写 `TypeLonglong` 的 8 个字节而不看符号（`pkg/server/internal/column/column.go:252-253`）。于是把参数选作列时，先用有符号整数、再用无符号 64 位的 `18446744073709551615` 执行，Go 仍描述为有符号 `BIGINT`（flags 0x0081），客户端读到 -1；先用整数、再用字符串执行，字符串按 `BIGINT` 写出，是乱码。tidb-rs 的 server 不移植这个缓存（`crates/server/src/resultset.rs` `ResultSet::columns`，L2），每次 EXECUTE 按自己的参数类型描述列（无符号时 flags 0x00a1），与 Go 对一条语句第一次 EXECUTE 的输出相同。这是错误结果，按 CONSTITUTION 第 1 条第 2 项不默认照搬，**保持现状，等负责人裁决**（没有写入 `docs/testing/rulings.toml`）。这个差别在本块之前就存在，基础算子让这些预处理语句（投影、排序）改走新栈之后，维护者的端到端比较才看到。会话测试 `prepared_unsigned_parameter_selected_as_a_column` 固定当前行为。

## 5. 待定与可能的上游问题

没有发现 Go 的问题。`LimitExec.adjustRequiredRows` 中 `int(e.end - e.cursor)` 在 `end` 接近 `MaxUint64`（`LIMIT 18446744073709551615`）时为负，`chk.SetRequiredRows` 把非正数改为 `MaxChunkSize`，结果仍然正确；Rust 用回绕运算照样计算（`M-INT`），govec 覆盖这些边界。

## 6. 测试

- **govec 领域 `basicexec`**（tracker `govec/domains/basicexec.go`，4532 条）：Go 的执行器由 `executor.NewMockExecutorBuilderForTest` 从物理计划构建并 Open-Next-Close，LIMIT 和 Selection 的子节点换成 Go `requiredRowsDataSource` 的对应物（每次 Next 返回 `min(requiredRows, 剩余行数)` 行）。
  - `limit`（317 条）：Go 的 `TestLimitRequiredRows`、空数据源、`LIMIT 0`、最大 count、内联投影（输出列是子节点列的子集或换序）、`MaxChunkSize` 为 1 和 7、300 条随机；比较每次 Next 的输出值和数据源每次返回的行数。
  - `selection`（318 条）：Go 的 `TestSelectionRequiredRows`、八种比较函数（`eq ne lt le gt ge nulleq isnull`）与 NULL、没有行通过、全部通过、多个条件、300 条随机；比较同上。
  - `adjust_required_rows`（3885 条）：cursor 在 begin 前、上、后，接近 `MaxUint64` 的 begin / end，requiredRows 越界；直接比较调整后的 requiredRows。
  - `table_dual`（12 条）：0 行、1 行（没有列时是一个虚拟行）、构建器对其他行数的报错。
  - 回放：`crates/executor/src/govec_basicexec_test.rs`（crate 内的测试模块，要用 `pub(crate)` 字段），同样由 `ExecutorBuilder` 构建再换子节点，全部一致。向量 `crates/executor/tests/vectors/v8.5.8/{amd64,arm64}/basicexec.jsonl.gz` 与 tracker 中的文件相同。作者的环境下载不到 tracker CI 的产物：amd64 文件在本地 x86_64 上生成，arm64 文件由为 linux/arm64 编译的同一个生成器在 qemu-user 下生成；tracker CI 在两个架构的 runner 上各自重新生成，用 `scripts/govec.sh --check` 逐字节核对已提交的文件。两个目标的记录相同（只有整数运算）。
- **Go 单测的移植**（`executor_required_rows_test.rs`）：`TestLimitRequiredRows`、`TestSelectionRequiredRows`、`TestProjectionUnparallelRequiredRows`、`TestProjectionParallelRequiredRows`（Go 中 `t.Skip`，这里 `#[ignore]`，手动运行通过）。
- **其他单测**（`basic_executors_test.rs`）：
  - 并行投影：输出保持子节点的顺序，结束后内存 tracker 归零；读完之前 `Close`；子执行器 panic 变成错误且子执行器照样被关闭；子执行器报错；没有 `Close` 就丢弃时 task 停止；在单线程 runtime 上运行；构建器按估计行数（不足一个 chunk 为 0 个 worker）和加锁语句决定 worker 数。
  - `SelectionExec.unBatchedNext`（govec 豁免：只有不可向量化的条件才走，即用户变量赋值，D08.9）：每次 Next 只返回一行。
  - `checkMaxExecutionTimeExceeded`（govec 豁免：依赖时钟）：没有会话、不是 SELECT、没有进程信息、开始时间在未来、未超时、`max_execution_time = 0` 都不报错，超时报 `ErrMaxExecTimeExceeded`。
  - `globalPanicOnExceed`：按 tracker 标签返回的消息、优先级。
  - TableDual 的行数、LIMIT 的内联投影、`markChildrenUsedCols`、构建器的四种计划与 TableDual 的报错。
- **会话测试**（`crates/session`）：
  - 正向：`SELECT 1` 经 `execute_stmt` 返回一行 1，`SELECT 1 LIMIT 0` 和 `SELECT 1 FROM DUAL WHERE 1 = 0` 返回 0 行；三者都使 `executed_stmts()` 加 1，读完并 close 之后会话空闲（`statements_without_a_table_run_on_the_basic_executors`）。参数测试中 `id > NULL` 被折叠为 TableDual，读出来是 0 行。
  - 原来用 `SELECT 1`（Projection over TableDual）和 TableDual 的 mock 计划作为"执行器构建器的边界"的例子，现在它们能执行。排序执行器（D12.5）的并行 sort 也已合入，`SELECT 1 FROM DUAL ORDER BY RAND()` 同样能执行；回退点的例子改用 sysbench 表上的范围扫描 `SELECT c FROM sbtest1 WHERE id BETWEEN 1 AND 10`（TableReader，`blocked-by:D12.1`，读取类执行器），"只计打开过的执行器"的 mock 计划改用 NominalSort（执行器框架 D12.0 的桩）。
  - 预处理语句中 `id = ? AND id = ?` 取不同值时是空范围（Go：TableDual，不进计划缓存），原来停在 `blocked-by:D12.2`，现在执行，读出 0 行。
  - 会话的集成测试 `crates/session/tests/checkpoint.rs` 原来也用 `SELECT 1 FROM DUAL ORDER BY RAND()` 作为编译之后的回退例子，现在改用 `SELECT COUNT(1)`（在规划器的聚合处回退，`blocked-by:D09.4`）。
  - **排序与 top-n**（与并行 sort #362 一起合入 main 之后）：`sort_and_top_n_over_dual_and_point_get`（文本协议）与 `prepared_sort_and_top_n_over_dual_and_point_get`（PREPARE / EXECUTE，计划缓存开启）比较 dual 上与点查上的 `ORDER BY`（含 `ORDER BY RAND()`）和 `ORDER BY ... LIMIT`（TopN）的行、列元数据和 `found_in_plan_cache`，期望值同样取自本地 Go v8.5.8 服务器（计划形状由 `EXPLAIN` 核对，写在测试注释中）。并行 sort 的 worker 是 task，这两个测试在多线程 tokio runtime 上运行。输入最多一行，`ORDER BY RAND()` 只比较行和列，不涉及顺序。`LIMIT ?` 在 PREPARE 处回退（`blocked-by:D13.1`，计划缓存的参数），测试断言回退。
  - **与 Go 逐列比较结果和列元数据**（`crates/session/src/tests/sbtest_results.rs`，见 §7）：表 `sbtest1` 的 TableInfo 取自 Go v8.5.8（`crates/session/src/tests/sbtest1_table_info.json`，v8.5.8 服务器状态端口 `/schema/test/sbtest1` 返回的值），期望的行与列元数据（名字、类型、flen、decimal、flag、字符集）都在 Go v8.5.8 上用同样的语句跑出来。
- ab：`SELECT 1`、`SELECT ... LIMIT`、点查上的投影等语句现在由新栈执行。按 [agent-workflow](../../docs/process/agent-workflow.md)，把语句从回退移到执行的 PR 要在合并前由维护者跑 `tools/ab`；第一次移植正是合并后由 ab 发现回归的。

## 7. 第一次移植的回归（#338 撤回的原因）

维护者在 #338 合并后的端到端比较中发现三个问题，#345 撤回。这一次执行器的代码与 #338 相同（执行器本身没有错；构建器的边界只随 main 上已合入的排序执行器 D12.5 调整），三个问题的根源都在执行器之外，已分别在 main 上修好；本 PR 加会话层的测试，使它们不会再回来。

| 现象 | 根源 | 修复 | 测试（`sbtest_results.rs`） |
|---|---|---|---|
| `SELECT c FROM sbtest1 WHERE id = 1` 在 `GoSlice::i64_at` panic | 投影的列引用没有经过投影自己的 `ResolveIndices`（物理计划后处理 D10.9 中的列下标解析），下标都停在 0：把整数列 `id`（定长、没有 offsets）当字符串列 `c` 读。投影执行器打开之后的 panic 无法回退 | #351（`PhysicalProjection.ResolveIndices`） | `projection_over_point_get_reads_its_columns`、`prepared_…`、`parallel_…`（多线程 runtime，行 id 取 `i64::MIN`，投影走并行路径） |
| 预处理的 `BETWEEN ? AND ?` 结果错 | 参数标记的序号都是 0（#352），每个标记都读第一个参数 | #354；在 main 上范围扫描在任何执行器打开之前就停在 TableReader（读取类执行器 D12.1）回退 | `ranges_fall_back_before_any_executor_opened`；`prepared_statement_markers_read_their_own_parameters`（`tests.rs`） |
| 表达式列的列名为空（如 `SELECT 1 + 1`） | 规划器拿不到语句文本，`SelectField.Text()`（Go `pkg/planner/core/logical_plan_builder.go:1462`）为空 | #364（语句文本经编译、计划缓存和 PREPARE 各条路径到达规划器） | `record_sets_name_expression_columns_by_their_text`：文本协议、PREPARE、EXECUTE 未命中与命中（`found_in_plan_cache` 依次为假、真），比较记录集的列名、行，以及 EXECUTE 与 PREPARE 的列相同；含评审补充的 `  /* x */ SELECT 1 + 1`（列名 `1 + 1`） |

每个测试都在模拟旧错误时失败（列下标固定为 0、标记序号固定为 0、语句文本为空），修复后通过。

期望值是同一台 v8.5.8 服务器（unistore，同样的表和行）对每条语句（文本协议、`COM_STMT_PREPARE`、`COM_STMT_EXECUTE`）发出的列定义。测试比较的是会话的结果列（`ResultField`），线协议上的列定义是它们经 `ConvertColumnInfo`（`crates/server`）转换的结果：字符串列的长度是 flen × 4（utf8mb4），decimal 是 flen + 1（有小数再 + 1），flen 未指定时取类型的默认长度，decimal 未指定时为 31（`NotFixedDec`）；期望值按这些规则从 Go 发出的定义反推。

