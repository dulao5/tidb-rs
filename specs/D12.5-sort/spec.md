# D12.5 排序类执行器（spec）

状态：第 1 部分（内存中的 Sort / TopN，VS1-36）已合并（#349）；第 2 部分（并行 sort，§7）评审中（2026-11-24）。块：D12.5（ROADMAP P4），crate `tidb-executor`（N0 rank 14）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（D12.5 一行）、§3.2（边界桩）；[D12.0 tasks](../D12.0-executor/tasks.md)（执行器框架，VS1-13）；[N3/A1](../exceptions/N3-A1-concurrency-memory/spec.md)（并发与内存记账）；[N13](../exceptions/N13-shared-metadata/spec.md)（共享元数据）。任务：tidb-rs#329（第 1 部分）、tidb-rs#339（第 2 部分）。

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

converge：`scripts/converge.py --block D12.5 --part specs/VS1-vertical-slice/functions.tsv --item VS1-36 --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv <crates/executor/src/sortexec/ 下除 tests.rs 以外的全部文件> crates/executor/src/builder.rs`：第 1 部分合并时为 59/65 移植 + 6 个边界桩；第 2 部分移植了其中的三个并行 sort 桩（`SortExec.fetchChunksFromChild`、`generateResult`、`generateResultFromMemory`）之后为 62/65 + 3 个边界桩，都是 D01.10（落盘容器）：`reloadCursor`、`TopNExec.fetchChunksFromChild`、`TestKillSignalInTopN`。

### 1.2 第 2 部分（并行 sort，#339）

函数清单是 `specs/D12.5-sort/functions.tsv` 中 item `D12.5-PS`（并行 sort）的 46 个函数，细节见 §7。

| Go | Rust（`crates/executor/src/`） | 内容 |
|---|---|---|
| `sortexec/parallel_sort_worker.go` | `sortexec/parallel_sort_worker.rs` | `parallelSortWorker` 的全部 12 个函数：收 chunk、每 `maxChunkSize*30` 行排一次序、通道关闭后多路归并本地的有序段 |
| `sortexec/multi_way_merge.go` | `sortexec/multi_way_merge.rs` | 全部 17 个函数：`memorySource`、`diskSource`、`sortPartitionSource`、`multiWayMergeImpl`（`heap.Interface`）、`multiWayMerger` |
| `sortexec/sort.go` | `sortexec/sort.rs` | 并行分支：`fetchChunksParallel`、`appendResultToChunkInParallelMode`、`spillRemainingRowsWhenNeeded`、`checkSpillAndExecute`、`generateResultFromDisk`、`generateResultWithMultiWayMerge`、`spillSortedRowsInMemory`、测试用的 getter（`IsSpillTriggeredInParallelSortForTest`、`GetSpilledRowNumInParallelSortForTest`、`GetSortMetaForTest`）；第 1 部分清单里的 `fetchChunksFromChild`、`generateResult`、`generateResultFromMemory` 也在这一部分移植 |
| `sortexec/sort_util.go` | `sortexec/sort_util.rs` | `injectParallelSortRandomFail`（failpoint 不移植，空函数）、`rowWithPartition`、`chunkWithMemoryUsage` |
| `sortexec/parallel_sort_spill_helper.go` | `sortexec/parallel_sort_spill_helper.rs` | 不落盘时用到的部分：`newParallelSortSpillHelper`、`close`、`isSpillNeeded`、`isSpillTriggered`；`spill`、`spillImpl` 是 `blocked-by:D01.10` 的桩 |

清单外随类型一起写出的：Go 标准库 `container/heap` 的 `Remove`（`heap.rs`，go-std，多路归并用）；`chunk.Iterator4Slice` 的部分（`SortedRowsIter`，glue，D01.7 的 chunk crate 里还没有）；`util.WaitGroupWrapper` 与 `sync.WaitGroup`（`WaitGroup`，glue）；chunk crate 新增 `RowHandle`（持有 `Arc<Chunk>` 的行，见 §7.2）。

converge：`scripts/converge.py --block D12.5 --part specs/D12.5-sort/functions.tsv --item D12.5-PS --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv <crates/executor/src/sortexec/ 下除 tests.rs 以外的全部文件> crates/executor/src/builder.rs` 为 44/46 移植 + 2 个边界桩（`parallelSortSpillHelper.spill`、`spillImpl` → D01.10）。

### 1.3 块内其余（`blocked-by:D12.5`、`blocked-by:D01.10`）

- **落盘**：sort 分区落盘、多分区归并（`externalSorting`）、topn worker 与落盘（`topn_worker.go`、`topn_spill.go` 的落盘部分）。落盘依赖 D01.10 的落盘容器（`DataInDiskByChunks`）。
- **rank topn**：`RankInfo` 前缀截断的 topn（`executeRankTopN` 等）。

## 2. 承载方式

