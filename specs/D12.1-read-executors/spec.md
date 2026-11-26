# D12.1 读取类执行器（spec）

状态：第 1 部分（点查执行器，VS1-12）评审中（2026-11-15）。块：D12.1（ROADMAP P4），crate `tidb-executor`（N0 rank 14）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（D12.1、D06.4 两行）、§3.2（边界桩）；[D12.0 tasks](../D12.0-executor/tasks.md)（执行器框架，VS1-13）；[D06.4 spec](../D06.4-table/spec.md)（table 读取部分）；[D07.1 tasks](../D07.1-store/tasks.md)（snapshot，VS1-18）；[N13](../exceptions/N13-shared-metadata/spec.md)（共享元数据）。

## 1. 范围

Go 范围：`pkg/executor` 中读取类执行器（point get、batch point get、table / index reader、index lookup、union scan、mem reader 等），tracker 中 245 个函数。

### 1.1 第 1 部分（VS1-12：点查，不含加锁）

函数清单是 `specs/VS1-vertical-slice/functions.tsv` 中 VS1-12 / D12.1 的 33 个函数（`fillRowChecksum`、`shouldFillRowChecksum` 按 #234 评审补登记）。

| Go | Rust（`crates/executor/src/`） | 内容 |
|---|---|---|
| `point_get.go` | `point_get.rs` | `buildPointGet`、`PointGetExecutor`（`Init`、`Open`、`Next`、`Close`、`get`、`buildVirtualColumnInfo`）、`GetPhysID`、`DecodeRowValToChunk`、`decodeOldRowValToChunk`、`tryDecodeFromHandle`、`notPKPrefixCol`、`getColInfoByID`、`runtimeStatsWithSnapshot`；`shouldFillRowChecksum`、`fillRowChecksum`；清单外顺带移植 `matchPartitionNames`、`getAndLock`、`lockKeyIfNeeded`、`lockKeyIfExists`、`lockKeyBase`（加锁分支是桩）、`verifyTxnScope` |
| `builder.go` | `builder.rs` | `NewRowDecoder`、`getSnapshot`、`getSnapshotTS`、`validCanRead*`、`buildIndexUsageReporter`、`newReplicaReadAdjuster`、`isCommonHandleRead`、`getPhysicalTableID`、`buildHandleColsForExec` |
| `table_reader.go` | `table_reader.rs` | 只有点查用到的 `buildVirtualColumnIndex`（其余随 VS1-34） |

converge：`scripts/converge.py --block D12.1 --part specs/VS1-vertical-slice/functions.tsv --item VS1-12 crates/executor/src/{point_get,builder,table_reader}.rs` 为 31/33 移植 + 2 个边界桩（`getCacheTable` → D06.4 第 2 部分；`newClosestReadAdjuster` → D07.3，只有 table reader 用）。

### 1.2 块内其余（`blocked-by:D12.1`）

BatchPointGet、TableReader（VS1-34）、IndexReader、IndexLookUp、IndexMerge、UnionScan、mem reader、TABLESAMPLE 等，见 [tasks.md](tasks.md)。

## 2. 承载方式

1. **会话（N2）。** Go 的执行器保存 `e.Ctx()`，在 `Next` 中读会话。Rust 的执行器不持有会话：`Next` 要读的会话状态（表达式上下文、时区、`IsPessimisticReadConsistency`、`tidb_enable_point_get_cache`）在 `Init` 时取一份；语句状态读 `ExecCtx`。`Init` 因此多一个会话参数，`glue::PointGetExecutor::init(sctx, plan)`（`ExecStmt.PointGet` 复用缓存执行器时调用，#222）随之改了签名。
2. **`SessionContext` 新增两项**（VS1-14 由会话实现）：`get_expr_ctx`（`GetExprCtx`）、`get_snapshot_with_stmt_read_ts`（`GetSnapshotWithStmtReadTS`，异步，provider 可能要等 TSO）。
3. **`executorBuilder.build` 改为 async**：建读取执行器要取语句的快照，取快照可能等 TSO；Go 在这里阻塞 goroutine，Rust 在会话的 task 上 await（N3）。`ExecStmt.buildExecutor` 随之改为 async。以后带子执行器的 build 递归时要 `Box::pin`。
4. **计划**：planner 的 `PointGetPlan`（D10.8，VS1-11；[D10.8 spec](../D10.8-point-get/spec.md) §2 第 5 条）。VS1-12 时用的承载类型 `glue::PointGetPlan` 已删除，builder 改一处 import，计划 ID 从 `PhysicalPlan::id()` 读。有分区的表在计划的副本上裁剪（Go 改写缓存计划的 `PartitionIdx`；Rust 的计划是共享的 `Arc`）；按值裁剪是 `blocked-by:D06.4`，`GetAvgRowSize` 是 `blocked-by:D10.5`。
5. **共享（N13）。** 解码器的 `ColInfo.ft` 是 schema 列 `RetType` 的 `Arc`（Go `Ft: col.RetType`），每次 `Init` 建一次，每行解码只借用；虚拟列的 `virtualColumnRetFieldTypes` 同样共享 schema 的 `Arc`；`fillRowChecksum` 解码时借用表列的 `&FieldType`（Go `&col.FieldType`）。为此 `table::column::fill_virtual_column_value`（D06.4）的类型参数改为 `F: Borrow<FieldType>`，与 `chunk::new_chunk_with_capacity` 相同。单测检查执行器和 schema 是同一个 `Arc`。
6. **快照的运行时统计**：client-go 的 `SnapshotRuntimeStats` 在 client-rust 中没有（D07.1 差异 2），`glue::SnapshotRuntimeStats` 是一个总为空的替身，方法按 Go 对空值的结果返回。`Close` 注册统计时注册副本（Go 注册指针），内容为空，所以没有差别。
7. **KV 的 `context.Context`**：用 `Context::background()`（M-GOCTX）；Go 的 ctx 中的 request source 已经由 provider 设置在快照上。

## 3. 不移植或停在边界桩的分支

| 位置 | 情况 | 处理 |
|---|---|---|
| `buildPointGet` | `p.Lock`（`SELECT ... FOR UPDATE`），以及 for-update 的快照 / TS | `blocked-by:X`（写路径；VS1 点查不含加锁）。在建执行器之前报错，路由器可以回退 |
| `lockKeyBase` | `e.lock` 为真 | `blocked-by:X`（不可达，build 已拒绝） |
| `get` | 事务的 mem buffer / 悲观锁缓存 | tidb-rs 的事务从不写，`txn.Valid() && !txn.IsReadOnly()` 恒为假，直接读快照 |
| `get` | `LOCK TABLES ... READ` 且 `tidb_enable_point_get_cache` | `cacheDB.UnionGet` 是 X 块：`blocked-by:X` |
| `buildPointGet` | 分区被裁剪为空（`TableDualExec`，D12.2） | 返回 0 行的 `TableDualExec`（VS1-35 之后，[D12.2 spec](../D12.2-basic-executors/spec.md) §3）；目前到不了（分区表在 `PrunePartitions` 已停） |
| `buildPointGet` | 缓存表（`getCacheTable`） | `blocked-by:D06.4`（第 2 部分） |
| `buildPointGet` | `tidb_replica_read=closest-adaptive`（`GetAvgRowSize`） | `blocked-by:D10.8` |
| `verifyTxnScope` | 非 global 的 txn scope（`distsql.VerifyTxnScope`） | `blocked-by:D07.3` |
| `Open` | `setOptionForTopSQL`（D12.2） | 不设置：resource group tagger 属于 Top SQL（D14.2），ROADMAP §3 决定不记录；快照本来也忽略这个选项 |
| `buildIndexUsageReporter` | 语句的 index usage collector（D11.2） | 没有语句带 collector，所以没有 reporter（Go 的 nil），也不为它预加载统计 |
| failpoint | `assertPointReplicaOption`、`pointGetRepeatableReadTest-*` | 不移植（测试钩子） |

## 4. 与 Go 的其他差异

1. `buildPointGet` 在执行器共享语句上下文之前写 `StmtCtx.IndexNames`（Go 在建好之后写，中间没有人读）。
2. `newReplicaReadAdjuster` 在建立时读 `tidb_adaptive_closest_read_threshold`（Go 在调用时读，同一语句内不变）；client-go 的 `StoreSelectorOption` 表示为它要匹配的 label 列表。

3. **行 checksum 的时区**：Go 的 `fillRowChecksum` 用 `SessionVars.TimeZone`，可能为 nil（此时 `DecodeRowWithMapNew` 不转换 TIMESTAMP，而非零值的 `ConvertTimeZone(UTC, nil)` 在 `Time.In` 中 panic）；Rust 传 `Location()`（nil 时为系统时区）。会话执行语句前总会加载 `time_zone`，此后两者相同（#234 评审认可，代码注释中写明）。TIMESTAMP 先解码到会话时区，`appendDatumForChecksum` 再换回 UTC 计算，所以只要两处用同一个时区，checksum 与时区无关；govec 的 `row_checksum` 记录覆盖 UTC 与 +8、-3、+5:30、-9:30 的会话时区（把 Rust 侧改成固定 UTC，有 66 条记录不同）。

## 5. 待定与可能的上游问题

1. **唯一索引点查**（已解决）：`EncodeUniqueIndexKey` / `EncodeUniqueIndexValuesForKey` 按决定并入 VS1-11，与 `ReportLookupInconsistent` 一起移植（D10.8 spec §1.1、§6），唯一索引路径经 govec `fastplan` 的 `uidx` 记录回放（D10.8 spec §7）。
2. **可能的上游问题（不提交）**：`NewRowDecoder` 把 `_tidb_rowid`（`ExtraHandleID`）当作 handle 列；如果表是聚簇（common handle）表，`ChunkDecoder.tryAppendHandleColumn` 会把 common handle 的第一列按它自己的类型解码进 `_tidb_rowid` 的 BIGINT 列，`chunk.Column.AppendMyDecimal` 等通过 unsafe 写入 8 字节的 element buffer，越界写坏堆。SQL 层到不了（聚簇表没有 `_tidb_rowid`，planner 拒绝这个名字），govec 生成器最初构造了这种用例，表现为同一次运行中别的记录的字符串被清零、结果不确定；生成器已排除这种用例（tracker `govec/domains/pointget.go`）。Rust 的同一路径不会越界（安全代码），结果不保证与 Go 相同。
3. tracker 的 `table` 领域 `decode_row`（D06.4）用 `var enc rowcodec.Encoder` 编码行，`Enable` 为 false，所以那批向量全是旧行格式（v1）。`pointget` 领域用 `Enable: true`，覆盖 v2。`table` 领域是否补 v2 由 D06.4 第 2 部分决定。

## 6. 测试

- **govec 领域 `pointget`**（tracker `govec/domains/pointget.go`）：`NewRowDecoder` + `DecodeRowValToChunk`，共 5850 条，其中 `decode_row_val` 4050 条（下面是最初 3600 条的分布）：行格式 v1 720 条、v2 2880 条（其中 large 452、带 checksum 726、含 NULL 列 728），common handle 536 条（含前缀列、需要 restored data 的列），`_tidb_rowid` 883 条，虚拟列 272 条；每行只存部分列，其余按 origin default 读（"新增列"），错误 649 条（默认值转换失败等）。`crates/executor/tests/govec_pointget.rs` 回放每条记录两次（第二次结果必须相同：每次解码只借用 field type），再把没有错误、没有虚拟列的记录交给完整的点查执行器（`glue::PointGetPlan` + 内存快照），行必须相同，且只读了那一个 record key。
- **行 checksum**（`row_checksum`，1800 条）：`fillRowChecksum` 写进新 chunk 的 `_tidb_row_checksum` 列，与 Go 比较每列（未写 / NULL / checksum 字符串）和错误。schema 中 checksum 列位置随机；int handle、`_tidb_rowid` 表与聚簇表；行只含部分列（新增列取 origin default，默认值转换失败的报错 359 条）；行格式 v1 的 checksum 为 NULL（356 条）；约一半的表带一个 TIMESTAMP 列；会话时区见 §4 第 3 条。另有 450 条 `decode_row_val` 的 schema 含 checksum 列：v2 时解码器给 NULL，v1 时 Go 的 `getColInfoByID(-4)` 为 nil 而 panic，Rust 同样 panic。Go 侧经 `go:linkname` 调用未导出的 `fillRowChecksum`。
- **record key**：`pointget` 不重复 handle 编码的向量。`tablecodec` 领域的 `keys` 记录（`EncodeRowKeyWithHandle`，int / common / partition handle，`rowh`）经点查执行器回放，执行器读的 key 必须等于 Go 的 `rowh`（评审要求：handle 编码与 VS1-18 的 snapshot `Get` 共用一套向量；VS1-18 本身没有 govec 领域，它读的 key 就是这一份）。
- 唯一索引路径（索引 key、`decode_handle_in_index_value`、restored data、全局索引分区 ID、不一致错误）：govec `fastplan` 的 `uidx` 记录经执行器回放（`crates/executor/tests/govec_fastplan_uidx.rs`，VS1-11，D10.8 spec §7），单测 `unique_index_point_get`。
- **单测**（`point_get/tests.rs`）：int handle 读行（handle 列、存储列、新增列的默认值）、快照选项、读到的 key；行不存在；`Init` 复用后读新 handle、虚拟列求值；`Init` 共享 schema 的 `Arc<FieldType>`；`FOR UPDATE` / 分区表 / 缓存表 / 本地临时表 + `tidb_snapshot` 各自停在对应的块或 Go 的错误；`GetPhysID`、`matchPartitionNames`、`runtimeStatsWithSnapshot`。
- CI：`govec_pointget` 也在 `TZ=America/Sao_Paulo` 下运行，arm64 job 回放 arm64 向量。
- ab `Point_Get`：要等 VS1-11（`TryFastPlan` / `PointGetPlan`）和 VS1-14（会话）接上才能跑，属于 VS1-20 的集成验收。

---

## 7. 第 2 部分：TableReader / IndexReader / IndexLookUp 范围与 Go 职责

状态：第 2 部分 spec 草案（#408，2026-11-26），只写 spec，不改动上方第 1 部分（§1–§6）已确认的内容。基线：Go TiDB `v8.5.8`（`8b857efa2`），下文 Go 路径相对于 `pkg/executor/`，另有说明的除外。crate：`tidb-executor`（N0 rank 14），本部分新增对 `tidb-store`（N0 rank 11，含 D07.2 / D07.3）的依赖，符合 N0 层次约束。

相关：[VS1 plan](../VS1-vertical-slice/plan.md) §1.1、§3.1（VS1-33、VS1-34）、[D07.2 spec](../D07.2-copr/spec.md)（coprocessor 客户端，已合入）、[D07.3 tasks](../D07.3-distsql/tasks.md)（#407，distsql 请求构建与结果）、[D12.2 spec](../D12.2-basic-executors/spec.md) §2 与 §7（并行算子固定写法与 #338 回归教训）、[D12.3 spec](../D12.3-agg-executors/spec.md)（并发执行器、A1 记账与第九条决定写法）、[N3+A1](../exceptions/N3-A1-concurrency-memory/spec.md)、[N2](../exceptions/N2-session-context/spec.md)、[N13](../exceptions/N13-shared-metadata/spec.md)、[mappings.md](../../docs/porting/mappings.md)。

### 7.1 D12.1 整体文件分布与第 2 部分重点

tracker `data/func_blocks.tsv` 中 `D12.1` 共 **245** 个函数，分布在 9 个 Go 文件：

