# D06.1 元数据模型：tasks

状态截至 2026-11-07。Part 1 = PR (a)；Part 2 = PR (b)。

| # | 任务 | 完成条件 | 状态 |
|---|---|---|---|
| T1 | tracker `rsgen -model`：结构体、常量、JSON 编解码、`GoCopy` | 生成文件 clippy `-D warnings` 干净 | 完成（tracker PR #13） |
| T2 | `tidb-base`：`gostd::json` 类型化解码与编码扩展；`gostd::time` 的 JSON 方法与 `time.Parse(RFC3339)` 回退 | 编译通过，config / time 测试不回退 | 完成 |
| T3 | 自建 v8.5.8 playground 导出 meta 语料（独立 tag，用完销毁） | 866 份 meta 进 tracker testdata | 完成 |
| T4 | govec `model` 领域 | `go test ./...` 通过；9,053 条记录 | 完成 |
| T5 | `tidb-model` 手写方法 139 个 | converge `matched 139/153`，缺的 14 个是 Part 2 | 完成 |
| T6 | Rust 回放 `tests/govec_model.rs` | arm64 全部通过 | 完成 |
| T7 | amd64 向量：取 tracker PR #13 的 CI artifact | x86_64 CI 通过 | 进行中 |
| T8 | mappings.md M-BYTES；ROADMAP（D06.1 行、D04.1 的 scramble 注记）；CI arm64 job 加 `tidb-model` | — | 完成 |
| T9 | Part 2：`job.go`、`reorg.go` 的 13 个函数（`errors.Error` 的 JSON、`json.RawMessage`），`ColumnNeedRestoredData`（等 D01.9 的 collate） | converge `matched 153/153` | 未开始 |
