# D07.2 coprocessor 客户端（不含 batch/MPP）：块 spec

状态：第 1 部分（2026-11-14）。阶段：P2。基线：Go TiDB v8.5.8（`8b857efa2`），client-go `c3b5079`。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[ADR 0008](../../docs/adr/0008-client-rust-replaces-client-go.md)（client-rust 替代 client-go，L3）、[client-go 对等对照表](../D07.1-store/client-go-parity.md)（§6 paging 默认开）、[N3+A1](../exceptions/N3-A1-concurrency-memory/spec.md)、[N0](../exceptions/N0-crate-layering/spec.md)。crate `tidb-store`（N0 rank 11，D07.1–D07.4 共用），模块 `copr`。

## 1. Go 范围

tracker 中 D07.2 有 116 个函数，在 `pkg/store/copr` 的 5 个文件：

| Go 文件 | 函数数 | 内容 |
|---|--:|---|
| coprocessor.go | 81 | 建任务、iterator、worker、响应处理、paging、rate limit action |
| key_ranges.go | 10 | `KeyRanges` |
| region_cache.go | 9 | 按 region / bucket 切分、store batch 的 `BuildBatchTask` |
| store.go | 14 | `Store` / `kvStore` / `tikvClient` 包装 |
| metrics/metrics.go | 2 | copr 的指标 |

不在本块：`batch_coprocessor.go`、`batch_request_sender.go`、`mpp*.go`（TiFlash，DEF.MPP），`coprocessor_cache.go`（D07.4，例外 A3）。本块中只服务于它们的函数：

- `RegionCache.OnSendFailForBatchRegions`：只处理 TiFlash store（TiKV store 直接打日志返回），唯一调用方是 `batch_coprocessor.go`。豁免（DEF.MPP）。
- `Store.GetMPPClient`：MPP。豁免（DEF.MPP）。

**store batch 不是 batch cop**：`batchStoreTaskBuilder`（`tidb_store_batch_size`，默认 4）把同一 TiKV store 的小任务合进一个请求，属于 TiKV 路径，照 Go 移植。它只在不开 paging、不保序、读 leader、非内部请求时生效（`checkStoreBatchCopr`）。

client-go 不逐函数移植（ADR 0008）。copr 调用的 client-go 部分（`tikv.RegionCache` 的 `BatchLocateKeyRanges`、`GetTiKVRPCContext`，`tikv.KeyLocation`、`RegionVerID`、`Bucket`）经 `copr::tikv` 边界：值类型和它们的小方法（`KeyLocation.Contains`、`LocateBucket` 等）照 client-go `c3b5079` 写，标 `go-l3: ADR-0008`；region cache 是 trait `TiKVRegionCache`，由 client-rust 适配层（第 2 部分）和测试实现。

## 2. 分部分交付

| 部分 | 内容 | 函数 |
|---|---|---|
| 1（本 PR） | `KeyRanges`；按 region / bucket 切分；`buildCopTasks` 与两种 task builder（含 store batch 的 `BuildBatchTask`）；small task 并发；paging 的 `calculateRemain` / `calculateRetry`；`priorityToPB`、`isolationLevelToPB`、`optRowHint`、`checkStoreBatchCopr`、`BuildKeyRanges`；`copTask.String` / `ToPBBatchTasks`；`pkg/util/paging`（D00.3，整文件） | 41 |
| 2 | `CopClient.Send` / `BuildCopIterator`、`copIterator`（open、Next、Close、lite worker）、worker 与 task sender、`handleTaskOnce`（经 client-rust 发送单个 region 的 cop 请求，即 client-go `SendReqCtx`）、`handleCopResponse` / `handleCopPagingResult` / `handleBatchCopResponse` / `handleLockErr`、运行时统计、`rateLimitAction`、`copResponse`、`store.go`、`metrics.go` | 72 |

第 2 部分需要 client-rust 提供的接口见 tasks.md T6。

## 3. 设计决定

### 3.1 `TiKVRegionCache` 边界

