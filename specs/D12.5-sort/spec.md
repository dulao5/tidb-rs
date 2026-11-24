# D12.5 排序类执行器（spec）

状态：第 1 部分（内存中的 Sort / TopN，VS1-36）评审中（2026-11-24）。块：D12.5（ROADMAP P4），crate `tidb-executor`（N0 rank 14）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（D12.5 一行）、§3.2（边界桩）；[D12.0 tasks](../D12.0-executor/tasks.md)（执行器框架，VS1-13）；[N3/A1](../exceptions/N3-A1-concurrency-memory/spec.md)（并发与内存记账）；[N13](../exceptions/N13-shared-metadata/spec.md)（共享元数据）。任务：tidb-rs#329。

## 1. 范围

Go 范围：`pkg/executor/sortexec` 全部（sort、并行 sort、多路归并、topn、topn worker、落盘），加上 `executorBuilder.buildSort` / `buildTopN`，tracker 中 188 个函数。

### 1.1 第 1 部分（VS1-36：内存中的 Sort / TopN）

函数清单是 `specs/VS1-vertical-slice/functions.tsv` 中 VS1-36 / D12.5 的 65 个函数。

| Go | Rust（`crates/executor/src/`） | 内容 |
|---|---|---|
| `sortexec/sort.go` | `sortexec/sort.rs` | `SortExec`：`Open`、`Next`、`Close`、单分区排序（`fetchChunksUnparallel`、`storeChunk`、`switchToNewSortPartition`、`onePartitionSorting`、`appendResultToChunkInUnparallelMode`）、`initCompareFuncs`、`buildKeyColumns`、`lessRow`、`compareRow`、测试用的 getter |
| `sortexec/sort_partition.go` | `sortexec/sort_partition.rs` | `sortPartition`：`add`、`sort`（`sort.Slice`，同一个 pdqsort）、`getNextSortedRow`、`keyColumnsLess`（每 10240 次比较检查一次 kill）、错误与状态 |
| `sortexec/sort_util.go` | `sortexec/sort_util.rs` | `rowWithError`、`processPanicAndLog`、`dataCursor`、落盘状态常量 |
| `sortexec/topn.go` | `sortexec/topn.rs` | `TopNExec`：`Open`、`Next`、`Close`、`fetchChunks`、`loadChunksUntilTotalLimit`、`executeTopN(WhenNoSpillTriggered)`、`generateTopNResults(WhenNoSpillTriggered)`、`greaterRow` |
| `sortexec/topn_chunk_heap.go` | `sortexec/topn_chunk_heap.rs` | `topNChunkHeap`：`init`、`initPtrs`、`update`、`processChk`、`doCompaction`、`heap.Interface` |
| `builder.go` | `builder.rs` | `buildSort`、`buildTopN` |

清单外随类型一起写出的：`sort_spill.go` 的 `sortPartitionSpillDiskAction`（`GetPriority` L1，`Action` 是桩）、`topn_spill.go` 的 `topNSpillHelper`（状态部分）、Go 标准库 `container/heap` 用到的函数（`sortexec/heap.rs`，go-std）。

converge：`scripts/converge.py --block D12.5 --part specs/VS1-vertical-slice/functions.tsv --item VS1-36 crates/executor/src/sortexec/{heap,mod,sort,sort_partition,sort_spill,sort_util,topn,topn_chunk_heap,topn_spill}.rs crates/executor/src/builder.rs` 为 59/65 移植 + 6 个边界桩：
- `SortExec.fetchChunksFromChild`、`generateResult`、`generateResultFromMemory` → D12.5（并行 sort，本部分以外）；
- `reloadCursor`、`TopNExec.fetchChunksFromChild`、`TestKillSignalInTopN` → D01.10（落盘容器）。

### 1.2 块内其余（`blocked-by:D12.5`、`blocked-by:D01.10`）

- **并行 sort**：`parallel_sort_worker.go`、`parallel_sort_spill_helper.go`、`multi_way_merge.go`，以及 `sort.go` 中的并行分支（`fetchChunksParallel`、`appendResultToChunkInParallelMode` 等）。
- **落盘**：sort 分区落盘、多分区归并（`externalSorting`）、topn worker 与落盘（`topn_worker.go`、`topn_spill.go` 的落盘部分）。落盘依赖 D01.10 的落盘容器（`DataInDiskByChunks`）。
- **rank topn**：`RankInfo` 前缀截断的 topn（`executeRankTopN` 等）。

## 2. 承载方式