1. **会话（N2）。** Go 的执行器保存 `e.Ctx()`，在 `Open` / `Next` 中读会话。Rust 的执行器在建立时拿到它要读的会话值：表达式上下文（`initCompareFuncs` 取类型）、会话内存限额（`spillLimit = 会话 tracker 的限额 / 10`，Go 在 `Open` 读，Rust 在 build 时读 `tidb_mem_quota_query`，同一语句内不变）。语句的 tracker 从 `ExecCtx` 读。
2. **Go 的 goroutine 与 channel（N3 映射规则 M-GO、M-CH）。** 并行 sort 的映射见 §7.2。Go 的 TopN 在 `Next` 里先装满前 `Offset+Count` 行，再用一个 goroutine 跑 `executeTopN`，结果行写进 `resultChannel`，`Next` 从里面读。Rust 在装载之后直接在执行器的 task 上跑 `executeTopN`，结果行进一个队列，`Next` 从队列读（`RowWithError` 与 Go 相同，队列关闭对应 channel 关闭）。行、顺序、每次 `Next` 返回的行数和错误都与 Go 相同（govec 逐个 chunk 比较）。
3. **panic（M-PANIC）。** Go 的 `Tracker.HandleKillSignal` 用 panic 中止排序，`sortNoLock` 的 `recover` 把它变成错误。Rust 的 `handle_kill_signal` 返回 `Err`，比较函数记下错误、排序在下一次比较时短路，`sort` 返回同一个错误。
4. **排序算法。** `sort.Slice` / `slices.SortFunc` 用 `tidb_base::gostd::sort::sort`（Go pdqsort 的逐行移植），相等键的行的次序因此与 Go 相同（govec 用小值池制造大量相等键来检验）。
5. **共享计划（N13）。** 执行器只读计划的 `ByItems`（`Arc`），每次执行的状态（分区、堆、队列）在执行器里。
6. **内存记账（A1）。** 分区、堆按 Go 的位置 `Consume`，数值是 Rust 的实际字节数，不与 Go 逐字节相同（A1 §3.5）。

## 3. 不移植或停在边界桩的分支

| 位置 | 情况 | 处理 |
|---|---|---|
| `SortExec.Open`（`sort.go:170`） | Go 无条件设 `e.IsUnparallel = false`，生产路径**总是**走并行 sort | 照 Go：第 2 部分（§7）移植了并行 sort，`Open` 走并行路径。单分区（unparallel）路径经测试钩子 `set_unparallel_for_test` 进入（Go 的测试在 `Open` 之后改 `IsUnparallel`）。负责人的决定见 §5 第 1 条 |
| `SortExec.Open`（`sort.go:182-188`）、`switchToNewSortPartition`（`sort.go:553`）、`TopNExec.Open`（`topn.go:121-157`） | `tidb_enable_tmp_storage_on_oom` 时把落盘动作挂到会话 tracker 上（`FallbackOldAndSetNewAction`），并建 topn worker / spill helper | Rust 的会话还没有内存 tracker（A1 的会话根 tracker 未接入 `SessionVars`），落盘容器也没有移植：这几处是 `blocked-by:D01.10` 的行内桩，动作不挂，所以**永远不会落盘**（#329 的评论里起初建议“挂动作的地方记为 `blocked-by:D15.1`”，那是维护者的建议、不是负责人的决定；tracker 的 `parts.py` 不接受指向已完成块的桩，D15.1 已完成，所以挂动作的两处和动作本身一样记为 D01.10，原因写在桩的说明里：会话 tracker 没有接入，见 A1。维护者在 #329 确认 D01.10 可以，会话内存 tracker 缺失由 #344 跟踪，两处桩旁注明 #344）；内存超限时按语句 tracker 上已有的动作（取消 / 日志）处理 |
| `externalSorting`、`reloadCursor`、`sortPartitionSpillDiskAction.Action`、`parallelSortSpillHelper.spill` / `spillImpl`、`TopNExec.fetchChunksFromChild` / `spillTopNExecHeap` / `executeTopNWhenSpillTriggered` / `generateTopNResultsWhenSpillTriggered`、`TestKillSignalInTopN` | 落盘路径 | `blocked-by:D01.10`，返回错误、不静默跳过；没有落盘时到不了 |
| `generateResultFromDisk`、`generateResultWithMultiWayMerge`、`spillSortedRowsInMemory`、`diskSource`、`sortPartitionSource` | 并行 sort 落盘之后的路径 | 照 Go 移植（L1 / L2），但没有落盘就到不了：落盘的入口（`spill`、`spillImpl`）是 D01.10 的桩，落盘动作也不挂（上一行），`isSpillTriggered` 永远为假 |
| `TopNExec.Open`（`topn.go:117-119`）、`initBeforeLoadingChunks`、`fetchChunks` 等（`RankInfo` 非空） | rank topn（planner 给 `PhysicalTopN.PrefixCol`） | `Open` 中 Go 为 rank topn 取 `typeCtx` 的分支处返回 `blocked(D12.5)`，在打开子执行器之前：路由器只在执行器打开之前回退到 Go（VS1 plan §4.3），停在 `Next` 会让语句在本节点以 1235 结束。后面的 rank 分支保留为桩（到不了） |
| failpoint | `ParallelSortRandomFail`、`SlowSomeWorkers`、`TopNRandomFail`、`SignalCheckpointForSort`、`errorDuringSortRowContainer`、`unholdSyncLock`、`waitForSpill`、`Issue59655`、`Issue63216` | 不移植（测试钩子）；对应函数标 L2，`injectErrorForIssue59655` / `injectPanicForIssue63216` / `injectParallelSortRandomFail` 是空函数 |