- `RegionCache<C: TiKVRegionCache>` 对应 Go 的 `RegionCache{*tikv.RegionCache}`；copr 的函数对 `C` 泛型。
- `C::Backoffer` 是 client-go `*retry.Backoffer` 的位置。TiDB 的 `backoff.Backoffer`（`pkg/store/driver/backoff`）属于 D07.1，第 1 部分只把它原样传给 region cache，所以不需要它的方法。
- trait 方法返回的已经是 TiDB 错误：Go 每次调用都包 `derr.ToTiDBErr`，对非 client-go 错误它是恒等的；转换在适配层做（`pkg/store/driver/error`，D07.1）。
- `GetTiKVRPCContext` 的 replica read 参数用 TiDB 的 `kv.ReplicaReadType`；Go 先经 `options.GetTiKVReplicaReadType`（D07.1）转换，适配层负责。`BuildBatchTask` 只在 leader 读时调用它。
- 可能做 I/O 的函数（切分、建任务）是异步的（N3 3.1 第 6 条）；trait 方法返回 `impl Future + Send`。

### 3.2 `KeyRanges`

Go 的 `mid` 是调用方切片的子切片，`Slice` 只重新切片不复制。移植用 `Arc<[KeyRange]>` 加偏移和长度，同样不复制；元素在构造后不再修改（Go 也不改）。`RefAt` 越过 `mid` 时 Go 返回 `last`（可能是 nil，解引用 panic），移植同样 panic。`ToPBRanges` Go 用 `unsafe.Pointer` 把 `kv.KeyRange` 当作 `coprocessor.KeyRange`，移植复制字节进 prost 消息（L2，字节相同）。

### 3.3 task builder 持有 task 的方式

Go 的 `taskBuilder.handle(task)` 之后，调用方仍通过指针修改这个 task（小 LIMIT 时关 paging）；`BuildBatchTask` 返回的 `batchedCopTask.task` 与传入的是同一个指针。移植中 builder 拥有 task：`handle` 返回 task 的位置（`Handled`），`buildCopTasks` 通过位置修改；`BuildBatchTask` 按值取 task，不合并时原样交还（`Result<BatchedCopTask, CopTask>`）。这几处标 L2。Go `batchStoreTaskBuilder.handle` 的 defer 兜底改为显式的提前返回。

`batchTaskList` 是 Go 的 map，`ToPBBatchTasks` 按 map 的随机顺序生成 `StoreBatchTask`。移植用按 task id 排序的 `BTreeMap`（M-MAPORDER：TiKV 按 task id 应答，顺序不影响结果）。

`batchStoreTaskBuilder` 在 Go 中保存创建时的 `*Backoffer`；移植中由 `handle` 的参数传入（调用方持有可变借用）。

### 3.4 `copTask.respChan`

Go `make(chan *copResponse, 2 或 18)`；移植用容量相同的 `tokio::sync::mpsc`（`RespChan`，worker 持 `tx`，iterator 取走 `rx`）。第 2 部分接入 N3 的取消安全收发（M-CH）。`copResponse` 在第 1 部分只有字段，访问器和内存记账在第 2 部分。

## 4. 已知差异与边界桩

1. **`buildTiDBMemCopTasks`**（`kv.TiDB` 的请求，读集群中各 TiDB 的内存表）：需要 `infosync.GetAllServerInfo`（D06.5）。边界桩 `blocked-by:D06.5`：返回错误，不静默返回空结果（宪章第五条）。单元测试 `tidb_store_type_is_a_boundary_stub`。D06.5 完成时去掉，登记在 D06.5 的 tasks.md。
2. **指标**：`metrics.TxnRegionsNumHistogramWithCoprocessor`（client-go 的 `tikv_client_go_txn_regions_num{type="coprocessor"}`）在 client-rust 中没有对应项，第 1 部分不记录（N9）。第 2 部分随 copr 自己的指标（`metrics.go`）一起处理。
3. **failpoint**：`setRangesPerTask`（测试用）改为 `BuildCopTaskOpt.ranges_per_task_limit` 字段，默认 `rangesPerTask`；`disableFixedRowCountHint`、`DisablePaging`、`checkKeyRangeSortedForPaging` 不移植。
4. **`eventCb`**（`trxevents`，锁事件回调）：只在 worker 的锁处理中使用，随第 2 部分加入 `copTask` 和 `buildCopTaskOpt`。

