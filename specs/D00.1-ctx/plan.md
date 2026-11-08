# D00.1：plan

## 第 1 部分（本 PR）

1. `tidb-base::contextutil`：warn.go、plancache.go、context.go 全部。
2. `tidb-sessionctx::errctx`。
3. `tidb-sessionctx::stmtctx`：StatementContext 的读路径部分（spec §3.3）。
4. govec `stmtctx` 领域（tracker `govec/stmtctx-D00.1`）与 Rust 回放 `tests/govec_stmtctx.rs`；单元测试 `tests/stmtctx_unit.rs`。

回放：`cargo test -p tidb-sessionctx --test govec_stmtctx`；额外的种子：`GOVEC_STMTCTX_FILES=a.jsonl.gz,b.jsonl.gz`（tracker CI 产物 `govec-extra-seeds-*`）。

## 第 2 部分（交接）

- ranger 类型（`util/ranger/types.go`、`points.go` 中的 `FullIntRange`/`FullRange`/`FullNotNullRange`/`NullRange`、`ranger/context`）：依赖 Datum 比较、collator、`codec.EncodeKey`（都已移植）和 `planctx.PlanContext`（第 2 部分的 trait）。可以先于表达式上下文单独做。
- `exprctx`/`exprstatic`/`expropt`/`sessionexpr`、`planctx`、`core/base`、`debugtrace`、`sessionctx/context.go`、`distsql/context`：以 trait 定义接口（object safe、`Send + Sync`），具体实现随 D08.1 表达式框架。
- D05.2 的 `variable/session.rs` 中 `StatementContext` 替身：D05.2 PR B 合并后，换成本块的 `stmtctx::StatementContext`（`SessionVars.StmtCtx`）。
- `types::context::IgnoreWarn`（D01.1 的 glue）可以改用 `tidb_base::contextutil::IgnoreWarn`。

## 第 2 部分进展（2026-11-09，交接）

- **分支**：tidb-rs `block/D00.1-ctx-2`、tracker `govec/ctx-D00.1-2`，都叠在第 1 部分上，尚未 rebase 到 main（#65、tracker #30 合并后要 `git rebase --onto origin/main block/D00.1-ctx`）。
- **已写（WIP，未验证）**：
  - `crates/sessionctx/src/ranger/{mod,types,context}.rs`：types.go 和 points.go 中归 D00.1 的 35 个函数，以及 `RangerContext.Detach`。本地 `cargo check` 通过；converge 中 ranger 部分 0 缺失。
  - `crates/sessionctx/src/exprctx.rs`、`planctx.rs`：只有 trait 骨架（glue），供 `RangerContext.ExprCtx` 和 `MutableRanges.Rebuild` 使用。
  - tracker `govec/domains/ranger.go`：ops `range`、`encode`、`pair`、`ranges`、`full`，抽取了 types_test.go 的 7 个测试场景；`stmtctx_pending.go` 中的 35 条 ranger 豁免已删除。`go vet` 通过，**还没有生成向量，Rust 回放测试也还没写**。
- **评审要求的顺序（Lucky）**：ranger 的 context 依赖 exprctx 和 exprstatic，所以顺序改为 exprctx → exprstatic → ranger，或者 ranger 和 exprctx 放在同一个 PR。现在的 trait 骨架只能作为过渡，exprctx 的方法集要在同一个 PR 里补齐。
- **要并入第一个第 2 部分 PR 的第 1 部分后续**（#65 评审意见）：
  1. 归属不一致：`pkg/util/context` 的 39 个函数在 tidb-base 中，但 tracker 归 D00.1（layers.toml 中属于 tidb-sessionctx）。改为 tidb-base 所属的新块（如 D00.1a，tracker rules + layers.toml），或者登记例外。
  2. 单元测试：拿到 type context 和 appender 后替换 handler、reset，再通过旧的副本写 warning，必须落到新的 handler。
  3. 在 tracker/spec 中记下：HandleError 的 ErrorGroup 分支在 INSERT 写路径上可以走到（rowcodec 的 multierr → WriteMemBufferEncoded → HandleError 按错误分别取级别；静态分析），供以后的写入工作参考。
  4. spec §4：`Reset` 总是返回 true；当执行器共享 statement context 时，Go 返回 false 的情况对应调用方的 `Arc::get_mut` 失败，此时新建一个。
- **其余**：T6、T7（exprstatic、expropt、sessionexpr、planctx 等）、T8、T9（等 D05.2 B2 合并后再替换 `variable/session.rs` 的替身）。
