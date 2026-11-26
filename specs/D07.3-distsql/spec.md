# D07.3 distsql 请求构建与结果读取：spec

状态：草案（2026-11-26，#407）。阶段：P2（VS1-33 对应本块第 1 部分的核心）。基线：Go TiDB `v8.5.8`（commit `8b857efa2`）。
crate：`tidb-store`（N0 rank 11，模块 `distsql`；与 D07.1 / D07.2 / D07.4 共用）；从 P1 移交的 `pkg/distsql/context` 与 `StatementContext.GetOrInitDistSQLFromCache` 属 D00.1，落在 `tidb-sessionctx`（N0 rank 7）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/distsql/`，另有说明的除外。

相关：[CONSTITUTION.md](../../CONSTITUTION.md)、[D07.2 spec](../D07.2-copr/spec.md)（coprocessor 客户端与 PoC `tidb-distsql` 现状）、[D12.1 spec](../D12.1-read-executors/spec.md)（读取类执行器，#408 将把 TableReader 接到本块）、[D12.2 spec](../D12.2-basic-executors/spec.md)（§7 为 #338 回归教训）、[D12.3 spec](../D12.3-agg-executors/spec.md)（参考写法）、[N0](../exceptions/N0-crate-layering/spec.md)（crate 分层）、[N3+A1](../exceptions/N3-A1-concurrency-memory/spec.md)（并发、取消与统一内存模型）、[N13](../exceptions/N13-shared-metadata/spec.md)（共享元数据与 `Arc<FieldType>`）、[mappings.md](../../docs/porting/mappings.md)、[tasks.md](tasks.md)（P1-1 移交清单）。

---

## 1. 这个块在 Go 里做什么

### 1.1 内容

`pkg/distsql` 位于执行器（`pkg/executor` 的 `TableReader` / `IndexReader` / `IndexLookUp` / `IndexMerge` 等）与底层 coprocessor 客户端（`pkg/store/copr`，D07.2）之间，负责两件事：

1. **请求构建**：把执行器给出的表/索引逻辑区间（`[]*ranger.Range`）或句柄集合（`[]kv.Handle`）编码成 KV 字节区间（`kv.KeyRanges`），组装 `tipb.DAGRequest` 编码方式与 `kv.Request` 各字段（并发度调整、隔离级别、优先级、paging、store batch、txn scope 校验等）。
2. **结果读取与统计**：经 `kv.Client.Send` 发出请求，把 `kv.Response` 流式返回的 `tipb.SelectResponse` 按 `TypeChunk` 或 `TypeDefault` 解码为 `chunk.Chunk`（或行迭代器 `SelectResultIter`），同时处理 TiKV 返回的错误与 warning、KILL 信号检查、内存记账，以及 `EXPLAIN ANALYZE` 所需的 `selectResultRuntimeStats` 与 `RuntimeStatsColl` 汇总；对多分区保序读提供串行合并（`serialSelectResults`）与多路归并排序（`sortedSelectResults`）。

tracker `data/func_blocks.tsv` 中 D07.3 共 **118 个函数**，分布在 3 个 Go 文件；另有 P1-1 从 D00.1 移交的 **3 个函数**（[tasks.md](tasks.md)）：

| Go 文件 | 所属块 | 行数 | 函数数 | 内容 |
|---|---|--:|--:|---|
| `pkg/distsql/distsql.go` | D07.3 | 288 | 13 | `Select` / `SelectWithRuntimeStats` / `Analyze` / `Checksum` / `GenSelectResultFromMPPResponse`、`SetEncodeType` 与大小端/对齐检查、`SetTiFlashConfVarsInContext`、`WithSQLKvExecCounterInterceptor` |
| `pkg/distsql/request_builder.go` | D07.3 | 917 | 57 | `RequestBuilder`（全部 setter + `Build` + 简单扫描小 LIMIT / 保序并发度调优 + `verifyTxnScope`）、表/公共句柄/索引区间与句柄列表到 `kv.KeyRanges` 的编码、`SplitRangesAcrossInt64Boundary`、`VerifyTxnScope`、`BuildTableRanges` |
| `pkg/distsql/select_result.go` | D07.3 | 1,156 | 48 | `SelectResult` / `SelectResultIter` 接口、`selectResult`（拉取响应、解包 `SelectResponse`、`TypeChunk` / `TypeDefault` 解码、runtime stats、warning、memTracker、`Close`）、`selectResultRuntimeStats`、`serialSelectResults`、`sortedSelectResults`（最小堆多路归并）、`selRespChannelIter` / `selectResultIter`（IndexLookUp 下推的中间结果读取） |
| `pkg/distsql/context/context.go` | D00.1（P1-1 移交） | 119 | 2 | `DistSQLContext` 结构体、`AppendWarning`、`Detach` |
| `pkg/sessionctx/stmtctx/stmtctx.go` | D00.1（P1-1 移交） | — | 1 | `StatementContext.GetOrInitDistSQLFromCache`（L1372） |

### 1.2 请求构建与结果读取流程要点（逐条与源码核对）

**请求构建（`request_builder.go` + `distsql.go`）**：

1. **编码方式（`SetEncodeType`，`distsql.go:228-276`）**：执行器构造 `tipb.DAGRequest` 时调用（`pkg/executor/internal/builder/builder_utils.go:85`）。当 `dctx.EnableChunkRPC` 为真且 `unsafe.Sizeof(types.MyDecimal{}) == 40` 时，置 `EncodeType_TypeChunk` 并写入本机字节序 `ChunkMemoryLayout{Endian: GetSystemEndian()}`；否则置 `EncodeType_TypeDefault`。
2. **无符号整型主键跨 `MaxInt64` 切分（`SplitRangesAcrossInt64Boundary`，`request_builder.go:565-614`）**：TiKV 行键把句柄编码为有符号 `int64`，无符号值 `> MaxInt64` 在字节序上排在 `0` 之前。对非 common handle 且非 `KindInt64` 的有序区间：
   - 若跨界，把跨界区间拆成 `[..., MaxInt64]` 与 `[MaxInt64+1, ...]`（边界排除导致半边为空时跳过该半边）；
   - `keepOrder == false`：把 `unsignedRanges` 拼在 `signedRanges` 前（即 KV 键序），合并为一组返回，省一次 RPC；
   - `keepOrder == true`：`desc == false` 返回 `(signed, unsigned)`，`desc == true` 返回 `(unsigned, signed)`，由 `TableReader` 建两个 `SelectResult` 顺序读取。
3. **区间编码**：
   - 整型句柄区间（`tableRangesToKVRangesWithoutSplit` / `encodeHandleKey`，`:523-549`）：`codec.EncodeInt` + `LowExclude` 时对低端 `PrefixNext()`、`!HighExclude` 时对高端 `PrefixNext()` + `tablecodec.EncodeRowKey(tid, ...)`；
   - 公共句柄区间（`CommonHandleRangesToKVRanges`，`:731-763`）：经 `EncodeIndexKey`（`codec.EncodeKey` + `ErrCtx.HandleError` + `PrefixNext`）编码后按 `tablecodec.EncodeRowKey` 组装；
   - 索引区间（`indexRangesToKVWithoutSplit`，`:784-827`）：每 8 个区间检查一次 `interruptSignal` 与 `memTracker.HandleKillSignal()`，第 0 个区间按样本估算全部区间内存并 `memTracker.Consume`；
   - 排序后的句柄集合（`TableHandlesToKVRanges` / `PartitionHandlesToKVRanges`，`:618-701`）：连续的整型句柄合并成一个 `KeyRange` 并记录 `hints`（行数提示，供 D07.2 small-task 启发式使用）；遇到 `MaxInt64` 或分区 ID 变化时截断。
4. **`RequestBuilder` 并发度与配置覆盖顺序（`:61-117, 190-231, 339-376`）**：
   - `SetDAGRequest`（`:190-231`）：设置 `Tp = ReqTypeDAG`、`Cacheable = true`、序列化 `Data`；若最后一个算子是 `Limit` 则设 `LimitSize`；若 `len(Executors) >= 2` 且 `Executors[1]` 是 `Limit` 且 `Limit < 100000`（`estimatedRegionRowCount`），并且（`len == 2`，即 `Scan -> Limit`；或 `len > 2` 且 `Limit.ParentIdx > 0` 指向 `IndexLookup`），则把 `Concurrency` 设为 `KeyRanges.PartitionNum()`（`KeyRanges == nil` 时为 `1`）。
   - `SetFromSessionVars`（`:339-376`）：若 `Concurrency == 0` 设为 `dctx.DistSQLConcurrency`，若大于它则截断到它；依次设置 `IsolationLevel`（`WeakConsistency` → `RC`、`RCCheckTS` → `RCCheckTS` 且强制 `ReplicaReadLeader`、否则 `getIsolationLevel()`）、`NotFillCache`、`TaskID`、`Priority`、`ReplicaRead`、`ResourceGroupTagger`、`Paging`、`RequestSource`、`StoreBatchSize`、`ResourceGroupName`、`StoreBusyThreshold`、`RunawayChecker`、`TiKVClientReadTimeout`。
   - `Build`（`:61-117`）：在 `intest.InTest` 下禁止重复使用同一 builder；空 `ReadReplicaScope` 补为 `GlobalReplicaScope`；`ReplicaRead.IsClosestRead() && ReadReplicaScope != GlobalReplicaScope` 时写入 `MatchStoreLabels`（key=`zone`）；调用 `verifyTxnScope()`；空 `KeyRanges` 补为空非分区 `KeyRanges`；最后若 `len(dag.Executors) == 1` 且 `KeepOrder` 且 `Concurrency == DefDistSQLScanConcurrency(15)` 且算子为 `TableScan / IndexScan / PartitionTableScan`，把 `Concurrency` 降为 `2`（仅在用户未显式修改默认并发度时生效）。

**请求发出与结果读取（`distsql.go` + `select_result.go`）**：

1. **`Select` / `SelectWithRuntimeStats`（`distsql.go:58-172`）**：组装 `kv.ClientSendOption`（`SessionMemTracker`、`EnabledRateLimitAction`、`EventCb`、`EnableCollectExecutionInfo`、`TryCopLiteWorker`），调用 `dctx.Client.Send(ctx, kvReq, dctx.KVVars, option)`，返回 `*selectResult`。`SelectWithRuntimeStats` 额外写入 `copPlanIDs` 和 `rootPlanID`。
2. **`selectResult.fetchRespWithIntermediateResults`（`select_result.go:374-488`）**：循环调用 `r.resp.Next(ctx)` 直到拿到非空 `Chunks`（或非空 `IntermediateOutputs`）或 EOF：
   - 累加 `fetchDuration`，EOF 时上报一次 `DistSQLQueryHistogram`（区分 `paging` / `common`）；
   - 反序列化 `tipb.SelectResponse`，对 `memTracker` 释放上一批、计入本批大小；
   - 若 `selectResp.Error != nil`，合成 `ClassTiKV` 错误返回；
   - 校验 `len(IntermediateOutputs) == len(intermediateOutputTypes)`；
   - 检查 `r.ctx.SQLKiller.HandleSignal()`；
   - 逐条把 `selectResp.Warnings` 合成 `ClassTiKV` 写入 `r.ctx.AppendWarning`；
   - 若 `resultSubset` 带 `CopRuntimeStats`，调用 `updateCopRuntimeStats` 更新 `selectResultRuntimeStats`、`CPUUsage.MergeTikvCPUTime`、`RuntimeStatsColl.RecordCopStats` / `RecordOneCopTask`，并合并进 `r.ctx.ExecDetails`。
3. **Chunk 解码**：
   - `TypeChunk`（`readFromChunk`，`:562-597`）：用 `chunk.Decoder` 零拷贝切分 `RowsData`；若当前解码器剩余行数 `> int(float64(chk.RequiredRows()) * 0.8)`，当 `chk` 已有行时先返回，为空时直接 `ReuseIntermChk(chk)` 交换列缓冲避免复制；否则调用 `Decode(chk)` 拼接多个小 chunk。
   - `TypeDefault`（`readFromDefault` / `readRowsData`，`:543-560, 703-716`）：逐行按列用 `codec.NewDecoder(chk, r.ctx.Location).DecodeOne` 解码 datum 到 `chk`。
4. **收尾（`selectResult.close`，`:732-763`）**：上报 `DistSQLPartialCountHistogram`，归还 `selectRespSize` 内存记账，收集 `HasUnconsumedCopRuntimeStats` 中未消费的统计；经 `defer` 在 `r.resp.Close()` **之后**读取 `CopInfo` 的 `buildTaskDuration` 与 `storeBatchedNum` / `storeBatchedFallbackNum` 并注册 `r.stats` 到 `RuntimeStatsColl`。
5. **多结果合并**：
   - `serialSelectResults`（`:281-332`）：按顺序逐个读完再切下一个；`Close` 关闭全部并返回最后一个错误。
   - `sortedSelectResults`（`:148-278`）：为每个子 `SelectResult` 缓存一个 `Chunk`，用最小堆（`chunkRowHeap`）按 `byItems` 归并输出。
   - `selectResultIter` / `selRespChannelIter`（`:765-956`）：供 IndexLookUp 下推读取主输出与多个 `IntermediateOutputs`；按 channel 编号**从大到小**先读更完整的最终行、再读未回表的索引行。

### 1.3 调用方（tracker `data/block_edges.tsv` 及读源码核对）

| 调用方 | 引用数（`block_edges.tsv`） | 实际调用内容 | 备注 |
|---|--:|---|---|
| D12.1 读取类执行器（`table_reader.go`、`distsql.go`、`mem_reader.go`、`point_get.go`、`builder.go`） | 137 | `RequestBuilder` 全套方法、`SelectWithRuntimeStats`、`SelectResult`（`Next` / `NextRaw` / `IntoIter` / `Close`）、`SelectResultIter`、`GetSelectResultConcurrency`、`SplitRangesAcrossInt64Boundary`、`CommonHandleRangesToKVRanges`、`IndexRangesToKVRangesWithInterruptSignal`、`TableHandlesToKVRanges`、`VerifyTxnScope`、`NewSerialSelectResults`、`NewSortedSelectResults` | **主要调用方**；#408（TableReader 等）依赖本块第 1 部分 |
| D12.0 执行器框架（`internal/builder/builder_utils.go`） | 1 | `SetEncodeType`（`ConstructDAGReq`） | 现有 `blocked-by:D10.9` 桩的注释已指明等 D07.3 `SetEncodeType` |
| D12.7 index join 系列（`builder.go:4813,4818,4952,4954,5221,5269`） | 23 | `TableHandlesToKVRanges`、`SetTableHandles`、`SetPartitionsAndHandles`、`IndexRangesToKVRangesWithInterruptSignal` | 需本块第 2 部分 |
| D12.11 IndexMerge（`index_merge_reader.go`） | 25 | `IndexRangesToKVRanges`、`NewSortedSelectResults`、`RequestBuilder`、`SelectWithRuntimeStats` | 需本块第 2 部分 |
| D12.12 简单执行器（`simple.go`） | 16 | `RequestBuilder` + `Client.Send`（少量内部查询，不调 `Select`） | 已有核心即可满足 |
| D12.14 管理/校验执行器（`admin.go`、`checksum.go`） | 96 | `Checksum`、`SetChecksumRequest`、`SetIndexRanges` 等 | **不在只读范围**（N0 §3 明确不移植 D12.14） |
| DEF.MPP（`mpp_gather.go`、`local_mpp_coordinator.go`） | 11 | `GenSelectResultFromMPPResponse`、`SetTiFlashConfVarsInContext`、`FillDummySummariesForTiFlashTasks` | **不在只读范围**（DEF.MPP） |
| X（`analyze_col.go`、`analyze_idx.go`、`ddl/*`、`br/*`、`lightning/*`） | 35 | `Analyze`、`SetAnalyzeRequest`、`SetIndexRangesForTables`、`IndexRangesToKVRangesForTables`、`BuildTableRanges`、`Checksum` | **不在只读范围**（委托给 Go TiDB / 运维写路径 / BR） |

此外，在当前 tidb-rs 代码库中，已有 3 处 `blocked-by:D07.3` 边界桩（`grep` 核对），按职责拆分解除归属：

1. `crates/session/src/session.rs:791`：`SessionCore::get_dist_sql_ctx`（D05.3）——**由本块第 1 部分解除**；
2. `crates/executor/src/point_get.rs:524`：`PointGetExecutor::verify_txn_scope`（D12.1 第 1 部分）——**由本块第 1 部分解除**；
3. `crates/executor/src/builder.rs:955`：`new_closest_read_adjuster`（D12.1 函数）——本块第 1 部分提供 `DistSQLContext` 与 `kv::Request.match_store_labels` 字段，**桩由 #411（D12.1 第 2A 部分）解除**。

### 1.4 PoC `tidb-distsql` 现状与正式替换路径

当前仓库中存在一个 `[legacy]` 原型 crate `crates/distsql/`（包名 `tidb-distsql`），共 4 个源文件：

- `crates/distsql/src/ranges.rs`：只支持整型句柄的 `HandleRange` / `IntRange`、`record_range`、`split_ranges_across_int64_boundary`；
- `crates/distsql/src/dag.rs`：自写 `DagSpec` / `build_dag`，固定用 `EncodeType::TypeDefault` 构造 `TableScan [-> Aggregation] [-> Limit]`；
- `crates/distsql/src/decode.rs`：用 vendored TiKV crate `tidb_query_datatype` 解码 `TypeDefault` 响应与点查行值；
- `crates/distsql/src/lib.rs`：通过 `poc-kv::KvSnapshot::coprocessor`（已在 D07.2 §7 切到正式 `tidb-store::copr`）发请求并一次性收齐全部行。

被谁依赖（`grep -rn 'tidb-distsql' crates/` 核对）：`poc-executor`、`poc-session`、`poc-domain`、`poc-server`、`tidb-sql`——全部属于 `layers.toml` 中的 `[legacy]`（`tidb-query-smoke` 不依赖 `tidb-distsql`）。

**替换路径（严格遵守 port-block §2「新代码不进默认路径」与 N0 §5 第 3 条）**：

1. **正式代码落在 `tidb-store`（N0 rank 11）的 `crates/store/src/distsql/`**，绝不放在 `[legacy]` 的 `crates/distsql/` 中，也不让任何正式 crate 依赖 `tidb-distsql`。
2. **默认 `--new-stack=off` 路径完全不触碰**：`poc-session` / `poc-executor` / `tidb-sql` 继续使用原型 `tidb-distsql`，零改动、零回归风险。
3. **`--new-stack=on` / `fallback` 路径切入正式 D07.3**：本块第 1 部分合入后，解除 `SessionCore::get_dist_sql_ctx` 与 `PointGetExecutor::verify_txn_scope` 的桩，并提供 `Request.match_store_labels` 供 #411（D12.1 第 2A 部分）解除 `new_closest_read_adjuster` 并接入 `TableReaderExecutor` 与 `ConstructDAGReq`。
4. **原型退场**：待 `poc-executor` / `poc-session` / `tidb-sql` 被新栈全面替代、无任何调用方引用 `tidb-distsql` 时，整包删除 `crates/distsql/` 并从 `layers.toml` `[legacy]` 移除。

---

## 2. 依赖（逐项 `grep` 核对）

核对范围：`crates/` 下全部正式 crate（排除 `crates/poc/`、`crates/distsql/`）。

### 2.1 已移植、可直接使用的依赖

| 块 | crate（N0 rank） | 用到的符号（`grep` 已确认存在） |
|---|---|---|
| D00.2 / D00.4 | `tidb-base` (0) | `Error`、`dbterror::ClassTiKV.synthesize`（`crates/base/src/terror.rs:296`）、`errno::ErrQueryInterrupted`、`config`（`Instance.EnableCollectExecutionInfo`、`TiKVClient.CoprCache.CapacityMB`，`crates/base/src/config/types_gen.rs:1271,3051`）、`kv::StoreType`（`TiKV`/`TiFlash`/`TiDB`）、`mysql::PriorityEnum`、`contextutil::WarnAppender` / `WarnAppenderRef`（`crates/base/src/contextutil.rs:118`）、`gostd::time::Location` |
| D14.1 | `tidb-metrics` (0) | `distsql_query`（`tidb_distsql_handle_query_duration_seconds`，`crates/metrics/src/lib.rs:494,808`）；需补一个直方图 `tidb_distsql_partial_num`（对应 `DistSQLPartialCountHistogram`） |
| D15.1 | `tidb-memory` (1) | `Tracker`（`consume`、`handle_kill_signal`）、`sqlkiller::SqlKiller`（`handle_signal`，`crates/memory/src/sqlkiller.rs:4`） |
| N3 | `tidb-runtime` (1) | `ExecPool`、`checkpoint` |
| D01.1–D01.6, D01.9 | `tidb-types` (2) | `Datum`、`MyDecimal`（`#[repr(C)]`，`MY_DECIMAL_STRUCT_SIZE == 40`，`crates/types/src/mydecimal.rs:288`）、`collate::get_binary_collator_slice`（`crates/types/src/collate/mod.rs:216`） |
| D01.7 | `tidb-chunk` (3) | `Chunk`、`Row`、`RowPtr`、`new_chunk_with_capacity`（接受 `F: Borrow<FieldType>`，N13 §8）、`Chunk::copy_construct`（`chunk.rs:464`）、`compare::get_compare_func`（`compare.rs:33`）、`codec::Decoder`（`new_decoder` / `decode` / `reset` / `is_finished` / `remained_rows` / `reuse_interm_chk`，`codec.rs:285-356`） |
| D06.1 | `tidb-model` (3) | `TableInfo`、`IndexInfo`、`PartitionInfo`、`StatePublic` |
| D14.0 | `tidb-execdetails` (4) | `RuntimeStats` trait、`RuntimeStatsColl`（`record_cop_stats` / `record_one_cop_task` / `register_stats`，`execdetails.rs:2025,2089`）、`SyncExecDetails`（`merge_cop_exec_details`，`execdetails.rs:629`）、`Percentile<T>`、`DurationWrapper`、`Int64`（`execdetails.rs:924,935,963`）、`TpSelectResultRuntimeStats`（`:1393`）、`FormatDuration`（`clientgo.rs:276`） |
| D01.8 | `tidb-codec` (4) | `encode_int`（`number.rs:41`）、`encode_key`（`codec.rs:364`）、`Decoder`（`new_decoder` / `decode_one`，`codec.rs:1465-1492`） |
| D07.0 | `tidb-kv` (5) | `Request`（32 个字段已就绪）、`Key`（`next` / `prefix_next`）、`KeyRange`、`KeyRanges`（`new_non_partitioned_key_ranges` / `new_partitioned_key_ranges` / `new_non_paritioned_key_ranges_with_hint` / `set_to_non_partitioned` / `append_self_to` / `first_partition_range` / `partition_num`）、`Handle` / `IntHandle` / `CommonHandle` / `PartitionHandle`、`Client` / `ClientSendOption` / `Response` / `ResultSubset`、`CoprRequestAdjuster`、`IsoLevel`（`SI`/`RC`/`RCCheckTS`）、`Priority*`、`ReplicaReadType`（`is_closest_read`）、`GlobalReplicaScope`、`GlobalTxnScope`、`ReqType*` |
| D01.11 | `tidb-tablecodec` (6) | `verify_table_id_for_ranges`（`tablecodec.rs:1662`）、`encode_row_key`（`:144`）、`encode_index_seek_key`（`:800`） |
| D00.1 / D05.1 / D05.2 | `tidb-sessionctx` (7) | `errctx::Context` / `StrictNoWarningContext`、`ranger::Range` / `FullRange` / `FullIntRange` / `FullNotNullRange`（`ranger/types.rs:809,839`）、`StatementContext`、`SessionVars`（含 `enable_chunk_rpc`、`enabled_rate_limit_action`、`enable_paging`、`min_paging_size`、`max_paging_size`、`store_batch_size`、`dist_sql_scan_concurrency`、`replica_closest_read_threshold`、`load_based_replica_read_threshold`、`tikv_client_read_timeout` 等全部变量字段） |
| D08.1 / D08.11 | `tidb-expression` (8) | `Schema`、`Column`、`ByItems`（`crates/expression/src/byitem.rs:21`，已按 N0 §4 下沉至此，rank 8 < `tidb-store` rank 11）、`EvalContext` |
| D06.3 | `tidb-infoschema` (9) | `InfoSchemaV2`（`table_info_by_id` / `find_table_info_by_partition_id` / `placement_bundle_by_physical_table_id` / `schema_meta_version`）、`placement::Bundle` 替身 |
| D07.2 | `tidb-store::copr` (11) | `CopClient`、`CopIterator`（`get_concurrency` / `get_store_batch_info` / `get_build_task_elapsed` / `collect_unconsumed_cop_runtime_stats`）、`CopResponse`（`get_cop_runtime_stats`）、`CopRuntimeStats`、`LazyCopResponse`、`RateLimit`、`Variables` |
| 外部/协议 | `tipb`、`protobuf`、`bytes` | `tipb::DagRequest`、`tipb::SelectResponse`、`tipb::Chunk`、`tipb::IntermediateOutput`、`tipb::EncodeType`、`tipb::Endian`、`tipb::ChunkMemoryLayout`、`tipb::ExecutorExecutionSummary`、`tipb::ExecType`（已在 `crates/store/Cargo.toml` 依赖） |

### 2.2 未移植或需要前置小调整的依赖

| 依赖项 | 所属块 / 来源 | 缺什么 | 本块处理方式 |
|---|---|---|---|
| `pkg/distsql/context` + `GetOrInitDistSQLFromCache` | D00.1（P1-1） | `DistSQLContext` 尚未定义；`StatementContext` 缺 `dist_sql_ctx_cache` | **随本块第 1 部分移植**（`tasks.md` P1-1），落 `tidb-sessionctx` |
| `pkg/util/ppcpuusage/cpuusages.go`（7 个小函数，86 行） | D00.3 | `SQLCPUUsages`、`CPUUsages`、`MergeTikvCPUTime` 等尚未移植 | 叶子工具文件，按宪章第四条 4 **随第 1 部分整文件移植**到 `tidb-execdetails`（rank 4） |
| `Variables`、`RateLimit` 的 crate 归属与 `Request` 字段 | D07.2 / D07.0 | 目前在 `tidb-store::copr::tikv`（rank 11），但 `DistSQLContext`（rank 7）和 `kv::Request`（rank 5）要持有它们；且 `kv::Request` 缺 `match_store_labels` 与 `copr_request_rate_limit`，`CopIterator` 尚未读 `copr_request_rate_limit`（`coprocessor.rs:1309, 1804`） | 第 1 部分按 N0 §4 规则 1 将 `Variables` / `RateLimit` 下沉到 `tidb-kv`（rank 5，在 `tidb-store::copr::tikv` 重导出）并补 `StoreLabel` + `Request.match_store_labels`；**第 2 部分**补 `Request.copr_request_rate_limit` 字段并完成 `CopIterator` 限速接线（供 #411 第 2C 部分使用） |
| `kv::ResultSubset` / `kv::Response` 读取 copr 统计与即时构造 `CopIterator` | D07.0 / D07.2 | `ResultSubset` / `Response` trait 尚无向 `selectResult` 暴露 `CopRuntimeStats` / `CopInfo` / `HasUnconsumedCopRuntimeStats` 的方法；且 `CopClient::send` 惰性构造 `CopIterator` 会导致首次 `Next` 前 `GetSelectResultConcurrency` 失真 | 按 `M-IFACE` 在 `tidb-kv`（rank 5，依赖 `tidb-execdetails` rank 4）为两个 trait 加带默认实现的统计访问方法；同时在 `CopClient` 补异步即时构建入口（供异步 `distsql::select` / `select_with_runtime_stats` 立即建好并打开 `CopIterator`，消除首次 `Next` 前并发度差异，见 §4.3、§4.6） |
| `infoschema::context::MetaOnlyInfoSchema` 扩展 | D06.3 | `tidb-sessionctx::expropt::MetaOnlyInfoSchema` 目前只有 `schema_meta_version`；注意 `InfoSchemaV2::table_info_by_id` / `find_table_info_by_partition_id`（`infoschema_v2.rs:1426, 1562`）是 **async**，而 `placement_bundle_by_physical_table_id`（`infoschema.rs:129`）是 **sync** | 在 `tidb-infoschema::context`（rank 9）扩展同步方法 `placement_bundle_by_physical_table_id`（与 `VerifyTxnScope` 同步签名一致）；命中 bundle 时 `VerifyTxnScope` 直接返回 `blocked("X", ...)`，不会走到需要查表名的分支，不引入同步调异步的问题 |
| `placement.Bundle.GetLeaderDC` | X（`pkg/ddl/placement`） | `crates/infoschema/src/placement.rs` 不构建规则（X）；tidb-rs 不支持本地 TSO | `VerifyTxnScope` 遇到非空、非 `global` 的 `txn_scope` 且表存在 placement bundle 时返回 `blocked("X", ...)`（见 §5） |
| **`client-rust` `CopRpcContext.addr`（硬前置任务）** | ADR 0008（`dulao5/client-rust`） | 目前 `CopRpcContext.addr` 为空且无 `RegionRequestRuntimeStats`，导致 `updateCopRuntimeStats`（`select_result.go:625`）对**每个**响应提前返回，丢失 `EXPLAIN ANALYZE` `cop_task`/`tikv_task`、`actRows`、慢日志 `Tikv_cpu_time`、`statements_summary` `SumTikvCPU` 与 processlist TiKV CPU | **维护者已定：将"在 `dulao5/client-rust` 填上被调用 TiKV store 的 `addr`"列为本块第 1 部分的前置任务**（见 §7 第 5 条、§9.2） |
| `tikv.RegionRequestRuntimeStats` | ADR 0008（client-rust） | client-rust 不产生 client-go 的 `RegionRequestRuntimeStats`（D07.2 §6.3 第 4 条） | 填上 `addr` 后 `updateCopRuntimeStats` 正常运行，仅 `rpc_info:{...}` 子段缺失（§7 第 4 条） |
| **`ResourceGroupTagger`（D07.0 T8，临时缺口）** | D07.0 / D05.2 / D07.2（N9） | Go 对每个 cop 请求均设置 `kvrpcpb.Context.ResourceGroupTag`（`copr/coprocessor.go:1350`、`stmtctx.go:868`、`kv/kv.go:872-899`，不受 Top SQL 开关控制，改变发给 TiKV 的请求字节）；`kv::Request` 与 `copr/coprocessor.rs:1789` 尚未接入 | **非永久空操作，属临时缺口（D07.0 T8）**：随本块第 1 部分补齐 `tidb-kv` 的 `ResourceGroupTagger`、`StatementContext::get_resource_group_tagger`、`RequestBuilder::set_resource_group_tagger` 与 `copr/coprocessor.rs:1789` 接线（见 §7 第 7 条、§9.2） |
| **`RunawayChecker`** | N10（未定） | `runaway/checker.go:179-222` 的 `BeforeCopRequest` 会改写 `OverridePriority`、`MaxExecutionDurationMs`、切换 `ResourceGroupName`、返回错误 8253，COOLDOWN 还会把并发度压到 1（`coprocessor.go:229`），配置了 runaway 规则时用户可见且改请求字节 | N10 尚未定稿；N10 决定前若存在 runaway 规则（通过会话/资源组检查）返回 `blocked("N10", ...)` 而非静默忽略（宪章第五条，§5、§7 第 8 条） |
| `stmtstats.KvExecCounter` | D14.2（Top SQL） | Top SQL 不记录（ROADMAP §3） | `WithSQLKvExecCounterInterceptor` 为 L2 空操作（§7 第 6 条） |
| `telemetry.CurrentCoprCacheHitRatio*` / `telemetryStoreBatched*` | X（`pkg/telemetry`） | 遥测包全属 X，不移植 | 省略遥测计数（§7 第 9 条） |
| `tikvmetrics.ObserveReadSLI` | client-go metrics（N9） | client-rust 无对应 SLI 指标 | 省略（§7 第 10 条） |
| `execdetails.MergeTiFlashRUConsumption` | DEF.MPP（`crates/execdetails/src/clientgo.rs:20` 已明确不移植） | TiFlash RU 汇总 | TiFlash 分支归 DEF.MPP，不走此路径 |
| `metrics.DistSQLPartialCountHistogram` | D14.1 | `crates/metrics/src/lib.rs` 尚缺 `tidb_distsql_partial_num` | 随第 1 部分在 `tidb-metrics` 中补注册 |

---

## 3. 函数清单与划分

tracker `data/func_blocks.tsv` 中 `D07.3` 共 **118 个函数**，划分为 **2 个移植部分 + 1 组不移植/豁免**（另含 P1-1 的 3 个 D00.1 函数与 D00.3 的 `ppcpuusage` 7 个函数）：

| 类别 | D07.3 函数数 | 另含（非 D07.3） | 内容 | 目标场景 / 依赖 |
|---|--:|--:|---|---|
| **第 1 部分** | **73** | D00.1 (3) + D00.3 (7) | `DistSQLContext` + `GetOrInitDistSQLFromCache` + `ppcpuusage`；`RequestBuilder`（TableReader 全套 setter、`Build`、`verifyTxnScope`）、表/公共句柄区间编码、`SplitRangesAcrossInt64Boundary`、`VerifyTxnScope`、`EncodeIndexKey`；`Select` / `SelectWithRuntimeStats`（async，即时建 `CopIterator`）、`SetEncodeType` 族、`selectResult`（`Next`/`NextRaw`/`Close`/`readFromChunk`/`readFromDefault`/统计）、`selectResultRuntimeStats`、`serialSelectResults` | **sysbench `oltp_read_only`**（`TableReader` 范围/全表扫描、保序、分页、`TypeChunk`/`TypeDefault`）、解除 `get_dist_sql_ctx` 与 `verify_txn_scope` 桩，前置 `client-rust` 填 `addr` |
| **第 2 部分** | **31** | — | 只读路径索引区间与句柄集合编码（`SetTableHandles`、`SetPartitionsAndHandles`、`TableHandles*`、`PartitionHandles*`、`IndexRangesToKVRanges`、`IndexRangesToKVRangesWithInterruptSignal`、`indexRangesToKVRangesForTablesWithInterruptSignal`、`indexRangesToKVWithoutSplit`）；`Request.copr_request_rate_limit` 与 `CopIterator` 限速接线；`sortedSelectResults`（最小堆多路归并）；`SelectResultIter` / `selRespChannelIter`（IndexLookUp 下推中间结果） | `IndexReader`、`IndexLookUp`、`IndexMerge`、多分区保序读（D12.1 2B/2C、D12.7、D12.11） |
| **不移植 / 豁免** | **14** | — | DEF.MPP（4）+ 委托/非只读 Analyze & Checksum & BR/Lightning 专用（9）+ 编译期替代 `init`（1） | 见 §3.3 逐条理由 |
| **合计** | **118** | **10** | 13（`distsql.go`）+ 57（`request_builder.go`）+ 48（`select_result.go`）= **118** | — |

**与 `VS1-33`（tracker `data/part_funcs.tsv`，69 个函数）的精确差异说明**：

`VS1-33` 清单原本由静态调用图粗抽得到，存在调用闭环缺口（例如收了 `CommonHandleRangesToKVRanges` 却漏了它调用的 `EncodeIndexKey`；收了 `selectResultIter.Next` 却漏了它调用的 `newSelRespChannelIter` / `selRespChannelIter.*`；收了仅 BR 备份调用的 `BuildTableRanges` / `appendRanges`）。本划分修正为真正的自洽闭环：
- 相比 `VS1-33`（69 个）：**移入第 2 部分 4 个**（`selectResult.IntoIter`、`newSelectResultIter`、`selectResultIter.Next`、`selectResultIter.Close`）；**改归不移植 3 个**（`distsql.go:init` 改编译期 `cfg(target_endian)`，`BuildTableRanges` / `appendRanges` 仅 BR 调用归 X）；**补入第 1 部分 11 个**（`EncodeIndexKey`、`tablesRangesToKVRanges`、`SetAllowBatchCop`、`SetPartitionIDAndRanges`、`SetPartitionKeyRanges`、`SetWrappedKeyRanges`、`NewSerialSelectResults`、`serialSelectResults.{NextRaw,Next,IntoIter,Close}`，均为第 1 部分内部调用或 `TableReaderExecutor.buildKVReq*` 直接调用）。`69 - 4 - 3 + 11 = 73`。

### 3.1 第 1 部分（73 个 D07.3 + 3 个 P1-1 D00.1 + 7 个 D00.3）

**`pkg/distsql/distsql.go`（8 个）**：
1. `Select`（L58）
2. `SelectWithRuntimeStats`（L161）
3. `SetEncodeType`（L228）
4. `canUseChunkRPC`（L237）
5. `checkAlignment`（L252）
6. `setChunkMemoryLayout`（L259）
7. `GetSystemEndian`（L264）
8. `WithSQLKvExecCounterInterceptor`（L280，L2 空操作）

**`pkg/distsql/request_builder.go`（42 个）**：
- `RequestBuilder` 方法（33 个）：
  9. `RequestBuilder.Build`（L61）
  10. `RequestBuilder.SetMemTracker`（L120）
  11. `RequestBuilder.SetTableRanges`（L129）
  12. `RequestBuilder.SetHandleRanges`（L156）
  13. `RequestBuilder.SetHandleRangesForTables`（L164）
  14. `RequestBuilder.SetDAGRequest`（L190）
  15. `RequestBuilder.SetKeyRanges`（L258）
  16. `RequestBuilder.SetKeyRangesWithHints`（L264）
  17. `RequestBuilder.SetWrappedKeyRanges`（L270）
  18. `RequestBuilder.SetPartitionKeyRanges`（L276）
  19. `RequestBuilder.SetStartTS`（L282）
  20. `RequestBuilder.SetDesc`（L288）
  21. `RequestBuilder.SetKeepOrder`（L294）
  22. `RequestBuilder.SetStoreType`（L300）
  23. `RequestBuilder.SetAllowBatchCop`（L306）
  24. `RequestBuilder.SetPartitionIDAndRanges`（L312）
  25. `RequestBuilder.getIsolationLevel`（L317）
  26. `RequestBuilder.getKVPriority`（L324）
  27. `RequestBuilder.SetFromSessionVars`（L339）
  28. `RequestBuilder.SetPaging`（L379）
  29. `RequestBuilder.SetConcurrency`（L385）
  30. `RequestBuilder.SetCoprRequestRateLimit`（L391）
  31. `RequestBuilder.SetTiDBServerID`（L400）
  32. `RequestBuilder.SetFromInfoSchema`（L407）
  33. `RequestBuilder.SetResourceGroupTagger`（L414，随第 1 部分按 D07.0 T8 接线）
  34. `RequestBuilder.SetResourceGroupName`（L420）
  35. `RequestBuilder.SetExplicitRequestSourceType`（L426）
  36. `RequestBuilder.verifyTxnScope`（L431）
  37. `RequestBuilder.SetTxnScope`（L470）
  38. `RequestBuilder.SetReadReplicaScope`（L476）
  39. `RequestBuilder.SetIsStaleness`（L482）
  40. `RequestBuilder.SetClosestReplicaReadAdjuster`（L488）
  41. `RequestBuilder.SetConnIDAndConnAlias`（L494）
- 区间与校验函数（9 个）：
  42. `TableHandleRangesToKVRanges`（L501）
  43. `TableRangesToKVRanges`（L511）
  44. `tablesRangesToKVRanges`（L519）
  45. `tableRangesToKVRangesWithoutSplit`（L523）
  46. `encodeHandleKey`（L539）
  47. `SplitRangesAcrossInt64Boundary`（L565）
  48. `CommonHandleRangesToKVRanges`（L731）
  49. `VerifyTxnScope`（L766）
  50. `EncodeIndexKey`（L830）

**`pkg/distsql/select_result.go`（23 个）**：
- `selectResult` 与并发查询（12 个）：
  51. `GetSelectResultConcurrency`（L85）
  52. `selectResult.fetchResp`（L370）
  53. `selectResult.fetchRespWithIntermediateResults`（L374）
  54. `selectResult.Next`（L490）
  55. `selectResult.NextRaw`（L524）
  56. `selectResult.readFromDefault`（L543）
  57. `selectResult.readFromChunk`（L562）
  58. `selectResult.updateCopRuntimeStats`（L623）
  59. `selectResult.readRowsData`（L703）
  60. `selectResult.memConsume`（L718）
  61. `selectResult.Close`（L725）
  62. `selectResult.close`（L732）
- `selectResultRuntimeStats`（6 个）：
  63. `selectResultRuntimeStats.mergeCopRuntimeStats`（L979）
  64. `selectResultRuntimeStats.Clone`（L1002）
  65. `selectResultRuntimeStats.Merge`（L1026）
  66. `selectResultRuntimeStats.String`（L1057）
  67. `selectResultRuntimeStats.Tp`（L1142）
  68. `selectResultRuntimeStats.calcCacheHit`（L1146）
- `serialSelectResults`（5 个）：
  69. `NewSerialSelectResults`（L281）
  70. `serialSelectResults.NextRaw`（L294）
  71. `serialSelectResults.Next`（L308）
  72. `serialSelectResults.IntoIter`（L321）
  73. `serialSelectResults.Close`（L325）

**随第 1 部分一起移植的非 D07.3 函数（10 个）**：
- **P1-1（D00.1，3 个，完成后从 tracker `ctx2b_pending.go` 与 `stmtctx_pending.go` 删除豁免）**：
  - `pkg/distsql/context/context.go`：`DistSQLContext.AppendWarning`（L94）、`DistSQLContext.Detach`（L103）
  - `pkg/sessionctx/stmtctx/stmtctx.go`：`StatementContext.GetOrInitDistSQLFromCache`（L1372）
- **D00.3 叶子工具文件（7 个，按宪章第四条 4 整文件移植）**：
  - `pkg/util/ppcpuusage/cpuusages.go`：`CPUUsages.Reset`（L36）、`SQLCPUUsages.SetCPUUsages`（L42）、`MergeTidbCPUTime`（L50）、`MergeTikvCPUTime`（L60）、`GetCPUUsages`（L67）、`AllocNewSQLID`（L74）、`ResetCPUTimes`（L82）
- **解除已有桩（不计入 D07.3 函数数）**：
  - `crates/session/src/session.rs:791`：`SessionCore::get_dist_sql_ctx`（D05.3，改为调用 `GetOrInitDistSQLFromCache` 构造 `DistSQLContext`）
  - `crates/executor/src/point_get.rs:524`：`PointGetExecutor::verify_txn_scope`（D12.1，改为调用 `distsql::verify_txn_scope`）
  - （注：`crates/executor/src/builder.rs:955` 的 `new_closest_read_adjuster` 属 D12.1 函数，本块只提供 `Request.match_store_labels` 字段与 `DistSQLContext`，该桩由 #411 D12.1 第 2A 部分解除。）

### 3.2 第 2 部分（31 个 D07.3 函数）

**2a. 索引区间与句柄列表编码（`pkg/distsql/request_builder.go`，8 个）**：
1. `RequestBuilder.SetTableHandles`（L173）
2. `RequestBuilder.SetPartitionsAndHandles`（L181）
3. `TableHandlesToKVRanges`（L618）
4. `PartitionHandlesToKVRanges`（L664）
5. `IndexRangesToKVRanges`（L704）
6. `IndexRangesToKVRangesWithInterruptSignal`（L710）
7. `indexRangesToKVRangesForTablesWithInterruptSignal`（L726）
8. `indexRangesToKVWithoutSplit`（L784）

**2b. 多分区归并排序结果（`pkg/distsql/select_result.go`，14 个）**：
9. `chunkRowHeap.Len`（L121）
10. `chunkRowHeap.Less`（L125）
11. `chunkRowHeap.Swap`（L132）
12. `chunkRowHeap.Push`（L136）
13. `chunkRowHeap.Pop`（L140）
14. `NewSortedSelectResults`（L148）
15. `sortedSelectResults.updateCachedChunk`（L176）
16. `sortedSelectResults.initCompareFuncs`（L189）
17. `sortedSelectResults.buildKeyColumns`（L197）
18. `sortedSelectResults.lessRow`（L209）
19. `sortedSelectResults.NextRaw`（L225）
20. `sortedSelectResults.Next`（L229）
21. `sortedSelectResults.IntoIter`（L264）
22. `sortedSelectResults.Close`（L268）

**2c. 行级结果迭代器（`pkg/distsql/select_result.go`，9 个，供 IndexLookUp 下推中间结果）**：
23. `selectResult.IntoIter`（L516）
24. `newSelRespChannelIter`（L783）
25. `selRespChannelIter.Channel`（L819）
26. `selRespChannelIter.Next`（L823）
27. `selRespChannelIter.nextChunk`（L843）
28. `selRespChannelIter.fillChunkFromDefault`（L879）
29. `newSelectResultIter`（L902）
30. `selectResultIter.Next`（L912）
31. `selectResultIter.Close`（L954）

### 3.3 不移植 / 豁免（14 个 D07.3 函数，逐条写理由）

| # | Go 文件与行号 | Go 函数 | 类别 | 理由 |
|--:|---|---|---|---|
| 1 | `distsql.go:42` | `GenSelectResultFromMPPResponse` | 豁免（`DEF.MPP`） | 唯一调用方是 `pkg/executor/mpp_gather.go:107`（MPP / TiFlash 推迟范围） |
| 2 | `distsql.go:136` | `SetTiFlashConfVarsInContext` | 豁免（`DEF.MPP`） | 向 TiFlash gRPC outgoing context 写 `tidb_max_tiflash_threads` 等头；唯一调用方是 `local_mpp_coordinator.go:812` 与 `Select` 的 `StoreType == TiFlash` 分支 |
| 3 | `select_result.go:600` | `FillDummySummariesForTiFlashTasks` | 豁免（`DEF.MPP`） | 为 TiFlash MPP 任务补齐空执行摘要，唯一调用方是 `local_mpp_coordinator.go:638` 与 `recordExecutionSummariesForTiFlashTasks` |
| 4 | `select_result.go:611` | `recordExecutionSummariesForTiFlashTasks` | 豁免（`DEF.MPP`） | 仅在 `updateCopRuntimeStats` 检测到 TiFlash 执行摘要（`ExecutorId != nil`）时调用 |
| 5 | `distsql.go:175` | `Analyze` | 委托 / 不移植（`X`） | `ANALYZE TABLE` 由集群中的 Go TiDB 承担（宪章第四条 3）；唯一调用方是 `analyze_col.go`、`analyze_idx.go`（X） |
| 6 | `request_builder.go:234` | `RequestBuilder.SetAnalyzeRequest` | 委托 / 不移植（`X`） | 仅服务于 `Analyze`（`analyze_col.go`、`analyze_idx.go`） |
| 7 | `distsql.go:208` | `Checksum` | 不移植（`D12.14` / `X`） | 仅由 `pkg/executor/checksum.go`（`ADMIN CHECKSUM TABLE`，不移植的 `D12.14`，N0 §3）与 `br/pkg/checksum/executor.go:292`（X）调用 |
| 8 | `request_builder.go:247` | `RequestBuilder.SetChecksumRequest` | 不移植（`D12.14` / `X`） | 仅服务于 `checksum.go`（`D12.14`）与 `br/pkg/checksum/executor.go`（X） |
| 9 | `request_builder.go:138` | `RequestBuilder.SetIndexRanges` | 不移植（`D12.14` / `X`） | 全库非测试调用方仅有 `checksum.go:309`（`D12.14`）、`admin.go:125`（`D12.14`）、`br/pkg/checksum/executor.go:280`（X），无只读 SQL 调用方 |
| 10 | `request_builder.go:147` | `RequestBuilder.SetIndexRangesForTables` | 委托 / 不移植（`X`） | 全库非测试调用方仅有 `pkg/executor/analyze_idx.go:151`（X），无只读 SQL 调用方 |
| 11 | `request_builder.go:720` | `IndexRangesToKVRangesForTables` | 不移植（`X`） | 全库非测试调用方仅有 `SetIndexRangesForTables`（`:149`，X）与 `pkg/lightning/backend/local/duplicate.go:237`（X），无只读 SQL 调用方 |
| 12 | `request_builder.go:860` | `BuildTableRanges` | 不移植（`X`） | 全库非测试调用方仅有 `br/pkg/backup/client.go:796`（BR 备份，X），无只读 SQL 调用方 |
| 13 | `request_builder.go:890` | `appendRanges` | 不移植（`X`） | 仅被 `BuildTableRanges` 调用（BR 备份，X） |
| 14 | `distsql.go:268` | `init` | 不移植（编译期替代） | Go 在运行时用 `unsafe.Pointer` 探测字节序赋给包级变量 `systemEndian`；Rust 中 `get_system_endian()` 用 `#[cfg(target_endian = "big")]` 编译期常量实现，无需 `unsafe` 与运行时 `init`（与 D07.2 `coprocessor.go:init` 同理） |

---

## 4. Go → Rust 的结构映射

### 4.1 Crate 归属与文件布局（N0）

| Go 位置 | 所属块 | Rust 位置 | crate（N0 rank） |
|---|---|---|---|
| `pkg/distsql/distsql.go` | D07.3 | `crates/store/src/distsql/mod.rs`（或 `distsql.rs`） | `tidb-store` (11) |
| `pkg/distsql/request_builder.go` | D07.3 | `crates/store/src/distsql/request_builder.rs` | `tidb-store` (11) |
| `pkg/distsql/select_result.go` | D07.3 | `crates/store/src/distsql/select_result.rs` | `tidb-store` (11) |
| `pkg/distsql/context/context.go` | D00.1（P1-1） | `crates/sessionctx/src/distsql_ctx.rs` | `tidb-sessionctx` (7) |
| `pkg/sessionctx/stmtctx/stmtctx.go`（`GetOrInitDistSQLFromCache`） | D00.1（P1-1） | `crates/sessionctx/src/stmtctx.rs` | `tidb-sessionctx` (7) |
| `pkg/util/ppcpuusage/cpuusages.go` | D00.3 | `crates/execdetails/src/ppcpuusage.rs`（或 `crates/sessionctx/src/ppcpuusage.rs`） | `tidb-execdetails` (4) |

**依赖方向核对（全部严格向下，满足 `scripts/check_layers.py`）**：
- `tidb-kv` (5) 依赖 `tidb-execdetails` (4)（新增，4 < 5）；下沉 `Variables`、`RateLimit` 与 `StoreLabel` 到 `tidb-kv`（原在 `tidb-store` rank 11），`tidb-store::copr::tikv` 重导出。
- `tidb-sessionctx` (7) 已依赖 `tidb-base` (0)、`tidb-memory` (1)、`tidb-execdetails` (4)、`tidb-kv` (5)，无需新增向上依赖即可容纳 `DistSQLContext` 与 `GetOrInitDistSQLFromCache`。
- `tidb-store` (11) 新增依赖 `tidb-metrics` (0)、`tidb-types` (2)、`tidb-chunk` (3)、`tidb-codec` (4)、`tidb-sessionctx` (7)、`tidb-expression` (8)、`tidb-infoschema` (9)，全部 rank < 11。
- `tidb-executor` (14) 新增依赖 `tidb-store` (11)（11 < 14），以调用 `tidb_store::distsql`。

### 4.2 `DistSQLContext` 与 `StatementContext` 缓存（P1-1，`M-CTX`）

- `DistSQLContext` 是 `Send + Sync`，通过 `Arc<DistSQLContext>` 在语句与各 `SelectResult` 间共享（对应 Go `*distsqlctx.DistSQLContext`）。
- 共享可变句柄均采用线程安全引用：`warn_handler: Option<WarnAppenderRef>`、`client: Option<Arc<dyn kv::Client>>`、`session_mem_tracker: Option<Arc<Tracker>>`、`runtime_stats_coll: Option<Arc<RuntimeStatsColl>>`、`sql_killer: Option<Arc<SqlKiller>>`、`cpu_usage: Arc<SQLCPUUsages>`、`exec_details: Arc<SyncExecDetails>`、`try_cop_lite_worker: Arc<AtomicU32>`。
  - `StatementContext.sync_exec_details`（`crates/sessionctx/src/stmtctx.rs:419`）改为 `Arc<SyncExecDetails>`（对外访问方法不变），使 `DistSQLContext` 共享同一份语句执行明细（与 Go `&sc.SyncExecDetails` 等价）。
- `DistSQLContext::detach(&self) -> DistSQLContext`：照 `context.go:103-119` 逐字段复制，并新建独立的 `SQLCPUUsages`（复制当前 CPUUsages 快照）、独立的 `kv_vars`（`killed` 仍指向同一个 `sql_killer` 原子信号）与独立的 `try_cop_lite_worker`（`Arc::new(AtomicU32::new(self.try_cop_lite_worker.load(Ordering::Relaxed)))`，对应 Go 值语义复制 `newCtx := *dctx`，避免与原上下文共享同一原子计数）。
- `StatementContext::get_or_init_dist_sql_from_cache`：在 `StatementContext` 上加 `dist_sql_ctx_cache: OnceLock<Arc<DistSQLContext>>`，写法与已有的 `get_or_init_ranger_ctx_from_cache`（`stmtctx.rs:1372`）完全一致。

### 4.3 `kv::Response` / `kv::ResultSubset` 的统计接口（`M-IFACE`）

Go 中 `selectResult` 对 `kv.ResultSubset` 和 `kv.Response` 做三次接口断言：
1. `resultSubset.(CopRuntimeStats)`（`select_result.go:463`）
2. `r.resp.(copr.CopInfo)`（`:90, 640, 749`）
3. `r.resp.(copr.HasUnconsumedCopRuntimeStats)`（`:739`）

在 Rust 中，`LazyCopResponse<S: TiKVStore>` 是泛型类型，无法对 `&dyn kv::Response` 做具体类型 `downcast_ref`（生产为 `ClientRustStore`，测试为 mock store）。因此按 `M-IFACE` 在 `tidb-kv` 中增加对象安全的默认方法（`CopRuntimeStats` 移入 `tidb-kv` 并由 `tidb-store::copr` 重导出）：

- `kv::ResultSubset`：
  - `fn get_cop_runtime_stats(&self) -> Option<&CopRuntimeStats> { None }`（`CopResponse` 返回 `self.detail.as_deref()`）
- `kv::Response`：
  - `fn cop_concurrency(&self) -> Option<(isize, isize)> { None }`
  - `fn cop_store_batch_info(&self) -> Option<(u64, u64)> { None }`
  - `fn cop_build_task_elapsed(&self) -> Option<Duration> { None }`
  - `fn collect_unconsumed_cop_runtime_stats(&self) -> Vec<CopRuntimeStats> { Vec::new() }`

**两处 D07.2 `CopClient` / `LazyCopResponse` 配合调整（维护者已定）**：
1. **异步即时构造 `CopIterator`（消除首次 `Next` 前并发度差异）**：Go 的 `Client.Send` 同步完成 region 切分与 `copIterator` 构造，`IndexLookUp` 的 `fetchHandlesRolling` 在首次 `Next` 之前调用 `GetSelectResultConcurrency`（`pkg/executor/distsql.go:1007-1016`）读取按任务数截断后的并发度与 small-task 并发度。为避免影响 `IndexLookUp` 的在途限流与 TiKV 请求调度，**将 `distsql::select` 与 `distsql::select_with_runtime_stats` 改为 `async fn`**，并在 `kv::Client` 上新增异步默认方法 `async fn send_async(&self, ctx, req, vars, option)`（默认委托 `self.send`，`CopClient` 覆写为立即异步 `build_cop_iterator` + `open` 并返回已就绪的 `CopResponse` 包装）。从而 `GetSelectResultConcurrency` 在首次 `Next` 之前即可返回与 Go 100% 相同的 `(concurrency, small_task_concurrency)`。
2. **`close()` 之后的 `buildTaskDuration` / `storeBatchInfo`**：Go `selectResult.close` 在 `r.resp.Close()` **之后**的 `defer` 中读 `GetBuildTaskElapsed` / `GetStoreBatchInfo`（`select_result.go:748-762`）；在 `close()` 丢弃内部 `CopIterator` 前先把最终的 `(build_task_elapsed, store_batch_info)` 缓存到外层响应结构体字段中，使 `close()` 后仍可读到与 Go 同一时刻的最终统计。

### 4.4 `RequestBuilder` 与类型映射

| Go | Rust | 说明 |
|---|---|---|
| `type RequestBuilder struct { kv.Request; is; err; used; dag }` | `pub struct RequestBuilder { pub request: kv::Request, is: Option<Arc<dyn MetaOnlyInfoSchema>>, err: Option<Error>, used: bool, dag: Option<tipb::DagRequest> }` | `pub request` 保持公开，方便 D12.1 直接读写 `builder.request.paging` / `cacheable` 或传给 `newClosestReadAdjuster` |
| `(*RequestBuilder).SetXxx(...) *RequestBuilder` | `pub fn set_xxx(&mut self, ...) -> &mut Self` | 支持与 Go 相同的链式调用 |
| `(*RequestBuilder).Build() (*kv.Request, error)` | `pub fn build(&mut self) -> Result<kv::Request, Error>` | 单次使用检查在 `#[cfg(test)]` 下生效（对应 `intest.InTest`）；返回构造好的 `kv::Request`（移动/替换出 `self.request`） |
| `dag.Marshal()` / `selectResp.Unmarshal()` / `selectResp.Size()` | `protobuf::Message::{write_to_bytes, merge_from_bytes, compute_size}` | `tipb` crate 使用 PingCAP `rust-protobuf 2.8`，与 `crates/store` / `crates/expr-pushdown` 一致 |
| `tableRangesToKVRangesWithoutSplit` / `CommonHandleRangesToKVRanges` | 参数 `dctx: Option<&DistSQLContext>` | Go 允许 `dctx == nil`（`request_builder.go:833`，如 `BuildTableRanges`、单测）；Rust 用 `Option` 显式表达 |
| `interruptSignal *atomic.Value`（L710, 726, 784） | `interrupt_signal: Option<&AtomicBool>` | Go 仅存 `bool`（L818），Rust 用 `AtomicBool` 避免未初始化 `nil` 类型断言 panic（L2） |

### 4.5 `SelectResult`、`SelectResultIter` 与所有权设计

```rust
#[async_trait]
pub trait SelectResult: Send {
    async fn next_raw(&mut self, ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>, Error>;
    async fn next(&mut self, ctx: &tidb_kv::Context, chk: &mut Chunk) -> Result<(), Error>;
    fn into_iter(
        &mut self,
        intermediate_field_types: Vec<Vec<Arc<FieldType>>>,
    ) -> Result<Box<dyn SelectResultIter>, Error>;
    fn close(&mut self) -> Result<(), Error>;
    // M-IFACE: 供 GetSelectResultConcurrency 查询底层 CopInfo
    fn cop_concurrency(&self) -> Option<(isize, isize)> {
        None
    }
}
```

- **`IntoIter` 所有权（L2 [M-IFACE]）**：`SelectResultImpl` 内部包一层 `inner: Option<SelectResultInner>`。调用 `into_iter` 时把 `SelectResultInner` 移入 `SelectResultIter`，原 `SelectResultImpl` 的 `inner` 变为 `None`：
  - 之后若对原 `SelectResult` 再调用 `next` / `next_raw` / `into_iter` / `close`，统一返回 `Error::plain("selectResult is invalid after IntoIter()")`，与 Go `TestSelectResultIter`（`select_result_test.go:387-395`）逐字一致；
  - `serialSelectResults` / `sortedSelectResults` 的 `into_iter` 直接返回 `Error::plain("not implemented")`，不破坏自身状态，调用方失败后仍可安全 `close()`（对应 `pkg/executor/distsql.go:912`）。
  - 第 1 部分中 `selectResult.into_iter` 暂为 `blocked-by:D07.3` 边界桩（§5），第 2 部分替换为正式实现。
- **`SelectResultRow`（`M-ROW`）**：第 2 部分中 `SelectResultRow` 跨 `SelectResultIter::next` 返回行，按 `M-ROW` 持有 `{ channel_index: isize, row: RowHandle }`（即 `Arc<Chunk>` + 行号），借用行时再调用 `row.as_row()`，避免自引用生命周期。
- **字段类型共享（N13 §8，`M-SHARE`）**：`Select` / `SelectWithRuntimeStats` 接收 `field_types: Vec<Arc<FieldType>>`（与 D12.0 `exec::ret_types` 返回的 `&[Arc<FieldType>]` 零复制对接）；传入 `chunk::new_chunk_with_capacity`（已支持 `F: Borrow<FieldType>`）时不复制。注：`chunk::codec::new_decoder`（`crates/chunk/src/codec.rs:287`）目前按值接收 `Vec<FieldType>`，第 1 部分同步将其参数放宽为 `impl IntoIterator<Item = impl Borrow<FieldType>>`（或由 `SelectResult` 仅在创建解码器时拷贝一次，不按行拷贝）。
- **零拷贝读取 `RowsData`**：
  - `readFromChunk`：`Bytes::from(std::mem::take(&mut self.select_resp.chunks[idx].rows_data))` 零拷贝交给 `chunk::Decoder::reset(Bytes)`；
  - `readFromDefault`：记录当前 `tipb.Chunk` 内的字节偏移 `rows_data_offset: usize`，`codec::Decoder::decode_one(&rows_data[offset..], ...)` 后只推进偏移，不反复 `drain` 或拷贝切片。
- **`selectResultRuntimeStats`（`M-MAPORDER`）**：
  - `cop_resp_time: Percentile<DurationWrapper>`、`proc_keys: Percentile<Int64>`、`backoff_sleep: BTreeMap<String, Duration>`（Go 为 `map[string]time.Duration`，`String()` 遍历顺序不确定；Rust 用 `BTreeMap` 键序确定输出）。
  - `String()` 与 Go `select_result.go:1057-1139` 逐字对齐（含 `copr_cache_hit_ratio` 与 `copr_cache: disabled` 分支、`build_task_duration`、`max_distsql_concurrency`、`max_extra_concurrency`、`store_batch_num`、`store_batch_fallback_num`、`backoff{...}`）。

### 4.6 并发、取消、错误与 `defer` 顺序（N3，`M-GO`、`M-KILL`、`M-DEFER`、`M-ERR`）

1. **无自起 goroutine**：`pkg/distsql` 本身不启动任何 goroutine / task，也没有 channel。所有后台并发均在下层 D07.2 `CopIterator` 中（已在 `ExecPool` 上 spawn 并实现 `Drop` 中止 worker）。
2. **`Select` / `SelectWithRuntimeStats` 为异步函数（维护者已定）**：为在返回 `SelectResult` 前即完成 region 定位并构造/打开 `CopIterator`（使 `GetSelectResultConcurrency` 在首次 `Next` 前与 Go 一致，§4.3），`distsql::select` 与 `distsql::select_with_runtime_stats` 定义为 `async fn`（调用 `kv::Client::send_async`）；`SelectResult::next`、`SelectResult::next_raw`、`SelectResultIter::next` 同样为 `async`。`SelectResult::close` 为同步函数（与 `kv::Response::close` 一致）。
3. **取消与 `Drop` 安全网（N3 §3.5，`M-DEFER`）**：
   - `SelectResultImpl`、`SerialSelectResults`、`SortedSelectResults`、`SelectResultIter` 均实现 `Drop`：若未经显式 `close()` 即被丢弃（future 取消、`?` 提前返回、panic unwind），`Drop` 关闭底层 `resp`（触发 `CopIterator` 的 `Drop`，立刻停止后台 worker）、归还 `selectRespSize` 内存记账，但**不注册** runtime stats 也不上报 histogram（与 N3 §3.5 第 2 条一致）。
   - `close()` 保持幂等（重复调用不重复扣减内存或重复注册统计）。
4. **KILL 与时间片让出（`M-KILL`）**：
   - 保留 `fetchRespWithIntermediateResults`（`select_result.go:454`）中的 `r.ctx.sql_killer.handle_signal()?` 检查点；
   - 在 `fetchRespWithIntermediateResults` 每次拿到响应后，以及 `readFromDefault` / `sortedSelectResults.next` 的解码循环中按 N3 规则放 `tidb_runtime::checkpoint`（与 PoC `crates/distsql/src/lib.rs:140` 一致），防止超大响应连续解码占住 worker 线程超过 1ms 时间片。
   - `indexRangesToKVWithoutSplit`（`request_builder.go:813-824`）保留每 8 个区间检查 `interrupt_signal` 与 `mem_tracker.handle_kill_signal()?`（`M-PANIC`：Go 中 `HandleKillSignal` panic，Rust 返回 `Result`）。
5. **`defer` 与清理顺序**：
   - `selectResult.close`（`select_result.go:732-763`）：`DistSQLPartialCountHistogram` → 释放 `selectRespSize` → 收集 `unconsumedCopStats` 并合并 → `let close_res = self.resp.close();` → 无论 `close_res` 是否出错，若 `r.stats` 与 `r.ctx` 非空，从 `self.resp` 读最终 `buildTaskDuration` / `storeBatchedNum` / `storeBatchedFallbackNum` 并注册到 `RuntimeStatsColl` → 返回 `close_res`（与 Go `defer` 在 `r.resp.Close()` 之后执行严格一致）。
   - `sortedSelectResults.Close`（`select_result.go:268-278`）：关闭全部子 `SelectResult` 并释放所有 `cachedChunks` 记账，返回遇到的第一个错误（见 §8 第 4 条，防止提前 return 泄漏剩余子 `CopIterator` 的 worker）。
   - `serialSelectResults.Close`（`select_result.go:325-332`）：照 Go 关闭全部子 `SelectResult` 并返回最后一个错误。
6. **错误（`M-ERR`）**：
   - `selectResp.Error`：`dbterror::ClassTiKV.synthesize(err.code, err.msg)`；
   - `selectResp.Warnings`：逐条 `dbterror::ClassTiKV.synthesize(w.code, w.msg)` 写入 `dctx.append_warning(...)`；
   - 其余文本错误（`"client returns nil response"`、`"unsupported encode type:..."`、`"selectResult is invalid after IntoIter()"`、`"table ... can not be read by ... txn_scope"` 等）保持 Go 文本逐字一致。

### 4.7 内存记账（A1，`M-MEM`）

`pkg/distsql` 中共有 4 处 `memTracker.Consume`（逐处说明）：

| Go 位置 | Go 做法 | Rust（A1 / `M-MEM`） |
|---|---|---|
| `select_result.go:414, 440, 718, 736`（`selectResult.memConsume`） | 反序列化 `tipb.SelectResponse` 后按 `selectResp.Size()` 手工 `Consume(+size)`，下一批或 `close` 时 `Consume(-size)` | `tipb::SelectResponse` 由 `rust-protobuf` 分配（不经 `TrackedAlloc`），沿用 `mem_tracker.consume(resp_size)?` + `Drop` 守卫归零（与 D07.2 `copResponse` 一致，超限由 `?` 返回 8175 错误，不再 panic）；**同时修正 Go `selectResultIter.Next` 漏减记账的缺陷**（§8 第 1 条），确保关闭后必归零（A1 §5） |
| `select_result.go:181, 234, 274`（`sortedSelectResults`） | 缓存 chunk 分配/更新/关闭时按 `MemoryUsage()` 手工 `Consume(±delta)` | 第 2 部分移植时，`cachedChunks` 的列缓冲挂到 `mem_tracker` 的 `TrackedAlloc` 自动记账，更新后调用 `mem_tracker.checkpoint()?`（A1 §3.2） |
| `request_builder.go:791, 816`（`indexRangesToKVWithoutSplit`） | 按 `unsafe.Sizeof(kv.KeyRange{}) * len(ranges)` 与第 0 项容量估算 `Consume` | 第 2 部分移植时，改为记入实际构建的 `KeyRange` 字节容量并在检查点调用 `checkpoint()?`（同时检查 `handle_kill_signal()?`） |

### 4.8 共享修改的审计（`M-FT`，N1 §5）

对 `pkg/distsql/*.go`（排除 `*_test.go`）运行 plan 模板 §4 的两个 grep：
- `.Index = ` / `.RetType = `：**0 处**。
- `.SetFlag(` / `.AddFlag(` / `.DelFlag(` / `.SetFlen(` / `.SetDecimal(` / `.SetCharset(` / `.SetCollate(` / `.SetType(` / …：**0 处**。

读代码补充检查发现 **2 处切片底层数组共享写入（无 `FieldType` 修改）**：

| Go 位置 | 写入 / 共享内容 | 结论 | 处理 |
|---|---|---|---|
| `request_builder.go:577, 608`（`SplitRangesAcrossInt64Boundary`） | `unsignedRanges = ranges[idx:]`，L608 执行 `append(unsignedRanges, signedRanges...)`；当调用方切片 `cap > len` 时写入调用方底层数组。`table_reader.go:292` 传入的是计划中的 `ts.Ranges`，在实例计划缓存下为共享计划 | **V0**（写入值相同，在 Go 中属潜在数据竞争） | Rust `split_ranges_across_int64_boundary` 接收 `&[Range]`、返回拥有型 `(Vec<Range>, Vec<Range>)`，绝不别名或修改调用方切片 |
| `request_builder.go:675`（`PartitionHandlesToKVRanges`） | `append(commonHandle.Encoded(), 0)` 在 `len >= 9 && cap > len` 时覆写 `CommonHandle` 底层数组下一字节（同 §8 第 6 条） | **V0**（输出 `EndKey` 与 Go 一致） | Rust 使用 `Key::next()` 分配独立 `Vec<u8>`，无底层数组别名覆写 |

其余只读检查：`CommonHandleRangesToKVRanges`（`request_builder.go:755`）仅修改新建临时切片 `rans[i]` 的 `LowVal[0]`；`selectResult`、`sortedSelectResults`、`selRespChannelIter` 对 `fieldTypes`、`schema`、`byItems` 全部只读。

### 4.9 对 D12.1（#411）等其他块的接口约定

供 #411（`TableReaderExecutor` / `IndexReaderExecutor` / `IndexLookUpExecutor`）与 `ConstructDAGReq`、`SessionCore` 直接对齐的公开 API：

1. **`tidb_sessionctx::distsql_ctx::DistSQLContext`**（`Arc<DistSQLContext>`，实现 `Send + Sync`）：
   - `pub fn append_warning(&self, warn: Error)`
   - `pub fn detach(&self) -> DistSQLContext`（其中 `try_cop_lite_worker` 为独立新 `Arc<AtomicU32>`）
   - `StatementContext::get_or_init_dist_sql_from_cache(&self, create: impl FnOnce() -> Arc<DistSQLContext>) -> Arc<DistSQLContext>`
   - `SessionCore::get_dist_sql_ctx(&self) -> Arc<DistSQLContext>`（D05.3，同步返回，不再返回 `blocked`）
2. **`tidb_kv::Context` 来源（`M-GOCTX`）**：
   - `tidb_kv::Context`（`crates/kv/src/context.rs`）只携带可选的 `RequestSource`（取消靠丢弃 future，不走 Context）。执行器在调用 `distsql::select*` 与 `SelectResult::{next, next_raw}` 时，由 `dctx` 构造轻量的 `tidb_kv::Context`（常规 SQL 为 `Context::background()`；若 `dctx.in_restricted_sql` 或设了 `request_source_type` / `explicit_request_source_type` 则 `Context::background().with_request_source(...)`，与 D12.1 第 1 部分 §2 第 7 条一致）。
3. **`tidb_infoschema::context::MetaOnlyInfoSchema`**（`InfoSchemaV2` 实现）：
   - 继承 `tidb_sessionctx::expropt::MetaOnlyInfoSchema`（`schema_meta_version`），补充同步方法 `placement_bundle_by_physical_table_id(&self, id: i64) -> Option<Arc<Bundle>>`（不要求把异步 `table_info_by_id` 暴露为同步，见 §2.2）。
4. **`tidb_store::distsql` 第 1 部分对外契约**：
   - `RequestBuilder`（`Default` 构造，`pub request: tidb_kv::kv::Request`）：全部 TableReader 所需 setter（`&mut self -> &mut Self`）+ `pub fn build(&mut self) -> Result<tidb_kv::kv::Request, Error>`。
   - `pub async fn select(ctx: &tidb_kv::Context, dctx: Arc<DistSQLContext>, kv_req: &tidb_kv::kv::Request, field_types: Vec<Arc<FieldType>>) -> Result<Box<dyn SelectResult>, Error>`
   - `pub async fn select_with_runtime_stats(ctx: &tidb_kv::Context, dctx: Arc<DistSQLContext>, kv_req: &tidb_kv::kv::Request, field_types: Vec<Arc<FieldType>>, cop_plan_ids: Vec<isize>, root_plan_id: isize) -> Result<Box<dyn SelectResult>, Error>`
   - `pub fn set_encode_type(ctx: &DistSQLContext, dag_req: &mut tipb::DagRequest)`（`ConstructDAGReq` 调用）
   - `pub fn get_system_endian() -> tipb::Endian`
   - `pub fn split_ranges_across_int64_boundary(ranges: &[Range], keep_order: bool, desc: bool, is_common_handle: bool) -> (Vec<Range>, Vec<Range>)`
   - `pub fn table_handle_ranges_to_kv_ranges(dctx: Option<&DistSQLContext>, tids: &[i64], is_common_handle: bool, ranges: &[Range]) -> Result<KeyRanges, Error>`
   - `pub fn table_ranges_to_kv_ranges(tid: i64, ranges: &[Range]) -> Vec<KeyRange>`
   - `pub fn common_handle_ranges_to_kv_ranges(dctx: Option<&DistSQLContext>, tids: &[i64], ranges: &[Range]) -> Result<KeyRanges, Error>`
   - `pub fn encode_index_key(dctx: Option<&DistSQLContext>, ran: &Range) -> Result<(Vec<u8>, Vec<u8>), Error>`
   - `pub fn verify_txn_scope(txn_scope: &str, physical_table_id: i64, is: &dyn MetaOnlyInfoSchema) -> Result<bool, Error>`（**L2**：Go `request_builder.go:766` 返回 `bool`，Rust 改为 `Result<bool, Error>` 以在命中 placement bundle 时显式返回 `blocked("X", ...)`；第 1 部分解除 `PointGetExecutor::verify_txn_scope` 桩）
   - `pub fn get_select_result_concurrency(sr: &dyn SelectResult) -> Option<(isize, isize)>`
   - `pub fn new_serial_select_results(select_results: Vec<Box<dyn SelectResult>>) -> Box<dyn SelectResult>`
   - `SelectResult` trait（见 §4.5）：`async fn next`、`async fn next_raw`、`fn into_iter`、`fn close`、`fn cop_concurrency`；对象安全且 `Send`。
5. **`tidb_store::distsql` 第 2 部分补充契约（完整签名）**：
   - `RequestBuilder::{set_table_handles(&mut self, tid: i64, handles: &[Handle]) -> &mut Self, set_partitions_and_handles(&mut self, handles: &[PartitionHandle]) -> &mut Self}`；`kv::Request.copr_request_rate_limit: Option<Arc<RateLimit>>` 字段 + `CopIterator` 限速接线（`coprocessor.rs:1309, 1804`）。
   - `pub fn table_handles_to_kv_ranges(tid: i64, handles: &[Handle]) -> (KeyRanges, Vec<isize>)`
   - `pub fn partition_handles_to_kv_ranges(handles: &[PartitionHandle]) -> (KeyRanges, Vec<isize>)`
   - `pub fn index_ranges_to_kv_ranges(dctx: Option<&DistSQLContext>, tid: i64, idx_id: i64, ranges: &[Range], mem_tracker: Option<&Tracker>) -> Result<KeyRanges, Error>`
   - `pub fn index_ranges_to_kv_ranges_with_interrupt_signal(dctx: Option<&DistSQLContext>, pid: i64, idx_id: i64, ranges: &[Range], mem_tracker: Option<&Tracker>, interrupt_signal: Option<&AtomicBool>) -> Result<KeyRanges, Error>`
   - `pub fn new_sorted_select_results(eval_ctx: &EvalContext, select_results: Vec<Box<dyn SelectResult>>, schema: Option<&Schema>, by_items: Vec<ByItems>, mem_tracker: Option<Arc<Tracker>>) -> Result<Box<dyn SelectResult>, Error>`（`schema` 允许 `None`，对应 `pkg/executor/distsql.go:974` 按前若干列归并）
   - `#[async_trait] pub trait SelectResultIter: Send { async fn next(&mut self, ctx: &tidb_kv::Context) -> Result<Option<SelectResultRow>, Error>; fn close(&mut self) -> Result<(), Error>; }` 与 `pub struct SelectResultRow { pub channel_index: isize, pub row: RowHandle }`。

---

## 5. 边界桩

| Rust 位置 | Go 位置 | 类型 | 触发条件与行为 |
|---|---|---|---|
| `selectResult::into_iter`（第 1 部分临时） | `select_result.go:516` | 临时 `blocked-by:D07.3` | 仅 push-down `IndexLookUp` 调用；第 2 部分移除 |
| `Select` 中 `kvReq.StoreType == kv.TiFlash` 分支 | `distsql.go:102-106` | 永久 `blocked-by:DEF.MPP`（防御性，与 #411 一致） | #411 已在 `buildTableReader` 提前返回 `blocked("DEF.MPP", ...)`；此处防御性同样返回 `blocked("DEF.MPP", ...)` 而非普通错误，确保一旦可达仍按路由器规则处理 |
| `updateCopRuntimeStats` 中 `hasExecutor == true` 分支 | `select_result.go:668-672` | 永久 `DEF.MPP` | 仅 TiFlash 执行摘要带 `ExecutorId`；不调用 `recordExecutionSummariesForTiFlashTasks` |
| `VerifyTxnScope` 中 `bundle` 存在且 `txn_scope` 非空、非 `global` | `request_builder.go:770-780` | 永久 `blocked-by:X` | `placement.Bundle` 规则属 X（未构建）且 tidb-rs 无本地 TSO；非 global scope 命中带 placement policy 的表时显式返回 `blocked("X", ...)`，绝不静默返回 `true`。空或 `global` scope 以及无 placement policy 的表均按 L1 返回 `Ok(true)` |
| `SetFromSessionVars` 中配置了 runaway 规则时 | `request_builder.go:373` | 临时 `blocked-by:N10` | N10 决定前若会话/资源组存在生效的 runaway 规则，显式返回 `blocked("N10", ...)` 而非静默忽略（宪章第五条）；未配置 runaway 规则时正常执行 |

**本块第 1 部分解除的既有边界桩（净减 2 个；另 1 个交 #411 2A）**：
- `crates/session/src/session.rs:791`（`get_dist_sql_ctx`，`blocked-by:D07.3`）——由本块第 1 部分解除；
- `crates/executor/src/point_get.rs:524`（`verify_txn_scope`，`blocked-by:D07.3`）——由本块第 1 部分解除；
- `crates/executor/src/builder.rs:955`（`new_closest_read_adjuster`，`blocked-by:D07.3`）——本块提供 `Request.match_store_labels`，由 #411（D12.1 第 2A 部分）解除。

---

## 6. 测试计划

### 6.1 要移植的 Go 单元测试

`pkg/distsql/` 下全部 **32 个** Go 测试（不含 `TestMain` / `TestBenchDaily`），均不依赖真实 TiKV，按部分随代码移植：

| Go 测试文件 | Go 测试名 | 对应部分 | 说明 |
|---|---|---|---|
| `context/context_test.go` | `TestContextDetach` | 第 1 部分（P1-1） | 验证 `DistSQLContext::detach` 复制各标量/独立副本（`CPUUsage`、`KVVars`）并共享 `SQLKiller`、`ExecDetails`、`WarnHandler` 等 |
| `distsql_test.go` | `TestSelectNormal`、`TestSelectMemTracker`、`TestSelectNormalChunkSize`、`TestSelectWithRuntimeStats`、`TestSelectResultRuntimeStats` | 第 1 部分 | 用 mock `kv::Client` / `kv::Response` 覆盖 `TypeChunk` 与 `TypeDefault`、各种 `RequiredRows`（含 0 / 负 / 1 / 17 / 170）、中途 `Close` 内存归零、`RuntimeStats` 格式化与幂等（`rpc_info` 段见 §7 第 4 条） |
| `distsql_test.go` | `TestAnalyze`、`TestChecksum` | 不移植 | 对应不移植的 `Analyze` / `Checksum` |
| `request_builder_test.go` | `TestTableRangesToKVRanges`、`TestRequestBuilder1`–`TestRequestBuilder8`、`TestRequestBuilderTiKVClientReadTimeout`、`TestTableRangesToKVRangesWithFbs`、`TestScanLimitConcurrency`、`TestIndexLookUpPushDownScanConcurrency` | 第 1 部分 | 完整验证 `RequestBuilder` 全部字段、隔离级别/优先级映射、小 LIMIT 并发度=1、保序简单扫描并发度=2、重复 `Build` 报错、`verifyTxnScope` |
| `request_builder_test.go` | `TestTableHandlesToKVRanges`、`TestTablePartitionHandlesToKVRanges`、`TestIndexRangesToKVRanges`、`TestIndexRangesToKVRangesWithFbs`、`TestBuildTableRangeIntHandle`、`TestBuildTableRangeCommonHandle`、`TestRequestBuilderHandle` | 第 2 部分 | 索引区间、连续句柄合并、分区句柄、全表区间 |
| `select_result_test.go` | `TestUpdateCopRuntimeStats` | 第 1 部分 | 验证 `RuntimeStatsColl == nil`、摘要长度不匹配、正常记录 `tikv_task` |
| `select_result_test.go` | `TestNewSelRespChannelIter`、`TestSelRespChannelIterRead`、`TestSelectResultIter` | 第 2 部分 | 验证中间结果与主输出混合、`TypeChunk`/`TypeDefault`、`IntoIter` 后旧方法报错、长度不匹配报错 |

**额外 Rust 专项单测（取消安全、Drop、A1、边界情况）**：
- `select_result_drop_without_close_stops_workers_and_zeros_mem_tracker`：在拉取中途直接 `drop(sr)`（不调 `close()`），断言底层 `CopIterator` worker 全部中止、`mem_tracker.bytes_consumed() == 0` 且不向 `RuntimeStatsColl` 注册统计（N3 §3.5）。
- `select_result_close_is_idempotent`：多次调用 `close()` 不重复扣减内存、不重复注册统计。
- `select_result_close_registers_final_cop_info_after_resp_close`：验证 `LazyCopResponse::close` 后仍正确回填 `build_task_duration` 与 `store_batch_num`。
- `select_result_warnings_errors_and_sql_killer`：TiKV 返回 `SelectResponse.Error`、多条 `Warnings`、`SQLKiller` 已置位（1317 / 3024 / 8175）时的精确错误码与 warning 顺序。
- `read_from_chunk_reuse_threshold_0_8`：覆盖 `RemainedRows > int(RequiredRows * 0.8)` 时（`chk` 空直接交换、`chk` 非空提前返回）与 `<= 0.8` 拼接路径。
- `verify_txn_scope_global_and_placement_stub`：空/`global` 返回 `Ok(true)`；非 global 无 bundle 返回 `Ok(true)`；非 global 有 bundle 返回 `blocked("X", ...)`。
- `sorted_select_results_close_drains_all_on_error`（第 2 部分）：第 0 个子结果 `close` 报错时，后续子结果仍全部关闭且内存归零。
- `select_result_iter_mem_tracker_zeros_across_multiple_responses`（第 2 部分）：钉住 §8 第 1 条内存记账修复。

### 6.2 govec 向量

在 tracker 新增领域 `distsql`（`govec/domains/distsql.go`，amd64 / arm64 双目标；回放测试放 `crates/store/tests/govec_distsql.rs`，并加入 CI 的 `TZ=America/Sao_Paulo` 时区校验）：

| op | 部分 | 覆盖函数与输入形状 | 比较内容 |
|---|---|---|---|
| `split_int64` | 第 1 部分 | `SplitRangesAcrossInt64Boundary`：有符号/无符号/common handle、空切片、全在左/全在右/跨 `MaxInt64`、边界开闭组合（含单边为空）、`keepOrder × desc` 四组合、随机多区间 | 返回的两组 `Range` 逐项一致 |
| `table_ranges` | 第 1 部分 | `TableHandleRangesToKVRanges` / `TableRangesToKVRanges` / `CommonHandleRangesToKVRanges` / `EncodeIndexKey` / `encodeHandleKey`：单表/多分区、整型（含 `MinInt64`/`MaxInt64`/开闭区间/空区间）、common handle（多列混合类型：INT/VARCHAR 带 collation/DECIMAL/TIMESTAMP 跨时区/NULL/严格模式错误） | 生成的 `KeyRanges` 字节与错误逐字节一致 |
| `request_builder` | 第 1 部分 | `RequestBuilder` 全部第 1 部分 setter + `Build` + `SetEncodeType`：`Scan -> Limit`（临界 99999 / 100000）、`IndexScan -> Limit -> IndexLookup`、单算子 `KeepOrder` 默认并发 15 vs 自定义并发、`WeakConsistency`/`RCCheckTS`/`SI`、各 `Priority`、paging/storeBatch/readReplicaScope/matchStoreLabels；**特别覆盖 `tipb.DAGRequest` 字段 3、4、8、11 为零值的情形**（Go gogoproto `nullable=false` 总写出，而 `rust-protobuf 2.8` 默认省略未显式设置的字段，须确认序列化 `Data` 逐字节一致） | 产出 `kv.Request` 各字段与序列化 `Data` 字节一致 |
| `select_decode` | 第 1 部分 | `selectResult.Next`（`TypeChunk` 与 `TypeDefault`）+ `selectResultRuntimeStats.{String,Clone,Merge,calcCacheHit}`：多类型列（INT/UINT/DOUBLE/DECIMAL/VARCHAR/TIMESTAMP/NULL）、多 chunk 拼接与 `0.8` 复用边界、各种 `RequiredRows`、TiKV error/warning；runtime stats 随机样本（排除多键 `backoffSleep` 的随机 map 顺序差异，单键及合并后按确定顺序比对） | 每次 `Next` 输出的 chunk 行值、warning、错误、统计文本 |
| `index_and_handles` | 第 2 部分 | `TableHandlesToKVRanges`、`PartitionHandlesToKVRanges`、`IndexRangesToKVRanges*`：连续/非连续/`MaxInt64`/跨分区整型与 common handle、多列索引、全局/本地索引分区表 | `KeyRanges` 与 `hints` 逐字节一致 |
| `sorted_and_iter` | 第 2 部分 | `NewSortedSelectResults`（多分区 ASC/DESC/多列/NULL/含空子结果）、`selectResultIter`（主输出 + 多个 `IntermediateOutputs`，混合 `TypeChunk`/`TypeDefault`） | 输出行序列与 `ChannelIndex` 逐行一致 |

不适合 govec 的函数在 tracker 豁免表中登记并指向对应单测（如依赖时钟测量的 `fetchDuration` 上报、`DistSQLContext.Detach` 指针语义、`WithSQLKvExecCounterInterceptor` 空操作，以及 §3.3 的 14 个不移植函数）。

### 6.3 端到端 `tools/ab` 对比验收（吸取 #338 回归教训）

按 [D12.2 spec §7](../D12.2-basic-executors/spec.md)（#338 撤回教训）与工作流要求：本块第 1 部分本身不直接把 SQL 语句从回退切到新栈（实际切入发生在 #408 `TableReaderExecutor` 接上本块之后）；但在本块第 1 部分与 #408 联调合入前，必须以真实 TiKV 集群（tiup playground `v8.5.8`）跑过以下端到端验收，绝不只靠 mock 单测：

1. **真实 TiKV 范围查询与双编码格式对比**：在 `sbtest1`（多 region 分裂）及含 `BIGINT UNSIGNED` 主键（跨 `MaxInt64`）、common handle、VARCHAR/DECIMAL/TIMESTAMP、NULL 的表上，分别以 `tidb_enable_chunk_rpc = ON`（`TypeChunk`）与 `OFF`（`TypeDefault`）、`tidb_enable_paging = ON` / `OFF` 跑 `tools/ab`（文本协议 + `PREPARE`/`EXECUTE` 计划缓存命中与未命中）：
   - 全表扫描、整型范围扫描（`BETWEEN ? AND ?`）、升序/降序 `ORDER BY id`（验证 `KeepOrder` 并发度=2 与 `SplitRangesAcrossInt64Boundary`）、小 `LIMIT`（验证并发度=1）、跨 region 扫描、空范围、多列投影（防范 #338 类列下标错位）；
   - 逐行比较结果与线协议列定义元数据（名字、类型、flen、decimal、flag、charset），要求 **0 mismatch**。
2. **`--new-stack=off` 回归验证**：验证默认模式仍走原型链路、不构造新栈 `DistSQLContext` / `selectResult`，现有 weekly / ab 基线零退化（宪章第七条 4）。
3. **sysbench `oltp_read_only`**：待 #408 与聚合/排序链路接通后，由维护者对真实集群执行 `tools/ab` 与 sysbench 对比。

---

## 7. 与 Go 的已知差异

每一处差异注明 Go 文件和行号：

1. **字节序探测改为编译期常量**（`distsql.go:268-276`）：Go 在 `init()` 中通过 `unsafe.Pointer` 判断大小端；Rust `get_system_endian()` 用 `#[cfg(target_endian = "big")]` 编译期决定，行为完全等价，无 `unsafe`。
2. **`checkAlignment` 用 `std::mem::size_of`**（`distsql.go:247-254`）：Go 用 `unsafe.Sizeof(types.MyDecimal{}) == 40`；Rust `MyDecimal` 为 `#[repr(C)]` 且 `size_of::<MyDecimal>() == 40`（`crates/types/src/mydecimal.rs:288`），在 64 位目标上与 Go 同样恒为 `true`。
3. **TiFlash / MPP 与 Analyze / Checksum / BR 专用路径不移植**（`distsql.go:42-54, 102-106, 136-156, 175-222`、`request_builder.go:138-154, 234-255, 720-722, 860-917`、`select_result.go:600-621, 660-664, 668-672`）：属 `DEF.MPP` / `X`（委托/BR/Lightning）/ `D12.14`，见 §3.3；`Select` 收到 `kv.TiFlash` 时防御性返回 `blocked("DEF.MPP", ...)`（与 #411 一致）。
4. **`RegionRequestRuntimeStats`（`rpc_info`）缺失**（`select_result.go:625, 970, 990-996, 1007, 1022, 1044, 1119-1124`）：沿用 D07.2 §6.3 第 4 条（client-rust 不提供 client-go 的 `RegionRequestRuntimeStats`），真实 TiKV 查询的 `EXPLAIN ANALYZE` 不输出 `rpc_info:{...}` 子段（用户可观测性差异）。
5. **`CopRuntimeStats.CalleeAddress` 须由前置任务在 `dulao5/client-rust` 补齐**（`select_result.go:624-627`）：若 `CalleeAddress` 为空且 `RegionRequestRuntimeStats` 为空，L625 守卫会对**每个**响应提前返回，不仅丢失 `EXPLAIN ANALYZE` 的 `cop_task`/`tikv_task`，还会跳过 `CPUUsage.MergeTikvCPUTime`（慢日志 `Tikv_cpu_time`、`statements_summary` `SumTikvCPU`、processlist TiKV CPU 全部缺失）与 `RecordCopStats`/`RecordOneCopTask`（cop 算子 `actRows` 全部缺失）。维护者已定将"在 `client-rust` 填上 `CopRpcContext.addr`"列为本块第 1 部分的前置任务，补齐后该整段统计正常产出，不存在此差异。
6. **`WithSQLKvExecCounterInterceptor` 为空操作**（`distsql.go:79, 280-288`）：Top SQL（D14.2）不记录（ROADMAP §3），且 client-rust 无 RPC interceptor（同 `crates/store/src/driver/txn/snapshot.rs:227`）。
7. **`ResourceGroupTagger`（D07.0 T8，随第 1 部分补齐，非永久省略）**（`request_builder.go:361, 414-417`、`copr/coprocessor.go:1350`）：Go 对每个 cop 请求均设置 `kvrpcpb.Context.ResourceGroupTag`（`stmtctx.go:868` 不受 Top SQL 开关控制，`EncodeTagWithKey` 写入 SQL digest、plan digest、table id 与 label，改变发往 TiKV 的请求字节）。第 1 部分实现时一并补齐 `tidb-kv` / `StatementContext` / `copr` 接线，消除此临时缺口。
8. **`RunawayChecker`（N10 未定，遇到配置规则时返回 `blocked("N10", ...)`）**（`request_builder.go:373`）：Go 中 `runaway/checker.go:179-222` 的 `BeforeCopRequest` 会改写 `OverridePriority`、`MaxExecutionDurationMs`、切换 `ResourceGroupName` 并可能返回错误 8253，COOLDOWN 还会把并发度压到 1（`coprocessor.go:229`），配置了 runaway 规则时对用户与发往 TiKV 的请求字节均可见。N10 决定前若检测到生效的 runaway 规则返回 `blocked("N10", ...)`（不静默忽略，宪章第五条）；无规则时不触发。
9. **遥测计数器省略**（`select_result.go:59-63, 375-403, 754-756`）：`pkg/telemetry` 属 X，不移植。
10. **`tikvmetrics.ObserveReadSLI` 与 `EventCb` 省略**（`select_result.go:629-634`、`distsql.go:69-77, 83`）：client-rust 无 ReadSLI 指标；`trxevents` 回调未移植（D07.2 §6.3 第 10 条已在 `handleLockErr` 中直接打 debug 日志）。
11. **failpoint 与测试 hook 不移植**（`distsql.go:63-65, 89-100`、`request_builder.go:77-82, 105-110`、`select_result.go:525-529`）：测试钩子，不移植；`tracing.StartRegionEx`（`distsql.go:59-60`）属 Go runtime trace，不移植。
12. **`VerifyTxnScope` 返回 `Result<bool, Error>` 并在命中 placement bundle 时返回边界桩错误**（`request_builder.go:766-780`）：见 §4.9、§5（L2）。
13. **`backoffSleep` 与 `visitPhysicalTableID` 遍历顺序确定**（`select_result.go:1129`、`request_builder.go:445`）：Go 遍历 map 顺序随机；Rust 用 `BTreeMap` / `BTreeSet` 按键序/表 ID 升序输出（`M-MAPORDER`）。
14. **`indexRangesToKVWithoutSplit` 与 `sortedSelectResults` 内存记账数值（可见，依据 A1）**（`request_builder.go:791, 816`、`select_result.go:181, 234, 274`）：按已确认基础设计 A1 §3.2，`sortedSelectResults` 的缓存 chunk 走 `TrackedAlloc`、`indexRangesToKVWithoutSplit` 记实际 `KeyRange` 字节容量（与 Go 采样估算数值不同，影响 `tidb_mem_quota_query` 触发点与内存观测列，依据已确认的 A1）。
15. **`Drop` 安全网与 `HandleKillSignal` / `Consume` 改走 `Result`**（`select_result.go:718-722, 732-763`、`request_builder.go:791, 816, 822`）：按 N3+A1 统一规则，`Drop` 释放资源但不注册 stats；OOM / KILL 通过 `Result` 返回而非 panic（`M-PANIC`、`M-MEM`、`M-DEFER`）。
16. **`SelectResult::IntoIter` 消耗内部状态而非保留指针**（`select_result.go:516-521, 902-909`）：对外错误文案与 Go 一致（§4.5）。
17. **内存记账在 `selectResultIter` 下正确归零（依据 A1 §5）**（`select_result.go:949`）：关闭后内存记账必归零，不照搬 Go 语句内漏减记账的缺陷（§8 第 1 条）。
18. **`sortedSelectResults.Close` 关闭全部子结果（依据 N3 §3.5）**（`select_result.go:268-278`）：遇错后仍关闭剩余子结果避免 worker 泄漏，返回第一个错误（§8 第 4 条）。

---

## 8. 读代码发现并在 Go v8.5.8 上实测核验的 Go 问题

以下 8 项问题（原读码发现 7 项 + 核验时新发现第 8 项）均已于 2026-11-26 在真实 Go TiDB `v8.5.8`（commit `8b857efa2`，`go1.25.12`）上运行复现并逐项核验可达性与宪章第九条归属：

1. **`selectResultIter.Next` 导致语句执行期间 `memTracker` 漏减全部已消费 `SelectResponse` 大小**（**已实测复现，SQL 可达**）
   - **位置**：`pkg/distsql/select_result.go:949` 与 `:413-418, 439-440, 734-737`。
   - **机制**：每消费完一个 `selectResp` 后，`selectResultIter.Next` 在 L949 执行 `iter.result.selectResp = nil`，但未扣减 `selectRespSize` 也未清零它。下一轮进入 `fetchRespWithIntermediateResults` 时，L413 的 `if r.selectResp != nil` 为假，跳过 `r.memConsume(-selectRespSize)`；随后 L439-440 覆盖 `selectRespSize` 并再次 `Consume(+newSize)`；到 EOF 时 L413 仍为假且 L418 把 `selectRespSize` 置 0；最终 `iter.Close()` 在 L734 读到 0，所有已消费响应的字节数残留在 `memTracker` 中直至语句结束 `DetachMemDiskTracker` 摘除（非跨语句永久泄漏，但语句执行期间记账虚高，可能误触发 `tidb_mem_quota_query`）。
   - **Go v8.5.8 实测结果**：通过导出 API `distsql.Select` + `sr.IntoIter(nil)` 消费 3 批 `SelectResponse`（`size1=10, size2=12, size3=8`，合计 30 字节）：第 1 批后 `BytesConsumed=10`、第 2 批后 `22`、第 3 批后 `30`；读至 EOF 并调用 `iter.Close()` 后仍为 **`BytesConsumed=30`**（普通 `Next` 路径为 0）。SQL 层经 `/*+ INDEX_LOOKUP_PUSHDOWN(t, ia) */` 走 `LocalIndexLookUp` → `IntoIter` 可达。
   - **tidb-rs 处理与归属**：按已确认基础设计 **A1 §3.5 / §5**（宪章第九条 2：每个 tracker 在关闭后必须归零），在切换/丢弃 `selectResp` 时正确归还记账，关闭后恒为 0；第 2 部分开 `upstream-pitfall` 标 `go-bug-fixed`。

2. **`RequestBuilder.SetHandleRanges` 未检查 `builder.err`，触发 nil 指针 panic 或静默吞掉前置错误**（**Go 单测已复现，SQL 不可达**）
   - **位置**：`pkg/distsql/request_builder.go:156-160` 与 `pkg/kv/kv.go:473-479`。
   - **机制与 Go v8.5.8 实测结果**：`SetHandleRanges` 调用 `SetHandleRangesForTables` 后无条件执行 `builder.err = builder.Request.KeyRanges.SetToNonPartitioned()`。(a) 传入非法 `KindInterface` Datum 或 `KindMysqlDecimal` 溢出值时，`SetHandleRangesForTables` 返回错误，`SetHandleRanges` 因 `KeyRanges == nil` 直接 nil 指针 panic；(b) 先置 `builder.err != nil` 且 `KeyRanges` 非空时，`SetHandleRanges` 会将 `builder.err` 覆盖为 `nil`。
   - **可达性与处理**：**SQL 不可达**（常规查询经 `ranger` 校验不会在编码阶段报错，且全部调用方总在新 `RequestBuilder` 上首个调用 `SetHandleRanges`）。属 Go API 层 nil panic（不改变 TiKV/TiFlash 请求），Rust 中 `Option` 与 `if self.err.is_none()` 自然返回错误不 panic（宪章第九条 3）；第 1 部分开 `upstream-pitfall`（`go-bug-fixed`）并注明仅 Go API 可达。

3. **`selectResultRuntimeStats.mergeCopRuntimeStats` 用 `maps.Copy` 覆盖而非累加 `backoffSleep`**（**已实测复现，SQL 可达**）
   - **位置**：`pkg/distsql/select_result.go:982-987`（对比 `Merge` L1038-1040 的 `+=`）。
   - **Go v8.5.8 实测结果**：经导出 API `distsql.SelectWithRuntimeStats` 返回 2 个带 `CopRuntimeStats` 的子结果——task 1 含 `BackoffSleep: {"RegionMiss": 10ms, "tikvRPC": 3ms}`（`respTime=5ms`）、task 2 含 `BackoffSleep: {"RegionMiss": 2ms}`（`respTime=7ms`），最终 `statsColl.GetRootStats(1).String()` 输出：
     `"cop_task: {num: 2, max: 7ms, min: 5ms, avg: 6ms, p95: 7ms, copr_cache_hit_ratio: 0.00}, backoff{RegionMiss: 2ms, tikvRPC: 3ms}"`
     确认 `RegionMiss` 被覆盖为 `2ms` 而非累加后的 `12ms`（`EXPLAIN ANALYZE` 多 region 查询直接可见）。
   - **tidb-rs 处理与归属**：仅影响可观测性（`EXPLAIN ANALYZE`），不造成宪章第一条 2 所列后果；按宪章第一条 2 与第九条 2 **照 Go 移植**（覆盖写），开 `upstream-pitfall` 标 `go-bug-kept`。

4. **`sortedSelectResults.Close` 遇首个错误提前返回，泄漏后续子 `SelectResult` 与缓存内存**（**Go 单测已复现，生产 SQL 不可达**）
   - **位置**：`pkg/distsql/select_result.go:268-278`（对比 `serialSelectResults.Close` L325-332）。
   - **Go v8.5.8 实测结果**：构造 2 个 mock `SelectResult`（`sr0.Close()` 返回 `"close err on sr0"`，`sr1.Close()` 返回 `nil`），调用一次 `Next` 后调用 `Close()`：`sr0.closed=true`、**`sr1.closed=false`**，`memBeforeClose=272`、**`memAfterClose=272`**。另实测未调 `Next` 直接 `Close()` 不 panic。
   - **可达性与处理**：**生产 SQL 不可达**（Go 中 `copIterator.Close` 与 `copErrorResponse.Close` 恒返回 `nil`）。按已确认基础设计 **N3 §3.5 + A1 §5**，Rust 关闭全部子结果并释放全部缓存内存、返回第一个错误；第 2 部分开 `upstream-pitfall`（`go-bug-fixed`）并注明生产不可达。

5. **`RequestBuilder.verifyTxnScope` 在 `TableInfoByID` 未命中时 nil 指针 panic**（**仅 mock 不一致 InfoSchema 复现，真实集群不可达**）
   - **位置**：`pkg/distsql/request_builder.go:455-456`。
   - **Go v8.5.8 实测结果**：mock `MetaOnlyInfoSchema` 返回 bundle 但 `FindTableInfoByPartitionID` / `TableInfoByID` 未命中时 panic。真实 `InfoSchemaV2` 不可达，且 tidb-rs 在 `VerifyTxnScope` 命中 bundle 时已返回 `blocked("X", ...)`，不开 `upstream-pitfall`。

6. **`PartitionHandlesToKVRanges` 对 `CommonHandle.Encoded()` 原地 `append(..., 0)` 篡改底层数组**（**Go 单测已复现，SQL 不可达**）
   - **位置**：`pkg/distsql/request_builder.go:675`（对比 `TableHandlesToKVRanges` L634 使用 `kv.Key(commonHandle.Encoded()).Next()`）。
   - **Go v8.5.8 实测结果**：对编码长 19 字节、`cap=23` 的 `CommonHandle` 切片末尾写哨兵 `0xAA, 0xBB, 0xCC, 0xDD`，调用 `PartitionHandlesToKVRanges` 后被原地改写为 `0x00, 0xBB, 0xCC, 0xDD`（`TableHandlesToKVRanges` 不覆写）。
   - **可达性与处理**：**SQL 不可达**（`pkg/executor/builder.go:4716-4724` 中 common handle 不走 `PartitionHandlesToKVRanges`）。Rust 使用 `Key::next()` 分配独立 `Vec<u8>`，输出字节与 Go 100% 一致，不开 `upstream-pitfall`。

7. **`indexRangesToKVWithoutSplit` 切片头估算未乘 `len(tids)`**（**仅未导出函数内部复现，v8.5.8 全部导出 API 与 SQL 不可达**）
   - **位置**：`pkg/distsql/request_builder.go:791`。
   - **可达性与处理**：全库多表调用仅来自 `lightning`（且传 `memTracker=nil`），`len(tids) > 1 && memTracker != nil` 在 v8.5.8 任何导出 API 与 SQL 中均不可达；且在 A1 下不复存在，不开 `upstream-pitfall`。

8. **（核验新发现）`RequestBuilder.Build` 在 `KeyRanges == nil` 且设了非 global `TxnScope` 时提前调 `verifyTxnScope` 导致 nil 指针 panic**（**Go 单测已复现，生产 SQL 不可达**）
   - **位置**：`pkg/distsql/request_builder.go:83-89` 与 `pkg/tablecodec/tablecodec.go:2023`。
   - **Go v8.5.8 实测结果**：`(&RequestBuilder{}).SetTxnScope("dc-west").SetFromInfoSchema(is).Build()` 直接 nil 指针 panic（L83 早于 L87-89 的空 `KeyRanges` 初始化且未判 `builder.err`）。生产 SQL 调用方均在 `Build` 前设置了非空 `KeyRanges`（仅 Go API 可达）；Rust 中将空 `KeyRanges` 初始化放在 `verify_txn_scope` 之前并在 `self.err.is_none()` 时才校验，避免 panic。

**`RequestBuilder` 复用与预处理语句重执行审计**：全库核验 `pkg/executor/` 全部调用方（`table_reader.go`、`distsql.go`、`index_merge_reader.go`、`builder.go`），均在每次 `buildKVReq*` 局部声明新的 `var builder distsql.RequestBuilder`，不跨请求或跨 `PREPARE`/`EXECUTE` 复用；`StatementContext.distSQLCtxCache` 随每条语句 `ResetContextOfStmt` 重建。Go v8.5.8 实测 `Build()` 二次调用：带 `-tags intest` 返回 `err="request builder is already used"`，不带 `-tags intest` 返回 `(&builder.Request, nil)`（对应 Rust `#[cfg(test)]` 检查）。