5. **上游的坑**（照搬，未向上游报告）：`isSmallTask` 中 `&&` 优先于 `||`，带 batched task 的任务在 row count hint ≤ 64（含 0 和 -1）时都算小任务；`KeyRanges.RefAt` 越过 `mid` 时返回可能为 nil 的 `last`。见 #216。

## 5. 验收（第 1 部分）

- converge：`matched 41/116`，其余 75 个在 tracker govec `copr` 领域的豁免表中（72 个 "D07.2 part 2"，3 个永久：DEF.MPP 两个、D06.5 边界桩一个）。
- govec `copr`（tracker `govec/domains/copr.go`）：2,621 条记录，抄自 14 个 Go 测试（key_ranges_test.go、coprocessor_test.go 中不依赖 iterator 的全部测试）加随机用例。`split` / `build` 在 mocktikv 上运行，记录 region 布局，Rust 回放按布局实现 client-go `BatchLocateKeyRanges` 的结果（`tests/govec_copr.rs` 的 `LayoutCache`）。arm64 本地生成，amd64 取自 tracker CI（两个目标的记录相同）。
- 反证：7 处单行改动（small task 的 `<=`、小 LIMIT 关 paging 的 `<`、`Split` 的 `>`、row hint 的回退一步、bucket 的结束 key 相等判断、合并 task 的 busy threshold、`GrowPagingSize` 的下限）都被回放发现。
- mocktikv 的限制：多个区间中含开放结束 key 时，批量 region 扫描返回 "PD server timeout"，随机用例只在单个区间时用开放结束 key。

## 6. 第 2 部分：client、iterator、worker 与响应处理

依赖 client-rust#22（`tikv_client::CopSender`：单个 region 的 cop 请求，client-go `SendReqCtx` 的语义；`locate_key_ranges`、`rpc_context`、`resolve_lock`；`Backoff::backoff` 等）。

### 6.1 边界的扩展（`copr::tikv`）

| trait / 类型 | Go | client-rust 实现（`copr::client_rust`） |
|---|---|---|
| `Backoffer` | TiDB `backoff.Backoffer`（D07.1）：`Backoff(BoRegionMiss)`、`BackoffWithMaxSleepTxnLockFast`、`GetTotalSleep` | `ClientRustBackoffer`（client-rust 的时间预算 `Backoff`） |
| `ClientHelper` | client-go `txnsnapshot.ClientHelper`（每个 iterator 一个，带 resolved / committed 锁集合）：`SendReqCtx`、`ResolveLocksWithOpts` | `ClientRustHelper`（iterator 的 `start_ts` 上的 `CopSender`） |
| `TiKVStore` | `tikv.KVStore`：`NewClientHelper`、`NewBackofferWithVars`、`CheckVisibility` | `ClientRustStore`（`TransactionClient`） |
| `RateLimit`、`get_request_source`、`merge_from_time_detail`、`merge_from_scan_detail_v2`、`EndpointType`、`Variables` | client-go `util.RateLimit`、`RequestSource.GetRequestSource`、`TimeDetail.MergeFromTimeDetail`、`ScanDetail.MergeFromScanDetailV2`、`tikvrpc.EndpointType`、`kv.Variables` | 按 client-go 写（`go-l3: ADR-0008`） |

`handleLockErr` 的"解锁 + `BackoffWithMaxSleepTxnLockFast(msBeforeExpired)`"在 client-rust 中是一次调用（`resolve_lock_and_wait`）。

### 6.2 并发与通道（N3）

