# D14.0：plan

## 本 PR

1. crate `tidb-execdetails`（`crates/execdetails`，N0 rank 4）：`execdetails.rs`、`clientgo.rs`、`tdigest.rs`、`tracing/opt_trace.rs`。
2. `tidb-base::gostd::math`：Go 的 `Sin`、`Asin`（含 `trigReduce`、`satan`、`xatan`）、`Min`、`Max`，按 M-FMA 区分目标；`gostd::strconv::parse_float("NaN")` 改为 Go 的 `math.NaN()`。
3. govec `execdetails` 领域（tracker `govec/domains/execdetails.go`、`execdetails_cases.go`），Rust 回放 `tests/govec_execdetails.rs`。

回放：`cargo test -p tidb-execdetails --test govec_execdetails`；其他种子：`GOVEC_EXECDETAILS_FILES=a.jsonl.gz,b.jsonl.gz`。

### govec `execdetails` 领域

| op | 内容 |
|---|---|
| `fmt` | `FormatDuration`（TiDB 与 client-go）、`FormatBytes`、`BytesToString`；含 `TestFormatDurationForExplain` 的用例 |
| `gomath` | `math.Sin` / `math.Asin` 的结果位（含 `trigReduce` 的大参数、NaN、±Inf、边界） |
| `tdigest` | 脚本：`Add`、`AddCentroidList`、`Quantile`、`Centroids`；批量输入用 splitmix64 在两侧生成（记录很小） |
| `percentile` | 脚本：跨过 1000 个值切换 t-digest，`MergePercentile`（含 t-digest 之间） |
| `details` | `ExecDetails.String` / `ToZapFields` |
| `cgdetails` | client-go detail 的 `String` / `Merge` / `Clone`，`RUDetails` |
| `sync` | `SyncExecDetails` 脚本：各种 merge、`Reset`、`GetExecDetails`、`CopTasksDetails`（P90）、`CopTasksSummary`；`copgen` 批量合并 cop detail（跨过 1000） |
| `coll` | `RuntimeStatsColl` 脚本：basic stats、`RegisterStats`、`RecordCopStats` / `RecordOneCopTask`（TiKV / TiFlash、executor id 改写 plan id、缺字段 panic）、各种读取；`bcop` 通过 `go:linkname` 调用未导出的 `basicCopRuntimeStats` |
| `stats` | 两个 `RuntimeStats`：`String`、`Tp`、`Clone`、`Merge`（含不同类型） |
| `opttrace` | `OptimizeTracer` 脚本，每步后比较 `json.Marshal` 的结果 |
| `ce`、`codec` | `DedupCETrace`、`CodecPlanName` |

## 后续

- `tracing/util.go`：随 D05.3（ExecCtx / M-GOCTX）与 `TRACE` 执行器。
- `MergeTiFlashRUConsumption`、`RUDetails.Update`：随 MPP / TiFlash 客户端。
- D00.1 中等 D14.0 的部分：`StatementContext` 加上 `SyncExecDetails`、`RuntimeStatsColl`、`LockKeysDuration` 等字段，`GetExecDetails`，`ResetForRetry` 重置 exec details；`DistSQLContext` 还要等 D07.2。涉及 sessionctx 的 govec `stmtctx` 领域，单独一个 PR。
