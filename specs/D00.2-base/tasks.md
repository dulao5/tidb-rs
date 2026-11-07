# D00.2 常量、错误码与配置：tasks

[P] = 可以并行。状态截至 2026-11-07。

| # | 任务 | 依赖 | 完成条件 | 状态 |
|---|---|---|---|---|
| T1 | crate 骨架 `crates/base`，注册到 workspace 和 `layers.toml`（去掉 `planned`） | — | `cargo build`；`check_layers.py` 通过 | 完成 |
| T2 | 生成器 `govec/cmd/rsgen`：常量、map/切片字面量、注册表、原型、unicode 表 | — | 生成结果编译通过 | 完成 |
| T3 [P] | `errors.rs`（pingcap/errors）、`gofmt.rs`（fmt/strconv）、`gostd/`（unicode） | T1 | 向量 `sprintf`/`sprint`/`unicode` 全部通过 | 完成 |
| T4 [P] | `terror.rs`、`dbterror/mod.rs` | T2、T3 | 向量 `terror_new_std`/`dbterror_new_std`/`prototype`/`to_sql_error`/`err_class`/`error_equal`/`synthesize` 通过 | 完成 |
| T5 [P] | `mysql/*.rs`：`const`（SQL mode、priority）、`type`、`privs`、`util`、`charset`、`error`、`errname`、`locale_format` | T2、T3 | 对应向量通过 | 完成 |
| T6 [P] | `errno/infoschema.rs`、`kv.rs` | T1 | `errno_stats`、`store_type` 通过 | 完成 |
| T7 | govec 领域 `base`（tracker）与 Rust 回放 `tests/govec_base.rs` | T3–T6 | 7,998 条全部通过（arm64） | 完成 |
| T8 | converge：93 个函数标注齐全 | T3–T6 | `matched 93/93, missing 0`（第二部分的文件缺失属预期） | 完成 |
| T9 | amd64 向量：tracker PR 的 CI（`ubuntu-latest`）上传的 artifact `govec-vectors-ubuntu-latest` 中的 `amd64/base.jsonl.gz` 提交到 tracker 和本 crate | T7 | x86_64 上 `cargo test -p tidb-base` 通过 | 完成（tracker PR #3 的 CI 生成；与 arm64 除首行外相同） |
| T10 | 第二部分：`pkg/config`（44 个函数）| T1 | 单独 PR | 未开始 |
| T11 | tracker 归属调整（plan §5）：`ExecuteErrorToLabel`、`WarnAppender`、`ColumnNeedRestoredData` | — | tracker PR | 未开始（随对应块） |
| T12 | `types/errors.rs` 的临时桩（D01.2 试点留下）换成本块的 `Error`/原型 | T4 | D01.1 开工时 | 未开始 |