## 4. 与 Go 的其他差异（已知差异）

1. **并行路径**：已照 Go 移植（§7），没有偏离；第 1 部分合并时 Sort 的 `Open` 停在 `blocked(D12.5)`，第 2 部分去掉了这个边界。并行 sort 本身与 Go 的差异见 §7.8。
2. **不落盘**：见 §3 第二行（`sort.go:187`、`sort.go:553`、`topn.go:149`）。同一查询在 Go 里内存超过 `tidb_mem_quota_query` 时会落盘并成功，在 Rust 里按语句 tracker 的动作处理（默认取消，8175）。会话内存 tracker 接入后（#344：新栈的语句还不受内存配额约束）才可能挂落盘动作，届时落盘本身仍是 D01.10。
3. **TopN 的 `executeTopN` 不在 goroutine 里跑**（`topn.go:310`）：见 §2 第 2 条。Go 中 goroutine 的 panic 由 `processPanicAndLog` 写进 `resultChannel`；Rust 中没有 panic 路径（错误按 `Result` 返回），`process_panic_and_log` 仍然移植（L2），只在调用方捕获到 panic 时用。
4. **`closeChannels`**（`sort.go:103`）：第 2 部分起与 Go 相同：`Open` 建并行 sort 的 channel，`close_channels` 丢掉两个发送端（`close(ch)` → drop 发送端，M-CH）。
5. **TopN 的 `Open`** 不建 topn worker 和 spill helper 的落盘部分（`topn.go:127-147`）：它们只在落盘后使用。`TopNExec.Close` 在没有取过数据时只关闭结果队列，不清零 `memTracker`，与 Go（`topn.go:158-185`）相同。
6. **执行器计数器**（`builder.go:2578`、`builder.go:2592` 的 `ExecutorCounterSortExec` / `ExecutorCounterTopNExec`）没有移植：Rust 的 `metrics.rs` 还没有执行器计数器（指标向量是 D14.1），`build_sort` / `build_top_n` 的注释写明。这是 Prometheus 计数，不影响结果。
7. **内存数值**：EXPLAIN ANALYZE 的 `memory` 列、慢日志等的数值来自 Rust 的实际分配，不与 Go 相同（A1 §3.5）。
8. **`Offset + Count` 溢出**：Go 用 `uint64` 相加（`topn.go:133`、`initBeforeLoadingChunks`），溢出时回绕；Rust 用 `wrapping_add` 照搬。`LIMIT 1, 18446744073709551615` 这类上限之和溢出时 Go 的行为（堆的上限变成很小的数）原样复现；govec 没有生成溢出的记录（见 §5 第 3 条）。

## 5. 待定与可能的上游问题

1. **并行 sort 的处理**（已决定，#329 的评论，2026-11-24）：选 (a) 照 Go：`Open` 走并行路径，在并行 sort 移植之前返回 `blocked(D12.5)`，没有偏离，不需要裁定。(b) 生产路径改走 unparallel 模式是偏离，不采用；(c) 并行 sort（worker 与多路归并）另立任务 #339，即本 spec 的第 2 部分（§7）。
2. **比较函数按 by-item 下标取**（可能的上游问题，未提交）：`lessRow`、`compareRow`、`greaterRow`、`keyColumnsLess` 用 `keyColumns` 的下标 `i` 去取 `keyCmpFuncs[i]` 和 `ByItems[i].Desc`（`sort.go:804-818`、`topn.go:188-202`），而 `buildKeyColumns` 跳过常量 by-item，`initCompareFuncs` 不跳过。常量排在列前面时，比较函数和 DESC 标志会错位到后面的列上；类型不同时会用错误的比较函数读列（可能读错或越界 panic）。Rust 照搬（govec 的 `gen leading constant` 记录用同类型的常量和列检验错位）。从 SQL 很难触发：`LogicalSort.PruneColumns` → `pruneByItems`（`pkg/planner/core/operator/logicalop/logical_plans_misc.go:128-157`）会去掉没有列、且是运行时常量的 by-item，TopN 的 `ByItems` 取自剪枝之后的 Sort；`RAND()` 这类没有列但不是运行时常量的项会经注入的 projection 变成列。所以预计只有把 `column_prune` 写进 `mysql.opt_rule_blacklist` 时才会出现（未在集群上验证）。已按 CLAUDE.md §2 第 6 条记为 `upstream-pitfall` issue #358（Lucky 在 #349 的评审意见）；照 Go 移植，不需要裁定。
3. **`Offset + Count` 回绕**：`LIMIT` 的 offset 与 count 之和超过 `uint64` 时，Go 的 topn 堆上限回绕；堆为空时 `processChk` 访问 `rowPtrs[0]` 会 panic。是否能从 SQL 触发（planner 可能先把这种 limit 改写掉）没有验证，govec 的生成器不生成这种记录。一并记在 #358。

