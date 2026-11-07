# D15.1 内存记账与配额：tasks

[P] = 可以并行。状态截至 2026-11-07。

| # | 任务 | 依赖 | 完成条件 | 状态 |
|---|---|---|---|---|
| T1 | crate 骨架 `crates/memory`，注册到 workspace 和 `layers.toml`（去掉 `planned`） | — | `cargo build`；`check_layers.py` 通过 | 完成 |
| T2 [P] | `sqlkiller.rs`（D00.3 整文件） | T1 | 单元测试 | 完成 |
| T3 [P] | `action.rs`、`tracker.rs`（含 A1 的 `record` / `checkpoint`）；`tidb-metrics` 增加 `tidb_server_memory_usage` | T2 | govec `script` 全部通过 | 完成 |
| T4 [P] | `meminfo.rs`、`memstats.rs`、`cgroup.rs` | T1 | 单元测试；本机读数合理 | 完成 |
| T5 | `alloc.rs`：`TrackedAlloc` | T3 | 单元测试 | 完成 |
| T6 | govec 领域 `memory`（tracker）与 Rust 回放 `tests/govec_memory.rs` | T3 | 1,850 条在 arm64 全部通过；改坏一条会报错 | 完成 |
| T7 | amd64 向量：tracker PR 的 CI artifact `govec-vectors-ubuntu-latest` 中的 `amd64/memory.jsonl.gz` 提交到 tracker 和本 crate | T6 | x86_64 上 `cargo test -p tidb-memory` 通过 | 待做 |
| T8 | 并发测试 `tests/concurrency.rs`、基准 `benches/tracker.rs` | T3、T5 | 见 plan §6 | 完成 |
| T9 | converge：76 个函数标注齐全 | T3、T4 | `matched 76/76` | 完成 |
| T10 | `ExecCtx::checkpoint` 接 `Tracker::checkpoint`；`SqlKiller::set_on_kill` 接 `CancellationToken`（N3 3.3 第 2 条） | — | D00.1 | 未开始 |
| T11 | `Consume` 上溯的引用计数争用（plan §6）：用真实负载重测后决定是否优化 | — | D01.7 / D12 的基准 | 未开始 |
