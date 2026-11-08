# D00.1 上下文接口骨架：spec

状态：第 1 部分（errctx、`pkg/util/context`、stmtctx 核心）已实现，PR 评审中；第 2 部分（表达式与优化器的上下文、ranger 类型）待做。

## 1. 范围

tracker 中 D00.1 共 433 个函数，分布在 29 个 Go 文件中。分两部分移植：

| 部分 | Go 文件 | 函数数 | 位置 |
|---|---|--:|---|
| 1 | `pkg/errctx/context.go` | 13 | `tidb-sessionctx::errctx` |
| 1 | `pkg/util/context/{warn,plancache,context}.go` | 39 | `tidb-base::contextutil`（已有 `WarnAppender`，见 §3.1） |
| 1 | `pkg/sessionctx/stmtctx/stmtctx.go` | 125（移植 98） | `tidb-sessionctx::stmtctx` |
| 2 | `pkg/expression/{exprctx,exprstatic,expropt,sessionexpr}/*` | 约 190 | 依赖表达式框架 D08.1 |
| 2 | `pkg/planner/{planctx,core/base,util/debugtrace}/*`、`pkg/sessionctx/context.go`、`pkg/distsql/context` | 约 24 | 依赖 D08.1、D07.x |
| 2 | `pkg/util/ranger/{types,points}.go`、`ranger/context` | 35 | Range 类型（N0 评审归 D00.1） |

第 1 部分收敛检查：150/433，三个 Go 文件中未移植的 27 个函数都登记了豁免（govec `stmtctx_pending.go`），原因见 §4。

## 2. 依赖

- `tidb-base`：`Error`（M-ERR）、`errno`、`dbterror::plannererrors::ErrInternal`、Go 标准库移植（json、time）。
- `tidb-types`：`types.Context`（D01.1，`Flags`、`HandleTruncate`）。
- `tidb-model`：`model.Flag*` 下推标志（新增依赖，rank 3 < 7）。
- 不依赖未移植的块：digest（D02.2）、execdetails（D14.0）、统计信息（D11）、表达式（D08.1）都以豁免处理。

## 3. 设计

### 3.1 `pkg/util/context` 放在 tidb-base

`WarnAppender` 在 D01.1 时已放在 tidb-base（`types.Context` 需要它）。本块把整个 `warn.go`、`plancache.go`、`context.go` 都放进 `tidb_base::contextutil`：它们只依赖 `Error`，而 errctx（D00.1）和 types（D01.1）都要用。

- `WarnHandler`、`WarnHandlerExt` 是 trait；共享的接口值是 `Arc<dyn … + Send + Sync>`（`WarnAppenderRef`、`WarnHandlerExtRef`）。
- `StaticWarnHandler` 的状态在 `Mutex` 中，方法取 `&self`，与 Go 的 `sync.Mutex` 一致。
- `GetWarnings` 返回副本（L2）：Go 返回不复制的 slice，但调用方不能修改它，所以副本不可观察。
- Go 的 `interface{ Reset() }` 类型断言对应 `WarnHandlerExt::try_reset`（glue，默认 false，`StaticWarnHandler` 实现为 Reset）。

### 3.2 errctx

`Context` 在 Go 中是值类型（`LevelMap` 数组加一个接口值），这里是 `Clone` 的结构体，warn handler 是 `Option<WarnAppenderRef>`（零值 `Context` 的 `WithStrictErrGroupLevel` 会得到 nil）。`HandleError` 接受一个 `Error`，返回 `Result<(), Error>`。

### 3.3 StatementContext 作为 WarnAppender

Go 中 `sc` 本身就是它的 `types.Context`、`errctx.Context`、`PlanCacheTracker`、`RangeFallbackHandler` 的 `WarnAppender`。Rust 中这个角色是一个共享的 `StmtWarnSink`：它转发给当前的 `WarnHandler`（`RwLock<WarnHandlerExtRef>`）。所以替换 handler（`Reset` 可能会这样做）对这些上下文立即可见，和 Go 传指针的效果一样。