| Go 文件 | 函数数 | 内容与归属 |
|---|--:|---|
| `point_get.go` | 26 | 单点查询；第 1 部分已完成 25 个，余 1 个加锁辅助（`getValueFromLockCtx`）归写路径 `X` |
| `table_reader.go` | 23 | 表扫描执行器 `TableReaderExecutor`、结果衔接 `tableResultHandler`、虚拟列辅助；第 1 部分已移植 1 个（`buildVirtualColumnIndex`），**第 2A 部分**移植余下 TiKV 路径 21 个，余 1 个 TiFlash BatchCop 专属函数归 `DEF.MPP` |
| `distsql.go` | 68 | `IndexReaderExecutor`、`IndexLookUpExecutor`（含 `indexWorker` / `tableWorker` / `lookupTableTask` / `IndexLookUpRunTimeStats`）及公共工具 `closeAll` / `getPhysicalPlanIDs`；**第 2A 部分**取 2 个公共工具，**第 2B 部分**取 IndexReader 12 个，**第 2C 部分**取 IndexLookUp 54 个 |
| `builder.go` | 41 | 读取类执行器构建与辅助；第 1 部分已移植 12 个，D12.2 顺带移植 1 个（`markChildrenUsedCols`），**第 2A/2B/2C 部分**共 13 个，余下 15 个分别归第 3 部分（BatchPointGet、分区/IndexJoin 辅助、UnionScan、Join 辅助）与 `X` |
| `batch_point_get.go` | 12 | 批量点查（第 3A 部分 5 个）、缓存表快照包装（第 3C 部分 2 个）、悲观锁/表锁/测试 mock（`X` 5 个） |
| `union_scan.go` | 8 | 合并脏写与缓存表读（第 3C 部分） |
| `mem_reader.go` | 37 | UnionScan 内存/缓存表读取器（第 3C 部分） |
| `sample.go` | 17 | `TABLESAMPLE` 区域采样（第 3D 部分） |
| `coprocessor.go` | 13 | 集群内存表 gRPC coprocessor 服务端 `CoprocessorDAGHandler`（第 3E 部分） |

按 #408 要求，第 2 部分聚焦 **`table_reader.go`、`distsql.go`（IndexReader、IndexLookUp）和 `builder.go` 中对应的构建**，并按依赖拆成三个可独立交付的子部分：

1. **第 2A 部分：TableReader（对应 VS1-34）**——支撑 sysbench `oltp_read_only` 所需的全表扫描与主键范围扫描（`TableFullScan` / `TableRangeScan`），只依赖 D07.3（表路径）和已在 `main` 上的 VS1 规划器与基础算子，**不等待索引规划器算子**；
2. **第 2B 部分：IndexReader**——覆盖索引单读，依赖 D10.1 / D10.2 / D10.9 的 `PhysicalIndexReader` / `PhysicalIndexScan` 及 D07.3 索引区间编码；
3. **第 2C 部分：IndexLookUp**——二级索引回表双读（并发 `indexWorker` + `tableWorker` 流水线），依赖第 2A、2B 部分、D10.1 / D10.2 / D10.9 的 `PhysicalIndexLookUpReader`、D07.3 handle 转区间与迭代接口、以及 D12.7 `dataReaderBuilder.buildTableReaderFromHandles`。

### 7.2 三个执行器在 Go 中的执行流程（逐条核对源码）

**TableReaderExecutor**（`table_reader.go`、`builder.go:3670-3919`）：

1. **构建**（`buildTableReader` :3829 → `buildNoRangeTableReader` :3670）：
   - 空 schema 修正（`` `50358` ``，:3841-3843）：读者自身 schema 为空而 `tablePlan` 非空时沿用 `tablePlan` 的 schema（Rust 已按 A3 只读方案在 `table_reader.rs:41` 预留 `table_reader_schema`）。
   - 记录 `StmtCtx.IsTiKV` / `IsTiFlash`（:3846-3851）；检查 TiFlash/MPP 路径（:3853-3863）；取唯一 `PhysicalTableScan` 并断言 `ByItems` 全为列（:3864-3869）。
   - `buildNoRangeTableReader` 调用 `builder.ConstructDAGReq`（D12.0，:3675）将下推算子列表序列化进 `tipb.DAGRequest`，按 schema 列数填 `dagReq.OutputOffsets = [0, 1, ..., n-1]`（:3732-3734），校验临时表/缓存表可读性（:3683），取表对象与 `startTS`（:3687-3696），记 `netDataSize = v.GetNetDataSize()`（:3708），检查相关列（`corColInFilter`、`corColInAccess`，:3715-3716），建虚拟列下标与返回类型（`buildVirtualColumnInfo`，:3722）。本地/全局临时表设 `e.dummy = true`（:3736-3738）。
   - `buildTableReader` 回填 `ranges` / `groupedRanges` / `groupByColIdxs`，把表 ID 追加进 `StmtCtx.TableIDs`（:3880-3883）。若开启动态分区裁剪且表为分区表（:3885-3917），调用 `partitionPruning`，裁剪为空时直接返回 0 行 `TableDualExec`（:3906-3908），否则按物理表 ID 排序构造 `kvRangeBuilderFromRangeAndPartition`。
2. **`Open`**（`table_reader.go:229-349`）：
   - 重置或新建 `e.memTracker` 并挂到 `e.stmtMemTracker`（:237-242）。
   - 若 `corColInFilter` 为真，重新构造 `dagPB.Executors`（或 TiFlash 的 `RootExecutor`）（:245-259）；若开执行统计，置 `dagPB.CollectExecutionSummaries = true`（:260-263）；若 `corColInAccess` 为真，重算 `ts.ResolveCorrelatedColumns()` 及 `groupedRanges`（:264-277）。
   - 将 `e.groupedRanges` 或单组 `e.ranges` 统一成 `groupedRanges`，对每组调用 `distsql.SplitRangesAcrossInt64Boundary(ranges, keepOrder, desc, isCommonHandle)` 拆分无符号整数主键跨 `MaxInt64` 边界的两段（`firstPartGroupedRanges`、`secondPartGroupedRanges`，:283-299）：TiKV 按有符号 `int64` 编码 handle，`(MaxInt64, MaxUint64]` 物理上排在 `[0, MaxInt64]` 之前，因此保序时必须拆成两次请求再由 `tableResultHandler` 顺序衔接。
   - `e.dummy` 时只算 `e.kvRanges`（供上层 `UnionScan` 使用），不发 TiKV 请求（:304-331）；否则调用 `buildRespForGroupedRanges`（:334、:343）→ `buildKVReqSeparatelyForGroupedRanges` → `buildKVReq` → `SelectResult`（`distsql.SelectWithRuntimeStats`），多于一个 `SelectResult` 时用 `distsql.NewSortedSelectResults` 做堆归并（:458-464），最后由 `e.resultHandler.open(firstResult, secondResult)`（或单段 `open(nil, firstResult)`）持有。
3. **`Next`**（`table_reader.go:353-377`）：`dummy` 返回空 chunk；否则调用 `e.resultHandler.nextChunk(ctx, req)`（先消费完 `optionalResult`，返回 0 行后切换消费 `result`，:672-684），再调用 `table.FillVirtualColumnValue` 求值虚拟生成列（:371）。
4. **`Close`**（`table_reader.go:380-394`）：若有 `indexUsageReporter` 则上报 handle 扫描计数；关闭 `resultHandler`（`closeAll(optionalResult, result)`，返回第一个错误）；清空 `kvRanges`。

**IndexReaderExecutor**（`distsql.go:184-439`、`builder.go:4042-4151`）：

1. **构建**：`buildNoRangeIndexReader`（:4042）构造 `IndexPlans` 的 DAG，与 TableReader 的区别在于 `dagReq.OutputOffsets` 按 `v.OutputColumns` 中每列的 `col.Index` 填写（:4091-4093），而不是 `0..n-1`；`buildIndexReader`（:4102）记录 `StmtCtx.IndexNames`（`表名:索引名`，:4118），分区表动态裁剪处理全局索引（`partitionIDMap`）与本地索引（`partitions`）（:4120-4150）。
2. **`Open` → `open`**（:302-439）：`corColInAccess` 时调用 `rebuildIndexRanges`（:158）重算索引区间；`buildKVRangesForIndexReader`（:333）调 `buildKeyRanges`（:625：`indexID == -1` 走 `CommonHandleRangesToKVRanges`，否则走 `IndexRangesToKVRangesWithInterruptSignal`）；`open`（:379）挂 `memTracker`、按 `StartKey` 排序 `kvRanges`：不需要归并（`!needMergeSort(e.byItems, len(kvRanges))`）时构造单个 `kv.Request` 调用 `SelectResult`；需要归并时为每个区间单独发请求并交给 `distsql.NewSortedSelectResults`（:419-437）。
3. **`Next` / `Close`**（:275-299）：`Next` 透传 `e.result.Next`（索引单读不包含虚拟列）；`Close` 上报 index usage 并关闭 `e.result`。

**IndexLookUpExecutor**（`distsql.go:441-2307`、`builder.go:4153-4417`）：

1. **构建**：`buildNoRangeIndexLookUpReader`（:4271）构造索引侧 DAG（普通模式 `buildIndexReq` :4213，含 `buildIndexScanOutputOffsets` :4231 按 `[byItems..., handleCols..., extraPhysTblID?]` 排列输出偏移；下推模式 `buildIndexLookUpPushDownDAGReq` :4171 额外设置 `IntermediateOutputChannels`）和表侧 DAG（`buildTableReq` :4153），建 `dataReaderBuilder`，确定 `handleIdx` / `handleCols` / `primaryKeyIndex`（:4341-4350）。`buildIndexLookUpReader`（:4358）写 `Ti.UseTableLookUp`、`StmtCtx.IndexNames`、`StmtCtx.TableIDs`，分区表动态裁剪设 `partitionTableMode` 与 `prunedPartitions`。
2. **`Open` → `open`**（:583-735）：`Open` 处理 `corColInAccess`、挂 `memTracker`、调用 `buildTableKeyRanges`（:655）把区间按分组/分区编成 `groupedKVRanges`；`open`（:697，注意 `IndexLookUpJoin` 会跳过 `Open` 直接调 `open`）初始化 `IndexLookUpRunTimeStats`、重置/挂 `memTracker`、创建 `finished` 通道和容量为 `LookupTableTaskChannelSize`（默认 50）的 `resultCh`、处理索引侧/表侧 `corColIn*`。
3. **首次 `Next` 启动双阶段流水线**（`startWorkers` :737 → `startIndexWorker` :814 + `startTableWorker` :1147）：
   - `workCh` 容量为 1，由 1 个 `indexWorker` 写、由 `indexLookupConcurrency`（`tidb_index_lookup_concurrency`，未设时取 `tidb_executor_concurrency` 默认 5）个 `tableWorker` 竞争消费；`resultCh` 由 `indexWorker` 写、主线程 `Next` 读。
   - **保序关键**：`indexWorker.buildAndDispatchLookupTasks`（:1653-1662）把同一个 `*lookupTableTask` 先发进 `workCh`、再发进 `resultCh`。主线程 `getResultTask`（:1288）从 `resultCh` 按索引扫描产出的批次顺序取 task，再等 `<-task.doneCh`，从而既让多个 `tableWorker` 并发回表，又保证输出批次严格按索引顺序。
   - **`indexWorker` 限流与批大小**：
     - 批大小以首次 `Next` 传入的 `req.RequiredRows()` 为基准（`distsql.go:1263`），经 `calculateBatchSize` / `CalculateBatchSize`（:1118-1144）在 `[initBatchSize, maxBatchSize]` 内计算：非 `indexPaging` 时先按估算行数倍增起步，`indexPaging`（带 `LIMIT`）只跳过起步倍增；后续每拉满一批仍执行翻倍直到 `tidb_index_lookup_size`（:1741、:1797，默认 20000）。该批大小逻辑与 `selectResult.readFromChunk` 的 `0.8 * RequiredRows` 整 chunk 复用阈值（`pkg/distsql/select_result.go:577`）共同影响 `EXPLAIN ANALYZE` 中的 `table_task num`（Go v8.5.8 unistore 实测：900 行、`tidb_index_lookup_size=100` 时，`tidb_max_chunk_size=1024` 为 9，`64` 因整 chunk 复用每批累积至 128 行降为 8）。
     - 不需归并时走 `fetchHandlesRolling`（:1495），按 `maxInFlight = getIndexScanMaxInFlight(DistSQLConcurrency)`（`2 * c`）与 `getSelectResultInFlightCost`（调 `distsql.GetSelectResultConcurrency`）滚动创建区间 `SelectResult`，避免多分区/多分组一次性并发爆棚；需归并（`keepOrder && needMergeSort`）时用共享限速器 `getMergeSortSharedCoprRequestRateLimit`（容量 `2 * c`）和分摊并发 `getMergeSortIndexScanConcurrency`（Go 写为 `min(max(4*c/n, 2), c)`，Rust 必须写 `x.max(2).min(c)`，不可写 `.clamp(2, c)` 否则 `c = 1` 时 panic）建全部 `SelectResult` 并交给 `NewSortedSelectResults` 再 `fetchHandles`（:1454）。
     - 支持 `PushedLimit`（跳过前 `Offset` 个键、超过 `Offset+Count` 停止）与 `indexLookUpPushDown`（经 `SelectResult.IntoIter` 分流：TiKV 本地命中的完整行 `channelIdxRow` 直接包成已完成 task 进 `resultCh`，跨 region 未命中的 handle `channelIdxIndex` 发给 `tableWorker` 回表）。
   - **`tableWorker`**（`pickAndExecTask` :1870 → `executeTask` :2160）：从 `workCh` 取 task → `buildTableReader`（:1172，经 `dataReaderBuilder.buildTableReaderFromHandles` 把 handle 排序并合并连续整数 handle 为 KV range，立即发 DAG）→ 循环 `exec.Next` 收集行 → `keepOrder` 时按 `task.indexOrder`（`HandleMap`）填 `rowIdx` 并 `sort.Sort(task)` 恢复索引序 → 非 `weakConsistency` 且 `len(tblPlans) == 1` 且未取消时校验 `len(task.rows) == handleCnt`，缺失行用 `GetLackHandles`（:2275）查出并经 `consistency.Reporter.ReportLookupInconsistent` 报 8133 错误 → 把结果错误写入容量 1 的 `task.doneCh`。
4. **`Close`**（:1204-1254）：以 `defer` 最后注册 `IndexLookUpRunTimeStats`（及指标）；函数体先上报 index usage；若已启动 worker，调用 `cancelFunc()` → `close(e.finished)` → `channel.Clear(e.resultCh)` → `idxWorkerWg.Wait()` → `tblWorkerWg.Wait()`；最后执行 `defer` 中的 stats 注册。

### 7.3 调用方（tracker `data/block_edges.tsv` 与源码核对）

| 调用方 | 引用数 | 用到 D12.1 的什么 |
|---|--:|---|
| D12.0 执行器框架 | 14 | `executorBuilder.build` 分派各读取计划（`builder.go`）；`detach.go` 的 `TableReaderExecutor` / `IndexReaderExecutor` / `IndexLookUpExecutor` `Detach`；`ExecStmt.PointGet` 复用 `PointGetExecutor` |
| D12.7 index join 系列 | 34 | `dataReaderBuilder.buildExecutorForIndexJoinInternal` 调用 `buildNoRangeTableReader`、`buildNoRangeIndexReader`、`buildNoRangeIndexLookUpReader`、`buildUnionScanFromReader`、`buildIndexRangeForEachPartition`、`getPartitionKeyColOffsets`、`dedupHandles`、`partitionPruning`、`getPartitionIDsAfterPruning`、`newClosestReadAdjuster`、`IndexLookUpExecutor.open`、`IndexReaderExecutor.open`、`TableReaderExecutor.buildVirtualColumnInfo` |
| D12.11 IndexMerge | 28 | `index_merge_reader.go` 直接构造并运行 `TableReaderExecutor`（复用 `newTableReaderExecutorContext`、`buildTableReq`、`buildIndexReq`、`buildKeyRanges`、`closeAll`、`getPhysicalPlanIDs`、`newClosestReadAdjuster`、`buildVirtualColumnInfo`） |
| D12.14 运维与分析 | 25 | `check_table_index.go`（`ADMIN CHECK TABLE/INDEX`）复用 `IndexLookUpExecutor`（设 `checkIndexValue` 走 `compareData`）；`checksum.go` / `analyze` 用 `TableReader` / `NewRowDecoder` |
| D12.2 基础算子 | 4 | `select.go` 静态接口断言（`TableReaderExecutor` / `IndexReaderExecutor` / `IndexLookUpExecutor` 实现 `Executor` / `dataSourceExecutor`） |
| D12.6 hash join | 3 | `markChildrenUsedCols`、`collectColumnIndexFromExpr` |
| D12.8 merge join | 1 | `markChildrenUsedCols` |
| D12.13 内存表执行器 | 4 | 读系统表/内存表复用行解码与构建辅助 |
| D03.1 / D03.2 服务与 gRPC | 2 + 4 | `pkg/server/rpc_server.go` 调用 `NewCoprocessorDAGHandler` 处理集群内存表 coprocessor RPC；点查结果集引用 |
| DEF.MPP | 2 | `mpp_gather.go` 复用 `buildVirtualColumnInfo` |