- worker 与 task sender 在 `Store` 的 ExecPool 上 spawn（M-GO）；没有 pool 时（测试）在当前 tokio runtime 上。`Close` 等待它们（M-WG）。
- `finishCh` 是 `watch` 通道；所有收发都与它 `select`（M-CH），与 Go 相同。`taskCh` 容量 1，多个 worker 共享接收端（`Mutex<Receiver>`）。
- 不保序时 Go 的 `respChan` 无缓冲；tokio 通道至少一个槽位（L2：worker 最多多领先一个响应，受 `sendRate` 约束不变）。`finCopResp` 是 `RespMsg::Fin`。
- `handleTask` 的 `recover` 是 `catch_unwind`，panic 变成错误响应。
- **取消**：N3 取消语句是丢弃 future，用 `?` 提前返回的执行器也不会调用 `Close`。所以 `CopIterator` 实现 `Drop`：关闭 `finishCh`、中止 worker（相当于 Go `Close` 中的 `rpcCancel.CancelAll()`）、关闭 OOM action；否则 worker 会在 region 退避、锁解析、paging 续读中一直访问 TiKV，直到退避预算用完（Lucky 的评审）。测试 `dropping_the_iterator_stops_the_workers`。
- `copIterator.Next` 不直接实现 `kv.Response`：`kv.Client.Send` 是同步接口，而建任务要定位 region（I/O），所以 `CopClient::send` 返回 `LazyCopResponse`，在第一次 `Next` 时建任务并 `open`。Go 中建任务的错误也是在第一次 `Next`（`copErrorResponse`）报出，行为相同（L2）。
- #123 的混合 Iterator（`advance` / `fill`）只针对 `kv.Iterator`；coprocessor 走 `kv.Response.Next`，每个响应一次 await，不受影响（D07.0 `iter-next-fill.md`）。

### 6.3 已知差异（第 2 部分）

1. **replica read 与 stale read**：client-rust 只读 leader（对照表 §3.1，RR-5）。`req.ReplicaRead` 非 leader 或 `IsStaleness` 时 `handleTaskOnce` 报错，不静默读 leader。`MatchStoreLabels`、`redirect2Replica` 随之不生效。
2. **region bucket**：client-rust 没有 bucket，位置不带 bucket，`UpdateBucketsIfNeeded` 为空操作（对照表 §2，P2）。
3. **store 的估计等待时间**恒为 0：store batch 不会因 leader 繁忙而退回（`BuildBatchTask` 的 busy 分支）。
4. **client-go 的请求统计**（`RegionRequestRuntimeStats`、backoff 次数）没有对应：`CopRuntimeStats` 只记录 TiKV 返回的时间 / 扫描明细与被调用地址（地址在 client-rust 中未知，为空）。
5. **`CheckVisibility`**：client-rust 没有 GC safe point 缓存，恒为可见。这是边界桩（`blocked-by`：client-rust 的 safe point），登记在 [client-go 对等对照表](../D07.1-store/client-go-parity.md) §4 的 `CheckVisibility` 一行（P1，正确性：低于 safe point 的读不报错，可能读到已被 GC 的数据）。
6. **错误转换**：D07.1 的 `derr.ToTiDBErr` 还没有，client-rust 的错误转成带原消息的普通错误。
7. **锁集合**：client-rust 的锁提示从空开始，不预先放入 `start_ts`（Go `resolvedLocks.Put(req.StartTs)`，只对 pipelined DML 写入的锁有意义，属于写路径）。
8. **未移植的 `kv.Request` 字段**（D07.0 spec §4 第 3 条）：`CoprRequestRateLimit`（`GetRequestRateLimit` 返回 None）、`ResourceGroupTagger`（N9，D07.0 T8）、`RunawayChecker`（N10）。`ResourceControlContext` 只填组名（N10 未定）。
9. **coprocessor cache**（D07.4，例外 A3）：`buildCacheKey` 不生成 key；TiKV 报 cache hit 时按 Go 返回 "Internal error: received illegal TiKV response"。
10. **`trxevents` 回调**未移植：`handleLockErr` 照 Go 无回调时的分支打 debug 日志。
11. **`rpcCancel.CancelAll()`**：client-rust 没有逐 RPC 的取消器；`Close` 后 worker 在下一次发送或检查 `finishCh` 时退出，进行中的 RPC 跑完。
12. **`regionErr.String()`**（gogo 文本格式）在退避错误消息中写成 prost 的 `Debug` 格式。
13. **`copResponse.MemSize`**：不缓存到 `respSize`，每次重算（值相同）；`pbResp.Size()` 是 prost 的 `encoded_len`。
14. **`OnSendFailForBatchRegions`、`GetMPPClient`**（DEF.MPP），**`kvStore.GetTiKVClient` 与 `tikvClient.*`**（client-go `tikv.Client` 的透传，读路径上没有调用方，ADR 0008），**`metrics.go`**（coprocessor cache 的指标，A3）：不移植。
15. **client-rust#22 的 `CopSender` 与 client-go `SendReqCtx` 的两处差异**：
    - (a) MaxTimestampNotSynced、ReadIndexNotReady、DataIsNotReady 等 region 错误：client-go 在调用内部按各自的退避（`BoMaxTsNotSynced`、`BoRegionScheduling`、stale read 的 DataIsNotReady 处理）重试；`CopSender` 把它们交回调用方，copr 按 `BoRegionMiss` 退避后重新拆分任务（多一次 region 定位，结果相同）。
    - (b) 设了读超时（`tikv_client_read_timeout`）时，第一次尝试的 gRPC DeadlineExceeded：client-go 不退避、立即重试（`onReadReqConfigurableTimeout`）；`CopSender` 按 `TikvRpc` 退避后重试（只对 TiKV 返回的 ServerIsBusy "deadline is exceeded" 立即重试）。

