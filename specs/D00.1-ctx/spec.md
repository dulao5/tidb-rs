# D00.1 上下文接口骨架：spec

状态：第 1 部分（errctx、`pkg/util/context`、stmtctx 核心）已合并（#65）。第 2a 部分（exprctx、expropt 的 provider 集合、exprstatic、ranger 类型）PR 评审中；第 2b 部分（expropt 的具体 provider、sessionexpr、planctx 等）待做。

## 1. 范围

tracker 中 D00.1 共 433 个函数，分布在 29 个 Go 文件中。分两部分移植：

| 部分 | Go 文件 | 函数数 | 位置 |
|---|---|--:|---|
| 1 | `pkg/errctx/context.go` | 13 | `tidb-sessionctx::errctx` |
| 1 | `pkg/util/context/{warn,plancache,context}.go` | 39 | `tidb-base::contextutil`（已有 `WarnAppender`，见 §3.1）；tracker 中改归新块 **D00.1a**（tidb-base 所有，#65 评审） |
| 1 | `pkg/sessionctx/stmtctx/stmtctx.go` | 125（移植 98） | `tidb-sessionctx::stmtctx` |
| 2a | `pkg/expression/exprctx/{context,optional,param}.go` | 23 | `tidb-sessionctx::exprctx`（trait 与包装类型） |
| 2a | `pkg/expression/expropt/optional.go` | 5 | `tidb-sessionctx::expropt` |
| 2a | `pkg/expression/exprstatic/{evalctx,exprctx}.go` | 81 | `tidb-sessionctx::exprstatic` |
| 2b | `pkg/expression/expropt/*`（具体 provider）、`pkg/expression/sessionexpr/*` | 约 81 | 依赖表达式框架 D08.1 |
| 2 | `pkg/planner/{planctx,core/base,util/debugtrace}/*`、`pkg/sessionctx/context.go`、`pkg/distsql/context` | 约 24 | 依赖 D08.1、D07.x |
| 2a | `pkg/util/ranger/{types,points}.go`、`ranger/context` | 35 | `tidb-sessionctx::ranger`（N0 评审归 D00.1） |

收敛检查：第 1 部分后 150/433；第 2a 部分后 D00.1 为 294/433（2a 的 7 个 Go 文件全部移植，缺的 27 个仍是 stmtctx.go 中登记了豁免的函数，其余是 2b 的文件），D00.1a 为 39/39。

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

### 3.4 表达式上下文（第 2a 部分）

- **接口**：`EvalContext`、`BuildContext`、`ExprContext`、`StaticConvertibleEvalContext`、`StaticConvertibleExprContext`、`PlanColumnIDAllocator`、`OptionalEvalPropProvider`、`ParamValues` 都是 object-safe 的 trait（`Send + Sync`），共享时用 `Arc<dyn …>`（N4）。
- **嵌入**：Go 中嵌入接口的包装类型（`NullRejectCheckExprContext`、`ConstantPropagateCheckContext`、`innerOverrideEvalContext`、`innerOverrideBuildContext`）持有内层的 `Arc`，逐个方法转发；转发方法标为 glue。
- **CtxWithHandleTruncateErrLevel**：没有变化时返回同一个 `Arc`，和 Go 返回同一个接口值一致（向量记录了这一点）。
- **exprstatic 的 warning 接收方**：Go 把 `EvalContext` 自身作为它的 `types.Context`、`errctx.Context` 的 `WarnAppender`，所以选项在构造之后替换 `warnHandler` 仍然生效。Rust 中这个角色是共享的 `EvalWarnSink`，在选项应用完之后设置一次（之后上下文不可变）。和 §3.3 的 `StmtWarnSink` 是同一个思路。
- **`*EvalContext` / `*ExprContext`** 是 `Arc<…>`；`EvalContext::apply` 取 `self: &Arc<Self>`，因为新上下文的当前时间要读旧上下文（Go 捕获 `ctx.CurrentTime`）。选项是 `Box<dyn FnOnce(&mut State)>`。
- **`*mathutil.MysqlRng`** 是 `Arc<parking_lot::Mutex<MysqlRng>>`（Go 的 `MysqlRng` 内部有锁）。
- **D05.2 未移植的部分**：`variable.UserVarsReader` 与空的 `UserVars` 作为替身放在 `exprctx::uservars`（D00.1 不改 `variable/*`），D05.2 移植用户变量后替换；`SessionVars.GetCharsetInfo` 用 `get_system_var` 读两个系统变量（go-dep）。

## 4. 偏离与豁免