### 7.4 sysbench 范围查询路径（第 2A 部分直接打通的链路）

在 `main` 上，sysbench `oltp_read_only` 的单表范围查询已经走通完整规划器链路，只停在 `executorBuilder.build` 对 `PhysicalKind::TableReader` 的桩（`crates/executor/src/builder.rs:230`，`blocked-by:D12.1`）：

- `simple_ranges`：`SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?` → `TableReader > TableRangeScan`（文本协议与预处理语句，含实例计划缓存命中后 range 重建）；
- `order_ranges`：`SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY c` → `Sort > TableReader > TableRangeScan`（`Sort` 在 D12.5 已移植）；
- 全表扫描与带过滤/投影/LIMIT/TopN 的单表查询：`TableReader > [Selection / TopN / Limit] > TableFullScan / TableRangeScan`，以及根侧套 `Selection` / `Projection` / `Limit` / `Sort` / `TopN`（D12.2、D12.5 已移植）。

第 2A 部分与 D07.3（#407）表路径合入后，上述查询将在新栈上端到端真实执行（`crates/session/src/tests/sbtest_results.rs:455` `ranges_fall_back_before_any_executor_opened` 将由回退断言转为结果与列元数据对照）。

---

## 8. 依赖核对与 D12.1 ↔ D07.3 边界

### 8.1 已移植、可直接使用的依赖（逐项 grep 核对）

| 块 | Rust 位置 | 本部分用到的能力 |
|---|---|---|
| D12.0 执行器框架 | `crates/executor/src/internal/exec/executor.rs`、`indexusage.rs` | `Executor` trait、`BaseExecutorV2`、`new_base_executor_v2`、`build_new_base_executor_v2`（:502）、`exec::next`（带 KILL 检查）、`TryNewCacheChunk`、`RetTypes`、`IndexUsageReporter`（`report_cop_index_usage_for_handle` / `report_cop_index_usage_for_table`） |
| D12.1 第 1 部分 | `crates/executor/src/builder.rs`、`table_reader.rs`、`consistency.rs` | `get_snapshot_ts`、`valid_can_read_temporary_or_cache_table`、`build_index_usage_reporter`、`get_physical_table_id`、`build_virtual_column_index`、`table_reader_schema`（A3 只读 schema）、`Reporter::report_lookup_inconsistent`（2C 用） |
| D12.2 基础算子 | `crates/executor/src/select.rs`、`projection.rs` | `TableDualExec`、并行执行器固定写法（`ExecCtx.pool`、`watch` 通道、`Drop` abort） |
| D10.1 / D10.9 物理计划与 `ToPB` | `crates/planner/src/core/physical_plans.rs`、`plan_to_pb.rs` | `PhysicalTableReader`、`PhysicalTableScan`（含 `is_partition`、`ranges`、`grouped_ranges`、`group_by_col_idxs`、`keep_order`、`desc`、`by_items`、`columns`）；`PhysicalTableScan` / `Selection` / `Projection` / `Limit` / `TopN` / `Sort` 的 `ToPB` 均已移植 |
| D07.0 kv 接口 | `crates/kv/src/kv.rs`、`key.rs` | `kv::Request`（含 `tp`、`start_ts`、`data`、`key_ranges`、`concurrency`、`isolation_level`、`priority`、`mem_tracker`、`keep_order`、`desc`、`not_fill_cache`、`replica_read`、`store_type`、`cacheable`、`schema_var`、`batch_cop`、`task_id`、`tidb_server_id`、`txn_scope`、`read_replica_scope`、`is_staleness`、`closest_replica_read_adjuster`、`paging`、`request_source`、`store_batch_size`、`resource_group_name`、`limit_size`、`store_busy_threshold`、`tikv_client_read_timeout`、`conn_id`、`conn_alias`）、`CoprRequestAdjuster`、`KeyRanges`（`new_non_partitioned_key_ranges`、`new_partitioned_key_ranges`、`first_partition_range`、`set_to_non_partitioned`、`append_self_to`、`sort_by_func`、`partition_num`）、`key_range_slice_mem_usage`、`HandleMap`（2C 用）、`PartitionHandle`（2C 用）、`kv::Client`、`kv::Response`、`kv::ResultSubset` |
| D07.2 copr 客户端 | `crates/store/src/copr/coprocessor.rs` | `CopClient::send`、`CopIterator`（含 `get_concurrency` :1280）、`RateLimit`（`copr::tikv`） |
| D01.7 / D01.8 / D01.11 | `crates/chunk`、`crates/codec`、`crates/tablecodec` | `Chunk`、`Row`、`Iterator4Chunk`、`codec::encode_key`、`tablecodec::truncate_index_values` / `truncate_index_value` / `verify_table_id_for_ranges` |
| D05.1 / D05.2 / D00.1 | `crates/sessionctx/src/` | `SessionVars`（`enable_paging`、`index_lookup_size`、`index_lookup_concurrency()`、`enable_redact_log`、`replica_closest_read_threshold`、`get_div_precision_increment()`）、`StatementContext`（`is_tikv`、`is_tiflash`、`table_ids`、`index_names`、`weak_consistency`、`push_down_flags()`、`use_dynamic_partition_prune()`）、`BuildPBContext`、`RangerContext`、`ExprContextRef` |
| D06.3 / D06.4 | `crates/infoschema/src/` | `InfoSchemaV2::table_by_id`、`table::column::fill_virtual_column_value`、`Table::meta` / `as_physical_table` |
| D14.0 / D15.1 / N3 | `crates/execdetails`、`crates/memory`、`crates/runtime` | `RuntimeStats`、`BasicRuntimeStats`、`RuntimeStatsColl`、`TpIndexLookUpRunTimeStats`（`execdetails.rs:1397`）、`memory::Tracker`、`LabelForIndexWorker`（`tracker.rs:1036`）、`ExecPool`、`WaitGroup`、`channel::clear`、`CheckpointStride` |
| D10.6 基数估算 | `crates/planner/src/cardinality/row_size.rs` | `get_avg_row_size`（为补齐 `PhysicalTableReader.GetNetDataSize` 提供计算） |

### 8.2 未移植的依赖与处理（按子部分分层）

| 块 / 设施 | 缺什么 | 受影响子部分 | 处理方式 |
|---|---|---|---|
| **D07.3 distsql（#407）** | `DistSQLContext`、`RequestBuilder`、`SelectWithRuntimeStats`、`SelectResult`、`SplitRangesAcrossInt64Boundary`、`SetEncodeType`、`TableHandleRangesToKVRanges`、`NewSortedSelectResults` 等（完整清单见 §8.3） | **2A / 2B / 2C**（硬依赖） | 第 2A 部分实现排在 D07.3（#407）表路径合入之后；第 2B / 2C 部分需 D07.3 的索引/handle 编码、归并结果与 `SelectResultIter` |
| **D12.0 `ConstructDAGReq` 与 `to_pb`** | `crates/executor/src/internal/builder/builder_utils.rs:18,58` 目前仍是 `blocked-by:D10.9` 的桩（写于 D10.9 之前），但 `PhysicalTableScan` 等的 `ToPB` 已在 `crates/planner/src/core/plan_to_pb.rs` 完成 | **2A**（硬依赖） | 随第 2A 部分解除桩并按 L1 移植 `ConstructDAGReq`（同时补 `D00.3` `timeutil.Zone`），计入 D12.0 |
| **D07.0 `kv::Request` 缺字段与 D07.2 接线** | `crates/kv/src/kv.rs:580` 尚缺 `match_store_labels: Vec<StoreLabel>`（2A `newClosestReadAdjuster` 与 `RequestBuilder.Build` 写入）和 `copr_request_rate_limit`（2C 归并限速），且 D07.2 `CopIterator`（`coprocessor.rs:1309, 1804`）尚未读取 `copr_request_rate_limit` | **2A**（`match_store_labels`）、**2C**（`copr_request_rate_limit`） | 由 **D07.3（#410）** 负责：D07.3 第 1 部分下沉 `Variables`/`RateLimit` 至 `tidb-kv` 并补 `StoreLabel` + `match_store_labels`；D07.3 第 2 部分补 `copr_request_rate_limit` 字段并完成 `CopIterator` 限速接线（2C 排在 D07.3 第 2 部分之后） |
| **D05.3 `SessionCore` 桩** | `crates/session/src/session.rs:790,797` 的 `get_dist_sql_ctx`、`get_build_pb_ctx` 是临时桩；`SessionContext`（`crates/executor/src/glue.rs`）需新增对应访问方法 | **2A**（硬依赖） | `get_dist_sql_ctx` 由 D07.3 第 1 部分实现；`get_build_pb_ctx` 与 `SessionContext` 访问方法随第 2A 部分接入（计入 D05.3 / glue） |
| **EXC.B `PhysicalTableReader.GetNetDataSize`、`PhysicalIndexReader.GetNetDataSize` 与 `getTblStats`** | `buildNoRangeTableReader`（`builder.go:3708`）与 `buildNoRangeIndexReader`（`:4073`）无条件调用 `v.GetNetDataSize()`；但这 3 个函数（`plan_cost_ver1.go:327, 243, 1284`，共 15 行）在 tracker `func_blocks.tsv` 中归 **`EXC.B`**（cost model v1，不移植）而非 D10.5 | **2A / 2B** | 改动已确认例外须按宪章第九条 1(b) **交 §15.1 由项目负责人决定**（建议从 `EXC.B` 移出并按 L1 移植，或按负责人选定的替代方案处理） |
| **D12.0 `assertByItemsAreColumns` 与 `detach.rs`** | `builder.go:3814`（10 行）；`detach.rs:36,71` 的 `TableReaderExecutor.Detach` 桩 | **2A** | `assertByItemsAreColumns` 随第 2A 部分移植、计入 D12.0；`TableReaderExecutor.Detach` 一并解除桩（游标仍停在 `D12.12`） |
| **D12.10 `corColInDistPlan` / `corColInAccess`** | `builder.go:3349,3382`（共 40 行），检查 `PhysicalSelection` / `Projection` / `TopN` / `TableScan` / `IndexScan` 是否含相关列 | **2A / 2B / 2C** | 依赖 `expression::util::extract_cor_columns` 已在 `tidb-expression` 就绪；随第 2A 部分移植（未移植的 `PhysicalIndexScan` 分支等 2B 补上），计入 D12.10 |
| **D12.14 `normalizeCtxErrWithCause`** | `analyze_utils.go:112`（11 行），被 `tableResultHandler.nextRaw` 调用 | **2A** | 无出边小函数，在 `M-GOCTX` 下检查取消原因；随第 2A 部分移植或按 `M-GOCTX` 映射，计入 D12.14 |
| **D12.0 `UpdateExecutorTableID`** | `pkg/executor/internal/util/partition_table.go:26`（42 行），被 `TableReaderExecutor.buildKVReqSeparately`（:508）调用 | **2A**（仅分区表/TiFlash 路径） | 无出边纯 protobuf 遍历，随第 2A 部分整文件移植到 `crates/executor/src/internal/util/partition_table.rs`、计入 D12.0（与 `buildKVReqSeparately` 一致） |
| **D00.3 `timeutil.Zone`** | `pkg/util/timeutil/time_zone.go:189`，被 `ConstructDAGReq` 编码时区名与偏移调用 | **2A** | 叶子工具函数，随第 2A 部分移植 |
| **D10.1 / D10.2 / D10.9 索引计划** | `PhysicalIndexReader`、`PhysicalIndexScan`、`PhysicalIndexLookUpReader`（含 `ToPB`、`GetNetDataSize`、`GetAvgTableRowSize`、`NeedExtraOutputCol`、`ResolveIndices`、`convertToIndexScan`） | **2B / 2C**（硬依赖） | 第 2B / 2C 部分排在索引物理算子合入之后；在此之前 `executorBuilder.build` 对索引路径仍回退（且 D10.2 `convert_to_index_scan` 在规划阶段已停在 `blocked-by:D10.2`） |
| **D12.7 `dataReaderBuilder.buildTableReaderFromHandles` / `buildTableReaderBase`** | `builder.go:4911-4960`（50 行），被 `IndexLookUpExecutor.buildTableReader`（`distsql.go:1193`）调用 | **2C**（硬依赖） | 随第 2C 部分提前按 L1 移植到 `builder.rs`、计入 D12.7（它们不依赖 IndexJoin 其余部分，只依赖 2A 和 D07.3） |
| **D09.12 分区裁剪 / D06.4 缓存表与 `CompareIndexAndVal` / D06.5 `GetDDLOwner` / D10.1 `ResolveCorrelatedColumns` / D10.2 `GroupRangesByCols` / D00.3 `ReportAdminCheckInconsistent`** | 各自对应非 sysbench 分支 | 2A / 2B / 2C 的非默认分支 | 保留边界桩 `blocked-by:<块>`（详见 §11），在进入不可达或未支持分支时显式报错 |
| **D14.1 执行器计数器** | `ExecutorCounterIndexLookUpExecutor`、`IndexLookUpPushDown*` 等指标（`builder.go:4379`、`distsql.go:1210-1222,1615-1628`） | **2C** | 沿用 D12.3 / D12.5 约定，D14.1 有向量前不记录 |

### 8.3 D12.1 ↔ D07.3（#407）精确接口契约

为保证 #407（D07.3 spec）与 #408（D12.1 第 2 部分 spec）接口无缝对齐，下表逐条列出 `table_reader.go`、`distsql.go`、`builder.go`（D12.1 及 `dataReaderBuilder`）、`builder_utils.go`、`point_get.go` 调用的**全部** `pkg/distsql` 与 `pkg/distsql/context` API：

