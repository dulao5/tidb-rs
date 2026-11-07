# D01.3 时间与 Duration：tasks

状态截至 2026-11-07。

| # | 任务 | 完成条件 | 状态 |
|---|---|---|---|
| T1 | Go `time` 包的移植：`Location`（TZif、`tzset`、`lookup`、`LoadLocation`、`Local`）、`Time`（日期算法、`Date`、`Round`、`AddDate`、`ZoneBounds`、`String`）、`Duration.String` | `zone_lookup`、`go_date` 向量通过 | 完成 |
| T2 | `context.rs`、`errors.rs` 的 `go_errors`、`util_parser.rs`、`isPunctuation`、`AddInt64`、`SubInt64` | 编译通过 | 完成 |
| T3 | `fsp.rs`、`core_time.rs`、`time.rs`，共 202 个函数 | converge `matched 202/202` | 完成 |
| T4 | govec `time` 领域、`cmd/minelits`、内嵌 TZif（tracker） | `go test ./...` 通过；101,814 条记录 | 完成 |
| T5 | Rust 回放 `tests/govec_time.rs` | arm64：debug 和 `release-checked` 全部通过 | 完成 |
| T6 | amd64 向量：取 tracker PR 的 CI artifact | x86_64 CI 通过 | 完成。amd64 与 arm64 只有 3 条记录不同，都是 `parse_time_float64` 的 NaN、+Inf、1e19（M-F2I） |