1. **会话（N2）。** Go 的执行器保存 `e.Ctx()`，在 `Open` / `Next` 中读会话。Rust 的执行器在建立时拿到它要读的会话值：表达式上下文（`initCompareFuncs` 取类型）、会话内存限额（`spillLimit = 会话 tracker 的限额 / 10`，Go 在 `Open` 读，Rust 在 build 时读 `tidb_mem_quota_query`，同一语句内不变）。语句的 tracker 从 `ExecCtx` 读。
2. **Go 的 goroutine 与 channel（N3 映射规则 M-GO、M-CH）。** Go 的 TopN 在 `Next` 里先装满前 `Offset+Count` 行，再用一个 goroutine 跑 `executeTopN`，结果行写进 `resultChannel`，`Next` 从里面读。Rust 在装载之后直接在执行器的 task 上跑 `executeTopN`，结果行进一个队列，`Next` 从队列读（`RowWithError` 与 Go 相同，队列关闭对应 channel 关闭）。行、顺序、每次 `Next` 返回的行数和错误都与 Go 相同（govec 逐个 chunk 比较）。
3. **panic（M-PANIC）。** Go 的 `Tracker.HandleKillSignal` 用 panic 中止排序，`sortNoLock` 的 `recover` 把它变成错误。Rust 的 `handle_kill_signal` 返回 `Err`，比较函数记下错误、排序在下一次比较时短路，`sort` 返回同一个错误。
4. **排序算法。** `sort.Slice` / `slices.SortFunc` 用 `tidb_base::gostd::sort::sort`（Go pdqsort 的逐行移植），相等键的行的次序因此与 Go 相同（govec 用小值池制造大量相等键来检验）。
5. **共享计划（N13）。** 执行器只读计划的 `ByItems`（`Arc`），每次执行的状态（分区、堆、队列）在执行器里。
6. **内存记账（A1）。** 分区、堆按 Go 的位置 `Consume`，数值是 Rust 的实际字节数，不与 Go 逐字节相同（A1 §3.5）。

## 3. 不移植或停在边界桩的分支

| 位置 | 情况 | 处理 |
|---|---|---|
| `SortExec.Open`（`sort.go:170`） | Go 无条件设 `e.IsUnparallel = false`，生产路径**总是**走并行 sort | 并行 sort 是本部分以外的 D12.5：`Open` 在打开子执行器之前返回 `blocked(D12.5)`，所以 `ORDER BY`（没有 LIMIT 的 Sort）目前停在边界。单分区（unparallel）路径完整移植，经测试钩子 `set_unparallel_for_test` 进入（Go 的测试在 `Open` 之后改 `IsUnparallel`）。负责人已决定照 Go（#329 的评论，2026-11-24，§5 第 1 条的选项 (a)）；并行 sort 另立任务 #339 |
| `SortExec.Open`（`sort.go:186-188`）、`switchToNewSortPartition`（`sort.go:553`）、`TopNExec.Open`（`topn.go:121-157`） | `tidb_enable_tmp_storage_on_oom` 时把落盘动作挂到会话 tracker 上（`FallbackOldAndSetNewAction`），并建 topn worker / spill helper | Rust 的会话还没有内存 tracker（A1 的会话根 tracker 未接入 `SessionVars`），落盘容器也没有移植：这几处是 `blocked-by:D01.10` 的行内桩，动作不挂，所以**永远不会落盘**（#329 的评论里起初建议“挂动作的地方记为 `blocked-by:D15.1`”，那是维护者的建议、不是负责人的决定；tracker 的 `parts.py` 不接受指向已完成块的桩，D15.1 已完成，所以挂动作的两处和动作本身一样记为 D01.10，原因写在桩的说明里：会话 tracker 没有接入，见 A1。维护者在 #329 确认 D01.10 可以，会话内存 tracker 缺失由 #344 跟踪，两处桩旁注明 #344）；内存超限时按语句 tracker 上已有的动作（取消 / 日志）处理 |
| `externalSorting`、`reloadCursor`、`sortPartitionSpillDiskAction.Action`、`TopNExec.fetchChunksFromChild` / `spillTopNExecHeap` / `executeTopNWhenSpillTriggered` / `generateTopNResultsWhenSpillTriggered`、`TestKillSignalInTopN` | 落盘路径 | `blocked-by:D01.10`，返回错误、不静默跳过；没有落盘时到不了 |
| `fetchChunksParallel`、`fetchChunksFromChild`、`generateResult(FromMemory)`、`appendResultToChunkInParallelMode` | 并行 sort | `blocked-by:D12.5`；`Open` 已停，到不了 |
| `TopNExec.Open`（`topn.go:117-119`）、`initBeforeLoadingChunks`、`fetchChunks` 等（`RankInfo` 非空） | rank topn（planner 给 `PhysicalTopN.PrefixCol`） | `Open` 中 Go 为 rank topn 取 `typeCtx` 的分支处返回 `blocked(D12.5)`，在打开子执行器之前：路由器只在执行器打开之前回退到 Go（VS1 plan §4.3），停在 `Next` 会让语句在本节点以 1235 结束。后面的 rank 分支保留为桩（到不了） |
| failpoint | `ParallelSortRandomFail`、`TopNRandomFail`、`SignalCheckpointForSort`、`errorDuringSortRowContainer`、`unholdSyncLock`、`waitForSpill`、`Issue59655`、`Issue63216` | 不移植（测试钩子）；对应函数标 L2，`injectErrorForIssue59655` / `injectPanicForIssue63216` 是空函数 |

