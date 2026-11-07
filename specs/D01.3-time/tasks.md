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
| T7 | 评审跟进（PR #12）：按 L1 移植 `Location` 的查找缓存（`cacheStart`/`cacheEnd`/`cacheZone`）；`$ZONEINFO` 指向 zip（`loadTzinfoFromZip`）；`$ZONEINFO` 每个进程只读一次（`zoneinfoOnce`）；M-F2I 中补上 arm64 的例子；`errors.rs` 头部改为 blocked-by D01.1 | 缓存与逐次搜索的结果在全部 18 个时区、现在前后各两年内逐小时一致；zip 测试通过；基准见 PR | 完成 |
| T8 | MyDecimal 的错误类型改为 `tidb_base::Error`，替换 `types/src/errors.rs` 中的桩（即 D00.2 T12）。**最迟在 D01.4 开工之前完成**：D01.4（convert）、D08.8（CAST）会大量出现 `ErrOverflow.Equal(err)`、`terror.ErrorEqual` 这类跨来源的错误比较，两套 `Error` 类型并存容易判错（PR #12 评审）。若 D01.1 排在 D01.4 之前，就随 D01.1 一起做，否则单独提一个小 PR | D01.1 或 D01.4 之前 | 未开始 |