## 6. 测试

- **govec 领域 `sortexec`**（tracker `govec/domains/sortexec.go`，导出垫片 `govec/fixes/pkg/executor/sortexec/govec_export.go.fixed`，因为 `pkg/executor/internal/exec` 是 internal 包）：Go 的 `SortExec`（op `sort`：`Open` 之后切到 unparallel 模式，与 `sort_spill_test.go` 相同；op `psort`：并行 sort，见 §7.6）和不落盘的 `TopNExec`，子执行器每次 `Next` 返回一个固定的 chunk。共 1301 条：`sort` 582 条、`topn` 582 条、`psort` 137 条（第 2 部分新增，其中 6 条是错误记录）。每个 op：随机 schema 与排序键 500 条（1 至 3 个列键，可重复，ASC / DESC 混合，有时带尾部常量；每列一半的值来自 4 个值的小池，制造相等键；约 1/7 的值是 NULL），每种 chunk 类型单独作排序键（ASC、DESC 各一次）42 条（整数、无符号、FLOAT、DOUBLE、DECIMAL、DATE / DATETIME / TIMESTAMP、TIME、VARCHAR `utf8mb4_bin` / `utf8mb4_general_ci`、CHAR `utf8mb4_unicode_ci`、BLOB、JSON、VECTOR、ENUM、SET、BIT、NULL 类型），前导常量 30 条，错误 10 条（相关列：`Get unexpected expression`；GEOMETRY 常量：`Sort executor not supports type geometry`）。chunk 大小（init 1–32、max 至 1024）、多批数据、每次 `Next` 前设置的 required rows 都随机；topn 的 offset / count 取 0、行数以内、等于行数、超过行数。`psort`：worker 数取 1、2、3、5、8、20（`tidb_executor_concurrency`），随机记录 100 条、每种 chunk 类型单独作排序键各 1 条、前导常量 10 条、相关列与 GEOMETRY 的错误各 3 条，chunk 大小 init 取 1 / 2 / 4、max 取 1 / 2 / 4 / 7 / 32（让一个 worker 排多段、多路归并），比较函数不能给出一致顺序的值换掉（FLOAT / DOUBLE 的 NaN 换成 0，大小写不敏感排序规则下字符串中不是合法 UTF-8 的字节换成 `a`，见 §7.6）。新排序规则（new collation）开启。`crates/executor/tests/govec_sortexec.rs` 回放（tokio 多线程运行时，worker 真正并行）：每个结果 chunk 逐格比较（`Row.GetRaw` 与 null 标志）、错误信息和出错的步骤（open / next / close）；`psort` 记录先按 §7.6 整理相等键的行再比较。向量文件 `crates/executor/tests/vectors/v8.5.8/{amd64,arm64}/sortexec.jsonl.gz` 从 tracker 原样复制，来源、生成方式和 sha256 见同目录的 `README.md`：tracker CI 的产物在本 PR 的环境中下载不到，按维护者在 #329 的决定，amd64 在本地 x86_64 上生成，arm64 由同一生成器为 linux/arm64 编译后在 qemu-user 下生成，tracker CI 的 `govec.sh --check` 在两个架构上逐字节核对。
- **并行 sort 的单测**（`sortexec/tests.rs`，第 2 部分）：`parallel_sort_test.go` 中不落盘、不依赖存储的测试：`TestParallelSort`（30000 行、两个排序键、新建与复用执行器各几轮，worker 数 5 / 1 / 2 / 8）→ `parallel_sort`；`TestFailpoint` 去掉 failpoint 后的配置（20 个 worker，即结果 channel 容量的两倍，65536 行）→ `parallel_sort_many_workers`；检查与 Go 的 `checkCorrectness` 相同：按排序键有序、行集合相同。`TestIssue55344` 要建表（testkit 与存储），不在这里。另加：两种 ExecPool 拓扑（Steal / NoSteal）上运行；空输入与 `Open` 后直接 `Close`；提前结束（只读一个 chunk 就 `Close`）、子执行器出错、kill、不 `Close` 就 drop 执行器、没有运行时——每个用例都断言所有 task 已结束（`Close` 之后没有未 await 的 task、`fetcherAndWorkerSyncer` 归零、worker 的有序行没有别的持有者、子执行器已归还并关闭、内存 tracker 归零；drop 用例断言子执行器随 task 被释放）。时间片检查点：worker 的最后归并和生成器的归并在插桩的 task 中（`ExecPool`，时间片 200 µs，不受 tokio 预算影响）确实让出（`merge_of_local_sorted_rows_yields`、`generate_result_from_memory_yields`；去掉检查点时两个测试都失败）。worker 的 task 中排序键比较函数 panic，端到端：`run` 在 task 边界捕获，把错误送进结果 channel，计数器由 drop guard 归零，task 正常结束（`panic_of_a_sort_key_function_goes_to_the_result_channel`）。`multi_way_merge.rs`、`sort_util.rs`（`processPanicAndLog` 把恢复的 panic 送进 channel）的小测试。
- **单测**（`sortexec/tests.rs`，第 1 部分，Go 中不依赖存储、不落盘的测试）：`onePartitionAndAllDataInMemoryCase`（`sort_spill_test.go`，含 Go 的结果检查器）、`TestInterruptedDuringSort`（`sortexec_pkg_test.go`，kill 信号）、`topNNoSpillCase`（`topn_spill_test.go`，多组 offset / count）；另加：空输入、常量 by-item 被跳过、相关列和 GEOMETRY 的错误、count 为 0 与 offset 超过行数、topn 堆的压缩（`doCompaction`）、topn 遵守 required rows、rank topn 停在 `Open`；`heap.rs`、`sort_util.rs` 的小测试。
- **builder**（`builder.rs` 的 `builds_sort_and_top_n`）：TopN 计划和 Sort 计划建出的执行器都返回正确的行（Sort 走并行路径）；计划类型不是 `PhysicalSort` / `PhysicalTopN` 时报错。
- 落盘的 Go 测试（`sort_spill_test.go` 的落盘用例、`parallel_sort_spill_test.go`、`topn_spill_test.go` 的落盘用例）、`rank_topn_test.go` 随 D01.10 / 本块其余部分移植。
- ab `Sort` / `TopN`：按 `docs/process/agent-workflow.md`（2026-11-24 起），会让原来回退的语句改走新栈的 PR 要在**合并前**由维护者跑 `tools/ab`。第 1 部分中 `Sort` 停在 `Open` 的 `blocked(D12.5)`，路由器仍把语句交给 Go；会改走新栈的只有 TopN，而且只在它的子执行器已经能建出时（Selection / Projection / Limit / TableDual 仍是 `blocked-by:D12.2`，TableReader 是 `blocked-by:D12.1`，目前只有 PointGet 能建出）。第 2 部分去掉了 Sort 的边界：子执行器能建出时，不带 LIMIT 的 `ORDER BY` 改走新栈，所以第 2 部分的 PR 同样要在合并前跑 `tools/ab`。