## 4. 与 Go 的其他差异（已知差异）

1. **并行路径**：见 §3 第一行（`sort.go:170`）。负责人决定照 Go（#329），Sort 的 `Open` 返回 `blocked(D12.5)`，直到并行 sort（#339）移植；TopN 不受影响（Go 的 TopN 本来就不用并行 sort）。
2. **不落盘**：见 §3 第二行（`sort.go:187`、`sort.go:553`、`topn.go:149`）。同一查询在 Go 里内存超过 `tidb_mem_quota_query` 时会落盘并成功，在 Rust 里按语句 tracker 的动作处理（默认取消，8175）。会话内存 tracker 接入后（#344：新栈的语句还不受内存配额约束）才可能挂落盘动作，届时落盘本身仍是 D01.10。
3. **TopN 的 `executeTopN` 不在 goroutine 里跑**（`topn.go:310`）：见 §2 第 2 条。Go 中 goroutine 的 panic 由 `processPanicAndLog` 写进 `resultChannel`；Rust 中没有 panic 路径（错误按 `Result` 返回），`process_panic_and_log` 仍然移植（L2），只在调用方捕获到 panic 时用。
4. **`closeChannels`**（`sort.go:103`）：Go 的 `Open` 总会建并行 sort 的 channel，排序键被拒绝时 `Next` 关闭它们。Rust 不建这些 channel，`close_channels` 是空函数；unparallel 模式下之后没有人读这些 channel，结果相同（govec 的错误记录覆盖这一路径）。
5. **TopN 的 `Open`** 不建 topn worker 和 spill helper 的落盘部分（`topn.go:127-147`）：它们只在落盘后使用。`TopNExec.Close` 在没有取过数据时只关闭结果队列，不清零 `memTracker`，与 Go（`topn.go:158-185`）相同。
6. **执行器计数器**（`builder.go:2578`、`builder.go:2592` 的 `ExecutorCounterSortExec` / `ExecutorCounterTopNExec`）没有移植：Rust 的 `metrics.rs` 还没有执行器计数器（指标向量是 D14.1），`build_sort` / `build_top_n` 的注释写明。这是 Prometheus 计数，不影响结果。
7. **内存数值**：EXPLAIN ANALYZE 的 `memory` 列、慢日志等的数值来自 Rust 的实际分配，不与 Go 相同（A1 §3.5）。
8. **`Offset + Count` 溢出**：Go 用 `uint64` 相加（`topn.go:133`、`initBeforeLoadingChunks`），溢出时回绕；Rust 用 `wrapping_add` 照搬。`LIMIT 1, 18446744073709551615` 这类上限之和溢出时 Go 的行为（堆的上限变成很小的数）原样复现；govec 没有生成溢出的记录（见 §5 第 3 条）。

## 5. 待定与可能的上游问题

1. **并行 sort 的处理**（已决定，#329 的评论，2026-11-24）：选 (a) 照 Go：`Open` 走并行路径，在并行 sort 移植之前返回 `blocked(D12.5)`（当前实现），没有偏离，不需要裁定。(b) 生产路径改走 unparallel 模式是偏离，不采用；(c) 并行 sort（worker 与多路归并）另立任务 #339，在本 PR 合并后开始。
2. **比较函数按 by-item 下标取**（可能的上游问题，未提交）：`lessRow`、`compareRow`、`greaterRow`、`keyColumnsLess` 用 `keyColumns` 的下标 `i` 去取 `keyCmpFuncs[i]` 和 `ByItems[i].Desc`（`sort.go:804-818`、`topn.go:188-202`），而 `buildKeyColumns` 跳过常量 by-item，`initCompareFuncs` 不跳过。常量排在列前面时，比较函数和 DESC 标志会错位到后面的列上；类型不同时会用错误的比较函数读列（可能读错或越界 panic）。Rust 照搬（govec 的 `gen leading constant` 记录用同类型的常量和列检验错位）。从 SQL 很难触发：`LogicalSort.PruneColumns` → `pruneByItems`（`pkg/planner/core/operator/logicalop/logical_plans_misc.go:128-157`）会去掉没有列、且是运行时常量的 by-item，TopN 的 `ByItems` 取自剪枝之后的 Sort；`RAND()` 这类没有列但不是运行时常量的项会经注入的 projection 变成列。所以预计只有把 `column_prune` 写进 `mysql.opt_rule_blacklist` 时才会出现（未在集群上验证）。已按 CLAUDE.md §2 第 6 条记为 `upstream-pitfall` issue #358（Lucky 在 #349 的评审意见）；照 Go 移植，不需要裁定。
3. **`Offset + Count` 回绕**：`LIMIT` 的 offset 与 count 之和超过 `uint64` 时，Go 的 topn 堆上限回绕；堆为空时 `processChk` 访问 `rowPtrs[0]` 会 panic。是否能从 SQL 触发（planner 可能先把这种 limit 改写掉）没有验证，govec 的生成器不生成这种记录。一并记在 #358。