### 6.4 验收（第 2 部分）

- converge：`matched 104/116`；其余 12 个：6.3 第 14 条的 10 个（DEF.MPP 2、`tikv.Client` 透传 6、cache 指标 2），`buildTiDBMemCopTasks`（D06.5 边界桩，标注为 `glue`），以及 `coprocessor.go` 的 `init`（只做 `finCopResp = &copResponse{}`，移植中这个哨兵是 `RespMsg::Fin` 变体，没有全局对象）。VS1-33 的清单（`specs/VS1-vertical-slice/functions.tsv`）含其中 8 个（`buildTiDBMemCopTasks`、`init`、`kvStore.GetTiKVClient`、`tikvClient.*` 5 个），保留并在 `not_ported` 列写明理由；`converge.py --part … --item VS1-33` 对 D07.2 为 88/88。
- govec：第 2 部分的函数需要 TiKV store，不出向量；tracker 豁免理由指向 mock store 单元测试 `crates/store/tests/copr_iter.rs`（保序 / 逆序 / 不保序、region 错误重建任务、锁解析后重试、paging 的剩余区间与页大小翻倍、write conflict、other error、lite worker）。client-rust 侧 7 个单测（client-rust#22）。
- PoC 接入、`Snapshot::coprocessor` 退役、playground 上与 Go 的对比：见 §7（已完成）。

## 7. 收尾：PoC 接入与真实集群对比（D07.2 完成）

### 7.1 PoC 接入

- `poc-kv` 的 `KvClient` 持有一个 `copr::store::Store<ClientRustStore>`；服务进程用 `with_exec_pool` 让 iterator 的 worker 跑在 N3 ExecPool 上（M-GO）。`KvSnapshot::coprocessor` 改为 `CopClient.Send` 加逐个 `Next`（`kv.Response`），请求是 TiDB 的 `kv.Request`。
- `tidb-distsql`（PoC）的 `select` 按 Go `distsql.RequestBuilder` 给表读取填 `kv.Request`（D07.3 会正式移植 builder）：DAG 类型与数据、`Cacheable`、末尾 Limit 的 `LimitSize`（小 LIMIT 关 paging）、key ranges、desc、keep order、并发 15、`StoreBatchSize` 4。**paging 默认开**（`SelectOptions::default()`：128 → 50000，`tidb_enable_paging` 等的默认值，client-go 对照表 §6）。Go 的"简单扫描加小 LIMIT 时并发设为 1"（`SetDAGRequest`）留给 D07.3。
- client-rust#23 删除 `Transaction::coprocessor` / `Snapshot::coprocessor` 及其流水线；tidb-rs 中没有调用方。