| 类别 | Go API（`pkg/distsql` / `pkg/distsql/context`） | D12.1 调用位置 | 所需子部分 | 对 Rust 接口的约定 |
|---|---|---|---|---|
| **上下文** | `distsqlctx.DistSQLContext`（字段：`Client`、`RuntimeStatsColl`、`DistSQLConcurrency`、`ReplicaClosestReadThreshold`、`ConnectionID`、`SessionAlias`、`EnableChunkRPC`、`EnablePaging`、`MinPagingSize`、`MaxPagingSize`、`ReplicaReadType`、`WeakConsistency`、`RCCheckTS`、`NotFillCache`、`TaskID`、`Priority`、`StoreBatchSize`、`ResourceGroupName`、`LoadBasedReplicaReadThreshold`、`TiKVClientReadTimeout`、`InRestrictedSQL`、`RequestSourceType`、`ExplicitRequestSourceType`、`Location`、`ErrCtx`、`SQLKiller`、`KVVars`、`SessionMemTracker`、`EnabledRateLimitAction`、`OriginalSQL`、`TryCopLiteWorker`、`ExecDetails`、`WarnHandler`、`CPUUsage`、`Detach()`） | `table_reader.go:125,260,520,527,606,613`；`distsql.go:199,370,374,878,932-933,1084,1088`；`builder.go:4898`；`builder_utils.go:85`；`detach.go:57,73` | **2A**（基础字段 + `Detach`）、2B、2C | 必须是 `Send + Sync`（`Arc<DistSQLContext>`），供 `TableReaderExecutor` 及 `IndexLookUpExecutor` 多个 worker task 共享（N2 / N3） |
| **编码类型** | `distsql.SetEncodeType(dctx, dagReq)` | `builder_utils.go:85`（`ConstructDAGReq`） | **2A** | 设置 `EncodeType_TypeChunk` + `ChunkMemoryLayout`（或 `TypeDefault`） |
| **区间拆分** | `distsql.SplitRangesAcrossInt64Boundary(ranges, keepOrder, desc, isCommonHandle) -> (first, second)` | `table_reader.go:292` | **2A** | 纯函数，无 I/O；不修改输入切片（或返回拥有型 `Vec<Range>`） |
| **区间编码（表）** | `distsql.TableHandleRangesToKVRanges(dctx, tids, isCommonHandle, ranges) -> Result<KeyRanges>` | `builder.go:4864,4881`（`kvRangeBuilderFromRangeAndPartition`）及 `RequestBuilder.SetHandleRanges` 内部 | **2A** | 支持 int handle 与 common handle，以及多分区表 ID |
| **区间编码（索引与 handle）** | `distsql.CommonHandleRangesToKVRanges(dctx, tids, ranges)`、`distsql.IndexRangesToKVRangesWithInterruptSignal(dctx, pid, idxID, ranges, memTracker, interruptSignal)` | `distsql.go:639,645`（`buildKeyRanges`） | **2B / 2C** | 2B 只需 `interruptSignal = None`；支持 `memTracker` 检查点 |
| **handle 转区间** | `distsql.TableHandlesToKVRanges(tid, handles)`、`distsql.PartitionHandlesToKVRanges(handles)`（经 `RequestBuilder.SetTableHandles` / `SetPartitionsAndHandles`） | `builder.go:4813,4818,4952,4954`（2C 回表与 3B IndexJoin） | **2C** | 合并连续 int handle 并返回 row count hints |
| **KV Context 来源** | `tidb_kv::Context`（`crates/kv/src/context.rs`，`M-GOCTX`） | `table_reader.go:334,672`；`distsql.go:292,370,1114,1460` | **2A / 2B / 2C** | `tidb_kv::Context` 只携带可选 `RequestSource`（取消靠丢弃 future，不走 Context）。执行器在调用 `distsql::select*` 与 `SelectResult::{next, next_raw}` 时，由 `dctx` 构造轻量 `tidb_kv::Context`（常规 SQL 用 `Context::background()`，受限 SQL 或显式来源用 `with_request_source(...)`，与第 1 部分 §2 第 7 条及 #410 §4.9 一致） |
| **TxnScope 校验** | `distsql::verify_txn_scope(txn_scope, physical_table_id, is) -> Result<bool, Error>`（L2） | `point_get.go:704`（第 1 部分桩 `verify_txn_scope`）、`RequestBuilder.Build` 内部 | **由 D07.3 第 1 部分（#410）直接解除第 1 部分桩** | 非 global txn scope 时经 `tidb_infoschema::context::MetaOnlyInfoSchema` 同步查询 `placement_bundle_by_physical_table_id`；命中 bundle 时返回 `blocked("X", ...)`（对齐 #410 §4.9 / §5） |
| **请求构建器** | `distsql.RequestBuilder`：公开 `pub request: tidb_kv::kv::Request` 与链式方法 `SetHandleRanges`、`SetPartitionKeyRanges`、`SetKeyRanges`、`SetTableHandles`（2C）、`SetPartitionsAndHandles`（2C）、`SetDAGRequest`（含小 LIMIT 降并发度为 1/分区数）、`SetStartTS`、`SetDesc`、`SetKeepOrder`、`SetTxnScope`、`SetReadReplicaScope`、`SetIsStaleness`、`SetFromSessionVars`、`SetFromInfoSchema`、`SetMemTracker`、`SetStoreType`、`SetPaging`、`SetAllowBatchCop`、`SetClosestReplicaReadAdjuster`、`SetConnIDAndConnAlias`、`SetTiDBServerID`、`SetConcurrency`（2C）、`SetCoprRequestRateLimit`（2C）、`Build()`；以及 2C 直接读写 `builder.request.paging` / `cacheable`、2A 传 `&builder.request` 给 `newClosestReadAdjuster`（注：`SetPartitionIDAndRanges` 唯一调用点为 `buildKVReqForPartitionTableScan`，在 #410 §3.3 与本块 §9.4 均归 `DEF.MPP` 不移植） | `table_reader.go:511-528,577-614`；`distsql.go:361-375,1071-1110`；`builder.go:4916-4929,4949-4959` | **2A**（D07.3 第 1 部分）、**2B**（`SetKeyRanges` 已在 D07.3 第 1 部分）、**2C**（`SetTableHandles`、`SetPartitionsAndHandles` 及 `copr_request_rate_limit` 字段与 `CopIterator` 接线由 D07.3 第 2 部分提供） | 公开 `pub request: tidb_kv::kv::Request`（对齐 #410 §4.4 / §4.9）；`new_closest_read_adjuster` 桩由本块 **2A** 解除（D07.3 只提供 `match_store_labels`）；`SetConcurrency` 必须在 `SetDAGRequest` 之前可设且 `SetDAGRequest` 可覆盖它（`distsql.go:1072-1076`） |
| **执行入口** | `pub async fn distsql::select_with_runtime_stats(ctx: &tidb_kv::Context, dctx: Arc<DistSQLContext>, kv_req: &tidb_kv::kv::Request, field_types: Vec<Arc<FieldType>>, cop_plan_ids: Vec<isize>, root_plan_id: isize) -> Result<Box<dyn SelectResult>, Error>` | `table_reader.go:71`（经 `selectResultHook`）、`distsql.go:1114` | **2A / 2B / 2C**（D07.3 第 1 部分） | **异步函数（维护者已定）**：内部经 `CopClient::send_async` 立即完成 region 定位并构造/打开 `CopIterator`，使 2C `fetchHandlesRolling` 在首次 `Next` 之前调用 `GetSelectResultConcurrency` 时与 Go 100% 一致；接收 `Vec<Arc<FieldType>>`（N13 §8）；返回 `Box<dyn SelectResult>`（`Send`） |
| **结果接口** | `distsql.SelectResult` trait：`async fn next(&mut self, ctx: &tidb_kv::Context, chk: &mut Chunk) -> Result<()>`、`async fn next_raw(&mut self, ctx: &tidb_kv::Context) -> Result<Option<Vec<u8>>>`、`fn into_iter(&mut self, intermediate_field_types: Vec<Vec<Arc<FieldType>>>) -> Result<Box<dyn SelectResultIter>>`（2C 下推用）、`fn close(&mut self) -> Result<()>`（同步，与 `kv::Response::close` 一致）、`fn cop_concurrency(&self) -> Option<(isize, isize)>` | `table_reader.go:333,342,655-708`；`distsql.go:235,281,298,910,1380,1412,1766` | **2A**（`next`/`next_raw`/`close`/`cop_concurrency`，D07.3 第 1 部分）、**2C**（`into_iter`，D07.3 第 2 部分） | **`SelectResult` 必须 `Send`** 且实现 `Drop` 安全网（取消时不泄漏底层 `CopIterator` worker）；`close` 同步且幂等；`into_iter` 经内部 `Option<SelectResultInner>` 移出状态，出错时不破坏 `self`（对齐 #410 §4.5） |
| **复合结果与内省** | `distsql.NewSerialSelectResults(results)`（**本块不可达，#410 §3.3 已归 `DEF.MPP` 不移植**：唯一非测试调用点在 `table_reader.go:421` TiFlash 分支，已在 `buildTableReader` 被 `blocked-by:DEF.MPP` 拦截）、`distsql.NewSortedSelectResults(evalCtx, results, schema, byItems, memTracker)`（D07.3 第 2 部分，多分区/多分组归并）、`distsql.GetSelectResultConcurrency(sr) -> Option<(isize, isize)>`（D07.3 第 1 部分，因 `select` 即时构造 `CopIterator`，首次 `Next` 前即返回准确值）、`distsql.SelectResultIter`（`Send`）& `SelectResultRow`（D07.3 第 2 部分，2C 下推模式） | `table_reader.go:421,464`；`distsql.go:436,910,974,1009,1381,1706` | **2A**（`NewSerialSelectResults` 归 `DEF.MPP` 不移植；`len(groupedRanges) > 1` 在 2A 提前停在 `blocked-by:D07.3` 直到 D07.3 第 2 部分提供 `NewSortedSelectResults`）、**2B**（`NewSortedSelectResults`）、**2C**（`GetSelectResultConcurrency`、`SelectResultIter`） | `NewSortedSelectResults` 接收 `Option<&Schema>`（`distsql.go:974` 传 `None` 表示按前几列排序） |

---

## 9. 全部 245 个函数的划分与对账

### 9.1 总数对账

D12.1 全部 **245** 个 Go 函数（来自 tracker `origin/main:data/func_blocks.tsv`）划分如下，0 遗漏、0 重复：

| 归属 | 函数数 | 文件构成 | 依赖与排期说明 |
|---|--:|---|---|
| **第 1 部分（已完成）** | 39 | `point_get.go` 25、`builder.go` 13、`table_reader.go` 1 | VS1-12 点查（38 个）+ D12.2 顺带移植 `markChildrenUsedCols`（1 个） |
| **第 2A 部分（TableReader，VS1-34）** | 28 | `table_reader.go` 21、`builder.go` 5、`distsql.go` 2 | 硬依赖：D07.3 表路径（#407）；sysbench 范围查询必经，优先实现 |
| **第 2B 部分（IndexReader）** | 14 | `distsql.go` 12、`builder.go` 2 | 硬依赖：2A + D07.3 索引编码 + D10.1 / D10.2 / D10.9 索引计划 |
| **第 2C 部分（IndexLookUp）** | 60 | `distsql.go` 54、`builder.go` 6 | 硬依赖：2A + 2B + D07.3 handle 编码 / `GetSelectResultConcurrency` / `SelectResultIter` + D10.1 `PhysicalIndexLookUpReader` + D12.7 `buildTableReaderFromHandles` |
| **第 3A 部分（BatchPointGet）** | 6 | `batch_point_get.go` 5、`builder.go` 1 | 依赖：D10.8 `BatchPointGetPlan` + D07.1 `Snapshot.BatchGet` |
| **第 3B 部分（分区裁剪与 IndexJoin 辅助）** | 6 | `builder.go` 6 | 依赖：D09.12 分区裁剪 + D06.4 分区表 + D12.7 IndexJoin |
| **第 3C 部分（缓存表与 UnionScan / MemReader）** | 52 | `batch_point_get.go` 2、`builder.go` 5、`union_scan.go` 8、`mem_reader.go` 37 | 依赖：D06.4 第 2 部分（缓存表）+ D10.1 `PhysicalUnionScan`；事务写缓冲部分在只读节点永不触发 |
| **第 3D 部分（TABLESAMPLE）** | 17 | `sample.go` 17 | 依赖：D10.1 `PhysicalTableSample` + D07.1 `Snapshot.Iter` + RegionCache |
| **第 3E 部分（集群内存表 Coprocessor 与 Join 辅助）** | 14 | `coprocessor.go` 13、`builder.go` 1 | `coprocessor.go` 依赖 D03.2 / D06.5 / D10.9 `PBPlanBuilder` / D12.13；`collectColumnIndexFromExpr` 供 D12.6 / D12.8 Join 调用 |
| **归 `X`（写路径 / DDL 运维 / 测试钩子）** | 8 | `point_get.go` 1、`batch_point_get.go` 5、`builder.go` 2 | 不移植，逐条见 §9.5 |
| **归 `DEF.MPP`（TiFlash BatchCop 专属）** | 1 | `table_reader.go` 1 | 不移植，见 §9.5 |
| **合计** | **245** | 9 个 Go 文件 | 39 + 28 + 14 + 60 + 6 + 6 + 52 + 17 + 14 + 8 + 1 = **245** |

**与 `specs/VS1-vertical-slice/functions.tsv` 中 VS1-34（26 个）的对照**（tracker `data/part_funcs.tsv` 的 `VS1-34` 将随第 2A 部分实现 PR 同步更新为此 28 个函数清单）：
- 已移植（2）：`buildVirtualColumnIndex`（第 1 部分）、`markChildrenUsedCols`（D12.2 顺带移植）；
- 归入第 2A 部分（21）：`buildNoRangeTableReader`、`executorBuilder.buildTableReader`、`kvRangeBuilderFromRangeAndPartition.{buildKeyRange,buildKeyRangeSeparately}`、`TableReaderExecutor.{Close,Next,Open,Table,buildKVReq,buildVirtualColumnInfo,memUsage,setDummy}`、`buildVirtualColumnInfo`、`newTableReaderExecutorContext`、`selectResultHook.SelectResult`、`sortAndGetKVRangesFromReqs`、`tableReaderExecutorContext.GetInfoSchema`、`tableResultHandler.{Close,nextChunk,nextRaw,open}`；
- 调出 VS1-34（3）：`buildTableReq`（仅 IndexLookUp / IndexMerge 调用，归 2C）、`collectColIdxFromByItems`（仅 UnionScan 调用，归 3C）、`collectColumnIndexFromExpr`（仅 MergeJoin / HashJoin 调用，归 3E）；
- 补入第 2A 部分（7）：`TableReaderExecutor.Open` 与 `tableResultHandler.Close` 直接调用的 `TableReaderExecutor.buildRespForGroupedRanges`、`buildKVReqSeparatelyForGroupedRanges`、`buildKVReqSeparately`、`tableReaderExecutorContext.GetDDLOwner`、`newClosestReadAdjuster`（VS1-12 桩转正）、`distsql.go` 中的 `closeAll` 与 `getPhysicalPlanIDs`。合计 21 + 7 = **28** 个。

### 9.2 第 2A 部分：TableReader（28 个函数）

- **`table_reader.go`（21）**：`selectResultHook.SelectResult`（:68）、`tableReaderExecutorContext.GetInfoSchema`（:94）、`tableReaderExecutorContext.GetDDLOwner`（:98）、`newTableReaderExecutorContext`（:106）、`TableReaderExecutor.Table`（:206）、`TableReaderExecutor.setDummy`（:210）、`TableReaderExecutor.memUsage`（:214）、`TableReaderExecutor.Open`（:229）、`TableReaderExecutor.Next`（:353）、`TableReaderExecutor.Close`（:380）、`TableReaderExecutor.buildRespForGroupedRanges`（:398）、`TableReaderExecutor.buildKVReqSeparatelyForGroupedRanges`（:467）、`sortAndGetKVRangesFromReqs`（:487）、`TableReaderExecutor.buildKVReqSeparately`（:501）、`TableReaderExecutor.buildKVReq`（:576）、`TableReaderExecutor.buildVirtualColumnInfo`（:632）、`buildVirtualColumnInfo`（:637）、`tableResultHandler.open`（:661）、`tableResultHandler.nextChunk`（:672）、`tableResultHandler.nextRaw`（:686）、`tableResultHandler.Close`（:704）。
- **`builder.go`（5）**：`buildNoRangeTableReader`（:3670）、`executorBuilder.buildTableReader`（:3829）、`kvRangeBuilderFromRangeAndPartition.buildKeyRangeSeparately`（:4854）、`kvRangeBuilderFromRangeAndPartition.buildKeyRange`（:4873）、`newClosestReadAdjuster`（:4892，将 VS1-12 桩转正）。
- **`distsql.go`（2）**：`closeAll`（:140）、`getPhysicalPlanIDs`（:2297）。

随第 2A 部分跨块顺带解除/移植的小函数（计入各自所属块）：
- D12.0：`ConstructDAGReq`、`ConstructListBasedDistExec`、`ConstructTreeBasedDistExec` 的 `to_pb` 解除桩（`builder_utils.rs`）、`assertByItemsAreColumns`（`builder.go:3814`）、`UpdateExecutorTableID`（`internal/util/partition_table.go:26`）、`TableReaderExecutor.Detach` / `tableReaderExecutorContext.Detach`（`detach.rs`）；
- EXC.B → 待负责人决定（§15.1）：`getTblStats`（`plan_cost_ver1.go:1284`）与 `PhysicalTableReader.GetNetDataSize`（`plan_cost_ver1.go:327`；2B 同涉 `PhysicalIndexReader.GetNetDataSize` :243）；
- D12.10：`executorBuilder.corColInDistPlan`（:3349）与 `corColInAccess`（:3382）；
- D12.14：`normalizeCtxErrWithCause`（`analyze_utils.go:112`）；
- D00.3：`timeutil.Zone`（`pkg/util/timeutil/time_zone.go:189`，`ConstructDAGReq` 编码时区名与偏移用）。

### 9.3 第 2B 部分：IndexReader（14 个函数）

