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

## 5. 验收（第 1 部分）

- converge：`matched 41/116`，其余 75 个在 tracker govec `copr` 领域的豁免表中（72 个 "D07.2 part 2"，3 个永久：DEF.MPP 两个、D06.5 边界桩一个）。
- govec `copr`（tracker `govec/domains/copr.go`）：2,621 条记录，抄自 14 个 Go 测试（key_ranges_test.go、coprocessor_test.go 中不依赖 iterator 的全部测试）加随机用例。`split` / `build` 在 mocktikv 上运行，记录 region 布局，Rust 回放按布局实现 client-go `BatchLocateKeyRanges` 的结果（`tests/govec_copr.rs` 的 `LayoutCache`）。arm64 本地生成，amd64 取自 tracker CI。
- 反证：7 处单行改动（small task 的 `<=`、小 LIMIT 关 paging 的 `<`、`Split` 的 `>`、row hint 的回退一步、bucket 的结束 key 相等判断、合并 task 的 busy threshold、`GrowPagingSize` 的下限）都被回放发现。
- mocktikv 的限制：多个区间中含开放结束 key 时，批量 region 扫描返回 "PD server timeout"，随机用例只在单个区间时用开放结束 key。