## 7. 并行 sort（第 2 部分，#339）

### 7.1 结构

Go 的 `SortExec.Open` 建两个 channel（`chunkChannel`、`resultChannel`）、`finishCh`、计数器 `fetcherAndWorkerSyncer`、每个 worker 一个有序行迭代器、内存中的多路归并器和 spill helper（`sort.go:155-192`）。第一次 `Next` 建好排序键之后由 `fetchChunksParallel`（`sort.go:661-682`）启动：

- `ExecutorConcurrency` 个 worker（`parallelSortWorker.run`）：从 `chunkChannel` 收 chunk，攒够 `maxChunkSize*30` 行就排一次序成为一段（`sortBatchRows`），通道关闭后把剩余的行排序，再把自己的各段多路归并成一个有序序列，放进自己的迭代器（`fetchChunksAndSortImpl`、`sortLocalRows`、`multiWayMergeLocalSortedRows`）；
- 一个 fetcher（`fetchChunksFromChild`）：读子执行器，把每个 chunk 记账后送进 `chunkChannel`，结束时等所有 worker 处理完已发出的 chunk，再关闭 `chunkChannel`；
- 一个结果生成器（`generateResult`）：等 worker 和 fetcher 都结束，把各 worker 的有序序列多路归并，逐行送进 `resultChannel`，最后关闭它；
- `Next`（`appendResultToChunkInParallelMode`）从 `resultChannel` 读行，直到 `req` 满、读到错误或 channel 关闭。

Rust 的结构逐个对应（`sort.rs`、`parallel_sort_worker.rs`、`multi_way_merge.rs`、`parallel_sort_spill_helper.rs`）。Go 的 goroutine 直接读写 `e.Parallel` 和 `e.Children(0)`；Rust 的每个 task 拿到自己的状态（`ParallelSortWorker`、`ChunkFetcher`、`ResultGenerator`），共享的部分是 `Arc`（M-SHARE），所以 `fetchChunksFromChild`、`generateResult*`、`spillSortedRowsInMemory`、`checkSpillAndExecute`、`spillRemainingRowsWhenNeeded` 是这些状态上的方法（L2）。

### 7.2 goroutine 与 channel 到 tokio task 的映射（N3-A1 §3.2 的 M-GO、M-CH、M-WG）