- **`distsql.go`（12）**：`rebuildIndexRanges`（:158）、`newIndexReaderExecutorContext`（:194）、`IndexReaderExecutor.Table`（:266）、`IndexReaderExecutor.setDummy`（:270）、`IndexReaderExecutor.Close`（:275）、`IndexReaderExecutor.Next`（:292）、`IndexReaderExecutor.Open`（:302）、`IndexReaderExecutor.buildKVRangesForIndexReader`（:333）、`IndexReaderExecutor.buildKVReq`（:360）、`IndexReaderExecutor.open`（:379）、`buildKeyRanges`（:625）、`needMergeSort`（:2305）。
- **`builder.go`（2）**：`buildNoRangeIndexReader`（:4042）、`executorBuilder.buildIndexReader`（:4102）。
- 随本部分解除 D12.0 `detach.rs` 中的 `IndexReaderExecutor.Detach` / `indexReaderExecutorContext.Detach` 桩，并在 `corColInAccess` 补上 `PhysicalIndexScan` 分支。

### 9.4 第 2C 部分：IndexLookUp（60 个函数）

- **`distsql.go`（54）**：
  - 任务排序（3）：`lookupTableTask.Len`（:119）、`Less`（:123）、`Swap`（:127）；
  - 上下文与执行器主体（22）：`newIndexLookUpExecutorContext`（:453）、`IndexLookUpExecutor.Table`（:574）、`setDummy`（:578）、`Open`（:583）、`buildTableKeyRanges`（:655）、`open`（:697）、`startWorkers`（:737）、`needPartitionHandle`（:751）、`isCommonHandle`（:786）、`getRetTpsForIndexReader`（:790）、`startIndexWorker`（:814）、`buildIndexSelectResultForRange`（:1052）、`calculateBatchSize`（:1118）、`startTableWorker`（:1147）、`buildTableReader`（:1172）、`Close`（:1204）、`Next`（:1257）、`getResultTask`（:1288）、`initRuntimeStats`（:1327）、`getIndexPlanRootID`（:1336）、`getTableRootPlanID`（:1343）、`getHandle`（:1902）；
  - 批大小与并发限流计算（5）：`getIndexScanMaxInFlight`（:1000）、`getSelectResultInFlightCost`（:1007）、`getMergeSortSharedCoprRequestRateLimit`（:1018）、`getMergeSortIndexScanConcurrency`（:1031）、`CalculateBatchSize`（:1131）；
  - `indexWorker` 与结果集合（15）：`indexWorker.syncErr`（:1371）、`newSelectResultList`（:1384）、`newSelectResultRowIterList`（:1392）、`selectResultList.Close`（:1406）、`selectResultWithMeta.Close`（:1427）、`indexWorker.fetchHandles`（:1454）、`fetchHandlesRolling`（:1495）、`prepareHandleFetch`（:1571）、`extractLookupTaskData`（:1590）、`buildAndDispatchLookupTasks`（:1608）、`getHandleOffsets`（:1672）、`extractLookUpPushDownRowsOrHandles`（:1691）、`extractTaskHandles`（:1749）、`buildCompletedTask`（:1810）、`buildTableTask`（:1819）；
  - `tableWorker` 与一致性校验（5）：`tableWorker.pickAndExecTask`（:1870）、`tableWorker.compareData`（:2029）、`getDatumRow`（:2146）、`tableWorker.executeTask`（:2160）、`GetLackHandles`（:2275）；
  - 运行时统计（4）：`IndexLookUpRunTimeStats.String`（:1969）、`Clone`（:2003）、`Merge`（:2009）、`Tp`（:2025）。
  - （3 + 22 + 5 + 15 + 5 + 4 = 54 个。）
- **`builder.go`（6）**：`buildTableReq`（:4153）、`buildIndexLookUpPushDownDAGReq`（:4171）、`buildIndexReq`（:4213）、`buildIndexScanOutputOffsets`（:4231）、`buildNoRangeIndexLookUpReader`（:4271）、`executorBuilder.buildIndexLookUpReader`（:4358）。
- 随本部分提前按 L1 移植 D12.7 的 `dataReaderBuilder.buildTableReaderBase`（:4911）与 `buildTableReaderFromHandles`（:4943）（计入 D12.7），并解除 D12.0 `detach.rs` 中的 `IndexLookUpExecutor.Detach` / `indexLookUpExecutorContext.Detach` 桩。

### 9.5 后续第 3 部分（95 个）与豁免 / 归 X（9 个）

**第 3A 部分：BatchPointGet（6）**：`batch_point_get.go` 的 `BatchPointGetExec.{buildVirtualColumnInfo,Open,Close,Next,initialize}`（5）+ `builder.go` 的 `executorBuilder.buildBatchPointGet`（1）。等待 D10.8 `BatchPointGetPlan`。

**第 3B 部分：分区动态裁剪与 IndexJoin 辅助（6）**：`builder.go` 的 `partitionPruning`（:5691）、`getPartitionIDsAfterPruning`（:5726）、`fullRangePartition`（:5753）、`buildIndexRangeForEachPartition`（:3921）、`getPartitionKeyColOffsets`（:3944）、`dedupHandles`（:4830）。等待 D09.12 分区裁剪与 D12.7 IndexJoin。

**第 3C 部分：缓存表与 UnionScan / MemReader（52）**：`batch_point_get.go` 的 `cacheTableSnapshot.{BatchGet,Get}`（2）、`builder.go` 的 `executorBuilder.getCacheTable`（:5955，现有桩）、`executorBuilder.buildUnionScanExec`（:1370）、`executorBuilder.buildUnionScanFromReader`（:1404）、`UnionScanExec.handleCachedTable`（:1551）、`collectColIdxFromByItems`（:1384）、`union_scan.go` 全部 8 个、`mem_reader.go` 全部 37 个。说明：虽然 tidb-rs 没有事务写入（`txn.MemBuffer` 恒为空），但缓存表（`ALTER TABLE ... CACHE`，D06.4 第 2 部分）的读路径在 Go 中通过 `UnionScanExec` + `mem_reader` 承载，因此不归 `X`，整体留待 D06.4 第 2 部分再决定移植或豁免。

**第 3D 部分：TABLESAMPLE（17）**：`sample.go` 全部 17 个函数。等待 D10.1 `PhysicalTableSample` 与 D07.1 `Snapshot.Iter`。

**第 3E 部分：集群内存表 Coprocessor 与 Join 辅助（14）**：`coprocessor.go` 全部 13 个函数（`CoprocessorDAGHandler` 集群内存表 gRPC 服务端，等 D03.2 / D06.5 / D12.13）+ `builder.go` 的 `collectColumnIndexFromExpr`（:1649，仅由 `extractUsedColumnsInJoinOtherCondition` 调用，随 D12.6 / D12.8 Join 移植）。

**归 `X`（8 个，永久不移植）与 `DEF.MPP`（1 个，永久不移植）**：

| 文件 | 函数与行号 | 归类 | 理由 |
|---|---|---|---|
| `point_get.go` | `PointGetExecutor.getValueFromLockCtx`（:622） | `X` | 悲观锁 `SELECT ... FOR UPDATE` 写路径，tidb-rs 只读（与第 1 部分 `lockKeyBase` 决定一致） |
| `batch_point_get.go` | `LockKeys`（:495） | `X` | 悲观锁加锁（写路径） |
| `batch_point_get.go` | `PessimisticLockCacheGetter.Get`（:533） | `X` | 悲观锁缓存读取（写路径） |
| `batch_point_get.go` | `cacheBatchGetter.BatchGet`（:555） | `X` | `LOCK TABLES ... READ` 时的本地缓存读（与第 1 部分 `point_get.go` `cacheDB.UnionGet` 同属 `X`） |
| `batch_point_get.go` | `newCacheBatchGetter`（:579） | `X` | 同上 |
| `batch_point_get.go` | `MockNewCacheTableSnapShot`（:176） | `X` | 仅用于 Go 单元测试的 mock 构造函数 |
| `builder.go` | `mockPhysicalIndexReader.MemoryUsage`（:4617） | `X` | 源码注释明写 `only for testing`，仅用于 `dataReaderBuilder` 单测 |
| `builder.go` | `buildIdxColsConcatHandleCols`（:563） | `X` | 仅被 `buildRecoverIndex`（:627）和 `buildCleanupIndex`（:697）调用，二者均为 `ADMIN RECOVER/CLEANUP INDEX` 运维写路径 |
| `table_reader.go` | `TableReaderExecutor.buildKVReqForPartitionTableScan`（:537） | `DEF.MPP` | 仅在 `e.storeType == kv.TiFlash && e.kvRangeBuilder != nil && e.batchCop`（:404、:424）调用，属 TiFlash BatchCop 专属（与 D07.2 spec §1 一致） |

---

## 10. Go → Rust 结构映射

### 10.1 文件与类型布局

模块保持与 Go 一一对应：
- `crates/executor/src/table_reader.rs`：补齐 `TableReaderExecutor`、`TableReaderExecutorContext`、`TableResultHandler`、`SelectResultHook`、`KvRangeBuilder` trait、`build_virtual_column_info` 等；
- `crates/executor/src/distsql.rs`（新文件）：第 2A 部分先放 `close_all`、`get_physical_plan_ids`；第 2B 部分加 `IndexReaderExecutor`、`IndexReaderExecutorContext`、`build_key_ranges`、`rebuild_index_ranges`、`need_merge_sort`；第 2C 部分加 `IndexLookUpExecutor`、`IndexLookUpExecutorContext`、`IndexWorker`、`TableWorker`、`LookupTableTask`、`IndexLookUpRunTimeStats`、`GetLackHandles` 等；
- `crates/executor/src/builder.rs`：加 `build_no_range_table_reader`、`build_table_reader`、`kv_range_builder_from_range_and_partition`、转正 `new_closest_read_adjuster`（2A）；`build_no_range_index_reader`、`build_index_reader`（2B）；`build_table_req`、`build_index_req`、`build_index_scan_output_offsets`、`build_index_look_up_push_down_dag_req`、`build_no_range_index_look_up_reader`、`build_index_look_up_reader` 及 `DataReaderBuilder`（2C）；
- `crates/executor/src/internal/builder/builder_utils.rs`：解除 `to_pb` 与 `construct_dag_req` 的桩；
- `crates/executor/src/internal/util/partition_table.rs`（新文件）：`update_executor_table_id`（D12.0）；
- `crates/executor/src/detach.rs`：按 2A / 2B / 2C 依次把三个读取器及其上下文的 `Detach` 桩转为真实实现（在 N2 下执行器不持有会话，拷贝配置与克隆 `Arc<DistSQLContext>` 即可，游标主入口仍停在 `D12.12`）。

### 10.2 上下文与会话边界（N2，`M-CTX`）

执行器不持有会话。`SessionContext`（`crates/executor/src/glue.rs`）新增三个只读访问方法（由 `tidb-session` 实现，替换 `session.rs:790,797` 的桩）：
- `get_dist_sql_ctx(&self) -> Arc<DistSQLContext>`（同步返回，对齐 #407 §4.9）
- `get_build_pb_ctx(&self) -> Result<Arc<BuildPBContext>, Error>`
- `get_ranger_ctx(&self) -> Arc<RangerContext>`（第 2B/2C `rebuildIndexRanges` 用）

各执行器上下文映射：

| Go 结构 | Rust 结构与字段 | 说明 |
|---|---|---|
| `tableReaderExecutorContext`（`table_reader.go:82`） | `TableReaderExecutorContext`：`dctx: Arc<DistSQLContext>`、`rctx: Arc<RangerContext>`、`build_pb_ctx: Arc<BuildPBContext>`、`ectx: ExprContextRef`、`stmt_mem_tracker: Option<Arc<Tracker>>`、`info_schema: InfoSchemaRef` | `getDDLOwner` 留为 `Option<...>`（恒为 `None`；cluster 表遇到 `DDLOwner` 目的地时报 `blocked-by:D06.5`） |
| `indexReaderExecutorContext`（`distsql.go:184`） | `IndexReaderExecutorContext`：同上 6 项（无 `getDDLOwner`） | 全字段 `Send + Sync` |
| `indexLookUpExecutorContext`（`distsql.go:441`） | `IndexLookUpExecutorContext`：嵌入 `table_ctx: TableReaderExecutorContext`，加 `stmt_runtime_stats_coll: Option<Arc<RuntimeStatsColl>>`、`index_lookup_size: isize`、`index_lookup_concurrency: isize`、`enable_redact_log: String`、`weak_consistency: bool` | 不保留 `storage: kv.Storage`（只供 MVCC 调试 dump 使用，`consistency.rs` 已登记不 dump） |

### 10.3 `TableReaderExecutor` / `IndexReaderExecutor` 的生命周期与取消

- **单流异步 I/O**：`TableReaderExecutor` 与 `IndexReaderExecutor` 自身不 spawn 额外 worker task，它们通过 `SelectResult`（底层为 D07.2 `CopIterator`）异步拉取 chunk。
- **`TableResultHandler` 所有权**：持有 `optional_result: Option<Box<dyn SelectResult>>`、`result: Option<Box<dyn SelectResult>>`、`optional_finished: bool`。`Close` 调用 `close_all` 依次关闭两个结果并清空为 `None`，返回第一个非空错误。
- **打开中途失败的清理**：`TableReaderExecutor.open` 在构造 `second_result` 失败时，或 `build_resp_for_grouped_ranges` / `IndexReaderExecutor.open` 在循环构造第 k 个 `SelectResult` 失败时，已建好的 `SelectResult` 随局部 `Option`/`Vec` 离开作用域自动 `Drop`（触发 `CopIterator::drop` 取消并中止后台 cop worker），不会像 Go 那样泄漏（见 §14 第 1 条）。
- **`Drop` 安全网**：语句 future 被取消而未调用显式 `close()` 时，`TableReaderExecutor` / `IndexReaderExecutor` 被 drop，其内部持有的 `SelectResult` 自动 drop 并中止所有在途 TiKV cop worker。

### 10.4 `IndexLookUpExecutor` 并发模型（`M-GO`、`M-CH`、`M-WG`、`M-ROW`）

参照 D12.2 §2 第 2 条、D12.3 §4.3 与 D07.2 §6.2 已验证的固定写法：

1. **共享不可变/原子状态**：把 `indexWorker` 与 `tableWorker` 都要读写的字段（`indexLookUpExecutorContext`、`table`、`index`、`dag_pb` 的不可变副本或 `Arc`、`table_request`、`start_ts`、`txn_scope`、`read_replica_scope`、`is_staleness`、`handle_idx`、`handle_cols`、`primary_key_index`、`idx_plans`、`tbl_plans`、`columns`、`keep_order`、`desc`、`index_paging`、`index_look_up_push_down`、`partition_table_mode`、`pruned_partitions`、`avg_row_size`、`idx_net_data_size`、`mem_tracker`、`stats: Option<Arc<IndexLookUpRunTimeStats>>`、`base_schema`、`max_chunk_size` 等）放进 `Arc<IndexLookUpShared>`（`L2 [M-GO]`），满足 task 闭包的 `'static + Send` 约束。
   - 注意：`IndexLookUpRunTimeStats` 中跨 task 原子累加的字段（`FetchHandleTotal`、`FetchHandle`、`TaskWait`、`TableRowScan`、`TableTaskNum`）用 `AtomicI64`；主线程独占的 `NextWait*` 用 `parking_lot::Mutex` 或留在主线程并在 `Close` 合并进注册的 stats（保证无锁竞争）。
   - `e.dag_pb` 在 `startIndexWorker` 中因 `len(kvRanges) == 1` 或分区表重写 `TableId` 而需要修改时，只修改 `indexWorker` 局部克隆的 `TipbDagRequest`，**不原地改写执行器自有的模板**（一并规避 §14 第 3 条的重复 `Open` 隐患）。
2. **task 与通道映射**（容量逐个照抄 Go）：

