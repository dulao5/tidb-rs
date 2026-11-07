# D01.6 Vector 类型：tasks

状态截至 2026-11-07。

| # | 任务 | 完成条件 | 状态 |
|---|---|---|---|
| T1 | tidb-base：`gostd::strconv::format_float`、`gostd::math::nan` | 编译通过 | 完成 |
| T2 | `jsoniter.rs`：`ParseString`、`ReadArrayCB`、`ReadFloat64` 及内部函数 | `parse` 向量通过 | 完成 |
| T3 | `vector.rs`、`vector_functions.rs`，共 31 个函数 | converge `matched 31/31`，`--fma-sites` 通过 | 完成 |
| T4 | govec `vector` 领域（tracker） | `go test ./...` 通过 | 完成 |
| T5 | Rust 回放 `tests/govec_vector.rs` | arm64：debug 和 `release-checked` 全部通过 | 完成 |
| T6 | amd64 向量：取 tracker PR 的 CI artifact；两目标差异登记到 `targetDiffs` | x86_64 CI 通过 | 完成。两个目标有 130 条 `distance` 记录不同：92 条是 FMA（M-FMA），38 条是无效运算的默认 NaN 符号（amd64 为负） |
