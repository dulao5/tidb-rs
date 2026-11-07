# D01.9 Collation 与字符集：tasks

状态截至 2026-11-07。

| # | 任务 | 完成条件 | 状态 |
|---|---|---|---|
| T1 | tracker rsgen：collate、charset、GBK 编码表 | 生成的 Rust 表能编译，govec `key_digest` 通过 | 完成 |
| T2 | `gocharset`：charset.go 整文件、GBK 编码器 | converge：charset.go 16/16 | 完成 |
| T3 | `stringutil.rs`：string_util.go 整文件 | converge（D00.3）：string_util.go 31/31 | 完成 |
| T4 | `collate/`：12 个 Go 文件，共 95 个函数 | converge `--block D01.9` OK | 完成 |
| T5 | govec `collate` 领域（tracker PR #12） | `go test ./...` 通过 | 完成。共 46,736 条记录 |
| T6 | Rust 回放 `tests/govec_collate.rs` | arm64：debug 和 `release-checked` 都通过；改坏记录能被发现 | 完成 |
| T7 | amd64 向量：取 tracker PR 的 CI artifact | x86_64 CI 通过 | 完成。两个目标相同，只有文件头不同 |
| T8 | 单元测试：`MemoizeStr`、`StringerFunc` | 通过 | 完成 |
| T9 | spec、plan、tasks，ROADMAP 链接 | — | 完成 |
| T10 | 合并 tidb-parser 的 PoC `charset.rs` 与 `gocharset`；移植 `encoding_gbk.go` 的其余部分 | — | 留给 D02.2 的后续工作 |
