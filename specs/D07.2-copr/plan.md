# D07.2：plan

## 第 1 部分（本 PR）

1. client-rust#21：`tikv_client::proto` 导出 kvproto 的 prost 消息（`coprocessor.KeyRange`、`StoreBatchTask`、`RegionInfo`、`metapb.Peer` / `RegionEpoch` / `Buckets`、`kvrpcpb` 的枚举）。不另外构建 rust-protobuf 的 `kvproto`（它带 grpcio 的 C++ 核心）。tidb-rs 先钉在该 PR 的提交上，合并后改钉合并提交。
2. 新 crate `crates/store`（`tidb-store`），`layers.toml` 去掉 `planned`（rank 11）。依赖：`tidb-base`、`tidb-kv`、`tikv-client`（proto）、`tokio`（`sync`，respChan）。
3. `src/copr/tikv.rs`：client-go 边界（spec §3.1）。
4. `src/copr/key_ranges.rs`、`src/copr/region_cache.rs`：全文件按 Go 顺序（`OnSendFailForBatchRegions` 除外）。
5. `src/copr/coprocessor.rs`：常量、`copTask`、`batchedCopTask`、`buildCopTasks`、两种 task builder、`buildTiDBMemCopTasks`（桩）、`reverseTasks`、`isSmallTask`、`smallTaskConcurrency`、`calculateRetry` / `calculateRemain`、`priorityToPB`、`isolationLevelToPB`、`BuildKeyRanges`、`optRowHint`、`checkStoreBatchCopr`。
6. `src/util/paging.rs`：`pkg/util/paging/paging.go` 整文件（D00.3，宪章第四条 4）。
7. govec `copr` 领域（tracker 分支 `block/D07.2-copr`）；Rust 回放 `tests/govec_copr.rs`；CI 的测试列表加上本 crate。

回放：`cargo test -p tidb-store --test govec_copr`；额外的种子：`GOVEC_COPR_FILES=a.jsonl.gz,b.jsonl.gz`。

### 偏离（标注 L2 / glue 的位置）

| 位置 | 等级 | 理由 |
|---|---|---|
| `KeyRanges.ToPBRanges` | L2 | 复制字节进 prost 消息；Go 用 `unsafe.Pointer` 重新解释，字节相同 |
| `legacyTaskBuilder.handle`、`batchStoreTaskBuilder.handle`、`RegionCache.BuildBatchTask` | L2 | builder 拥有 task，`handle` 返回位置（spec §3.3）；`BuildBatchTask` 不合并时交还 task |
| `TaskBuilder` 枚举的分派方法、`with_task` | glue | `taskBuilder` 接口的两个实现（M-IFACE）；通过位置修改已交给 builder 的 task |
| `RespChan::new` | glue | `make(chan *copResponse, n)`（M-CH，spec §3.4） |
| `BuildCopTaskOpt::new` | glue | Go 复合字面量的零值；`ranges_per_task_limit` 是 failpoint `setRangesPerTask` 的位置 |
| `KeyRanges::from_parts`、`shape` | glue | Go 测试直接构造 / 读取未导出字段，回放用 |
| `build_tidb_mem_cop_tasks` | glue（桩） | `blocked-by:D06.5`（spec §4） |

### M-INT / 浮点

- `GrowPagingSize`：`size <<= 1` 在 Go 中丢掉最高位，写 `wrapping_shl`。
- `CalculateSeekCnt`：`expectCnt - pagingGrowingSum + MaxPagingSize - 1` 是 uint64 运算（前面已判断 `expectCnt > pagingGrowingSum`，不回绕，仍写 `wrapping_*`）；`math.Log` 用 `tidb_base::gostd::math::log`（Go 的实现）。
- `smallTaskConcurrency`：`1 + smallTaskSigma*math.Sqrt(...)` 在 arm64 上可能被融合为 FMA；`smallTaskSigma = 0.5`，乘积是精确的，融合与否结果相同，不需要按目标区分。`int(float64)` 的值在 `[0, res]` 内（M-F2I 无越界）。

### 反向引用（N0 §4）

`data/block_edges.tsv` 中指向 D07.2 的引用：DEF.MPP → D07.2（86，batch cop / MPP 用 `copTask`、`RegionCache`、`KeyRanges`，不移植）、D07.3 → D07.2（11，distsql 用 `CopRuntimeStats`、`copr.CopInfo` 等，第 2 部分提供）、D07.1 → D07.2（4，store driver 创建 `copr.Store`，第 2 部分）、D07.4 → D07.2（1）。都在 `tidb-store` 同一个 crate 内，没有跨 crate 的方向问题。
