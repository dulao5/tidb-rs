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
