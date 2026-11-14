# D07.2 coprocessor 客户端（tasks）

## 第 1 部分

- [x] T1 crate `tidb-store`（N0 rank 11，`layers.toml` 去掉 `planned`），模块 `copr`；client-rust 导出 kvproto 消息（client-rust#21，`tikv_client::proto`）
- [x] T2 `copr::tikv`：copr 调用的 client-go 部分（`KeyLocation`、`RegionVerID`、`Bucket`、`RPCContext` 子集，trait `TiKVRegionCache`），ADR 0008（spec §3.1）
- [x] T3 `key_ranges.go` 全文件（10 个函数）
- [x] T4 `region_cache.go`：除 `OnSendFailForBatchRegions`（DEF.MPP）外的 8 个函数
- [x] T5 `coprocessor.go` 的建任务部分（23 个函数）与 `pkg/util/paging`（D00.3，整文件）；govec `copr` 领域与回放、反证（spec §5）

## 第 2 部分

- [ ] T6 client-rust：把单个 region 的 cop 请求发送开放给 tidb-rs，即 client-go `ClientHelper.SendReqCtx` 的语义：按 `RegionVerID` 选 peer、RPC 错误与可重试的 region 错误在内部按 Backoffer 处理、不可重试的 region 错误（EpochNotMatch 等）连同响应返回给调用方；`BatchLocateKeyRanges`、`GetTiKVRPCContext`（store 的估计等待时间）、`UpdateBucketsIfNeeded`、`ResolveLocksWithOpts`、`CheckVisibility`，以及 TiDB 的 `backoff.Backoffer`（D07.1）所需的 `Backoff(cfg, err)`、`BackoffWithMaxSleepTxnLockFast`、`GetTotalSleep`。client-rust 现有的 `Snapshot::coprocessor`（自己的 task 拆分、paging、重试）由本块的移植取代
- [ ] T7 `TiKVRegionCache` 的 client-rust 实现（适配层），`derr.ToTiDBErr`、`options.GetTiKVReplicaReadType` 的接入（D07.1）
- [ ] T8 `coprocessor.go` 其余部分：`CopClient`（`Send`、`BuildCopIterator`）、`copIterator`（open、Next、Close、lite worker、运行时统计）、worker 与 task sender（N3：ExecPool 上的 task、取消安全的 channel，M-GO / M-CH）、`handleTaskOnce`（请求字段：`Context` 的隔离级别、priority、`RecordTimeStat`、`BusyThresholdMs`、`BucketsVersion`、`max_execution_duration_ms` / `tikv_client_read_timeout`）、响应处理（region 错误重建任务、锁、paging 的剩余区间、store batch 的应答）、`rateLimitAction`（A1）、`copResponse`；`eventCb`（trxevents，D00.3）
- [ ] T9 N9 / N10 钩子：`ResourceGroupTagger`（与 D07.0 T8 一起）、request source、`ResourceControlContext` 只填组名（N10 未定之前为桩）；copr 的指标（`metrics.go`）
- [ ] T10 `store.go` 的 `Store` / `kvStore` / `tikvClient` 包装（`coprCache` 为 D07.4，例外 A3）
- [ ] T11 paging 默认开（client-go-parity §6）：`tidb_enable_paging`、min 128 / max 50000、index lookup 与小 LIMIT 关闭由 D07.3 的请求构建与本块共同保证；PoC 的 coprocessor 读改走本块
- [ ] T12 集成测试：tiup playground v8.5.8 上与 Go TiDB 对比 cop 读（region 分裂、锁、paging）
