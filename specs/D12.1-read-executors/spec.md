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