| 项 | 级别 | 说明 |
|---|---|---|
| `errctx.Context.HandleError` 的 `errors.ErrorGroup` 分支 | L2 | M-ERR 的单一 Error 类型没有 error group；Go 测试 `TestContext` 中 multierr 的部分不回放。**注意**：这个分支在 INSERT 的写路径上可以走到（rowcodec 用 multierr 合并各列的错误 → `WriteMemBufferEncoded` → `HandleError` 按每个错误各自的级别处理；静态分析，#65 评审）。tidb-rs 只读阶段走不到；以后移植表的写入时，M-ERR 需要能表示 error group |
| `StaticWarnHandler.GetWarnings` | L2 | 返回副本 |
| `StatementContext.Reset` | L2 | `TryLock` 总是成功（§3.3）。以后执行器之间共享 statement context 时（`Arc`），Go 返回 false 的情况对应调用方 `Arc::get_mut` 失败：此时新建一个 statement context，而不是重置（#65 评审） |
| `StatementContext.ResetForRetry` | L2 | 没有 `SyncExecDetails.Reset`（D14.0）和 DistSQL 上下文缓存（第 2 部分） |
| `StaticWarnHandler.TruncateWarnings` 负数参数 | L1 [M-PANIC] | 与 Go 一样 panic，消息相同 |
| `StatementContext.GetLockWaitStartTime` | L1 | 返回 Unix 纳秒（Go 返回 `time.Unix(0, ns)`） |
| `exprstatic.WithParamList` | L2 | Go 取 `*variable.PlanCacheParamList`（D05.2 未移植），这里直接取参数值 |
| `exprctx.AssertLocationWithSessionVars` | L2 | 只是 `intest.Assert`（生产构建中无效果），Rust 为 `debug_assert!`；D05.2 的 StatementContext 替身没有时区，只比较上下文与 session 的时区 |
| `expropt.getPropProvider` | L2 | 类型转换失败的消息中，Go 的 `%T` 换成 `std::any::type_name`（这个分支在 Go 中是 `intest.Assert(false)`） |
| `EvalContext.LoadSystemVars` 等 | L1 [M-MAPORDER] | 参数是 `HashMap`；只有两个变量都出错时结果才依赖顺序（govec 每条记录最多一个出错的变量） |
| `exprctx.OptionalEvalPropKey` 负数 | L1 | Go 的负数移位会 panic，Rust 照样 panic；`intest.Assert` 不移植，越界的 key 按生产构建的行为处理 |
| `newSessionVarsWithSystemVariables` | L1 | `NewSessionVars(nil)`：nil 的 `GlobalVarAccessor` 被调用时 panic，Rust 的 `NilGlobalVarAccessor` 也 panic |

豁免（govec `stmtctx_pending.go`，共 249 个）：
- stmtctx.go：digest 相关（`parser.Digest`，D02.2）、`GetResourceGroupTagger`、内存/磁盘 tracker（D12.0/D01.10）、`GetExecDetails`（D14.0）、used-stats（D11）、`Save/RestoreLogicalPlanBuildState`（P3）、`WithStmtLabel`/`GetStmtLabel`（AST）、`GetOrInitDistSQLFromCache`（第 2 部分）。
- 由单元测试覆盖、不进向量的：`CtxID`、`AllocateTaskID`、`GenContextID`（进程级计数器）、`GetPlan`/`SetPlan`/`GetFlatPlan`/`SetFlatPlan`、ranger/pb 上下文缓存（不透明的 `any`）、`GetLockWaitStartTime`（墙钟）。
- 第 2 部分的文件：全部登记为"D00.1 part 2"；第 2a 部分合并后只剩 2b 的文件（106 个）。
- govec `exprctx` 豁免 `AssertLocationWithSessionVars`（只有 intest 断言）；`ranger` 豁免 `RangerContext.Detach`（复制接口值），由单元测试 `exprctx_unit.rs` 覆盖。

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

### 第 2a 部分

- govec `ranger`：3,839 条（types_test.go 的 7 个测试场景 + 1,500 个随机 range 等），回放 0 条不一致。
- govec `exprctx`：2,242 条（keyset、alloc、eval、expr、truncate、load、static、getprop），回放 0 条不一致。`load` 从变量池中随机取 1–4 个系统变量，同一个名字的不同大小写不会出现在同一条记录里（否则结果依赖 map 顺序）。
- 两个领域本地用种子 1–4 生成后回放都通过；tracker CI 生成种子 1–8。amd64 的向量取自 tracker CI。
- 单元测试：`exprctx_unit.rs`（`getPropProvider` 的成功与类型不符、`RangerContext.Detach`）；`stmtctx_unit.rs` 新增"替换 handler 并 reset 之后，通过旧的 type context、errctx、appender 写的 warning 落到新 handler"（#65 评审）。

反证：

| 改坏 | 不一致 |
|---|--:|
| ranger：`formatDatum` 中 MaxValue 写成 `+INF` | 339 |
| ranger：MinInt64 不判断 `isLeftSide` | 1 |
| ranger：`Width` 取 `HighVal` 的长度 | 94 |
| exprctx：`CtxWithHandleTruncateErrLevel` 不比较错误级别 | 45 |
| exprctx：`OptPropSequenceOperator` 的描述改为正确的名字（Go 写成了 "OptPropDDLOwnerInfo"） | 34 |
| exprstatic：不设置 `EvalWarnSink` 的 handler | 792 |
| exprstatic：`Apply` 不沿用旧上下文的当前时间 | 362 |
| exprstatic：`newSessionVarsWithSystemVariables` 先设 collation 再设 charset | 4 |

