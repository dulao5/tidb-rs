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
  1. 归属不一致：`pkg/util/context` 的 39 个函数在 tidb-base 中，但 tracker 归 D00.1（layers.toml 中属于 tidb-sessionctx）。改为 tidb-base 所属的新块（如 D00.4，tracker rules + layers.toml），或者登记例外。
  2. 单元测试：拿到 type context 和 appender 后替换 handler、reset，再通过旧的副本写 warning，必须落到新的 handler。
  3. 在 tracker/spec 中记下：HandleError 的 ErrorGroup 分支在 INSERT 写路径上可以走到（rowcodec 的 multierr → WriteMemBufferEncoded → HandleError 按错误分别取级别；静态分析），供以后的写入工作参考。
  4. spec §4：`Reset` 总是返回 true；当执行器共享 statement context 时，Go 返回 false 的情况对应调用方的 `Arc::get_mut` 失败，此时新建一个。
- **其余**：T6、T7（exprstatic、expropt、sessionexpr、planctx 等）、T8、T9（等 D05.2 B2 合并后再替换 `variable/session.rs` 的替身）。

## 第 2a 部分完成（2026-11-09）

- 已 rebase 到 main（#65 合并之后）。
- 完成：exprctx（context/optional/param.go）、expropt/optional.go、exprstatic（evalctx/exprctx.go）、ranger 类型；govec `exprctx`、`ranger` 领域与回放；#65 评审的四条后续（D00.4、单元测试、ErrorGroup 可达、Reset 与 `Arc::get_mut`）。
- 下一步（第 2b 部分，叠在 2a 上）：expropt 的具体 provider（current_user、sessionvars、infoschema、kvstore、sqlexec、sequence、advisory_lock、ddlowner、priv）、sessionexpr、planctx、core/base、debugtrace、sessionctx/context.go、distsql/context；T8；T9 等 D05.2 B2 合并。
- PR：tidb-rs #68（ready，CI 两个平台通过）、tracker #32（CI 通过）。合并顺序：先 tracker #32，再 #68。
- 生成器注意：exprctx 的 `load` 会触发 `tidb_redact_log` 的 SetSession hook（进程级状态，#62），每条记录后恢复；exprctx、ranger 每条记录前打开 new collation。回放侧同样处理。
- 2b 尚未开始（没有分支）。

## 第 2b 部分交接（2026-11-09，分支 block/D00.1-ctx-2b，叠在 2a 上；已完成，见下方"第 2b 部分完成"）

- 已完成（WIP，编译、clippy、sessionctx 测试通过；converge D00.1 299/394）：
  - expropt 的 8 个 provider 文件（advisory_lock、current_user、ddlowner、infoschema、kvstore、priv（`priv_.rs`）、sequence、sqlexec）。未移植类型用替身：`MetaOnlyInfoSchema`（D06.3）、`SQLExecutor`（无方法，ExecRestrictedSQL 随 util/sqlexec）。
  - `planctx.rs`：`PlanContext` trait（只声明类型已移植的方法）、`EmptyPlanContextExtended`、`BuildPBContext`（GetExprCtx、GetClient、Detach）。
  - `planbase.rs`：`PlanCounterTp`（Dec 用 wrapping_sub）。
  - `context.rs`：`basicCtxType.String`（`BasicCtxType`）。
  - stmtctx：`mem_tracker` / `disk_tracker` 字段，`InitMemTracker`、`InitDiskTracker`（L2：新建 tracker，不复用缓存，N2 §3.4）、`DetachMemDiskTracker`；`StatsLoadResult.HasError`、`ErrorMsg`。
- 下一步：
  1. debugtrace（`planner/util/debugtrace/base.go`）：root 用 arena（`Vec<ctx>` + 下标）表示树；step = JSON 值或子 context 下标；EncodeJSONCommon 关闭 HTML 转义、末尾换行，NaN/Inf 时按 Go 输出 `{"Value":{},"Str":"NaN"}` 并返回 `json: unsupported value: NaN`，嵌套 context 出错时为 MarshalerError（`{"Type":{},"Err":{...}}`）。四个取 `PlanContext` 的函数（GetOrInitDebugTraceRoot、Enter/LeaveContextCommon、RecordAnyValuesWithNames）等 N2 的 PlanCtx（D05.3）登记豁免，逻辑放在 root 的 glue 方法上。
  2. tracker：`stmtctx_pending.go` 删掉已移植的 5 个；新增豁免：sessionexpr 全部 57 个（等 D05.3 Session 与 N2 三层结构、T9）、expropt/sessionvars.go 4 个（N2：EvalContext 要 Send+Sync，SessionVars 归会话任务，D05.3 决定）、distsql/context 2 个（DistSQLContext 字段类型属 D14.0/D07.2）、ValidateSnapshotReadTS（等 kv.Storage.GetOracle，D07.1）、debugtrace 4 个。
  3. govec 新领域（如 `ctx2b`）：PlanCounterTp 序列、basicCtxType.String、StatsLoadResult.ErrorMsg、expropt 各 reader 在有/无 provider 时的结果与错误文字（exprstatic EvalContext + WithOptionalProperty）、debugtrace 编码（随机 any 树、HTML 字符、U+2028、NaN、嵌套 context、空/单/多 step）、tracker init 的 label/limit；minelits；多种子；进程级状态每条记录重置（tidb_redact_log #62、new collation）。
  4. 单元测试、spec §2b/§4、tasks、ROADMAP；开 PR（base 依 2a 是否合并而定）。

## 第 2b 部分完成（2026-11-09）

- debugtrace（arena、`EncodeJSONCommon` 实际不返回错误，见 spec §3.5）、govec `ctx2b`（731 条）、`planctx_unit.rs`、豁免移到 tracker `ctx2b_pending.go`。
- PR：tracker #36（draft，叠在 #32 上）、tidb-rs 对应 PR（draft，叠在 #68 上）。#32/#68 合并后 rebase 到 main、改 base、标为 ready。
- 剩下：T8（stmtctx 依赖 D02.2/D14.0/D11/P3 的部分）、T9（等 D05.2 完成）、T12（随 D05.2 的 `PlanCacheParamList`）。