| Go 原语 | 容量 | 发送方 → 接收方 | Rust 对应物 |
|---|---|---|---|
| `indexWorker` goroutine（:855） | 1 个 | — | 在 `ExecCtx.pool`（`None` 时当前 runtime）上 `spawn_with_recover` |
| `tableWorker` goroutines（:1163） | `indexLookupConcurrency` 个 | — | 同上，`Vec<JoinHandle<()>>` |
| `workCh chan *lookupTableTask`（:742） | 1 | `indexWorker` → 多个 `tableWorker` | `tokio::sync::mpsc::channel(1)`；接收端用 `Arc<tokio::sync::Mutex<mpsc::Receiver<LookupTableTask>>>` 在多个 `tableWorker` 间共享（与 D07.2 `copr/coprocessor.rs:1131` `task_ch` 完全同构；`tokio::sync::Mutex` 可跨 `await`，不触发 `await_holding_lock`） |
| `resultCh chan *lookupTableTask`（:711） | `LookupTableTaskChannelSize`（50） | `indexWorker` → `Next` | `tokio::sync::mpsc::channel(50)`，发的是 `ResultTaskHandle`（经 `done_rx` 传回已完成任务） |
| `task.doneCh chan error`（:1850） | 1 | `tableWorker`（或已完成 task / `syncErr`）→ `Next` | **`tokio::sync::oneshot::channel::<Result<LookupTableTaskResult, Error>>()`**：`workCh` 传任务输入（`handles`、`index_order`、`partition_table`、`done_tx`），`resultCh` 只传 `done_rx`（与 `build_done_time` 一起经 `oneshot` 交回结果行），彻底消除 `lookupTableTask` 在 `tableWorker` 与主线程之间的共享可变指针！这是安全 Rust 下的零锁所有权移交（`L2 [M-CH]`） |
| `finished chan struct{}`（:710）与 `cancelFunc`（:740） | 关闭广播 + 丢弃 future 取消 | `Close` / `Drop` → 全部 worker 及在途 `SelectResult` | **`tokio::sync::watch::channel(false)` + `tokio::select!` 丢弃 future + `JoinHandle::abort()`**（`crates/` 中没有 `CancellationToken`，`crates/kv/src/context.rs` 与 N3 §3.1 第 6 条规定取消靠丢弃 future，与 D12.2 / D12.3 / D07.2 完全一致）：`Close` 发送 `finished_tx.send(true)`；`indexWorker` 与 `tableWorker` 对每个 `distsql::select*` / `SelectResult::next` future 与 `finished_rx.changed()` 做 `tokio::select!`，收到退出信号即丢弃在途 future 并立即关闭/丢弃所持 `SelectResult`（触发 `CopIterator::drop` 中止底层 TiKV cop worker） |

3. **`lookupTableTask.rows` 的行表示（`M-ROW`，N5a）**：Go 的 `task.rows []chunk.Row` 保存指向多个 `Chunk` 内部缓冲区的行视图，并在 `keepOrder` 时按 `rowIdx` 排序后跨多次 `Next` 调用输出。Rust 中 `LookupTableTaskResult` 持有 `chunks: Vec<Arc<Chunk>>` 与 `rows: Vec<(usize, usize)>`（`(chunk_idx, row_idx)`），`keepOrder` 排序只交换 `(row_idx_key, (chunk_idx, row_idx))`，写入输出 chunk 时直接按索引取行，零行拷贝且无悬垂借用。
4. **`tableWorker` 监听退出信号的语义**（`distsql.go:1881-1891`）：Go 注释强调 `pickAndExecTask` 的 `select` 只监听 `workCh` 和 `finished`，不监听 `ctx.Done()`，防止语句取消时 worker 提前退出导致主线程永久卡在 `<-task.doneCh`。在 Rust 使用 `oneshot` 后，若 `tableWorker` 退出并 drop `done_tx`，主线程 `done_rx.await` 会立即收到 `RecvError`（不会永久卡住）；同时 `pick_and_exec_task` 循环依然只 `select!` `workCh` 与 `finished`，在 `execute_task` 内部对在途 `tableReader` 的 `next` 与 `finished` 做 `select!`，使 `Close` / `Drop` 及时中止在途回表请求。
5. **`Close` 与 `Drop`（顺序严格对齐 §10.5 与 Go `distsql.go:1204-1254`）**：
   - `Close`：**先上报 index usage（`:1225-1230`）** →（若已启动 worker）`finished_tx.send(true)`（等价于 Go `cancelFunc()` + `close(e.finished)`）→ 排空并丢弃 `result_rx`（唤醒可能阻塞在 `result_tx.send` 的 `indexWorker`）→ `await` `idx_worker_handle` → `await` 全部 `tbl_worker_handles` → 清理 `result_curr` → **最后注册 `IndexLookUpRunTimeStats`**（对应 Go `defer` LIFO 在函数退出时执行）。
   - `Drop`：`finished_tx.send(true)` → `abort()` 全部尚未结束的 `JoinHandle`（同时触发每个 task 持有的 `SelectResult` / `CopIterator` 的 `Drop`）。`Close` 等待 handle 时只在每个 handle `await` 完成后再移除，确保 `Close` future 中途被 drop 时剩余 task 仍被 `Drop` 中止（与 D12.2 §2 第 2 条、D12.3 §4.4 相同）。

### 10.5 `defer` 清理顺序清单（`M-DEFER`、`M-PANIC`）

每处 `defer` 都用显式 drop guard 或结构化收尾实现，保证正常返回、出错返回、panic 三种路径下按 Go 顺序执行：

| Go 位置 | 清理顺序 |
|---|---|
| `TableReaderExecutor.Close`（`table_reader.go:380-394`） | 上报 index usage → `resultHandler.Close()`（`closeAll(optionalResult, result)`）→ 清空 `kvRanges` |
| `IndexReaderExecutor.Close`（`distsql.go:275-289`） | 上报 index usage → 关闭 `e.result` → 置空 `e.result`、清空 `kvRanges` |
| `IndexLookUpExecutor.Close`（`distsql.go:1204-1254`） | 上报 index usage →（若已启动）`cancel()` → 通知 `finished` → 排空 `resultCh` → 等 `idxWorker` → 等全部 `tblWorker` → 最后注册 `IndexLookUpRunTimeStats`（Go 的 `defer` LIFO 在函数退出时执行） |
| `startIndexWorker` goroutine（`distsql.go:857-861`） | `fetchHandles` / `fetchHandlesRolling` 返回错时 `syncErr` → 丢弃 `work_tx`（关闭 `workCh`）→ 丢弃 `result_tx`（关闭 `resultCh`）→ task 结束 |
| `indexWorker.fetchHandles`（`distsql.go:1455-1461`） | 依次关闭 `results`（`SelectResult` / `SelectResultIter`）→ 边界 `catch_unwind` 将 panic 转为错误经 `syncErr` 发给 `resultCh` |
| `indexWorker.fetchHandlesRolling`（`distsql.go:1496-1517`） | 关闭当前全部在途 `results` → 边界 `catch_unwind` 转错经 `syncErr` 发给 `resultCh` |
| `extractLookUpPushDownRowsOrHandles`（`distsql.go:1698-1702`） | 记录本次扫描耗时与行数到 `indexScanBasicStats` |
| `tableWorker.pickAndExecTask`（`distsql.go:1873-1879`） | `catch_unwind` 捕获 `executeTask` 中的 panic，若存在当前 `done_tx` 则发送恢复后的错误（避免 §14 第 7 条 nil 崩溃） |
| `tableWorker.executeTask`（`distsql.go:2169`） | 无论成功或失败都关闭临时构造的 `tableReader`（记录错误日志，不覆盖主返回错） |

### 10.6 内存记账（`M-MEM`、A1）、运行时统计与 KILL 检查点（`M-KILL`）

- **内存记账（A1）与 Go 记账边界**：
  - `TableReaderExecutor.Open`、`IndexReaderExecutor.open`、`IndexLookUpExecutor.Open` / `open` 照 Go 建执行器级 `Tracker` 并挂到 `stmt_mem_tracker`（`ExecCtx` 传入），经 `RequestBuilder.SetMemTracker` 传给 D07.3 / D07.2 的 `selectResult` 与 `CopIterator`。
  - **`IndexLookUpExecutor` 各子 tracker（照 Go 树形结构）**：`startIndexWorker` 在 `distsql.go:819-820` 建子 tracker（标签 `LabelForIndexWorker`）挂到 `e.memTracker` 供索引侧 `SelectResult` 与 `indexWorker` 使用；`startTableWorker`（`:1159`）为每个 `tableWorker` 建独立子 tracker 挂到 `e.memTracker`。**注意**：Go 中回表侧的 `TableReaderExecutor`（由 `dataReaderBuilder.buildTableReaderBase`，`builder.go:4911` 构造）**不设置 `memTracker`**，因此表侧 cop 响应在 Go 中不记账，Rust 照抄此行为并注明。
  - `IndexLookUpExecutor` 中 `tableWorker.executeTask`（`distsql.go:2175-2236`）与 `getResultTask`（:1320-1322）对 `task.handles`、`tableReader.memUsage()`、`task.rows`、`chk.MemoryUsage()`、`task.rowIdx` 的手工估算 `Consume(±delta)` 按已确认的 A1 例外映射：容器经 `TrackedAlloc` 自动记实际容量、Drop 自动归零；Go 正向 `Consume` 的位置改为 `mem_tracker.checkpoint()?`，返回 `Err` 经 `task.done_tx` 交回 `Next`（而非 panic，与 D12.3 §4.5、§4.6 一致）。记账数值与 Go 不同，用户可见（影响 `tidb_mem_quota_query` 触发点、`EXPLAIN ANALYZE` 内存列与慢日志 `Mem_max`，依据已确认的 A1）。
  - `TableReaderExecutor.memUsage`（`table_reader.go:214`）保留函数及 `// go:` 标注供 `converge` 与 2C 调用，按 A1 返回估算值或实际容量（不用于手工扣减）。
- **`IndexLookUpExecutor` 索引侧 `BasicRuntimeStats`（`distsql.go:1580-1584`）**：`prepareHandleFetch` 在语句开了 `RuntimeStatsColl` 时会把索引计划根 ID 的 `BasicRuntimeStats` 替换为语句级 stats（决定 `EXPLAIN ANALYZE` 中 `IndexRangeScan` 的 `time` / `loops`）；Rust 中共享的 `IndexLookUpShared` 需通过内部可变（原子计数或 `OnceLock` / `parking_lot::Mutex`）支持该统计更新。
- **KILL 检查点（`M-KILL`）**：
  - `TableReaderExecutor.Next` / `IndexReaderExecutor.Next` / `IndexLookUpExecutor.Next` 外层已有 `exec::next` 检查点，且底层 `CopIterator` 在每次响应检查 `SQLKiller`。
  - 另外按 N3 第二部分第 5 条，在不经过 `exec::next` 的内层循环加 `CheckpointStride::new(32)`：`indexWorker.extractTaskHandles` 的逐行 decode handle 循环（`distsql.go:1779-1795`）、`tableWorker.executeTask` 收集行后构 `rowIdx`（:2224-2231）与一致性校验构 `obtainedHandlesMap` 循环（:2244-2250）。

### 10.7 共享修改审计（`M-FT`，N1 §5）与其他映射规则

对 `table_reader.go`、`distsql.go` 及 `builder.go` 中 D12.1 全部函数运行 plan 模板 §4 的两个 grep：

| Go 位置 | 改什么 | 结论 | 处理 |
|---|---|---|---|
| `distsql.go:1917`（`IndexLookUpExecutor.getHandle`） | `rtp = rtp.Clone(); rtp.SetCollate(charset.CollationBin)` | **S**（先复制再改） | 改本地副本，照抄 |
| `builder.go:661`（`buildHandleColsForExec`，第 1 部分已移植） | 新建局部 `tblCols[c.Offset].Index = j` | **S**（新建对象） | 已移植 |

读代码补充（grep 抓不到的共享写入）：
- **`builder.go:3841-3843`（`buildTableReader`，`` `50358` ``）**：Go 原地写 `v.SetSchema(v.GetTablePlan().Schema())`；在实例计划缓存（A3）下 `PhysicalTableReader` 被所有会话共享，Rust 已在 `crates/executor/src/table_reader.rs:41` 改为只读函数 `table_reader_schema(v)`（A3 spec §2.3 row 9b），不改计划。
- **`table_reader.go:245-263`、`508`、`549`、`distsql.go:381-391`、`830`、`1068`**：均写入执行器自己通过 `ConstructDAGReq` 新建的 `e.dagPB`，不涉及共享计划；其中 `distsql.go:830,1068` 改为在 worker 局部克隆上修改（§10.4）。

其他映射规则核对：
- **`M-FMA`**：核对 tracker `data/go_fma_sites.tsv`，D12.1 全部 9 个文件均为 **0 处** FMA 融合位置。
- **`M-POOL`**：`distsql.go:1575` `AllocPool.Alloc` 直接建 chunk。
- **`M-INT`**：`builder.go:3733,4092,4159,4207,4249,4261,4266` 偏移量转 `u32`；`CalculateBatchSize`（`distsql.go:1138`）`batchSize = batchSize * 2` 在 `maxBatchSize` 接近 `isize::MAX` 时可能溢出，按 Go 写 `wrapping_mul(2)` 或检查溢出（`M-INT`）。
- **`M-BYTES`**：`kv.Key` / `StartKey` / `EndKey` / `handleEncoded` 均为 `Vec<u8>`。
- **`M-MAPORDER`**：`distsql.go:2085` `task.indexOrder.Range`（`ADMIN CHECK` 报错取第一条缺失索引行，Go map 顺序不确定，Rust 取确定顺序，且 2C 前 `checkIndexValue` 不可达）。

---

## 11. 边界桩与路由器回退

### 11.1 第 2 部分各阶段的边界桩

| Rust 位置 | Go 位置 | 类型 | 触发条件与处理 |
|---|---|---|---|
| `executorBuilder.build` 对 `PhysicalIndexReader` / `PhysicalIndexLookUpReader` / `BatchPointGet` / `UnionScan` / `TableSample` | `builder.go` | 临时 `blocked-by:D12.1`（随 2B / 2C / 3A / 3C / 3D 逐个移除） | 在执行器打开之前报错，路由器安全回退到 PoC（2A 阶段 `PhysicalIndexReader` / `PhysicalIndexLookUpReader` 规划器本身也尚未合入，在 D10.2 `convert_to_index_scan` 即已回退） |
| `buildTableReader`：TiFlash / MPP（`v.StoreType == TiFlash` 或 `ReadReqType != Cop`） | `builder.go:3853-3863` | 永久 `blocked-by:DEF.MPP` | 建执行器前报错，不进入 `buildMPPGather` / `buildKVReqForPartitionTableScan` |
| `buildTableReader`：动态分区表裁剪（`UseDynamicPartitionPrune() && !isPartition && pi != nil`） | `builder.go:3885-3917` | 临时 `blocked-by:D09.12` | 建执行器前报错回退（当前规划器在 D09.12 也尚未放行分区表）；3B 移植 `partitionPruning` 后解除 |
| `buildTableReader` / `buildNoRangeTableReader`：静态分区表（`ts.IsPartition()` 为真） | `builder.go:3689-3692` | 临时 `blocked-by:D06.4`（分区表对象 `PartitionedTable::get_partition`） | 建执行器前报错回退 |
| `buildNoRangeTableReader`：缓存表（`TableCacheStatusEnable`）与本地/全局临时表 | `builder.go:3683,3736` | 临时 `blocked-by:D06.4`（缓存表在 `validCanReadTemporaryOrCacheTable` 之外需缓存数据） | 缓存表在 `UnionScan` / `getCacheTable` 报 `blocked-by:D06.4`；临时表照 Go 设 `e.dummy = true`（但无写路径且无 `UnionScan` 时走常规校验） |
| `TableReaderExecutor.buildKVReq`：集群内存表需路由到 `DDLOwner` | `table_reader.go:588-597` | 临时 `blocked-by:D06.5` | `table.Type().IsClusterTable() && copDestination == DDLOwner` 时报 `blocked-by:D06.5`（当前 D06.6 / D12.13 集群内存表在规划前已拦截） |
| `TableReaderExecutor.Open`：`corColInAccess`（`ts.ResolveCorrelatedColumns()`）与 `groupByColIdxs` 重建（`GroupRangesByCols`） | `table_reader.go:264-277` | 临时 `blocked-by:D10.1` / `blocked-by:D10.2` | 复用 `PhysicalTableScan::resolve_correlated_columns` 现有桩（`D10.1`）；在 **build 阶段**检测到 `corColInAccess` 或 `corColInFilter` 涉及未支持路径时提前在 build 阶段返回 `blocked-by:D10.1`，**避免 `Open` 之后无法回退**（见 §11.2） |
| `TableReaderExecutor.Open`：`e.storeType == TiKV && len(e.groupedRanges) > 1`（或动态分区 `byItems != nil`）需 `NewSortedSelectResults` | `table_reader.go:462-464` | 依赖 D07.3；若 D07.3 第 1 批尚未含 `NewSortedSelectResults`，则临时 `blocked-by:D07.3` | 在 build 阶段若 `len(grouped_ranges) > 1` 提前报 `blocked-by:D07.3`（sysbench 单表扫描永远只有 1 组区间） |
| `IndexLookUpExecutor`：`tableWorker.compareData`（`checkIndexValue != nil`，`ADMIN CHECK` 专用） | `distsql.go:2029-2144` | 临时 `blocked-by:D06.4`（`tables.CompareIndexAndVal`）/ `D00.3`（`ReportAdminCheckInconsistent`） | 仅 2C；SELECT 路径 `checkIndexValue` 恒为 `None`，不可达 |
| `IndexLookUpExecutor`：`indexLookUpPushDown` 若 D07.3 暂未提供 `SelectResultIter` | `distsql.go:909-915,979-986` | 临时 `blocked-by:D07.3`（在 `buildNoRangeIndexLookUpReader` 检测到 `v.IndexLookUpPushDown` 时提前拦截） | 仅 2C；`tidb_index_lookup_pushdown_policy` 默认 `hint-only`，常规查询不触发 |
| `PointGetExecutor.verifyTxnScope`（第 1 部分遗留桩） | `crates/executor/src/point_get.rs:523` | **由 D07.3 第 1 部分（#410）解除** | D07.3 第 1 部分提供 `distsql::verify_txn_scope` 并一并消除该桩；本块 2A 负责解除 `crates/executor/src/builder.rs:955` 的 `new_closest_read_adjuster` 桩 |

