# D12.3 聚合执行器框架（hash agg / stream agg）：spec

状态：已确认（2026-11-25），只写 spec（#335）。第 8 节记录各决定点的结论：全部按本 spec 的建议，由维护者按宪章第九条（谁来决定，#374）决定。下一步按 §2 的部分拆成实现任务。第 A 部分（StreamAgg 与 vecgroupchecker）由 #376 实现，实现中新增的差异见 §6 第 16-21 条；第 B 部分（单线程与并行 HashAgg）由 #382（PR #383）实现，实现中新增的差异见 §6 第 22-26 条。

块：D12.3（ROADMAP P4 的第 2 层；Go 约 1,959 行，8 个文件），crate `tidb-executor`（N0 rank 14）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/executor/aggregate/`，另有说明的除外。

相关：[D12.0 tasks](../D12.0-executor/tasks.md)（执行器框架）、[D12.1 spec](../D12.1-read-executors/spec.md)（读取类执行器，写法照它）、D12.2 spec（基础算子，`specs/D12.2-basic-executors/spec.md`，在 PR #338 中评审；§2 第 2–3 条是并行投影的承载方式，本块照它）、[N3](../exceptions/N3-A1-concurrency-memory/spec.md)（并发、取消、内存，含 A1、A2）、[mappings.md](../../docs/porting/mappings.md)。同批的兄弟 spec：聚合函数描述 D08.11（`specs/D08.11-aggregation/spec.md`，PR #341）、聚合相关规则 D09.9（`specs/D09.9-agg-rules/spec.md`，PR #340）、聚合函数实现 D12.4（#336，另一个 worker 在写）。

## 1. 这个块在 Go 里做什么

### 1.1 内容

`pkg/executor/aggregate` 是两个聚合执行器和它们的辅助代码。真正"怎么算 SUM / COUNT"的是 D12.4 的 `aggfuncs.AggFunc`：本块负责分组、调度、内存记账和落盘，对每一组调用 `AllocPartialResult` / `UpdatePartialResult` / `MergePartialResult` / `AppendFinalResult2Chunk`。

| Go 文件 | 行数 | 内容 |
|---|--:|---|
| `agg_stream_executor.go` | 253 | `StreamAggExec`：输入已按分组键排好序，用 `VecGroupChecker` 切出相邻的同组行，一组算完就输出一行 |
| `agg_hash_executor.go` | 886 | `HashAggExec`：单线程（`unparallelExec`）和并行（`parallelExec`：1 个 fetcher、P 个 partial worker、F 个 final worker）两种执行方式；`Open` / `Next` / `Close`；单线程方式的落盘模式 |
| `agg_hash_partial_worker.go` | 385 | partial worker：对一个 chunk 算分组键，按 murmur3 哈希把每组分给一个 final worker，更新部分结果；结束时把 F 个部分结果表各发给对应的 final worker；落盘 |
| `agg_hash_final_worker.go` | 265 | final worker：合并来自各 partial worker 的部分结果表，输出最终结果；落盘后按分区从磁盘恢复 |
| `agg_hash_base_worker.go` | 57 | 两种 worker 的公共字段 |
| `agg_spill.go` | 382 | 并行方式的落盘状态机 `parallelHashAggSpillHelper`（256 个分区、恢复、合并），两个内存超限动作 `AggSpillDiskAction`（单线程）和 `ParallelAggSpillDiskAction`（并行） |
| `agg_util.go` | 312 | `GetGroupKey`（把分组表达式求值并编码成字节串键，shuffle 也用）、运行时统计 `HashAggRuntimeStats`、缓冲区池、panic 恢复、计时 |
| `pkg/executor/builder.go:2014-2136` | — | `buildHashAgg` / `buildHashAggFromChildExec`、`buildStreamAgg` / `buildStreamAggFromChildExec`：从物理计划建执行器，决定单线程还是并行、拆 partial / final 描述、空输入时的默认行 |

### 1.2 执行流程要点（逐条与源码核对过）

**StreamAgg**（`agg_stream_executor.go`）：

1. `OpenSelf`（:79）为每个聚合函数分配一份部分结果，记下初始内存 `memUsageOfInitialPartialResult`，`TrackAggregateMemoryUsage` 为真时把 tracker 挂到语句的 tracker 下。
2. `Next`（:118）循环 `consumeOneGroup` 直到输出 chunk 满或结束。一组可能跨多个子 chunk（:148-166 的 `meetLastGroup` 循环）。
3. 每组算完 `appendResult2Chunk`（:236）：输出、`ResetPartialResult`、把 tracker 重置为"子 chunk + 初始部分结果"（:248）。组内的内存增量先攒在 `pendingMemDelta`，超过 1 KiB 才 `Consume`（:191-198）。
4. 子节点没有任何行时：不是"全部 FIRST_ROW"、没有 GROUP BY 且是 final 模式的聚合，输出 `DefaultVal`（builder.go:2117-2131，每个函数的 `GetDefaultValue`，如 `COUNT` 为 0、`SUM` 为 NULL）。
5. 没有聚合函数（`SELECT DISTINCT` 只剩分组时也有 FIRST_ROW，这里指真的为 0 个）时输出虚拟行（:249-251）。

**HashAgg 单线程**（`unparallelExec`，:675）：子节点 → `GetGroupKey` → `groupSet`（`map[string]struct{}`）和 `partialResultMap`（`map[string][]PartialResult`）；读完后按 `groupKeys`（首次出现的顺序）输出，所以**输出顺序是确定的**（:680-682 的注释）。没有 GROUP BY 也没有数据时插入空键 `""` 这一组（:705-712）。落盘模式（`inSpillMode`，由 `AggSpillDiskAction` 置位）下，新组的行写到 `dataInDisk`，本轮输出后再从磁盘读回、开始下一轮（`resetSpillMode`，:717）。

**HashAgg 并行**（`parallelExec`，:635；流程图 :55-92）：

- 第一次 `Next` 时 `prepare4ParallelExec`（:580）启动 fetcher、P 个 partial worker、F 个 final worker，以及三个辅助 goroutine（关闭通道、记录 wall time）。
- fetcher（`fetchChildData`，:449）从 `inputCh` 取一个空 chunk（`HashAggInput`，自带"还给哪个 partial worker"的 `giveBackCh`），读子节点填满，发给那个 partial worker；空 chunk 的数量等于 P，起到限流的作用（:340-345）。
- partial worker（`fetchChunkAndProcess`，partial_worker.go:87）把 chunk 换进自己的 `chk`、立即把空 chunk 还回 `inputCh`，再更新部分结果；结束时（`finalizeWorkerProcess`，:171）把第 j 个部分结果表发给第 j 个 final worker（`shuffleIntermData`，:287）。
- final worker（`run`，final_worker.go:207）**先等所有 partial worker 结束**（:211），再合并收到的 P 个表（`mergeInputIntoResultMap`，:73），然后遍历自己的表输出（`generateResultAndSend`，:121）。遍历的是 Go map，所以**并行方式的输出顺序不确定**。
- 主线程从 `finalOutputCh` 读结果，`SwapColumns` 后把 chunk 还给 final worker（:662-666）。通道关闭时，子节点从未返回过行且有 `DefaultVal`，就输出默认行（:652-657）。
- 任何 worker 的错误或 panic 都写进 `finalOutputCh`，`Next` 返回第一个错误。

**落盘**（`agg_spill.go`）：内存超过会话 tracker 的软限额时，动作链调用本块的动作。并行方式：动作只置 `needSpill`（:191），fetcher 在下一次发出 chunk 后看到它（`spillIfNeed`，agg_hash_executor.go:527），等在途的 chunk 处理完，让每个 partial worker 把部分结果序列化写到 256 个分区文件（partial_worker.go:316）；所有数据读完后 final worker 按分区逐个读回、合并、输出（final_worker.go:157-174）。最多落盘 10 次（`maxSpillTimes`，:43，单线程方式用）。

### 1.3 调用方（tracker `data/block_edges.tsv`，以及读代码）

| 调用方 | 引用 | 用到什么 |
|---|--:|---|
| D12.0 执行器框架 | 2 | `executorBuilder.build` 分派 `PhysicalHashAgg` / `PhysicalStreamAgg`（builder.go:278-281） |
| D12.7 index join 系列 | 4 | `dataReaderBuilder.buildExecutorForIndexJoinInternal` 在内表侧建聚合执行器后直接调 `OpenSelf`（builder.go:4655-4670） |
| D12.6 hash join v1（其中的 Apply） | 2 | `aggExecutorTreeInputEmpty` 对 `*HashAggExec` / `*StreamAggExec` 做类型断言读 `IsChildReturnEmpty`（join/hash_join_v1.go:1259-1264） |
| D12.10 shuffle | 1 | `partitionHashSplitter.split` 调 `GetGroupKey`（shuffle.go:438） |

sysbench `oltp_read_only` 的两条聚合语句：`SELECT SUM(k) ... WHERE id BETWEEN ? AND ?`（没有 GROUP BY，根上通常是 final 模式的 StreamAgg，partial 部分下推给 TiKV）和 `SELECT DISTINCT c ... ORDER BY c`（按 `c` 分组的 FIRST_ROW，根上通常是 final 模式的 HashAgg，按默认并发度 5 / 5 **走并行方式**）。所以 sysbench 链路需要第 A、B 两部分（§2）；具体计划形状以 EXPLAIN 为准，见 PR 的 "Not checked"。

### 1.4 依赖

已移植、可以直接用的：

| 块 | 用到什么 |
|---|---|
| D12.0 执行器框架 | `Executor` trait、`BaseExecutor`、`exec::next`（带 KILL 检查）、`TryNewCacheChunk`、`RetTypes`、`new_chunk_with_capacity` |
| D01.7 chunk | `Chunk`（`SwapColumns`、`SetNumVirtualRows`、`Append`、`AppendDatum`、`AppendRow`、`AppendString`、`SetSel`、`IsFull`、`MemoryUsage`）、`Iterator4Chunk` |
| D01.8 codec | `HashGroupKey`（`crates/codec/src/codec.rs`） |
| D15.1 内存（A1） | `Tracker`（`consume` 返回 `Result`、`replace_bytes_used`、`attach_to`、`reset`、`fallback_old_and_set_new_action_for_soft_limit`）、`ActionOnExceed`、`DefSpillPriority`、`QueryForceDisk`、`LabelForChunkDataInDiskByChunks`；`disk.Tracker` 在 Go 中就是 `memory.Tracker` |
| D14.0 执行统计 | `RuntimeStats` trait、`TpHashAggRuntimeStat`、`RuntimeStatsColl::register_stats` |
| D05.1 / D05.2 变量 | `hash_agg_partial_concurrency` / `hash_agg_final_concurrency`（未设置时取 `tidb_executor_concurrency`，默认 5）、`TrackAggregateMemoryUsage`（默认 ON）、`EnableParallelHashaggSpill`（默认 ON）、`EnableVectorizedExpression`、全局 `EnableTmpStorageOnOOM`（默认 ON） |
| D08.1 表达式 | `EvalExpr`（`GetGroupKey` 用） |
| D00.1 上下文 | `ErrCtx::handle_error`、`EvalContext`（`Send + Sync`，可以在 task 间共享） |
| N3 运行时 | `ExecPool::spawn` / `spawn_with_recover`、`checkpoint`、`CheckpointStride` |

未移植的，写成 `blocked-by` 边界或要求先合并：

| 块 / 设施 | 缺什么 | 影响与处理 |
|---|---|---|
| D12.4 聚合函数实现（#336） | `AggFunc` trait、`aggfuncs.Build`、`PartialResult`、`AggPartialResultMapper`、`SerializeHelper`（只有第 C 部分用）、group_concat 的 `SetTruncated` / `GetTruncated` | **硬依赖**：没有它两个执行器都无法计算。第 A、B 部分的实现排在 D12.4 中 count / sum / avg / max / min / first_row 那一部分之后。对 D12.4 的要求见 §4.10 |
| D08.11 聚合函数描述（PR #341） | `AggFuncDesc`（`Name`、`HasDistinct`、`OrderByItems`、`Split`、`GetDefaultValue`）、`IsAllFirstRow` | 硬依赖（只读）。计划节点按值还是按 `Arc` 持有描述（D08.11 决定点 4）**本块不需要选**：执行器只通过 `&AggFuncDesc` 读，`Split` 返回新的描述，两种表示下写法相同；跟随负责人对 D08.11 决定点 4 的结论 |
| D10.1 物理算子 | `PhysicalHashAgg` / `PhysicalStreamAgg`（`GroupByItems`、`AggFuncs`、`IsFinalAgg`） | 4 个 builder 函数要它们。D10.1 的聚合算子移植之前，builder 分派仍停在现有的 `blocked-by` 桩（改成指向 D10.1）；执行器本身的测试直接构造执行器，不经过 builder（Go 的 `agg_spill_test.go:194` 就是这样做的） |
| D12.0 的 `vecgroupchecker`（8 个函数，未移植） | `VecGroupChecker` | StreamAgg 要它（builder.go:2113）。随第 A 部分一起移植、计入 D12.0（决定点 3） |
| D12.2 第 1 部分（PR #338） | `ExecCtx.pool`、`take_all_children` / `set_all_children`、并行执行器的固定写法（watch 通道作 finish、`Drop` 时 abort） | 第 B 部分照它写，所以排在 #338 合并之后 |
| D01.10 落盘容器 | `chunk.DataInDiskByChunks`（`chunk_in_disk.go`，23 个函数） | 落盘全部停在 `blocked-by:D01.10`（§3、决定点 4、5）。单线程方式在 `Open` 时无条件 `NewDataInDiskByChunks`（agg_hash_executor.go:283）、`resetSpillMode` 读 `NumChunks`（:723）、`Close` 调 `Close`（:177），这几个方法在没有文件时不做 I/O |
| D00.3 工具文件 | `util/set/set_with_memory_usage.go`（`StringSetWithMemoryUsage`）、`util/hack/map_abi.go`（`MemAwareMap`，读 Go runtime 的 swiss map 内部结构估算内存）、`util/channel/channel.go`（`Clear`） | 按宪章第四条 4 整文件移植，或按映射规则替换；`MemAwareMap` 的处理见决定点 8 |
| A1 在会话中的接入 | 会话的内存 tracker（`vars.MemTracker`）、语句 tracker 挂到会话 tracker 下、`tidb_mem_quota_query` 限额 | 现在新栈没有会话 tracker（`crates/executor/src/select.rs:103` 的注释），所以软限额动作不会被触发。处理见决定点 6 |
| 第三方模块 | `github.com/twmb/murmur3` v1.1.6 的 `Sum32` | 按 `go-dep` 移植（约 40 行），partial worker 用它把组分给 final worker（partial_worker.go:226），落盘用它分区（:339） |
| D14.1 指标 | `ExecutorCounterHashAggExec`、`ExecutorStreamAggExec`（builder.go:2097、2134） | 按 D12.0 已有的 observer 写法加两个计数器，D14.1 有向量之前不记录 |

## 2. 函数清单的划分

函数清单是 tracker `data/func_blocks.tsv` 中 block 为 `D12.3` 的 107 个函数。按依赖分成三部分加一组归 X：

| 部分 | 函数数 | 内容 | 依赖 |
|---|--:|---|---|
| A | 11 | StreamAgg 与它的 builder | D12.4（基本函数）、D08.11、D12.0 的 vecgroupchecker（随本部分移植）；builder 另要 D10.1 |
| B | 74 | HashAgg 单线程和并行方式，不含落盘；落盘触发的状态机照移植，真正写盘的地方停在 `blocked-by:D01.10` | 第 A 部分的依赖，加 D12.2 第 1 部分（#338） |
| C | 15 | 并行方式的落盘与恢复 | D01.10 |
| X | 7 | 只在测试中有作用的钩子 | — |

第 B 部分较大，但作为一个任务移植，不再拆成单线程和并行两部分（决定点 1）。

### 2.1 第 A 部分（11）

- `agg_stream_executor.go`：`StreamAggExec.Open`、`OpenSelf`、`Close`、`Next`、`consumeOneGroup`、`consumeGroupRows`、`consumeCurGroupRowsAndFetchChild`、`appendResult2Chunk`（8）
- `agg_util.go`：`closeBaseExecutor`（1）
- `builder.go`：`executorBuilder.buildStreamAgg`、`buildStreamAggFromChildExec`（2）

### 2.2 第 B 部分（74）

**B1 单线程（19）**

- `agg_hash_executor.go`：`HashAggExec.Close`、`Open`、`OpenSelf`、`initForUnparallelExec`、`Next`、`unparallelExec`、`resetSpillMode`、`execute`、`spillUnprocessedData`、`getNextChunk`、`getPartialResults`（11）
- `agg_util.go`：`GetGroupKey`、`HashAggExec.actionSpillForUnparallel`、`HashAggExec.ActionSpill`（3）
- `agg_spill.go`：`hasEnoughDataToSpill`、`AggSpillDiskAction.Action`、`AggSpillDiskAction.GetPriority`（3）
- `builder.go`：`executorBuilder.buildHashAgg`、`buildHashAggFromChildExec`（2）

`spillUnprocessedData`、`getNextChunk`、`resetSpillMode` 照 Go 移植：它们调用的 `DataInDiskByChunks.Add` / `GetChunk` 是 D01.10 的桩，所以第一次要写盘时返回 `blocked-by:D01.10`；没有落盘时它们只读 `NumChunks()`（为 0），与 Go 相同。

**B2 并行（55）**

- `agg_hash_executor.go`：`initPartialWorkers`、`initFinalWorkers`、`initForParallelExec`、`fetchChildData`、`spillIfNeed`、`waitPartialWorkerAndCloseOutputChs`、`waitAllWorkersAndCloseFinalOutputCh`、`prepare4ParallelExec`、`parallelExec`、`initRuntimeStats`（10）
- `agg_hash_base_worker.go`：`newBaseHashAggWorker`、`getPartialResultSliceLenConsiderByteAlign`、`checkFinishChClosed`（3）
- `agg_hash_partial_worker.go`：`getChildInput`、`fetchChunkAndProcess`、`finalizeWorkerProcess`、`run`、`getPartialResultsOfEachRow`、`updatePartialResult`、`shuffleIntermData`、`processError`（8）
- `agg_hash_final_worker.go`：`getPartialInput`、`mergeInputIntoResultMap`、`consumeIntermData`、`generateResultAndSend`、`sendFinalResult`、`receiveFinalResultHolder`、`run`、`cleanup`（8）
- `agg_spill.go`：`newSpillHelper`、`close`、`isSpilledChunksIOEmpty`、`isNoSpill`、`checkNeedSpill`、`setNeedSpill`、`waitForTheEndOfSpill`、`checkError`、`setError`、`ParallelAggSpillDiskAction.Action`、`actionImpl`、`GetPriority`（12）
- `agg_util.go`：`getBuffer`、`tryRecycleBuffer`、`recoveryHashAgg`、`getGroupKeyMemUsage`、`HashAggRuntimeStats.workerString`、`String`、`Clone`、`Merge`、`Tp`、`AggWorkerStat.Clone`、`HashAggExec.actionSpillForParallel`、`updateWaitTime`、`updateWorkerTime`、`updateExecTime`（14）

`newSpillHelper`、`close`、`isSpilledChunksIOEmpty` 等放在第 B 部分，是因为并行方式每次 `Open` 都建 spill helper（agg_hash_executor.go:411）、`Close` 都调 `close`（:221），final worker 每次都问 `isSpilledChunksIOEmpty`（final_worker.go:157、215）。落盘之前这些函数都在空状态上运行，结果与 Go 相同。

### 2.3 第 C 部分（15，`blocked-by:D01.10`）

- `agg_hash_executor.go`：`HashAggExec.spill`（1）
- `agg_hash_partial_worker.go`：`prepareForSpill`、`spillDataToDisk`、`spillDataToDiskImpl`、`spillRemainingDataToDisk`（4）
- `agg_hash_final_worker.go`：`getInputFromDisk`、`restoreDataFromDisk`（2）
- `agg_spill.go`：`getNextPartition`、`addListInDisks`、`getListInDisks`、`setInSpilling`、`setSpillTriggered`、`restoreOnePartition`、`restoreFromOneSpillFile`、`processRow`（8）

理由：都要读写 `DataInDiskByChunks` 的文件，或只在已经落盘之后才被调用。`setInSpilling` / `setSpillTriggered` 只在 `spill` 中调用（agg_hash_executor.go:543-544）。第 C 部分移植之前，§3 的桩保证 Go 走落盘分支的地方都显式报 `blocked-by:D01.10`。

第 C 部分移植时要另行设计一处：`ParallelAggSpillDiskAction.actionImpl` 在内存 tracker 的 `Consume` 里同步等待落盘结束（`waitForTheEndOfSpill` 用 `sync.Cond`，agg_spill.go:203-210）。Rust 的 `Consume` 是同步函数，可能在任意 task 上被调用，在那里阻塞等待别的 task 会占住 runtime 线程（M-BLOCK）。在第 B 部分中，状态永远到不了 `inSpilling`（`spill` 是桩，见 §3），这个等待不会阻塞。

### 2.4 归 X：测试钩子（7）

`HashAggExec.IsSpillTriggeredForTest`、`HashAggExec.IsInvalidMemoryUsageTrackingForTest`、`intestDuringPartialWorkerRun`、`intestBeforePartialWorkerRun`、`intestBeforeFinalWorkerStart`、`HashAggFinalWorker.intestDuringFinalWorkerRun`、`failpointError`。

理由：前两个只被 Go 的测试调用；后五个的函数体只有 failpoint（`enableAggSpillIntest`、`slowSomePartialWorkers`），failpoint 关闭时什么都不做。D12.1 / D12.2 的 spec 对 failpoint 的处理都是"不移植（测试钩子）"。tracker 怎么登记见决定点 2。`IsInvalidMemoryUsageTrackingForTest` 读的字段 `invalidMemoryUsageForTrackingTest`（agg_hash_executor.go:158、213）也不移植，`Close` 中对应的 warn 日志保留。

### 2.5 清单外、随本块一起移植的

| 内容 | 所属块 | 为什么 |
|---|---|---|
| `internal/vecgroupchecker/vec_group_checker.go` 整个文件（8 个函数） | D12.0 | StreamAgg 的分组；随第 A 部分移植，计入 D12.0（决定点 3） |
| `chunk_in_disk.go` 中不做 I/O 的 `NewDataInDiskByChunks`、`GetDiskTracker`、`NumChunks`、`NumRows`、`Close`；其余为桩 | D01.10 | 单线程 HashAgg 每次 `Open` 都要建一个空容器（决定点 4） |
| `util/set/set_with_memory_usage.go`、`util/channel/channel.go` | D00.3 | 单线程 HashAgg 的 `groupSet`；`Close` 排空通道。叶子工具文件，按宪章第四条 4 整文件移植 |
| `util/hack/map_abi.go` 的 `MemAwareMap` | D00.3 | 部分结果表与 `groupSet` 的底层（决定点 8） |
| `github.com/twmb/murmur3@v1.1.6.Sum32` | 第三方 | §1.4 |
| `ExecutorCounterHashAggExec`、`ExecutorStreamAggExec` | D12.0（`executor/metrics/metrics.go`） | builder 调用 |

## 3. 边界桩

| Rust 位置 | Go 位置 | 类型 | 调用时 |
|---|---|---|---|
| `executorBuilder.build` 对 `PhysicalHashAgg` / `PhysicalStreamAgg` 的分派 | builder.go:278-281 | 临时 `blocked-by:D10.1` | 建执行器之前报错，路由器可以回退。D10.1 的聚合算子移植后随第 A / B 部分解除 |
| `HashAggExec.spill`（整函数桩） | agg_hash_executor.go:542-560 | 临时 `blocked-by:D01.10` | 照 Go 中每个 partial worker 写盘失败时的路径：`processError`（错误写进 `finalOutputCh`、`setError`），`Next` 返回该错误 |
| `fetchChildData` 结束时"把剩下的数据落盘"的分支 | agg_hash_executor.go:465-473 | 临时 `blocked-by:D01.10` | 在 `!isNoSpill() && !checkError()` 成立的同一位置调上面的桩；`addListInDisks`（第 C 部分）不调用（此时没有落盘文件） |
| final worker 中"已经落盘"的两个分支 | final_worker.go:160-173、221-224 | 临时 `blocked-by:D01.10` | 第 C 部分之前 `isSpilledChunksIOEmpty()` 恒为真，不可达；仍在分支处显式报错，不留空分支 |
| `DataInDiskByChunks.Add`、`GetChunk` 等写读方法 | util/chunk/chunk_in_disk.go:93、164 等 | 临时 `blocked-by:D01.10`（属 D01.10，在 tracker 登记） | 单线程 HashAgg 进入落盘模式后第一次要写盘时（agg_hash_executor.go:791、804、809） |
| 会话 tracker 缺失时的软限额动作注册 | agg_hash_executor.go:294、426 | 临时，只有注释（见决定点 6） | 不报错：会话没有 tracker 时也没有限额，动作不会被触发 |

所有 `blocked-by:D01.10` 都发生在执行器已经 `Open`、已经读过子节点之后，按路由器规则（port-block §2"路由器与回退"）不能回退到 PoC，直接把错误返回给客户端。这是落盘移植之前的用户可见差异（§6 第 10 条）。

## 4. Go → Rust 的结构映射

### 4.1 文件与类型

模块对应 Go 包：`crates/executor/src/aggregate/`，每个 Go 文件一个 Rust 文件（`agg_hash_executor.rs` 等），builder 函数在 `builder.rs`，vecgroupchecker 在 `crates/executor/src/internal/vecgroupchecker/vec_group_checker.rs`。类型名不变（`HashAggExec`、`StreamAggExec`、`HashAggPartialWorker`、`HashAggFinalWorker`、`HashAggRuntimeStats`、`AggWorkerStat`、`AfFinalResult`、`HashAggInput`）。

| Go | Rust | 说明 |
|---|---|---|
| `HashAggExec`、`StreamAggExec` 嵌入 `exec.BaseExecutor` | 字段 `base: BaseExecutor`，`impl Executor`（`M-SELF`） | 与 D12.0 / D12.2 相同 |
| `HashAggExec.Sc *stmtctx.StatementContext` | 不保留 | 本包中没有读它的地方 |
| `prepared`、`executed atomic.Bool`（:122-123） | 普通 `bool` | 只有执行器自己（`Next` / `Close`）读写；Go 用原子类型没有跨 goroutine 的需要（L2） |
| `inSpillMode uint32`（原子读写，:141） | `Arc<AtomicU32>` | 动作 `AggSpillDiskAction` 在任意 task 的 `Consume` 中写它，执行器读它 |
| `AggSpillDiskAction{e *HashAggExec}`、`ParallelAggSpillDiskAction{e, spillHelper}` | 动作持有 `Arc<AtomicU32>`、执行器 tracker 的 `Arc<Tracker>`、spill helper 的 `Arc` | 动作不能持有执行器本身（执行器是 `&mut self` 独占的）；它只读写这几项（agg_spill.go:336-350、374-377） |
| `parallelHashAggSpillHelper.lock`（`sync.Mutex` + `sync.Cond`），`hasError atomic.Bool` | `parking_lot::Mutex` + `Condvar`（`M-LOCK`），`AtomicBool`；整个 helper 在 `Arc` 中 | 不跨 `await` 持锁 |
| `partialResultsMapMem atomic.Int64` | `AtomicI64` | 第 C 部分的落盘 goroutine 代 worker 读它 |
| `HashAggRuntimeStats{PartialWallTime int64（原子读写）, PartialStats []*AggWorkerStat}` | wall time 是 `AtomicI64`；`PartialStats: Vec<Arc<AggWorkerStat>>`，`AggWorkerStat` 的四个字段是 `AtomicI64` | 每个 worker 写自己的统计，`Close` 注册时读；`Clone` 复制数值 |
| `AggPartialResultMapper`、`PartialResult` | D12.4 的类型 | 要求见 §4.10 |
| `groupSet set.StringSetWithMemoryUsage` | D00.3 的 Rust 类型（决定点 8） | |
| 分组键 `[][]byte`、`string(key)` | `Vec<Vec<u8>>`；表的键是 `Vec<u8>`（键是 `HashGroupKey` 的编码字节，不是 UTF-8 文本，`M-BYTES`） | `hack.String` / `hack.Slice` 的零拷贝换成借用 |

### 4.2 会话与上下文（N2，`M-CTX`）

执行器不持有会话。Go 在 `Open` / `Next` / worker 中从 `e.Ctx()` 读的东西，Rust 在 builder 建执行器时取一份（与 D12.1 §2 第 1 条、D12.2 §2 第 1 条相同）：

- 表达式上下文 `ExprContextRef`（worker 共享它的 `EvalContext`，是 `Send + Sync`）；
- `EnableVectorizedExpression`、`TrackAggregateMemoryUsage`、`EnableParallelHashaggSpill`、两个并发度；
- 会话的内存 tracker（决定点 6）。

语句的 tracker、disk tracker、`ErrCtx`、时区从 `ExecCtx.stmt` 读。Go 在 `Open` 时读这些会话变量，Rust 在建执行器时读：同一条语句中它们不变，所以结果相同（L2）。

`GetGroupKey(ctx sessionctx.Context, ...)`（agg_util.go:106）改为接收它实际用到的几项：求值上下文、`ErrCtx`、时区、是否向量化（L2，N2）。shuffle（D12.10）用同一个签名。

### 4.3 并发（`M-GO`、`M-CH`、`M-WG`，照 D12.2 §2 第 2 条）

**goroutine → task**：fetcher、P 个 partial worker、F 个 final worker 各是一个 task，在 `ExecCtx.pool` 上 spawn（没有池时在当前 runtime 上，与 D12.2 相同）。worker 数量与 Go 相同（N3 的 A2：保留 Go 的算子结构）。Go 的三个辅助 goroutine（:604-609 关闭 partial 输出并记 wall time，:618-623 记 final 的 wall time，:627 关闭 `finalOutputCh`）也是 task。

**通道**（容量照抄 Go；Go 的无缓冲通道在 tokio 中至少容量 1，这里没有无缓冲通道）：

| Go 通道 | 容量 | 发送方 → 接收方 | Rust |
|---|---|---|---|
| `inputCh chan *HashAggInput`（:383） | P | `initPartialWorkers`、partial worker（还回空 chunk）→ fetcher | `mpsc`；`HashAggInput.giveBackCh` 换成目标 partial worker 的下标（见下） |
| `partialInputChs[i] chan *chunk.Chunk`（:388） | 1 | fetcher → partial worker i | `mpsc`，发送端全部由 fetcher 持有 |
| `partialOutputChs[j] chan AggPartialResultMapper`（:392） | P | 每个 partial worker → final worker j | `mpsc`，每个 partial worker 持有一组发送端 |
| `finalOutputCh chan *AfFinalResult`（:382） | F+P+1 | fetcher、partial、final worker、panic 恢复 → `Next` | `mpsc` |
| `finalResultHolderCh chan *chunk.Chunk`（:358） | 1 | `Next`（还回 chunk）→ final worker | `mpsc`；`AfFinalResult.giveBackCh` 是它的发送端 |
| `finishCh chan struct{}`（:384） | 1，只用 `close` 广播 | `Close` → 所有 worker | `watch::channel(false)`，关闭即发送 `true` |

Go 的 `select` 写成没有 `biased;` 的 `tokio::select!`：两边都就绪时随机选，与 Go 相同。所有阻塞的收发都同时等 finish（或对端关闭），保证 `Close` 时不会卡住。

**关闭通道**：Go 显式 `close`（:192-199、485-487、564-570、577）；tokio 的 `mpsc` 在所有发送端丢弃时关闭。两处要改写（L2 [M-CH]）：

1. `HashAggInput.giveBackCh` 是 partial worker 输入通道的发送端，在 `inputCh` 中流转（:340-345，partial_worker.go:105-108）。如果照搬，fetcher 在 :485-487 关闭这些通道时，`inputCh` 缓冲中的消息还拿着发送端，partial worker 的 `for range w.inputCh`（partial_worker.go:173）会一直等下去。Rust 中消息只带 worker 下标，发送端全部在 fetcher 手里，fetcher 在 Go 关闭它们的同一位置丢弃，partial worker 的排空循环随之结束。
2. `waitPartialWorkerAndCloseOutputChs`（:562）在 partial worker 全部结束后关闭 `inputCh` 并排空它，把剩余 chunk 的内存从 tracker 中减掉。Rust 中 `inputCh` 的接收端属于 fetcher：fetcher 结束时把接收端连同子执行器一起作为 task 的返回值交回，排空和记账在等到 partial worker 结束之后做，总记账量与 Go 相同。`partialOutputChs` 在所有 partial worker 结束、发送端丢弃时自然关闭，`finalOutputCh` 在所有 task 结束时自然关闭，就是 Go 关闭它们的时刻。

**WaitGroup**：Go 有四个（fetcher、partial、final 各一个，`inflightChunkSync`），第 C 部分还有 `spillWaiter`。`M-WG` 目前只写了"保存 `JoinHandle`，关闭时逐个 `await`"，这只覆盖 `Close` 等 task。本块还有跨 task 的等待：final worker 开始前等所有 partial worker 结束（final_worker.go:211）；fetcher 在 `spillIfNeed` 和结束时等在途 chunk 处理完（`inflightChunkSync`，:463、537；fetcher `Add`，partial worker `Done`，partial_worker.go:99、174）。这需要一个异步的计数等待原语，见决定点 7。

**子执行器的所有权**：照 D12.2，`prepare4ParallelExec` 用 `take_all_children` 把子执行器交给 fetcher task；fetcher 结束时把它作为返回值交回，`Close` 等到 task 结束后 `set_all_children` 放回，再由 `BaseExecutor.Close` 关闭。

### 4.4 生命周期：`Close` 与 `Drop`

- Go 的 `Close`（:164-234）在并行方式下关闭 `finishCh` 后排空 `partialOutputChs`、`partialInputChs`、`finalOutputCh`（`channel.Clear`，:202-208）。`Clear(finalOutputCh)` 要等到通道被关闭才返回，而它要等 fetcher、partial、final 三组都结束（:573-578）。所以 Go 的 `Close` 实际上等所有 worker 结束（只有两个记 wall time 的 goroutine 不等，:585-590 的注释说的是它们）。Rust 的 `Close` 发 finish、排空并丢弃接收端、await 所有 `JoinHandle`，与此等价。
- `Close` 在 `Open` 之后、`Next` 之前调用（`prepared` 为假，:191-200）：Go 关闭通道；Rust 丢弃通道，worker 从未启动。
- **`Drop`**（Rust 独有）：语句的 future 被取消、执行器没有 `Close` 就被丢弃时，发送 finish 并 abort 所有 task；`Close` 的 future 中途被丢弃时也一样（与 D12.2 §2 第 2 条相同）。
- 运行时统计：Go `defer RegisterStats(e.ID(), e.stats)`（:165-167）注册指针；Rust 在所有 task 结束之后注册一份（`register_stats` 接收 `Box`，与 D12.1 §2 第 6 条相同），注册在 `Close` 的每条返回路径上（含单线程分支中 `BaseExecutor.Close` 出错提前返回，:183-186）。

### 4.5 panic、错误与 `defer` 的顺序（`M-PANIC`、`M-DEFER`）

- **panic**：Go 的 worker 和 fetcher 用 `defer recover()` + `recoveryHashAgg`（agg_util.go:90-94）把 panic 写进 `finalOutputCh`。Rust 的 task 用 `catch_unwind`（`spawn_with_recover`），把 panic 变成同样文本的错误写进 `finalOutputCh`。日志没有栈（`catch_unwind` 拿不到 panic 处的栈，与 D12.2 相同）。
- **内存超限**：Go 的 `Consume` 在动作中 panic；Rust 的 `mem.checkpoint()` 返回 `Err`。在 task 中按 panic 同样处理（写进 `finalOutputCh`，接着走清理）；在执行器自己的 `Next` / `Open` 中用 `?` 返回，最终给客户端的错误相同（与 D12.2 §2 第 3 条相同）。
- **`defer` → drop guard**：下面每一处的清理在正常结束、返回错误、panic 三种情况下都要按 Go 的顺序执行，每一处配一个测试（含出错和提前返回）：

| Go 位置 | 清理内容（按 Go 的执行顺序） |
|---|---|
| `fetchChildData`（:457-489） | 恢复 panic → 等在途 chunk（`inflightChunkSync.Wait`）→ 需要时把剩余数据落盘（§3 的桩）→ 关闭各 worker 的落盘文件 → 关闭 `partialInputChs` → `waitGroup.Done` |
| `HashAggPartialWorker.run`（partial_worker.go:195-209） | 恢复 panic → `finalizeWorkerProcess`（排空输入，每个 `Done`；没有出错、没有落盘时把部分结果发给 final worker）→ 减去 `chk` 的内存 → 记 worker 时间 → `waitGroup.Done` → 归还缓冲区（M-POOL 下为空操作） |
| `fetchChunkAndProcess`（partial_worker.go:99） | `inflightChunkSync.Done()` |
| `HashAggFinalWorker.cleanup`（final_worker.go:229-235） | 恢复 panic → 记 worker 时间 → `waitGroup.Done` |
| `getPartialInput`（final_worker.go:61） | 记等待时间 |
| `HashAggExec.execute`（:731-736） | 没有出错且 `tmpChkForSpill` 有行时写盘（第 C 部分之前，有行就意味着已进入落盘模式，写盘处是 D01.10 的桩） |
| `HashAggExec.Close`（:165-167） | 最后注册运行时统计（§4.4） |
| `StreamAggExec.Open`（agg_stream_executor.go:74，`closeBaseExecutor`） | 只在 `OpenSelf` panic 时：关闭子执行器，再把 panic 继续抛出。Rust：`OpenSelf` 返回 `Err` 时关闭子执行器再返回该错误。HashAgg 的 `Open` 没有这一步（:237-248），照抄 |
| `agg_spill.go` 各方法的 `defer p.lock.Unlock()` | `parking_lot` 的 guard |

- **错误的先后**：`parallelExec` 返回 `finalOutputCh` 中的第一个错误（:659-661）；worker 出错后 `setError`，其他 worker 在下一次 `checkError` 时停止（partial_worker.go:88-91）。Rust 不改变这些检查的位置。

### 4.6 内存记账与落盘（`M-MEM`、A1）

- 记账按 A1（`specs/exceptions/N3-A1-concurrency-memory/spec.md` §3.2 第一行、§3.3）：删除 Go 的 delta 计算（包括聚合函数返回的 `memDelta`、`getGroupKeyMemUsage`、部分结果表与 `groupSet` 的增量估算）；会增长的容器（分组键、部分结果表、`groupSet`、聚合状态）用 `TrackedAlloc` 分配，按实际字节数自动记进 tracker；Go 中正向 `Consume` 的位置改为 `mem.checkpoint()?`，在同一位置执行动作链（所以"在哪一步报错"与 Go 相同）；负向 `Consume`（释放）删除，由 Drop 完成。这与 D12.4（#342）决定点 1 定下的 B 是同一套机制：D12.4 不再返回 `memDelta`，本块也不再 `consume` 它。按 A1，记的数值与 Go 不同。每个调用点在 tracker 中按例外（A1）登记。部分结果表的内存怎么计入见决定点 8。
- 两个动作的优先级都是 `DefSpillPriority`（agg_spill.go:353、380），在 `TrackAggregateMemoryUsage && EnableTmpStorageOnOOM`（并行方式还要 `EnableParallelHashaggSpill`）时注册到会话 tracker 的软限额动作上（:286-295、418-427）。
- 落盘：第 B 部分照移植触发落盘的状态机（动作、`needSpill`、`inSpillMode`），真正写盘的地方停在 `blocked-by:D01.10`（§3）。不允许不注册动作（那样内存超限时会直接走 fallback 取消语句，是静默的行为差异）。

### 4.7 KILL 检查点（`M-KILL`）

子节点的 `exec::next` 已经带 KILL 检查。另外按 N3 第二部分第 5 条，在不经过 `Next` 的长循环中放 `CheckpointStride`：final worker 合并部分结果表的循环（final_worker.go:84-100）和遍历结果表输出的循环（:124-142）；第 C 部分的恢复循环（agg_spill.go:265-290）同样。partial worker 和单线程方式按 chunk 处理，每个 chunk 都经过子节点的 `Next`，不另加。

### 4.8 其他映射规则

- **`M-POOL`**：`partialResultsBufferPool`、`groupKeyPool`（agg_util.go:47-79）删除，`getBuffer` 直接分配，`tryRecycleBuffer` 什么都不做（两者仍标 `go:`，L1 [M-POOL]）；`expression.GetColumn` / `PutColumn`（agg_util.go:121、139、152、155）直接分配列；`e.AllocPool.Alloc`（builder.go:2053、2122）直接建 chunk。
- **`M-IFACE`**：D12.6 对 `*HashAggExec` / `*StreamAggExec` 的类型断言是 `Executor` trait 上的方法（如 `is_child_return_empty_agg() -> Option<bool>`，默认 `None`），D12.6 移植时加；builder 中 group_concat 的 `SetTruncated` / `GetTruncated` 接口断言（builder.go:2084-2089）是 D12.4 `AggFunc` trait 上的方法。
- **`M-INT`**：`int(murmur3.Sum32(key)) % finalConcurrency`（partial_worker.go:226）：`uint32` 转 64 位 `int` 不会为负，Rust 写 `as isize`；`length + length&1`（base_worker.go:47）照写。
- **`M-MAPORDER`**：Go 遍历 map 的地方——final worker 输出（final_worker.go:124）、合并（:84）、第 C 部分的落盘（partial_worker.go:338）与恢复。Go 的输出顺序本来就不确定，Rust 取某种确定的顺序（不限定为插入顺序，免得给表的数据结构和 N6 增加约束）；多个 final worker 之间的先后取决于调度，与 Go 一样不确定。对照测试按多重集比较（`docs/testing/result-comparison.md` §2.2）。合并的顺序不影响结果，除了下面两类与顺序有关的聚合（Go 同样不确定，govec 生成器按结构排除，§5.2）：非分组列上的 FIRST_ROW，浮点数的 SUM / AVG（加法顺序不同，末位可能不同）。
- **`M-ERR`**：`errors.New("partialConcurrency or finalConcurrency is 0")`（:378）、`errors.NewNoStackError("len(aggFuncsForRestoring) != len(finalWorkerAggFuncs)")`（agg_spill.go:82）照文本移植。前者在 builder 已把非正并发度改为单线程（builder.go:2064-2066）之后不可达，照样移植。

### 4.9 共享修改的审计（M-FT，N1 §5）

对本块文件和 builder.go:2014-2136 运行 plan 模板 §4 的两个 grep，只有两处，都在 `GetGroupKey` 中，都是先复制再改（S）：

| Go 位置 | 改什么 | 结论 | 处理 |
|---|---|---|---|
| agg_util.go:132-136 | ENUM 列：`newTp := *tp; newTp.AddFlag(EnumSetAsIntFlag)` | S | 改本地副本，照抄 |
| agg_util.go:143-147 | DECIMAL 列：`newTp := *tp; newTp.SetFlen(0)` | S | 同上 |

读代码补充：builder 把 `v.GroupByItems` 交给执行器（builder.go:2028、2113）只读共享；`aggDesc.Split` 先 `Clone`（D08.11 descriptor.go:153），不写计划。执行器从不写计划（`crates/executor/clippy.toml`），本块不需要例外。

### 4.10 对其他块的要求

- **D12.4（#336）**：所有 partial worker 共用同一组 `PartialAggFuncs` 对象（agg_hash_executor.go:312），spill helper 在 final worker 中也用它们（:411）。所以 `AggFunc` 要 `Send + Sync`，方法接收 `&self`；`PartialResult` 要 `Send`（从 partial worker 经通道交给 final worker）。group_concat 的 partial 与 final 函数共享一个"已截断"标志（builder.go:2084-2089，Go 中是 `*int32` 加原子操作，func_group_concat.go:76），需要一个可共享的原子标志。`AggPartialResultMapper` 的表示要能算内存增量（决定点 8）。
- **D08.11（PR #341）**：只用 `Split`、`GetDefaultValue`、`IsAllFirstRow` 和描述的只读字段。
- **D10.1**：`PhysicalHashAgg` / `PhysicalStreamAgg` 和 `IsFinalAgg`。
- **D12.6 / D12.7 / D12.10**：`IsChildReturnEmpty` 的读取（§4.8）、`OpenSelf` 是公开方法（D12.7 直接调用）、`GetGroupKey` 的签名（§4.2）。

## 5. 测试计划

### 5.1 要移植的 Go 单测

| Go 测试 | 位置 | 怎么移植 |
|---|---|---|
| `TestHashAggRuntimeStat` | `pkg/executor/test/aggregate/aggregate_test.go:38` | 不依赖存储，直接移植（`String`、`Clone`、`Merge` 的文本） |
| `TestStreamAggRequiredRows` | `pkg/executor/executor_required_rows_test.go:659` | 用 D12.2 移植 required rows 测试时建的数据源（PR #338） |
| `TestGetCorrectResult`、`TestFallBackAction`、`TestRandomFail` | `agg_spill_test.go:409、458、467` | 都是并行落盘：随第 C 部分移植。第 B 部分改为 Rust 测试：同样的设置下触发落盘，返回 `blocked-by:D01.10`，worker 全部结束，tracker 归零 |
| `TestParallelHashAgg`、`TestParallelStreamAggGroupConcat`、`TestIssue20658`、`TestStreamAggPendingMemDeltaBatching`、`TestIssue50849`、`TestAggInDisk`、`TestRandomPanicConsume`，以及 `tests/integrationtest/t/executor/aggregate.test` | `aggregate_test.go` 等 | 依赖存储（testkit）：由维护者在合并后用 `tools/ab` 做 SQL 级对比。其中不依赖存储就能表达的部分（`TestParallelHashAgg` 的大小写不同的分组键、`TestStreamAggPendingMemDeltaBatching` 的 1 KiB 阈值）改写成执行器级单测 |

### 5.2 govec

新领域 `aggexec`（tracker `govec/domains/aggexec.go`，决定点 11）。Go 侧不经过 planner，像 `agg_spill_test.go:194` 那样直接构造执行器（字段都是导出的），子节点是内存数据源；Rust 侧同样直接构造后回放。要等 D12.4 的基本聚合函数移植后才能回放。

| op | 覆盖 | 比较 |
|---|---|---|
| `group_key` | `GetGroupKey`：各种类型的分组表达式（ENUM 走 `EnumSetAsIntFlag`、DECIMAL 走 flen 0、带 collation 的字符串、带时区的 TIMESTAMP、JSON、NULL）、多个分组项、向量化开 / 关、严格模式下求值报错（经 `ErrCtx`） | 每行的键字节、错误 |
| `stream_agg` | StreamAgg：有 / 无 GROUP BY，一组跨多个子 chunk，`max_chunk_size` 很小，空输入 × final / partial 模式（`DefaultVal`），全部 FIRST_ROW，0 个聚合函数，子节点中途报错 | 每次 `Next` 的输出（顺序确定）、错误 |
| `hash_agg_unparallel` | 单线程 HashAgg（并发度 1 / 1，或带 DISTINCT / ORDER BY 的聚合函数）：同上的形状，以及没有 GROUP BY 的空输入（空键那一组） | 每次 `Next` 的输出（顺序确定：首次出现的顺序）、错误 |
| `hash_agg_parallel` | 并行 HashAgg：并发度 P、F 各取 1–8，空输入（`DefaultVal`），子节点报错，avg 拆成两列的 ordinal | 全部输出行按多重集比较、错误文本。生成器按结构排除与合并顺序有关的用例：非分组列上的 FIRST_ROW、浮点 SUM / AVG、不带 ORDER BY 的 GROUP_CONCAT（M-MAPORDER） |
| `runtime_stats` | `HashAggRuntimeStats` 的 `String` / `Clone` / `Merge`，随机的时间和并发度 | 文本 |

builder 的决定（何时单线程、`DefaultVal` 的内容、partial / final 函数的拆分）要等 D10.1 有聚合算子后再加一个 op `build`。

向量在本地生成；tracker CI 用 `scripts/govec.sh --check` 在两个架构上逐字节核对已提交的文件。回放中出现 `blocked` 的条数按 op 固定（port-block §4）。

### 5.3 Rust 单测（不依赖 govec 的部分）

- §4.5 表中每一处 drop guard：正常结束、子节点报错、子节点 panic、内存超限四种情况下清理都执行，`Close` 之后 tracker 归零，所有 task 结束。
- 并行方式：读完之前 `Close`；`Open` 之后不调 `Next` 就 `Close`；没有 `Close` 就丢弃时 task 停止；单线程 runtime 上运行；`Close` 的 future 中途被丢弃。
- 落盘边界：单线程方式由动作置位 `inSpillMode` 后返回 `blocked-by:D01.10`；并行方式由动作置位 `needSpill` 后返回 `blocked-by:D01.10`；两者都没有输出任何行。
- 两个动作：`hasEnoughDataToSpill` 的 20% 阈值、`maxSpillTimes`、fallback 的调用、优先级。
- `StreamAggExec.Open` 中 `OpenSelf` 出错时子执行器被关闭。

### 5.4 端到端

sysbench 的 SUM / DISTINCT 要等 D09.4、D09.9、D10.1、D10.3、D12.3、D12.4 都接上，属于集成验收，由维护者在合并后用 `tools/ab` 运行（CLAUDE.md §3）。

## 6. 与 Go 的已知差异

1. **通道的关闭方式**（agg_hash_executor.go:192-199、485-487、564-570、577；partial_worker.go:105-108）：Go 显式 `close`；Rust 靠丢弃发送端，`giveBackCh` 换成 worker 下标，`inputCh` 的排空移到 fetcher 交回接收端之后（§4.3）。关闭的时刻和记账总量与 Go 相同（L2 [M-CH]）。
2. **`Drop` 路径**（§4.4）是 Rust 独有的。
3. **内存超限走错误而不是 panic**（`Consume` 的所有调用处；recoveryHashAgg，agg_util.go:90-94）：task 中按 panic 同样处理；恢复日志没有栈。
4. **缓冲区池删除**（agg_util.go:47-79，`M-POOL`）。
5. **内存记账的数值**（agg_hash_executor.go:271-275、850-854；partial_worker.go:243-249；final_worker.go:87-89）：取决于决定点 8，按 A1 可以与 Go 不同；落盘触发的时机（`hasEnoughDataToSpill`，agg_spill.go:322-324）随之不同。
6. **并行方式的输出顺序**（final_worker.go:124）：Go 随机，Rust 每个 final worker 内确定、worker 之间取决于调度（§4.8）。
7. **`workerString` 原地排序**（agg_util.go:183）：Go 的 `String()` 会把 `PartialStats` / `FinalStats` 按 worker 时间原地排序；Rust 的 `string(&self)` 排序一份副本。输出文本相同；之后再 `Merge` 时切片中的先后不同，但 `String` 总是先排序，看不出差别。
8. **运行时统计注册的时刻**（agg_hash_executor.go:165-167、604-609、618-623）：Go 在 `Close` 中注册指针，两个记 wall time 的 goroutine 可能在注册之后才加上时间；Rust 等所有 task 结束后注册一份，EXPLAIN ANALYZE 的 `wall_time` 总是完整的。时间本来不参与对照。
9. **会话变量的读取时刻**（agg_hash_executor.go:259、286、373-375、397-398）：Go 在 `Open` 时读，Rust 在建执行器时读（§4.2）。同一条语句中不变。
10. **落盘移植之前**（§3）：内存超过软限额、Go 会落盘并成功的语句，在 tidb-rs 上返回 `blocked-by:D01.10`（错误 1235），不能回退。会话 tracker 接入（决定点 6）之前不会发生。
11. **会话没有内存 tracker**（agg_hash_executor.go:294、426）：见决定点 6。这是 A1 尚未在会话中接入的结果，所有执行器都一样，不是本块特有的。
12. **`execute` 的 `defer` 在 panic 时**（agg_hash_executor.go:731-736）：Go 在 `Consume` panic 时具名返回值 `err` 仍为 nil，延迟函数会把 `tmpChkForSpill` 写盘；Rust 走错误返回，`err` 不为空，不写。只在落盘模式下才有区别，第 C 部分之前不可达。
13. **新增的 KILL 检查点**（final_worker.go:84-100、124-142，§4.7）：tidb-rs 在 final worker 的长循环中检查 KILL，可能比 Go 更早响应 KILL（N3 第二部分第 5 条）。
14. **failpoint 不移植**（测试钩子）：agg_hash_executor.go:226-232、238-242、274、334、515、640-644、743、749-753、797、856；final_worker.go:113、153；partial_worker.go:259；agg_stream_executor.go:64-68、102、190、194、212、245。
15. **`HashAggExec.Sc` 字段与 `invalidMemoryUsageForTrackingTest` 字段不保留**（agg_hash_executor.go:96、158）：前者本包中没有读者，后者只有测试读（§2.4）。

第 A 部分（StreamAgg 与 vecgroupchecker，#376）实现时新增的差异：

16. **`buildStreamAgg` 不移植，§3 的分派桩不加**（builder.go:280、2101-2107）：Rust 还没有 `PhysicalStreamAgg`（D10.1，物理算子定义），聚合语句在构建逻辑计划时就回退（D09.4，聚合的构建），没有任何计划会分派到这里；`build` 的默认分支本来就返回 `blocked`，再加一个桩不可达，还会让桩棘轮在 port-block §2 允许的两种情况之外上升。所以 `buildStreamAgg` 在 `functions.tsv` 中记为 not_ported（理由同上），只移植 `buildStreamAggFromChildExec`：它在 Rust 中接收计划的各字段（schema、id、`GroupByItems`、`AggFuncs`）作参数，`IsFinalAgg` 写成本文件中的 glue（与 Go 的 `basePhysicalAgg.IsFinalAgg` 同一判断）。D10.1 有了 `PhysicalStreamAgg` 之后再移植 `buildStreamAgg`，并把分派接上。`executor_metrics.ExecutorStreamAggExec.Inc()`（builder.go:2134）不移植（D14.1，metrics）。
17. **`aggfuncs.Build` 返回 nil 时**（builder.go:2127-2128；agg_stream_executor.go:157、173、238）：Go 把 nil 放进 `AggFuncs`，之后调用它的方法时 nil 指针 panic。Rust 存 `None`，用到它的地方返回同样文本的错误 `runtime error: invalid memory address or nil pointer dereference`（M-PANIC）。在 Go 中这个 panic 不被本执行器恢复，可能使连接断开；Rust 只让这条语句失败。D12.4 第 A 部分的函数中，govec 生成器只用 `Build` 返回非 nil 的描述，这条路径没有向量。
18. **内存记账（A1，§4.6）在 StreamAgg 中的具体做法**（agg_stream_executor.go:83-103、113-118、186-198、246-250）：`memUsageOfInitialPartialResult`、`pendingMemDelta` 和它的 1 KiB 批量阈值、`ReplaceBytesUsed` 都删除；部分结果和子 chunk 用 `TrackedAlloc` 记实际字节数；Go 每组 `Consume` 正数的位置（:191-198 的批量刷新）改为每组更新之后 `checkpoint()?`，所以超限报错的那一组可能比 Go 早（Go 要攒够 1 KiB）。`OpenSelf` 中先重置或新建 tracker、再分配部分结果（Go 先分配、后重置，:85-96），这样分配的字节记在重置之后的 tracker 上。Go 的 `TestStreamAggPendingMemDeltaBatching` 数的是批量刷新的次数，Rust 没有这个批量，改为执行器级单测：记账随数据增长、`Close` 后归零、只在 `TrackAggregateMemoryUsage` 为真时挂到语句的 tracker 下、组内超限时返回 8175 错误。
19. **`inputIter` / `inputRow` 不保留**（agg_stream_executor.go:49-50、82-83）：Rust 按行下标遍历子 chunk（M-ROW），`consumeOneGroup` 中 Go 用迭代器取的行改为下标。
20. **vecgroupchecker 的缓冲区钩子不移植**（vec_group_checker.go:31、34、354-359 的 `allocateBuffer` / `releaseBuffer`）：Rust 直接分配列（M-POOL）。Go 的 `TestVecGroupCheckerDATARACE` 用这两个钩子让求值写进输入 chunk 的列，再改写这一列检查首尾 datum 不变；Rust 版改为在 split 之后改写输入列，检查同样的内容。`Reset` 中 Go 只在 `groupOffset` 等非空时才清空，Rust 无条件清空，结果相同。
21. **会话值的读取时刻**（agg_stream_executor.go:98）：`TrackAggregateMemoryUsage` 与求值上下文在建执行器时读（N2），语句的 tracker 在 `Open` 时从 `ExecCtx` 取。同一条语句中不变（与第 9 条相同）。

第 B 部分（单线程与并行 HashAgg，#382 / PR #383）实现时新增的差异：

22. **`buildHashAgg` 不移植，§3 的分派桩不加**（builder.go:278-279、2014-2020）：与第 16 条（`buildStreamAgg`）相同，Rust 还没有 `PhysicalHashAgg`（D10.1，物理算子定义），`build` 的默认分支已返回 `blocked`，再加分派桩不可达。`buildHashAgg` 在 `functions.tsv` 中记为 `not_ported`，只移植 `buildHashAggFromChildExec`（参数为子执行器、schema、id、`GroupByItems`、`AggFuncs`）。`executor_metrics.ExecutorHashAggExec.Inc()`（builder.go:2097）不移植（D14.1，metrics）。
23. **`aggfuncs.Build` 返回 nil 时**（builder.go:2038-2090）：与第 17 条相同，单线程和并行模式下的 partial / final 聚合函数列表遇到 `Build` 返回 `None` 时保留 `None`，并在调用点返回与 Go 相同文本的错误，不导致进程崩溃。
24. **内存记账（A1，§4.6）在 HashAgg 中的具体做法**：`AggPartialResultMapper` 改为 `MemAwareMap<AllocBox<[u8], A>, AllocVec<PartialResult, A>, A>`，与单线程 `groupSet`（`MemAwareMap<AllocBox<[u8], A>, (), A>`）和 `PartialResult` 一样共用执行器的 `TrackedAlloc` 自动记账；`getPartialResults` / `getPartialResultsOfEachRow` 只在插入新分组时调用 `mem_tracker.checkpoint()?`（命中已有分组零分配、不查限额），chunk 结尾再检查一次；`getGroupKeyMemUsage` 与 `memDelta` 估算删除（`getGroupKeyMemUsage` 保留签名与 `go:` 标注，返回 0，按 A1 解释）。
25. **`WaitGroup` 与 `DataInDiskByChunks` 共享到基础 crate**：异步 `WaitGroup` / `DoneGuard` 和 `channel::clear`（`pkg/util/channel/channel.go`，D00.3）落入 `tidb-runtime`，`DataInDiskByChunks` 的 5 个非 I/O 方法与 2 个 `blocked-by:D01.10` 桩落入 `tidb-chunk`；`sortexec/sort_util.rs` 改为重导出它们，避免重复翻译。
26. **`ParallelAggSpillDiskAction.actionImpl` 的阻塞等待**（agg_spill.go:374-377）：按 §2.3，第 B 部分中 `spill` 是 `blocked-by:D01.10` 桩，状态永远不会进入 `inSpilling`，`waitForTheEndOfSpill` 不会阻塞 runtime 线程。第 C 部分移植真实写盘时需按 M-BLOCK 处理。

## 7. 可能的 Go 问题（读代码发现，部分已在 v8.5.8 上复现）

按 CLAUDE.md，issue 要写清在 v8.5.8 上怎么复现；本任务没有运行中的集群，也不改 tracker，所以下面几条由实现任务复现后再开 issue（标签 `upstream-pitfall`），并更新本 spec（决定点 10）。

1. **并行方式吞掉输出时的错误**（已复现，见 **#384**）：final worker 的 `generateResultAndSend` 在 `AppendFinalResult2Chunk` 出错时只记日志、继续（final_worker.go:125-129）；单线程方式（agg_hash_executor.go:687-691）和 StreamAgg（agg_stream_executor.go:238-242）返回这个错误。已在 Go TiDB v8.5.8 上经由 `govec` `aggexec` 的 `hash_agg_parallel` 复现：`baseAvgDecimal`、`sum4Decimal` 与 `maxMin4Decimal` 的 `AppendFinalResult2Chunk` 在 `DecimalDiv` / `Round` 溢出或截断时返回错误而不向该列追加值，导致多分组场景下各列错位（静默错误结果，且结果随 Go map 顺序变化）以及单分组或读越界场景下 Go 自身 panic（`runtime error: index out of range [0] with length 0`）。已开 `upstream-pitfall` issue #384 按宪章第一条 2 请负责人裁定；PR #383 暂照 Go 实现（记日志继续），待裁定后再按需调整。
2. **StreamAgg 的初始内存在每次重新打开时累加**：`OpenSelf` 中 `memUsageOfInitialPartialResult += memDelta`（agg_stream_executor.go:90），这个字段从不清零。Apply 对每一行外表都重新 `Open` 内表的执行器（join/hash_join_v1.go:1308），所以第 n 次打开时记账的初始内存是第一次的 n 倍（:103、:248），外表行数多时可能因为记账虚高而触发内存超限。只影响内存记账，不影响结果。按 A1 删除 delta 计算后（§4.6），Rust 记的是实际字节数，这个累加不存在，所以不照搬；这是 A1 带来的记账差异，不单独登记裁定。
3. **builder 给 avg 的 ordinal**（builder.go:2073-2078）：avg 得到 `[p, p+2]`（不是 `[p, p+1]`），下一个函数从 `p+2` 开始。这些 ordinal 只用作 final 描述参数列的 `Index`（D08.11 descriptor.go:168-175），而并行方式的 final 函数只做 `MergePartialResult` / `AppendFinalResult2Chunk`，不对参数求值，所以看起来没有影响。照 Go 移植，govec `hash_agg_parallel` 含多个 avg 的用例来确认。

## 8. 已决定的点

### 8.1 负责人已定

本 spec 没有需要负责人决定的点。相关的一点在 D12.4：负责人 2026-11-24 决定落盘字节与 Go 的内存布局逐字节相同（照 A1，D12.4 spec §9 第 5 条，由维护者 du-maya 在 #342、#343 转达）；本块第 C 部分写盘的就是 D12.4 序列化出的字节。

### 8.2 维护者已定（宪章第九条，#374：全部按本 spec 的建议）

1. **实现任务的拆分**：A，第 A 部分一个任务（含 vecgroupchecker），第 B 部分一个任务（74 个），第 C 部分在 D01.10 移植后一个任务。
2. **7 个测试钩子函数（§2.4）**：A，tracker 改归 X（测试工具），不移植。
3. **D12.0 的 vecgroupchecker**：A，随第 A 部分提前整文件移植，计入 D12.0。
4. **落盘容器的边界**：A，`chunk_in_disk.go` 中没有文件时不做 I/O 的 5 个方法提前按 L1 移植到 chunk crate、计入 D01.10，其余是 `blocked-by:D01.10` 的桩。
5. **并行方式的落盘边界**：A，`HashAggExec.spill` 是整函数桩，在 Go 第一次真正写盘的位置报 `blocked-by:D01.10`。
6. **会话还没有内存 tracker 时**：A，不注册软限额动作，只有注释的临时桩，写进 §6 第 11 条；在 A1 接入会话的任务中解决。
7. **跨 task 的计数等待**：A，`tidb-runtime` 加异步 `WaitGroup`，`M-WG` 扩展为"跨 task 等待用它，`Close` 仍等 `JoinHandle`"，实现要求：计数已经为 0 时 `wait()` 立即返回；先登记等待、再检查计数，避免漏掉通知；按 port-block 配一个在等待中途 drop 的测试。
8. **内存记账**：A，按 A1 的 `TrackedAlloc` 按实际分配记账，`map_abi.go` 依赖 Go runtime 布局的部分不移植（与 D12.4 决定点 1 相同）。
9. **N6**：A，不等 N6，照 Go 的结构移植，N6 以后作为实现例外替换内部结构。
10. **§7 的可能问题**：A，由实现任务在 v8.5.8 上复现后开 issue；第 1 条若是结果错误，按宪章第一条 2 请负责人裁定。
11. **govec**：A，新领域 `aggexec`。

D08.11 决定点 4（计划节点按值还是按 `Arc` 持有聚合函数描述）不需要本块另行决定，见 §1.4。

## 本文所用 ID

- A1：内存记账与配额的设计（`TrackedAlloc`、tracker 树、动作链），并入 N3 spec
- A2：执行模型（保留 Go 的算子 worker 结构，worker 映射到共享线程池），并入 N3 spec
- D00.1：上下文接口骨架（sessionctx、exprctx、errctx 等）
- D00.3：`util/*` 叶子工具文件（按需整文件移植）
- D01.7：Chunk 与列存
- D01.8：编码（util/codec）
- D01.10：落盘容器与磁盘 chunk
- D05.1：系统变量定义与校验
- D05.2：SessionVars 与 StatementContext
- D05.3：会话
- D08.1：表达式框架
- D08.11：聚合函数描述（expression/aggregation）
- D09.4：聚合、窗口的构建与逻辑算子
- D09.9：聚合相关规则（下推、消除、skew、max/min）
- D09.14：逻辑算子公共部分与基本逻辑改写规则
- D10.1：物理算子定义与 task
- D10.3：物理计划枚举
- D12.0：执行器框架与构建骨架
- D12.1：读取类执行器
- D12.2：基础算子执行器（projection、selection、limit、dual 等）
- D12.3：本块，聚合执行器框架（hash agg / stream agg）
- D12.4：聚合函数实现（aggfuncs）
- D12.6：hash join 与 joiner（含 Apply）
- D12.7：index join 系列
- D12.10：apply、CTE、shuffle、expand
- D14.0：执行统计数据结构（execdetails、runtime stats）
- D14.1：metrics
- D15.1：内存统计与配额（A1 的实现）
- N0：crate 分层
- N1：计划表示（arena 与 `PlanId`，§5 是共享修改的审计）
- N2：会话与语句上下文的设计
- N3：并发、取消与错误边界（含 A1、A2）
- N6：哈希聚合、哈希 join、排序的数据结构（例外，原则同意，尚无 spec）
- M-BLOCK、M-BYTES、M-CH、M-CTX、M-DEFER、M-ERR、M-FT、M-GO、M-IFACE、M-INT、M-KILL、M-LOCK、M-MAPORDER、M-MEM、M-PANIC、M-POOL、M-SELF、M-WG：`docs/porting/mappings.md` 中的映射规则（Go 写法 → Rust 对应物）
- L1 / L2：来源标注的等级（L1 逐行对应；L2 有理由的改写）
- X：不在 tidb-rs 范围内的代码（DDL、写路径、mock store、测试工具等）
- PoC：原型实现（新栈路由回退的目标）