---

## 9. 决定点（按宪章第九条）

### 9.1 需要负责人决定

无。

说明：本块前置补齐 `client-rust` 的 `CopRpcContext.addr`（避免 `updateCopRuntimeStats` 整段丢失统计）、随第 1 部分补齐 `ResourceGroupTagger`（D07.0 T8）、将 `select` 改为 async 即时建 `CopIterator`（保持首次 `Next` 前并发度与 Go 一致），对 N10 runaway 规则存在时显式返回 `blocked("N10", ...)`，因此不引入任何新的长期用户可见或 TiKV 线协议行为差异；`DEF.MPP`、`Analyze`（委托）、`Checksum` / BR / Lightning 专用函数（N0 §3 / X）、client-rust `rpc_info` 缺口（D07.2 §6.3）与 N3+A1 均为已确认规则。

### 9.2 维护者已定（宪章第九条 2）

1. **前置任务：`dulao5/client-rust` 填上 `CopRpcContext.addr`**：本块第 1 部分实现前先在 `client-rust` 补齐被调用 TiKV store 的地址，确保 `updateCopRuntimeStats` 正常运行，不丢失 `EXPLAIN ANALYZE` `cop_task`/`tikv_task`、`actRows`、慢日志 `Tikv_cpu_time` 与 `statements_summary` `SumTikvCPU`。
2. **随第 1 部分补齐 `ResourceGroupTagger`（D07.0 T8）**：补齐 `tidb-kv` 的 `ResourceGroupTagger`、`StatementContext::get_resource_group_tagger`、`RequestBuilder::set_resource_group_tagger` 与 `copr/coprocessor.rs:1789` 接线，使发往 TiKV 的 `kvrpcpb.Context.ResourceGroupTag` 字节与 Go 一致。
3. **`select` / `select_with_runtime_stats` 改为 `async fn` 并即时构造 `CopIterator`**：在 `kv::Client` 上新增 `send_async` 供 `CopClient` 立即构建并打开 `CopIterator`，使 `GetSelectResultConcurrency` 在首次 `Next` 之前即返回与 Go 一致的 `(concurrency, small_task_concurrency)`，不改变 `IndexLookUp` 限流调度。
4. **分两部分交付与桩归属**：第 1 部分 73 个 D07.3 函数（+ P1-1 的 3 个 D00.1 函数与 D00.3 的 `ppcpuusage` 7 个函数）覆盖 `TableReader` / sysbench 闭环，解除 `get_dist_sql_ctx` 与 `verify_txn_scope` 桩（`new_closest_read_adjuster` 桩由 #411 第 2A 部分解除）；第 2 部分 31 个 D07.3 函数覆盖只读 `IndexReader` / `IndexLookUp` / `IndexMerge`、多分区归并排序，并负责 `Request.copr_request_rate_limit` 字段与 `CopIterator` 限速接线；另 14 个归豁免/不移植（§3.3）。同步在 tracker 修正 `VS1-33` 与本划分的对应关系。
5. **`Variables`、`RateLimit`、`StoreLabel` 下沉到 `tidb-kv`（rank 5）**：按 N0 §4 规则 1 消除反向引用，`tidb-store::copr::tikv` 原名重导出；`ppcpuusage`（86 行）整文件移入 `tidb-execdetails`（rank 4）。
6. **`ResultSubset` / `Response` 统计访问走 trait 默认方法（`M-IFACE`）**：`tidb-kv`（rank 5）依赖 `tidb-execdetails`（rank 4），移入 `CopRuntimeStats`；响应外层在 `close()` 丢弃 `CopIterator` 前缓存最终 `build_task_elapsed` / `store_batch_info`。
7. **`MetaOnlyInfoSchema` 在 `tidb-infoschema::context`（rank 9）扩展同步 `placement_bundle_by_physical_table_id`**：与同步 `VerifyTxnScope` 匹配，不依赖异步 `table_info_by_id`；命中 bundle 返回 `blocked("X", ...)`（`VerifyTxnScope` 改为 `Result<bool, Error>`，标 L2）。
8. **`SelectResult::into_iter` 用内部 `Option<SelectResultInner>` 转移所有权**：保留 `"selectResult is invalid after IntoIter()"` 语义且出错不破坏自身状态；`Select` 的 `kv.TiFlash` 分支防御性返回 `blocked("DEF.MPP", ...)`；存在 runaway 规则时返回 `blocked("N10", ...)`。
9. **PoC `tidb-distsql` 隔离保留**：正式代码落 `crates/store/src/distsql/`，默认 `--new-stack=off` 路径零改动；`chunk::codec::new_decoder` 放宽为借用 `FieldType` 或按次拷贝。
10. **govec 新领域 `distsql` 与 §8 登记**：双架构向量在 tracker CI 核对（`request_builder` op 覆盖 `DAGRequest` 字段 3/4/8/11 零值）；32 个 Go 单测全量移植（`Analyze`/`Checksum` 除外）；合入 #411 联调前在真实 TiKV `v8.5.8` 上以 `TypeChunk` / `TypeDefault` 跑 `tools/ab`；§8 已核验问题按各条结论登记 `upstream-pitfall`（仅第 1、3 条 SQL 可达，第 2、4、8 条仅 Go API / mock 可达，第 5、6、7 条不开 issue）。