| Go | Rust |
|---|---|
| `workersWaiter.Run(worker.run)`、`fetcherWaiter.Run(e.fetchChunksFromChild)`、`go e.generateResult(...)`（`sort.go:667-680`） | 每个 goroutine 一个 task（M-GO），数量与 Go 相同。task 交给 `ExecCtx` 的 `ExecPool`（`ExecPool::spawn`）；`ExecCtx` 没有池时交给当前的 tokio 运行时（N3-A1 §3.2 第 2 条：进程只有一个运行时）。会话目前还不把它的池放进 `ExecCtx`（`ExecCtx::new` 给 `None`），所以生产中 task 在当前运行时上，见 §7.8 第 2 条。`WaitGroupWrapper.Run` 是 `Spawner::run_in`：先 `add(1)`，再把一个 drop guard（`DoneGuard`）移进 task，task 结束、panic 或被取消时都会 `Done` |
| fetcher goroutine 读 `e.Children(0)` | 子执行器移进 fetcher task，task 结束时作为 `JoinHandle` 的返回值交回（N3-A1 §3.5 第 1 条的模板），`Close` 取回后放回执行器再关闭 |
| `chunkChannel`：容量 `ExecutorConcurrency`，多个 worker 接收（`sort.go:176`） | tokio 有界 `mpsc`，容量相同；tokio 的接收端只有一个，所以放在 tokio 的异步锁后面，由各 worker 和 `Close` 共享（M-CH） |
| `resultChannel`：容量 `ResultChannelCapacity` = 10（`sort.go:179`） | tokio 有界 `mpsc`，容量 10 |
| `close(ch)`（`closeChannels`、fetcher 的 defer、`generateResult` 的 defer） | 丢掉发送端：最后一个发送端被 drop 时 channel 关闭。fetcher 持有 `chunkChannel` 唯一的发送端，在 Go 关闭它的位置 drop。`resultChannel` 的发送端由 worker、fetcher、生成器各持一份：worker 的 task 结束时先丢掉它的那份（`exit()`），再由 `DoneGuard` 计数；生成器等到所有 worker 和 fetcher 结束才开始，它在 Go 关闭 channel 的位置丢掉最后一份，所以 channel 关闭的时刻与 Go 相同 |
| `finishCh`：`Close` 中 `close(e.finishCh)` | tokio `watch` channel：`Close` 写入 `true`，每个 task 在 `select!` 中等待它（`FinishCh::closed`），与 Go 的 `select { case <-finishCh: ... }` 对应 |
| `fetcherAndWorkerSyncer`（`sync.WaitGroup`）；worker 的 `defer p.fetcherAndWorkerSyncer.Done()`（`parallel_sort_worker.go:222`） | `WaitGroup`（glue：计数器加 `Notify`）；`Done` 用 drop guard，所以提前返回、出错（`?`）和 unwind 时都会执行 |
| `chunk.Row`（指向 chunk 内部）、`chunk.Iterator4Slice` | `RowHandle`（`Arc<Chunk>` 加行号，M-ROW）：worker 保留它收到的 chunk，有序序列里的行和结果行都指向这些 chunk；每个 worker 的迭代器是 `Arc<Mutex<SortedRowsIter>>`，与生成器、多路归并器共享（M-SHARE，`parking_lot`，M-LOCK） |
| `slices.SortFunc(rows, p.keyColumnsLess)` | `tidb_base::gostd::sort`（Go pdqsort 的逐行移植），同一个 worker 内相等键的次序与 Go 相同 |
| 归并器的 `e.lessRow`（闭包，读 `Next` 之后才建好的 `e.keyColumns`） | `SortKeys` 值：`Open` 建的归并器在 `fetchChunksParallel` 里换成 `Next` 建好的排序键，比较结果相同 |
| Go 的抢占式调度（逐行循环不需要让出） | 两个逐行的归并循环有时间片检查点（N3-A1 §3.1 第 5 条，`CheckpointStride`，每 100 行读一次时钟）：worker 结束时归并本地各段（`multiWayMergeLocalSortedRows`，放在 Go 每 100 行检查 kill 信号的位置，`parallel_sort_worker.go:139`）和生成器的归并（`generateResultFromMemory`，每归并一行；结果 channel 有空位时发送不会让出）。task 在 `ExecPool` 的插桩下运行时，当前 poll 超过时间片（默认 1 ms）就让出；不经过池时（§7.8 第 2 条，接入池之前）检查点不让出。每批排序（`sortBatchRows`）中间不能让出，耗时见 §7.8 第 9 条 |

### 7.3 worker 数

worker 数和 `chunkChannel` 的容量都等于会话变量 `tidb_executor_concurrency`（`ExecutorConcurrency`，默认 5，系统变量的下限是 1，`sort.go:175-176`），与 N3-A1 §3.2 第 3 条一致：task 数严格等于 Go 的 goroutine 数，物理并行度由运行时的线程数决定。Go 在 `Open` 中读这个变量，Rust 在 build 时读（`new_sort_exec` 的参数 `executor_concurrency`，与 §2 第 1 条的其他会话值相同），同一语句内不变。每个 worker 每攒够 `maxChunkSize*30` 行排一次序（`parallel_sort_worker.go:82`）。

### 7.4 取消与关闭顺序

`Close` 的顺序逐行对应 Go（`sort.go:109-151`），只多一步（N3-A1 §3.5 第 1 条）：