### 7.2 与 Go TiDB 的对比（tiup playground v8.5.8，1 PD、1 TiKV）

准备（在 Go TiDB 上执行）：`copr.t(id BIGINT PRIMARY KEY, k INT, c VARCHAR(32))` 20000 行，`SPLIT TABLE t BETWEEN (0) AND (20000) REGIONS 8`（每 2500 个 handle 一个 region）；`copr.acct(id INT PRIMARY KEY, bal INT)` 2000 行，`SPLIT TABLE acct BY (500), (1000), (1500)`；空表 `copr.e`。

1. **结果对比**：`tools/ab/workloads/manual/copr.sql`（26 条：跨 region 的全表 / 范围扫描、region 边界两侧的范围、正序 / 逆序加 LIMIT、聚合与 GROUP BY、刚好 128 / 129 行的范围（paging 第一页的边界）、空结果、空表），文本协议与预处理语句各一遍：**52/52 一致**。tidb-rs 的 `tidb_distsql_handle_query_duration_seconds_count{copr_type="paging"}` 表明全部走 paging。
2. **锁与 region 分裂并发**（40 s）：4 个连接在 Go TiDB 上不断做乐观事务转账（`acct` 两行 ±1、`t` 两行 ±1，总和不变），另一个连接每秒在新的位置 `SPLIT TABLE`；同时 Go TiDB 一个、tidb-rs 两个连接循环读 `SUM(bal), COUNT(*)` 与 `COUNT(*), SUM(k)`。结果：Go 1082 次、tidb-rs 1354 + 1361 次，**全部等于不变量，0 次错误**。期间 tidb-rs 解锁 1177 次（其中 595 次查到已提交、559 次等待未过期的锁）、遇到 96 次 EpochNotMatch 并按 `BoRegionMiss` 退避 98 次后重建任务。
3. `tidb-distsql` 与 `poc-session` 的集群测试（`TIDB_RS_PD` / `TIDB_RS_TIDB_PORT`）对同一集群通过。

没有发现与 Go 不同的结果。未覆盖：TiKV 宕机与 leader 切换（`tools/ab/faults` 的故障场景，留给 D07.1 / 运维回归）、超过 4 MiB 的单个响应（P0-3 已在 client-rust 侧验证过）。

### 7.3 weekly（故障场景与 sysbench）

在本分支上手动运行 weekly（run 43574020336，commit `2f87985`），与当天 main 上一次 GREEN（run 43551977017，`c1bbf46`，旧的 client-rust cop 路径）对比：

- **整体 GREEN**：ab 0 mismatch（1515 match，与基线相同）；锁 18/18；故障场景 9 个全部 0 错结果、0 错误。
- **故障场景**（最长卡顿 Go / tidb-rs，基线在括号内）：kill_tikv 17.8 / 14.8 s（12.9 / 12.9），kill_tikv_meta 16.8 / 16.2 s（13.9 / 13.9），transfer_leader 0.0 / 0.0 s（0.1 / 0.0），slow_tikv（CPU 限制）3.1 / 2.9 s（5.2 / 5.6），net_cut 10.5 / 13.1 s（10.9 / 13.3），pause_tikv 5.0 / 5.0 s（同）。kill_tikv 两边同时变长，是本次运行的选举时间，不是 tidb-rs 侧的差异。
- **sysbench**（B/A QPS，基线在括号内）：oltp_read_only 1 线程 1.23（1.27），8 线程 1.20（1.27）；oltp_point_select 1.22（1.23）、1.10（1.14）。tidb-rs 的 read_only 8 线程 QPS 5979（6304，−5%）；不走 cop 的 point_select 也降了 3%，主要是运行器波动，read_only 多出的约 2 个百分点可能是新 cop 路径（每个请求的 worker spawn、paging 默认开）的开销，留待下周 weekly 确认。
- ab 的 both_error 从 18 变为 70：多出的 52 条是本 PR 一开始把 `copr.sql` 放进了 `tools/ab/workloads/`，而 weekly 只准备 sbtest 数据；已移到 `tools/ab/workloads/manual/`。
