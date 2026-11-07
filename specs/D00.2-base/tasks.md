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
| T7 | govec 领域 `base`（tracker）与 Rust 回放 `tests/govec_base.rs` | T3–T6 | 7,998 条全部通过（arm64）；修订后 28,047 条 | 完成 |
| T8 | converge：93 个函数标注齐全 | T3–T6 | `matched 93/93, missing 0`（第二部分的文件缺失属预期） | 完成 |
| T9 | amd64 向量：tracker PR 的 CI（`ubuntu-latest`）上传的 artifact `govec-vectors-ubuntu-latest` 中的 `amd64/base.jsonl.gz` 提交到 tracker 和本 crate | T7 | x86_64 上 `cargo test -p tidb-base` 通过 | 完成（tracker PR #3 的 CI 生成；与 arm64 除首行外相同） |
| T10 | 第二部分：`pkg/config`（44 个函数）、BurntSushi/toml 解码器、govec 领域 `config` 与回放 `tests/govec_config.rs` | T1 | 8,399 条向量在两个目标通过；converge `matched 138/138` | 完成（arm64 本地生成，amd64 取自 tracker PR 的 CI） |
| T11 | tracker 归属调整（plan §5）：`ExecuteErrorToLabel` → D00.2、`WarnAppender` → D00.2、`ColumnNeedRestoredData` → D06.1 | — | tracker PR；向上/同层引用 130 → 124 | 完成 |
| T12 | `types/errors.rs` 的临时桩（D01.2 试点留下）换成本块的 `Error`/原型 | T4 | D01.1 开工时 | 未开始 |
| T13 | 评审修订（PR #6）：Go 字符串改为字节串；最短浮点按 L1 移植 `ftoaryu`；宽度/精度到 1e6；`%U`；`IsPrint` 表；`M-GONAME`、`go-dep`；`Error::source` | T7 | 28,047 条向量在两个目标通过；评审 fuzz 30 万 + 100 万浮点无差异 | 完成 |
| T14 | 评审跟进（PR #6 / tracker PR #3 的建议项）：`rsgen -protos` 经 `go/format` 写出；plan §4 增加 `%p` 用于 `[]byte` 的已知差异 | — | `rsgen -protos` 输出与已提交文件相同 | 完成 |
| T15 | 第二部分评审（PR #7）：全局配置改用 `ArcSwap`（M-SNAP）；M-MAPORDER 补充原则；M-ERR 第 7 条（`LoadError`）；`tikvutil` 归入 D00.2；`tidb-model` rank 3、`tidb-execdetails` rank 4；`gostd::conv` 与 M-F2I；`EqualFold`/`ToUpper`/`ToLower` 按生成的 `CASE_RANGES`、`FOLD_ORBIT` 精确移植 | T10 | 向量在两个目标通过；`--block-edges` 123 | 完成 |