1. 关闭 `finishCh`：fetcher（在 `chunkChannel` 上等着发送）、worker（等着接收）、生成器（等着发送结果行）都在下一次等待时退出；
2. 没有取过数据（`fetched` 为假，没有 task 启动）时关闭两个 channel；否则排空 `chunkChannel`，每取出一个 chunk 调一次 `Done`；
3. 排空 `resultChannel`，直到生成器关闭它（`channel.Clear`）。生成器关闭它时，worker 和 fetcher 都已结束；
4. **（多出的一步）await 所有 task 的 `JoinHandle`**：生成器、fetcher（交回子执行器）、每个 worker（交回 worker 状态，供下一步 `reset`）。task 没有正常结束（被取消，或运行时关闭）时，这个错误在后面几步做完之后返回；
5. 每个 worker `reset`、归并器置空、spill helper `close`（Go 的 `spillAction` 在 Rust 中没有建，见 §7.7）；
6. 内存 tracker 清零（`ReplaceBytesUsed(0)`）；
7. 关闭子执行器（子执行器在执行器里时；第 4 步的错误优先返回）。

提前结束（上层不再读，例如外层的 Limit）走同一条路：`Close` 的第 1 步唤醒所有 task，第 2、3 步收掉 channel 里剩下的东西。

kill：worker 在比较中每 20000 次检查一次 kill 信号（`keyColumnsLess`，`parallel_sort_worker.go:236-254`），多路归并本地有序段时每 100 行检查一次（`parallel_sort_worker.go:139-144`）。检查点与 Go 相同；channel 上的等待不会被 kill 唤醒，与 Go 相同（N3-A1 §3.3 第 2 条的语句级取消 token 还不在 `ExecCtx` 里），`Close` 总能经 `finishCh` 唤醒它们。

不 `Close` 就 drop 执行器（出错路径、unwind）：执行器持有所有 task 的 `JoinHandle`，drop 时取消（abort）它们，task 在下一个等待点结束，fetcher 持有的子执行器随 task 一起释放（N3-A1 §3.5 第 2 条的安全网；那里写的是取消并 join，Rust 的 `Drop` 不能 await，所以只取消，见 §7.8 第 4 条）。

### 7.5 panic 恢复

Go 的三种 goroutine 都有 `defer func() { if r := recover(); r != nil { processPanicAndLog(...) } }()`（`parallel_sort_worker.go:257-261`、`sort.go:702-705`、`sort.go:435-438`）：把 `util.GetRecoverError(r)` 送进 `resultChannel`（阻塞地送），记一条 `executor panicked` 警告日志，`Next` 把它作为错误返回。

Rust 在每个 task 的函数体外面 `catch_unwind`（M-PANIC：只在 task 边界，与 copr 的做法相同），捕获到的 panic 经 `process_panic_and_log`（L2）送进结果 channel（等到有空位再送，与 Go 相同）并记同一条日志。Go 中用 panic 表示的两种错误——比较中的 kill 信号（`keyColumnsLess` 中 `panic(err)`）和超过内存配额（`Tracker.Consume` 的动作）——在 Rust 中按 N3-A1 §3.4 第 1 条以 `Result` 返回：比较函数记下错误，这次排序剩下的比较立即返回（与第 1 部分的 `sortPartition` 相同），错误从 `run` 经同一个 `process_panic_and_log` 送出，用户看到同一个错误。`GetRecoverError`：panic 的值是错误时就是这个错误，是字符串时是这段文字（Go 的 `%v`）。锁用 `parking_lot`，panic 不会毒化锁（M-LOCK）。

### 7.6 相等排序键的行的次序不比较

哪个 worker 收到哪个 chunk 取决于调度，各 worker 内的排序和最后的多路归并都只保证排序键的顺序，所以排序键相等的行之间的先后在 Go 里每次运行都可能不同。Rust 不复现、也不比较这个次序：

- govec 的 `psort` 记录（tracker `govec/domains/sortexec.go` 的 `sortexecTieOrder`）：在 Go 的输出中，把每一段相邻且按 `compareRow`（排序键、比较函数取自 `GetSortMetaForTest`）比较相等的行，按各格的编码（null 标志、`Row.GetRaw` 的十六进制，null 格也带上它下面的字节）重新排列；Rust 的回放（`govec_sortexec.rs` 的 `tie_order`）对自己的输出做同样的整理再逐格比较。因此比较的是：排序键的顺序、每次 `Next` 返回的行数、行的集合（多重集合）、错误和出错的步骤。
- 比较函数不能给出一致顺序的值不进 `psort` 记录（生成器的 `sortexecConsistent`）：这种值使"相等"不满足传递性，排序结果连排序键的顺序都取决于哪些行在同一个 worker 里比较过，同样取决于调度。一是 FLOAT / DOUBLE 的 NaN（NaN 与任何值比较都"相等"）；二是大小写不敏感的排序规则（`utf8mb4_general_ci`、`utf8mb4_unicode_ci` 等）下不是合法 UTF-8 的字节：`compareCommon`（`pkg/util/collate/collate.go:384-386`）遇到这种字节就返回 0，于是 `"c\xa9"` 与 `"ca"`、`"cb"` 都相等，而 `"ca"` 与 `"cb"` 不等。第一版向量没有排除后者时，amd64 与 arm64 的输出在 11 条记录上不同。Rust 照搬这两种比较（不是差异），只是这种输入的并行排序结果在 Go 里本身就不确定。
- 单测（§6）与 Go 的 `checkCorrectness` 一样只检查按排序键有序和行集合相同。
- 第二种不一致已记为 `upstream-pitfall` issue #365（附 SQL 复现，未在集群上运行）。