## 6. 测试

- **govec 领域 `sortexec`**（tracker `govec/domains/sortexec.go`，导出垫片 `govec/fixes/pkg/executor/sortexec/govec_export.go.fixed`，因为 `pkg/executor/internal/exec` 是 internal 包）：Go 的 `SortExec`（`Open` 之后切到 unparallel 模式，与 `sort_spill_test.go` 相同）和不落盘的 `TopNExec`，子执行器每次 `Next` 返回一个固定的 chunk。共 1164 条：`sort` 582 条、`topn` 582 条。每个 op：随机 schema 与排序键 500 条（1 至 3 个列键，可重复，ASC / DESC 混合，有时带尾部常量；每列一半的值来自 4 个值的小池，制造相等键；约 1/7 的值是 NULL），每种 chunk 类型单独作排序键（ASC、DESC 各一次）42 条（整数、无符号、FLOAT、DOUBLE、DECIMAL、DATE / DATETIME / TIMESTAMP、TIME、VARCHAR `utf8mb4_bin` / `utf8mb4_general_ci`、CHAR `utf8mb4_unicode_ci`、BLOB、JSON、VECTOR、ENUM、SET、BIT、NULL 类型），前导常量 30 条，错误 10 条（相关列：`Get unexpected expression`；GEOMETRY 常量：`Sort executor not supports type geometry`）。chunk 大小（init 1–32、max 至 1024）、多批数据、每次 `Next` 前设置的 required rows 都随机；topn 的 offset / count 取 0、行数以内、等于行数、超过行数。新排序规则（new collation）开启。`crates/executor/tests/govec_sortexec.rs` 回放：每个结果 chunk 逐格比较（`Row.GetRaw` 与 null 标志）、错误信息和出错的步骤（open / next / close）。向量文件 `crates/executor/tests/vectors/v8.5.8/{amd64,arm64}/sortexec.jsonl.gz` 从 tracker 原样复制，来源、生成方式和 sha256 见同目录的 `README.md`：tracker CI 的产物在本 PR 的环境中下载不到，按维护者在 #329 的决定，amd64 在本地 x86_64 上生成，arm64 由同一生成器为 linux/arm64 编译后在 qemu-user 下生成，tracker CI 的 `govec.sh --check` 在两个架构上逐字节核对。
- **单测**（`sortexec/tests.rs`，Go 中不依赖存储、不落盘的测试）：`onePartitionAndAllDataInMemoryCase`（`sort_spill_test.go`，含 Go 的结果检查器）、`TestInterruptedDuringSort`（`sortexec_pkg_test.go`，kill 信号）、`topNNoSpillCase`（`topn_spill_test.go`，多组 offset / count）；另加：`Open` 停在并行路径、空输入、常量 by-item 被跳过、相关列和 GEOMETRY 的错误、count 为 0 与 offset 超过行数、topn 堆的压缩（`doCompaction`）、topn 遵守 required rows、rank topn 停在 `Open`；`heap.rs`、`sort_util.rs` 的小测试。
- **builder**（`builder.rs` 的 `builds_sort_and_top_n`）：TopN 计划建出的执行器返回正确的行；Sort 计划的 `Open` 返回 `blocked(D12.5)`；计划类型不是 `PhysicalSort` / `PhysicalTopN` 时报错。
- 落盘、并行 sort 的 Go 测试（`sort_spill_test.go` 的落盘用例、`parallel_sort_*_test.go`、`topn_spill_test.go` 的落盘用例、`rank_topn_test.go`）随 D01.10 / 本块其余部分移植。
- ab `Sort` / `TopN`：按 `docs/process/agent-workflow.md`（2026-11-24 起），会让原来回退的语句改走新栈的 PR 要在**合并前**由维护者跑 `tools/ab`。本 PR 中 `Sort` 在并行 sort（#339）移植之前停在 `Open` 的 `blocked(D12.5)`，路由器仍把语句交给 Go；会改走新栈的只有 TopN，而且只在它的子执行器已经能建出时（Selection / Projection / Limit / TableDual 仍是 `blocked-by:D12.2`，TableReader 是 `blocked-by:D12.1`，目前只有 PointGet 能建出）。