- `mu`（行数计数与 message）、`stmtCache`、`StaleTSOProvider` 是 `Mutex`，与 Go 相同。
- `Reset(&mut self)`：在 `&mut` 借用下三个 `TryLock` 必然成功，所以总是返回 true（L2；Go 返回 false 需要另一个 goroutine 在重置时持有锁，`&mut` 借用排除了这种情况，`TestIssue58600` 测的正是这种情况）。
- `any` 字段是 `Option<Arc<dyn Any + Send + Sync>>`；`StatementContext: Send`（单元测试断言）。
- 第 1 部分没有的字段（`StmtHints`、`MultiSchemaInfo`、`SyncExecDetails`、内存/磁盘 tracker、`RuntimeStatsColl`、digest、optimizer trace、`StatsLoad` 的 items 和 channel、`usedStatsInfo`、`ColRefFromUpdatePlan`、`TblInfo2UnionScan`、DistSQL 上下文缓存）在模块文档中列出，随对应的块加入。

## 4. 偏离与豁免

| 项 | 级别 | 说明 |
|---|---|---|
| `errctx.Context.HandleError` 的 `errors.ErrorGroup` 分支 | L2 | M-ERR 的单一 Error 类型没有 error group；Go 测试 `TestContext` 中 multierr 的部分不回放 |
| `StaticWarnHandler.GetWarnings` | L2 | 返回副本 |
| `StatementContext.Reset` | L2 | `TryLock` 总是成功（§3.3） |
| `StatementContext.ResetForRetry` | L2 | 没有 `SyncExecDetails.Reset`（D14.0）和 DistSQL 上下文缓存（第 2 部分） |
| `StaticWarnHandler.TruncateWarnings` 负数参数 | L1 [M-PANIC] | 与 Go 一样 panic，消息相同 |
| `StatementContext.GetLockWaitStartTime` | L1 | 返回 Unix 纳秒（Go 返回 `time.Unix(0, ns)`） |

豁免（govec `stmtctx_pending.go`，共 249 个）：
- stmtctx.go：digest 相关（`parser.Digest`，D02.2）、`GetResourceGroupTagger`、内存/磁盘 tracker（D12.0/D01.10）、`GetExecDetails`（D14.0）、used-stats（D11）、`Save/RestoreLogicalPlanBuildState`（P3）、`WithStmtLabel`/`GetStmtLabel`（AST）、`GetOrInitDistSQLFromCache`（第 2 部分）。
- 由单元测试覆盖、不进向量的：`CtxID`、`AllocateTaskID`、`GenContextID`（进程级计数器）、`GetPlan`/`SetPlan`/`GetFlatPlan`/`SetFlatPlan`、ranger/pb 上下文缓存（不透明的 `any`）、`GetLockWaitStartTime`（墙钟）。
- 第 2 部分的文件：全部登记为"D00.1 part 2"。

## 5. 验证

- govec `stmtctx`：1,142 条记录（脚本 600 个随机脚本 + 11 个 Go 测试场景，`sqlwarn_marshal`/`sqlwarn_unmarshal`/`push_down`/`resolve`），回放 0 条不一致，debug 和 release-checked 都通过；种子 1–4 本地生成后回放也都通过，tracker CI 生成种子 1–8。
- 错误按 tidb-server v8.5.8 的注册表选 class（`stmtctxClasses`），保证 Go 与 Rust 构造出同一个 terror。
- Go 测试场景（minelits 的等价物，登记在 `minedGoTests["stmtctx"]`）：`TestContext`、`TestSQLWarn`、`TestIgnoreWarn`、`TestStaticWarnHandler`、`TestCopyWarnHandler`、`TestStatementContextPushDownFLags`、`TestNewStmtCtx`、`TestSetStmtCtxTypeFlags`、`TestResetStmtCtx`、`TestErrCtx`、`TestReservedRowIDAlloc`。
- 单元测试 `stmtctx_unit.rs`：ID 计数器、不透明值与 Once 缓存、锁等待起始时间、`StatementContext: Send`。

反证（故意改坏一处，回放报不一致）：

| 改坏 | 不一致 |
|---|--:|
| errctx 去掉 ErrDupEntry 的分组 | 6 |
| warning 上限 `<` 改为 `<=` | 1 |
| `newErrCtx` 中 ignore 与 warn 的判断顺序对调 | 157 |
| 下推标志漏掉 `FlagOverflowAsWarning` | 137 |
| `SQLWarn` JSON 的 `msg` 不再 omitempty | 6 |
| 外键触发时仍累加 affected rows | 3 |
| `InitFromPBFlagAndTz` 不看 `InInsertStmt` | 75 |