### 7.7 落盘的边界

spill helper 移植了不落盘时用到的部分（状态、`isSpillNeeded`、`isSpillTriggered`、`close`）。Go 在 `Open` 中建落盘动作并在 `tidb_enable_tmp_storage_on_oom` 时挂到会话 tracker 上（`sort.go:182-188`），Rust 在这里是 `blocked-by:D01.10` 的行内桩（与第 1 部分 §3 第二行的原因相同：落盘容器没有移植，会话内存 tracker 也还不在 Rust 的会话里，#344），所以 spill helper 的状态永远是"未落盘"，`spill` / `spillImpl` 是返回 `blocked(D01.10)` 的桩、到不了。落盘之后的路径（`generateResultFromDisk`、`generateResultWithMultiWayMerge`、`spillSortedRowsInMemory`、`diskSource`、`sortPartitionSource`）照 Go 移植，同样到不了。超过 `tidb_mem_quota_query` 时按语句 tracker 上已有的动作处理（默认取消，8175），见 §4 第 2 条。

### 7.8 与 Go 的差异（并行 sort）

1. **没有 tokio 运行时也没有池时不能启动 task**（`sort.go:661-681`：Go 总能启动 goroutine）：`fetchChunksParallel` 关闭两个 channel 并返回错误 `the parallel sort needs a tokio runtime to run its tasks`，`Next` 返回它，`Close` 照常关闭子执行器。生产中执行器总在运行时上，只有测试能走到。
2. **task 不经过会话的 `ExecPool`**：会话还不把池放进 `ExecCtx`，task 由当前运行时的 `spawn` 启动，N3 的 task 计数和停顿检测看不到它们（N3-A1 §3.6 的插桩只覆盖经 `ExecPool` 启动的 task）。不影响结果；`ExecCtx.pool` 接入之后自动改走池（单测在两种拓扑的池上都跑过）。
3. **`Close` await 所有 task**（`sort.go:131-134` 只等生成器退出）：比 Go 严格，N3-A1 §3.5 第 1 条的规则；task 没有正常结束时 `Close` 返回一个 Go 没有的错误（`a task of the parallel sort did not complete`），只在 task 被取消或运行时关闭时出现。
4. **不 `Close` 就 drop 时只取消、不 join**：见 §7.4。Go 没有对应（goroutine 继续运行直到 `finishCh` 或 channel 让它们退出）。
5. **最后一段排序中的 kill**（`parallel_sort_worker.go:214-217`）：Go 在通道关闭后的 `sortLocalRows` 里因 kill 信号 panic，经 `run` 的 recover 送出并记 `executor panicked` 日志；Rust 中这次排序返回的错误走 `fetchChunksAndSortImpl` 送出其他错误的那条路，不记这条警告日志。送进结果 channel 的错误相同，只差一条日志。攒满一段时的排序（`parallel_sort_worker.go:228-230`）中的 kill 与 Go 相同，经 `run` 送出并记日志。
6. **panic 的值不是错误也不是字符串时**，错误文字是 `panic`（Go 用 `%v` 格式化那个值）。Rust 的 panic 值实际上总是字符串。
7. **内存数值**：fetcher 按 Go 的公式（`chk.MemoryUsage() + RowSize*行数`）记账，数值是 Rust 的 chunk 大小，不与 Go 逐字节相同（A1 §3.5）。
8. **`multiWayMerger.next`**（`multi_way_merge.go:183-200`，L2）：取出堆顶的行时把它移出元素，来源出错时放回去，堆不变；Go 复制行。只有 `diskSource` 会出错，落盘前到不了。
9. **一批排序不能中途让出**（`parallel_sort_worker.go:173-177`，`slices.SortFunc`）：Go 的 goroutine 在排序中途可以被抢占，Rust 的 task 在一次 poll 里排完一批（`maxChunkSize*30` 行）。实测（2026-11-24，release 构建，`parallel_sort_worker.rs` 中被忽略的测试 `batch_sort_time`，两个 BIGINT 键，30720 行，4 vCPU 的 Xeon 2.1 GHz，同时有另一个构建在跑）：中位数 8.6–11.2 ms（三轮，每轮 7 次），最小 8.2 ms，最大 17 ms。这已接近 N3-A1 的 10 ms 停顿阈值。是否需要改变批大小或在排序中加检查点（偏离 Go），**尚未决定**，已在 #362 的评审回复中提出，等负责人决定；目前照 Go。