### 11.2 路由器与回退纪律（吸取 D12.2 #338 / #345 回归教训）

第 2A 部分是继点查（VS1-12）与无表基础算子（VS1-35）之后，**第一次把真实的表扫描语句从回退切到新栈执行**。必须严格遵守 port-block §2 与 VS1 plan §4.3–§4.7 的路由纪律：

1. **回退必须在任何执行器 `Open` 之前完成**：所有第 2A 部分尚未支持的分支（分区表、TiFlash/MPP、带相关列访问条件的扫描、多组 `groupedRanges` 若 D07.3 未含 `NewSortedSelectResults`）必须在 `executorBuilder.buildTableReader` 返回 `blocked`（错误 1235），不得推迟到 `TableReaderExecutor.Open` 发出 TiKV 请求之后再报 `blocked`。
2. **绝不静默跳过**：遇到 Go 会走的未实现分支（如 TiKV 之外的 `StoreType`、`UseDynamicPartitionPrune` 分区表）显式返回 `blocked`，不留默认空分支。
3. **重点防回归项（对照 #338 的三个根源）**：
   - **列下标与 `OutputOffsets`**：核对 `TableReader > TableRangeScan / TableFullScan`（以及下推了 `Selection` / `TopN` / `Limit`、或上层套 `Projection` / `Selection` / `Sort` / `TopN` / `Limit`）时，`PhysicalTableScan.Columns`、`PhysicalTableReader.Schema()`、`dagReq.OutputOffsets` 和上层算子 `ResolveIndices` 后的列下标是否逐列对齐（特别是只选非主键列 `SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?`）；
   - **预处理语句与实例计划缓存**：`SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?` 连续用不同参数 `(1, 10)`、`(-100, 50)`、`(i64::MIN, i64::MAX)`、`(5, 104)`、`LIMIT ?` 执行时，验证参数标记序号、计划缓存命中（`found_in_plan_cache`）与 range 重建结果，以及 `PREPARE` 与每次 `EXECUTE` 的列定义（名字、类型、flen、decimal、flag、charset）与 Go v8.5.8 完全一致；
   - **会话状态与计数器**：回退前后 `SessionVars` 快照一致，被回退的语句不计入 `executed_stmts`，正常执行完 `Close` 后会话 `is_idle()` 为真。

---

## 12. 测试计划

### 12.1 Go 单元测试移植

| Go 测试 | 位置 | 所属子部分 | 移植方式 |
|---|---|---|---|
| `TestTableReaderRequiredRows` | `table_readers_required_rows_test.go:172` | **2A** | 不依赖存储（使用 `selectResultHook` + `requiredRowsSelectResult`），直接移植为 Rust 单测，验证 `RequiredRows` 透传 |
| `TestIndexReaderRequiredRows` | `table_readers_required_rows_test.go:225` | **2B** | 不依赖存储，直接移植 |
| `TestGetLackHandles` | `distsql_test.go:136` | **2C** | 纯函数单测，直接移植 |
| `TestIndexLookUpStats` | `distsql_test.go:250` | **2C** | 纯结构 `String` / `Clone` / `Merge` 单测，直接移植 |
| `TestInconsistentIndex`、`TestPartitionTableRandomlyIndexLookUpReader`、`TestPartitionTableIndexJoinIndexLookUp`、`TestCoprocessorPagingSize`、`TestAdaptiveClosestRead`、`TestCoprocessorPagingReqKeyRangeSorted`、`TestCoprocessorBatchByStore`、`TestIndexLookUpPushDownCopTask`、`TestPartitionIndexLookUpMergeWithSkewedPartitions`、`TestDistsqlPartitionTableConcurrency`、`TestDistSQLSharedKVRequestRace` | `distsql_test.go`、`test/distsqltest/distsql_test.go` | 2A / 2B / 2C | 依赖 testkit / mocktikv；其中不依赖真实存储的核心逻辑（`TestInconsistentIndex` 8133 报错路径、`TestAdaptiveClosestRead` 阈值切换、`TestIndexLookUpPushDownCopTask` 双通道分流、`TestDistsqlPartitionTableConcurrency` 并发度计算）改写为 mock `SelectResult` 执行器单测；完整 SQL 场景在合并后经 `tools/ab` 对照 |

### 12.2 govec 向量

新增 govec 领域 `readexec`（tracker `govec/domains/readexec.go`），在 amd64（tracker CI）和 arm64 双目标核对，回放中出现 `blocked` 的条数固定（`EXPECTED_BLOCKED`）：

| op | 所属子部分 | 覆盖内容 | 比较项 |
|---|---|---|---|
| `table_reader` | **2A** | `TableReaderExecutor`（mock `SelectResult`）：普通 int handle、unsigned int handle 跨 `MaxInt64` 边界（正序 / 逆序 / 不保序，验证两次 `SelectResult` 请求的 `KeyRanges`、`Desc`、`KeepOrder` 与 `tableResultHandler` 衔接输出）、common handle、空范围、虚拟生成列求值（含依赖列顺序）、`RequiredRows` 变化、中途中止与 `Close`、错误传播 | 生成的 `kv.Request`（ranges、desc、keep_order、concurrency、paging）与每次 `Next` 的输出行和错误 |
| `closest_read_adjuster` | **2A** | `newClosestReadAdjuster`：`ReplicaReadLeader` / `ClosestAdaptive`、`netDataSize / copTaskCount` 边界（阈值上下、整数截断） | 返回布尔值、`MatchStoreLabels` 与改写后的 `ReplicaRead` |
| `index_reader` | **2B** | `IndexReaderExecutor`（mock `SelectResult`）：单区间与多区间、`needMergeSort` 开/关、common handle 与普通索引 `buildKeyRanges`、`OutputOffsets` | `kv.Request`、输出行、错误 |
| `index_lookup_helpers` | **2C** | `CalculateBatchSize`（含首次 `Next` `RequiredRows` 起步、非 paging 初始倍增、`indexPaging` 跳过初始倍增但保留每批倍增，以及 900 行 / `index_lookup_size=100` 下 `max_chunk_size=1024` vs `64` 的 `table_task num` 9 vs 8）、`getIndexScanMaxInFlight`、`getMergeSortIndexScanConcurrency`（含 `c = 1` 边界不 panic）、`GetLackHandles`、`buildIndexScanOutputOffsets`、`IndexLookUpRunTimeStats.{String,Clone,Merge}` | 精确返回值与文本 |
| `index_lookup` | **2C** | `IndexLookUpExecutor`（mock 索引侧与表侧 `SelectResult`）：`keepOrder=true/false`、`desc=true/false`、int handle 与 common handle（含 `CommonHandleVersion=0` 非二进制 collation 字符串 sortKey 处理）、`PushedLimit`（offset + count）、多批翻倍 `batchSize`、索引表数据不一致（8133 错误，含 `weakConsistency=true` 豁免）、`indexLookUpPushDown` 双通道分流 | `keepOrder=true` 逐行比序，`keepOrder=false` 按多重集比对，错误码与错误文本 |

### 12.3 Rust 专项单元测试（不依赖存储）

- **第 2A 部分**：
  - `TableReaderExecutor` 在未 `Close` 时直接 `Drop`（以及 `Open` 中 `secondResult` 失败、多区间第 k 个失败），断言所有底层 mock `SelectResult` 均已关闭/取消，无 task 泄漏；
  - `table_reader_schema`（已有单测保持通过）：缓存计划的空 schema 不被原地修改；
  - `TableReaderExecutor` + `Selection` / `Projection` / `Limit` / `Sort` / `TopN` 组合执行，以及单线程/多线程 tokio runtime 下的取消与 `KILL`；
  - `crates/session/src/tests/sbtest_results.rs` 更新：用支持内存 copr 的测试 store 跑通 `SELECT c FROM sbtest1 WHERE id BETWEEN ...`（文本协议 + 预处理 `BETWEEN ? AND ?` + 计划缓存命中/未命中 + `ORDER BY c` + `ORDER BY id DESC LIMIT 5`），逐列验证行和列元数据与 Go v8.5.8 一致；原回退断言改为测未移植算子（如 `SELECT COUNT(*)` 或 `IndexReader`）。
- **第 2C 部分**：
  - §10.5 表中每一处 drop guard / 清理顺序在正常、索引侧报错、表侧报错、worker panic、内存超限五种路径下的验证，`Close` 后 `tracker` 归零、全部 worker task 数为 0；
  - 读完前 `Close`、`Open` 后未 `Next` 即 `Close`、不 `Close` 直接 `Drop`、`Close` future 中途被 `Drop`；
  - 单线程 tokio runtime（NoSteal 拓扑）与多线程 tokio runtime（Steal 拓扑）下并发 `indexWorker` + `tableWorker` 无死锁。

### 12.4 端到端与真实 TiKV 验证（必须，#338 教训）

按 `docs/process/agent-workflow.md` 与 D12.2 §6 / §7 教训，**第 2A 部分合并前必须在真实 TiKV 集群（如 `tiup playground v8.5.8`）上运行 `tools/ab` 对照**，不能只靠 mock store 单测：

1. **ab 常规工作负载**：`--new-stack=fallback` 与 `--new-stack=strict` 跑完 `tools/ab/workloads/` 全部用例以及 `tools/ab/workloads/manual/copr.sql`（26 条跨 region 范围扫描、正逆序加 LIMIT、paging 128/129 边界、空范围、空表，文本 + 预处理各一遍），要求 0 mismatch；
2. **sysbench 范围查询专项**：`simple_ranges`（`WHERE id BETWEEN ? AND ?`）、`order_ranges`（`WHERE id BETWEEN ? AND ? ORDER BY c`）、全表扫描、无符号主键跨 `MaxInt64` 边界范围扫描、虚拟生成列查询，核对 `tidbrs_route_total{engine="new"}` 计数确实由新栈执行（没有悄悄回退到 PoC），且结果与列元数据与 Go v8.5.8 100% 一致；
3. **默认模式不退化**：`--new-stack=off` 下 ab 与 sysbench 基线不变（宪章第七条 4），单测验证 `off` 模式下不构建任何新栈读取执行器。

---

## 13. 与 Go 的已知差异

1. **空 schema 只读不回写计划**（`builder.go:3841-3843`，`` `50358` ``）：Go 原地写 `v.SetSchema(v.GetTablePlan().Schema())`；Rust 在实例计划缓存（A3 spec §2.3 row 9b）下通过 `table_reader_schema` 只读获取，不修改共享计划，用户不可见。
2. **执行器不持有会话，上下文在构建时冻结**（`table_reader.go:106-133`、`distsql.go:194-205,453-465`）：按 N2（`M-CTX`），执行器只持有 `Arc<DistSQLContext>` / `Arc<BuildPBContext>` / `Arc<RangerContext>` 与 `ExprContextRef`，同一条语句内会话变量不变，行为等价。
3. **集群内存表 `GetDDLOwner` 与 TiFlash / MPP 路径**（`table_reader.go:98-121,400-434,537-574,588-597`、`builder.go:3853-3863`）：TiFlash / BatchCop / MPP 属 `DEF.MPP` 永久报错；cluster 表 `DDLOwner` 停在 `blocked-by:D06.5`。
4. **`TableReaderExecutor.Open` 与 `IndexReaderExecutor.open` 中途出错时关闭已建 `SelectResult`**（`table_reader.go:333-348,413-420,450-457`、`distsql.go:428-435`）：Rust 经 `Drop` 自动关闭并中止底层 worker，避免 Go 的资源泄漏（§14 第 1 条），错误返回值与 Go 相同。
5. **`IndexLookUpExecutor` 的 `lookupTableTask` 所有权与 `oneshot` 通道**（`distsql.go:81-117,742,1300-1324,1850,1898`）：Rust 用 `oneshot` 将已完成结果交回主线程、`rows` 用 `Vec<Arc<Chunk>> + Vec<(usize, usize)>`（`M-ROW`）、`workCh` 多消费者用 `Arc<tokio::sync::Mutex<mpsc::Receiver>>`（`L2 [M-CH]`），不改变批次顺序和输出行。
6. **`IndexLookUpExecutor` `Drop` 安全网与 `Close` 等全部 task**（`distsql.go:1225-1253`）：语句取消或未 `Close` 被丢弃时 `Drop` 通知 `finished` 并中止全部 worker，不会遗留后台 task；按 `crates/kv/src/context.rs` 通过 `select!` 丢弃在途 `SelectResult` future 取消请求，不引入 `CancellationToken`。
7. **内存记账数值与超限走错误而非 panic（用户可见，依据已确认的 A1）**（`table_reader.go:214-226`、`distsql.go:1320-1322,2175-2236`）：按已确认的 A1 例外用 `TrackedAlloc` 记实际分配（会影响 `tidb_mem_quota_query` 触发点、`EXPLAIN ANALYZE` 内存列与慢日志 `Mem_max`），正向 `Consume` 改为 `checkpoint()?` 返回错误，worker 内按与 Go 相同的错误通道返回；`catch_unwind` 恢复日志不含 Go 堆栈（与 D12.2 / D12.3 相同）。表侧回表 reader（`buildTableReaderBase`）照 Go 不设 tracker。
8. **内存复用池删除与新增 `M-KILL` 检查点**（`distsql.go:1575,1779-1795,2224-2250`）：`AllocPool.Alloc` 直接分配（`M-POOL`）；`indexWorker` / `tableWorker` 内层循环加 `CheckpointStride` 可能比 Go 更早响应 KILL（N3 第二部分第 5 条）。
9. **一致性报错无 MVCC 调试 dump**（`distsql.go:2040-2068,2252-2267`）：沿用 `crates/executor/src/consistency.rs`（第 1 部分已确认），错误码 8133 和消息文本与 Go 相同，日志不含 TiKV debug MVCC dump。
10. **failpoint、Go runtime trace 与未移植的 D14.1 执行器指标**（`table_reader.go:230-235`、`distsql.go:856,1164,1210-1222,1615-1628,2216`、`builder.go:3830-3839,4379`）：failpoint 与 trace 不移植；D14.1 执行器指标缺失对运维可观测性可见，属 D14.1 临时缺口（待 D14.1 统一接入）。
11. **`tableResultHandler.nextRaw` 的上下文取消原因归一化**（`table_reader.go:686-702`、`analyze_utils.go:112`）：在 `M-GOCTX` 下保留 `next_raw` 方法并归一化取消错误（第 2A 部分常规 SQL 只走 `next_chunk`，`next_raw` 供后续 `ANALYZE` / `CHECKSUM` 调用）。
12. **`GetSelectResultConcurrency` 对 2C 限流无差异**：按维护者决定，D07.3（#410）已将 `distsql::select*` 改为 `async fn` 并即时构造 `CopIterator`，因此 2C `fetchHandlesRolling` 在首次 `Next` 之前读到的并发度与 Go 100% 一致，不改变发往 TiKV 的请求数量与时机。