---

## 本文所用 ID

- A1：统一内存记账与配额设计（`TrackedAlloc`、tracker 树、动作链），并入 N3 spec
- ADR 0008：以 `client-rust` 替代 `client-go`（L3 同源替换）
- D00.1：上下文接口骨架（`sessionctx`、`stmtctx`、`distsql/context`、`ranger` 类型等）
- D00.2：基础常量、错误码与配置（`tidb-base`）
- D00.3：`util/*` 叶子工具文件（按需整文件移植）
- D00.4：`contextutil`（warning 收集器等）
- D01.1–D01.6, D01.9：数据类型与排序规则（`tidb-types`）
- D01.7：Chunk 与列存解码器（`tidb-chunk`）
- D01.8：Datum 编解码（`util/codec`，`tidb-codec`）
- D01.11：表/索引键与行编解码（`tablecodec` / `rowcodec`，`tidb-tablecodec`）
- D05.1 / D05.2：系统变量定义与 `SessionVars` / `StatementContext`（`tidb-sessionctx`）
- D05.3：会话（`tidb-session`）
- D06.1：元数据模型（`tidb-model`）
- D06.3：InfoSchema 构建与缓存（`tidb-infoschema`）
- D07.0：KV 抽象与键/区间类型（`tidb-kv`）
- D07.1：TiKV 存储驱动（`tidb-store::driver`）
- D07.2：coprocessor 客户端（`tidb-store::copr`）
- D07.3：本块，distsql 请求构建与结果读取（`tidb-store::distsql`）
- D07.4：coprocessor cache（例外 A3）
- D08.1 / D08.11：表达式框架与 `ByItems`（`tidb-expression`）
- D10.9：物理计划后处理与 `ToPB`
- D12.0：执行器框架与 `ConstructDAGReq`（`tidb-executor`）
- D12.1：读取类执行器（`PointGet`、`TableReader`、`IndexReader`、`IndexLookUp` 等）
- D12.2：基础算子执行器（`Selection`、`Limit`、`Projection`、`TableDual`）
- D12.3：聚合执行器框架（`StreamAgg`、`HashAgg`）
- D12.7：IndexJoin 系列执行器
- D12.11：IndexMerge 执行器
- D12.12：简单执行器
- D12.14：管理/校验类执行器（`ADMIN`、`CHECKSUM` 等，不在只读范围）
- D14.0：执行统计数据结构（`util/execdetails`，`tidb-execdetails`）
- D14.1：Prometheus 指标（`tidb-metrics`）
- D14.2：Top SQL / 慢日志 / 语句摘要（Top SQL 不记录）
- D15.1：内存统计与配额实现（`tidb-memory`）
- DEF.MPP：TiFlash / MPP 推迟范围
- L1 / L2 / L3：来源标注等级（L1 忠实移植；L2 有理由的 Rust 化改写；L3 同源替换）
- M-CTX、M-DEFER、M-ERR、M-FT、M-GO、M-GOCTX、M-IFACE、M-KILL、M-MAPORDER、M-MEM、M-PANIC、M-ROW、M-SHARE：`docs/porting/mappings.md` 中的全局映射规则
- N0：crate 分层与依赖方向（`layers.toml`）
- N1：计划表示与共享修改审计（§5；其中 V0 表示审计前零共享修改基线）
- N2：会话与语句上下文分层设计
- N3：并发、取消、错误边界与统一内存模型（含 A1、A2）
- N9 / N10：观测/指标与资源管控（Runaway）相关例外
- N13：长期共享元数据与 `Arc<FieldType>` 写时复制
- P1-1：P1 收尾移交到本块的 D00.1 函数条目（`tasks.md`）
- PoC：原型实现（`[legacy]` crate，`--new-stack=off` 默认路径）
- T8：D07.0 规格中记录的 `ResourceGroupTagger` 临时缺口编号
- VS1-33 / VS1-34：垂直切片 1 中对应 distsql / TableReader 的工作项
- X：不在 tidb-rs 只读范围内的 Go 代码（DDL、写路径、ANALYZE、遥测、placement 规则等）
