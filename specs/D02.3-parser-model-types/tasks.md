# D02.3 parser 的 model、types、auth、duration：tasks

状态截至 2026-11-07。

| # | 任务 | 完成条件 | 状态 |
|---|---|---|---|
| T1 | tracker：新块 D02.3，DDL job 可达性分析，219 个函数改归 X | `tracker.py check` 通过 | 完成（tracker PR #10） |
| T2 | `tidb-base` `gostd::json`：`Raw`、`unmarshal_raw`、`Decoder`、`find_field`、`append_go_string`、`go_lossy` | 编译通过，config 测试不回退 | 完成 |
| T3 | `model.rs`、`types/`、`auth.rs`、`duration.rs`、`ast_ddl.rs`，共 86 个函数 | converge `matched 86/86` | 完成 |
| T4 | govec `parser_types` 领域（tracker） | `go test ./...` 通过；18,099 条记录 | 完成 |
| T5 | Rust 回放 `tests/govec_parser_types.rs` | arm64 全部通过；故意改坏两条能报错 | 完成 |
| T6 | amd64 向量：取 tracker PR 的 CI artifact | x86_64 CI 通过 | 完成。与 arm64 只有 3 条 `duration` 记录不同（M-F2I） |
| T7 | `layers.toml`：D02.3 归 `tidb-parser` | `check_layers --blocks` 通过 | 完成 |