---

## 14. 读代码发现并在 Go v8.5.8 上核验的 Go 问题

以下 7 条均已对照 Go TiDB `v8.5.8`（`8b857efa2`，unistore）核验机制与可达性：

1. **`TableReaderExecutor.Open`、`buildRespForGroupedRanges` 与 `IndexReaderExecutor.open` 中途出错时泄漏已打开的 `SelectResult`（生产不可达）**：
   - **位置**：`table_reader.go:333-348`、`413-420`、`450-457`；`distsql.go:428-435`。
   - **核验结果**：`CopClient.Send`（`copr/coprocessor.go:87`）从不返回错误，`distsql.Select` 仅在响应为 `nil` 时报错，因此**只能经测试钩子注入，生产 SQL 不可达**。Rust 经 `Drop` 自动关闭，无泄漏，不开 `upstream-pitfall`。

2. **`IndexLookUpExecutor.startWorkers` 失败时泄漏 `cancelFunc`（不可达）**：
   - **位置**：`distsql.go:737-749`、`1236-1244`。
   - **核验结果**：`startIndexWorker` 中唯一能返回错误的分支 `:846` 被 `intest.Assert(false)` 守卫，**不可达**；Rust 中使用 `watch` + `Drop` 也不会泄漏，不开 `upstream-pitfall`。

3. **`IndexLookUpExecutor.startIndexWorker` 在 `len(kvRanges) == 1` 时清空 `e.byItems` 并重切片 `e.dagPB.OutputOffsets`（机制成立但不可达）**：
   - **位置**：`distsql.go:829-832`。
   - **核验结果**：代码机制确实会原地改写执行器字段，但在 v8.5.8 SQL 上**不可达**——相关列的 `IN` / `OR` 不会成为索引访问条件（`order_index` + 相关 `=` + `b IN (1,2)` 报 1815），且分区数在构建阶段固定，各次 `Open` 的 `len(kvRanges)` 不会在 1 与 >1 之间切换。Rust 在 worker 局部克隆 `dag_pb` 与 `by_items`（§10.4），无此隐患，不开 `upstream-pitfall`。

4. **`buildNoRangeIndexLookUpReader` 把 `idxNetDataSize` 误设为单行平均大小 `GetAvgTableRowSize()`（已确认成立，照 Go 移植）**：
   - **位置**：`builder.go:4335`、`distsql.go:1086`（对比同文件旁已有未被调用的 `PhysicalIndexLookUpReader.GetIndexNetDataSize()`，`pkg/planner/core/physical_plans.go:562`，D10.1）。
   - **机制**：`TableReader`（`builder.go:3708`）和 `IndexReader`（`builder.go:4073`）传给 `newClosestReadAdjuster` 的都是估算总传输量 `v.GetNetDataSize()`，而 `buildNoRangeIndexLookUpReader` 在 `:4335` 把 `idxNetDataSize` 误设成了单行表宽 `v.GetAvgTableRowSize()`（未乘行数，且不用现成的 `GetIndexNetDataSize()`），导致 `distsql.go:1086` 传入 `e.idxNetDataSize / float64(totalRanges)` 时远小于 `tidb_adaptive_closest_read_threshold`（默认 **4096** 字节）。
   - **处理**：只影响副本路由阈值、不影响结果正确性，第 2C 部分照 Go 移植并开 `upstream-pitfall`（`go-bug-kept`）。

5. **`IndexLookUpRunTimeStats.String` 在前两项为 0 时遗漏 `next` 等待统计（机制成立，实际不可达）**：
   - **位置**：`distsql.go:1990-1998`。
   - **核验结果**：`fmt.Fprintf(&buf, " next: ...")` 确实错写在 `if buf.Len() > 0` 内，但实际运行中只要存在任务必有 `FetchHandle > 0`，**生产不可达**；第 2C 部分照 Go 移植（单测可覆盖）。

6. **`indexLookUpExecutorContext.Detach` 返回未分离的旧值 `iluCtx`（已确认成立，属 D12.0，游标路径可达）**：
   - **位置**：`pkg/executor/detach.go:83-88`。
   - **核验结果**：第 85 行改了 `newCtx.tableReaderExecutorContext`，但第 87 行写成 `return iluCtx` 而非 `return newCtx`。已在 Go v8.5.8 unistore 实测复现：开启 `tidb_enable_lazy_cursor_fetch = 'ON'`、打开 `CURSOR_TYPE_READ_ONLY` 游标对 4,000 行 `IndexLookUp` 含虚拟列 `HOUR(FROM_UNIXTIME(...))` 先在 `+00:00` 下 fetch 10 行（得 `22`），随后同会话切 `SET SESSION time_zone = '+05:00'` 并继续 fetch 剩余 3,990 行，结果混杂两套时区值（`{22: 118, 3: 3872}`）。在 N2 下 Rust 执行器不持有会话（上下文天然是静态快照），游标支持（D12.12）时自然不存在该竞争；由第 2C 部分开 `upstream-pitfall` 记录。

7. **`tableWorker.pickAndExecTask` 的 recover 闭包未判 `task != nil`（不可达）**：
   - **位置**：`distsql.go:1873-1878`。
   - **核验结果**：panic 仅可能发生在 `executeTask(ctx, task)` 内（此时 `task != nil`），**不可达**；Rust 经 `oneshot` 拥有 `Option<done_tx>` 天然安全，不开 `upstream-pitfall`。

---

## 15. 决定点（按宪章第九条分类）

### 15.1 需要负责人决定（宪章第九条 1）

1. **是否将 `EXC.B`（cost model v1）中的 3 个 `GetNetDataSize` 辅助函数移出例外并移植（宪章第九条 1(b)）**
   - **背景**：`buildNoRangeTableReader`（`builder.go:3708`，第 2A 部分）和 `buildNoRangeIndexReader`（`builder.go:4073`，第 2B 部分）在 cost model v2 下也**无条件调用** `v.GetNetDataSize()`，以向 `newClosestReadAdjuster` 传入估算传输字节数（决定 `tidb_replica_read = 'closest-adaptive'` 是否路由至同可用区副本）。但这 3 个函数——`getTblStats`（`plan_cost_ver1.go:1284`，9 行）、`PhysicalTableReader.GetNetDataSize`（`:327`，3 行）、`PhysicalIndexReader.GetNetDataSize`（`:243`，3 行）——在 tracker `data/func_blocks.tsv` 中被整体划归 **`EXC.B`**（cost model v1，已确认不移植）。直接移植会改动已确认例外 `EXC.B` 的范围，按宪章第九条 1(b) 需负责人确认。
   - **选项**：
     - **A（建议）**：将这 3 个共 15 行的函数从 `EXC.B` 改归 `D10.1`（与同用途的 `PhysicalIndexLookUpReader.GetIndexNetDataSize`，`physical_plans.go:562`，归 `D10.1` 一致），随第 2A / 2B 部分按 L1 移植到 `tidb-planner`（其依赖 `cardinality::get_avg_row_size` 与 `tbl_col_hists` 均已就绪；因 `get_avg_row_size` 仅从 `&PlanCtx` 读 `enable_chunk_rpc`，补一个接 `&SessionVars` / `enable_chunk_rpc: bool` 的小辅助即可）。
     - **B**：维持 3 个函数留在 `EXC.B` 不移植；在 `buildNoRangeTableReader` / `buildNoRangeIndexReader` 中仅当 `tidb_replica_read = 'closest-adaptive'` 时返回 `blocked("EXC.B", ...)`，其余副本读模式置 `net_data_size = 0.0`（注：当前 D07.2 `client-rust` 只支持读 leader，`closest-adaptive` 在发送阶段本就报错，D07.2 §6.3 第 1 条）。
   - **建议与理由**：选 **A**。这 3 个函数只是碰巧定义在 `plan_cost_ver1.go` 里，实质是执行器构建的现役依赖（与 cost model v1 的代价公式无关），仅 15 行且依赖已全部就绪；归回 `D10.1` 按 L1 移植最干净，待未来 `client-rust` 支持 replica read 时无需再返工。

其余各项（内存记账 A1、并发与取消 N3/A2、计划只读 A3、共享元数据 N13、9 个 `X`/`DEF.MPP` 函数）均遵循已确认基础设计或既有裁决，不新增需要负责人决定的点。

### 15.2 维护者已定（宪章第九条 2）

1. **第 2 部分拆分与实现顺序**：按依赖拆为 2A（TableReader / VS1-34，28 个函数，同步更新 tracker `data/part_funcs.tsv` 的 `VS1-34`）、2B（IndexReader，14 个函数）、2C（IndexLookUp，60 个函数）三个独立实现任务；2A 排在 D07.3（#410）第 1 部分合入之后立即推进，优先打通 sysbench `simple_ranges` / `order_ranges`；2B / 2C 排在 D10.1 / D10.2 / D10.9 索引算子与 D07.3 第 2 部分合入之后。
2. **D12.1 其余函数归属**：95 个剩余函数分为 3A（BatchPointGet，6）、3B（分区裁剪与 IndexJoin 辅助，6）、3C（缓存表与 UnionScan / MemReader，52，保留供 D06.4 第 2 部分而非归 `X`）、3D（TABLESAMPLE，17）、3E（集群内存表 Coprocessor 与 Join 辅助，14）；8 个写路径/DDL 运维/测试钩子归 `X`，1 个 TiFlash BatchCop 专属函数归 `DEF.MPP`。245 个函数 100% 对账。
3. **与 D07.3（#410）的边界与桩归属**：`verify_txn_scope` 桩由 D07.3 第 1 部分解除；`new_closest_read_adjuster` 桩由本块 2A 解除（D07.3 只提供 `match_store_labels`）；`copr_request_rate_limit` 字段与 `CopIterator` 接线归 D07.3 第 2 部分；`distsql::select*` 改为 `async fn` 即时构造 `CopIterator`，消除 2C `GetSelectResultConcurrency` 差异；执行器从 `dctx` 构造 `tidb_kv::Context`。
4. **随第 2A / 2C 部分跨块顺带移植/解除桩的小函数**：2A 解除 D12.0 `builder_utils.rs`（`ConstructDAGReq`、`to_pb`）和 `detach.rs`（`TableReaderExecutor.Detach`）的桩，顺带移植 D12.0 `assertByItemsAreColumns` / `UpdateExecutorTableID`、D12.10 `corColInDistPlan` / `corColInAccess`、D12.14 `normalizeCtxErrWithCause`、D00.3 `timeutil.Zone`（`time_zone.go:189`）；2C 顺带移植 D12.7 `dataReaderBuilder.{buildTableReaderBase, buildTableReaderFromHandles}`（`getTblStats` / `GetNetDataSize` 见 §15.1 待负责人定）。
5. **边界桩放置层级（防止执行后无法回退）**：所有 2A 尚未支持的路径（分区表、`TiFlash`/`MPP`、相关列访问条件、多组 `groupedRanges`）统一在 `executorBuilder.buildTableReader`（任何执行器 `Open` 之前）返回 `blocked-by:<块>`，确保路由器可安全回退到 PoC。
6. **并发与取消内部设计**：`IndexLookUpExecutor` 使用 `Arc<IndexLookUpShared>` + `workCh`（容量 1 `mpsc` + `Arc<tokio::sync::Mutex<Receiver>>`）+ `resultCh`（容量 50 `mpsc`）+ `oneshot` 交回 `LookupTableTaskResult` + `M-ROW`（`Vec<Arc<Chunk>> + Vec<(usize, usize)>`）+ `watch` + `tokio::select!` 丢弃在途 future 取消 + `Drop` 中止全部 worker（不引入 `CancellationToken`），`Close` 顺序严格按 Go（index usage → 通知退出与排空等待 → 注册 stats）。
7. **测试、govec 与合并前真实 TiKV 门禁**：新建 govec 领域 `readexec`（含批大小与 `c = 1` 并发度边界）；2A 合并前必须由维护者在真实 TiKV 集群跑 `tools/ab`（含 `copr.sql` 与 sysbench 范围查询）并验证 `sbtest_results.rs` 逐列元数据，防止重演 #338 回归。
8. **§14 已核验的 Go 问题**：仅第 4、6 条在第 2C 部分开 `upstream-pitfall`（第 4 条 `go-bug-kept`，第 6 条属 D12.0）；第 1、2、3、5、7 条均已确认生产不可达，不开 issue。

---

## 本文所用 ID

- A1：统一内存记账与配额设计（`TrackedAlloc`、tracker 树、动作链），并入 N3 spec
- A2：执行模型（保留 Go 算子 worker 结构，映射到固定大小任务池），并入 N3 spec
- A3：缓存类基础设计（实例级 plan cache、InfoSchema v2 缓存、coprocessor cache）
- D00.1：上下文接口骨架（`sessionctx`、`exprctx`、`errctx`、`planctx` 等）
- D00.3：`util/*` 叶子工具文件（`timeutil`、`consistency`、`channel` 等）
- D01.7：Chunk 与列存
- D01.8：基础键值编码（`util/codec`）
- D01.11：表/索引/行编解码（`tablecodec`、`rowcodec`）
- D03.1 / D03.2：MySQL 协议服务与内部 gRPC 服务
- D05.1 / D05.2 / D05.3：系统变量、SessionVars/StmtCtx、会话主流程
- D06.3 / D06.4 / D06.5 / D06.6：InfoSchema、table 抽象、schema 同步与 domain、内存表
- D07.0 / D07.1 / D07.2 / D07.3：kv 接口、store driver、coprocessor 客户端、distsql 请求构建与结果
- D09.12：分区表规划与动态分区裁剪
- D10.1 / D10.2 / D10.5 / D10.6 / D10.8 / D10.9：物理算子、FindBestTask、代价模型、基数估算、点查快速路径、计划后处理（`ResolveIndices` / `ToPB`）
- D11.2：索引使用统计（`indexusage`）
- D12.0：执行器框架与构建骨架（`ExecStmt`、`executorBuilder`、`detach`、`internal/builder`）
- D12.1：本块，读取类执行器（PointGet、TableReader、IndexReader、IndexLookUp、BatchPointGet、UnionScan、MemReader、TableSample、CoprocessorDAGHandler）
- D12.2 / D12.3 / D12.5 / D12.6 / D12.7 / D12.8 / D12.10 / D12.11 / D12.12 / D12.13 / D12.14：基础算子、聚合、排序、HashJoin、IndexJoin、MergeJoin、Apply/CTE、IndexMerge、简单/预处理/游标执行器、内存表执行器、运维与分析执行器
- D14.0 / D14.1 / D14.2：执行统计结构、metrics、Top SQL / 慢日志
- D15.1：内存统计与配额实现
- DEF.MPP：TiFlash / BatchCop / MPP（不移植）
- EXC.B：已批准例外块，旧代价模型 v1（`cost_model_version = 1` 不支持）
- L1 / L2：来源标注等级（L1 按映射规则忠实移植；L2 有理由的 Rust 化改写）
- M-BYTES、M-CH、M-CTX、M-DEFER、M-FMA、M-FT、M-GO、M-GOCTX、M-INT、M-KILL、M-MAPORDER、M-MEM、M-PANIC、M-POOL、M-ROW、M-WG：`docs/porting/mappings.md` 中的全局映射规则
- N0 / N1 / N2 / N3 / N5a / N13：基础设计 spec（crate 分层、计划表示、会话与语句上下文、并发取消与内存、Chunk 表示、共享元数据 `Arc` 与写时复制）
- P4：路线图阶段编号（后续完整覆盖阶段）
- VS1-11 / VS1-12 / VS1-13 / VS1-14 / VS1-18 / VS1-20 / VS1-33 / VS1-34 / VS1-35：第一条纵向切片（VS1）的各工作项编号
- X：不在 tidb-rs 范围内的代码（写路径、加锁、DDL 运维、测试钩子等）
- PoC：原型实现（新栈路由器回退的目标）
