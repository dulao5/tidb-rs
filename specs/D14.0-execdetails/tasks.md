# D14.0：tasks

- [x] T1 crate `tidb-execdetails`（rank 4）、layers.toml
- [x] T2 依赖：client-go detail 类型、tdigest、`gostd::math` 的 `Sin`/`Asin`/`Min`/`Max`（M-FMA）
- [x] T3 `execdetails.go`：ExecDetails、SyncExecDetails、P90、Percentile、RuntimeStats、RuntimeStatsColl、TiFlash 统计、FormatDuration
- [x] T4 `opt_trace.go`：优化器 trace 与 JSON
- [x] T5 govec `execdetails` 领域与回放；反证
- [ ] T6 `tracing/util.go`（随 D05.3 / TRACE）
- [ ] T7 `MergeTiFlashRUConsumption`（随 MPP / TiFlash 客户端）
- [ ] T8 D00.1 的后续：`StatementContext` 接入 `SyncExecDetails` / `RuntimeStatsColl`、`GetExecDetails`、`ResetForRetry` 重置 exec details；`DistSQLContext`（还要等 D07.2）
