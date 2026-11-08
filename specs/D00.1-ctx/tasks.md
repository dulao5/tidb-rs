# D00.1：tasks

- [x] T1 `contextutil`：warn.go、plancache.go、context.go
- [x] T2 errctx
- [x] T3 stmtctx 核心（warnings、错误级别、类型标志、下推标志、行数计数、stmt cache、stale TSO、ReferenceCount、ReservedRowIDAlloc）
- [x] T4 govec `stmtctx` 领域与回放；单元测试；反证
- [x] T5 ranger 类型（types.go、points.go 的 Full*Range、ranger/context）
- [x] T6a exprctx、expropt/optional.go、exprstatic（第 2a 部分）
- [ ] T6b expropt 的具体 provider、sessionexpr（随 D08.1）
- [ ] T7 planctx、core/base、debugtrace、sessionctx/context.go、distsql/context
- [ ] T8 stmtctx 剩余：digest（D02.2）、内存/磁盘 tracker（D12.0）、ExecDetails（D14.0）、used-stats（D11）、LogicalPlanBuildState（P3）
- [ ] T9 D05.2 `session.rs` 的 StatementContext 替身换成 `stmtctx::StatementContext`
- [x] T10 #65 评审的后续：`pkg/util/context` 改归 tidb-base 所有的块 D00.4；替换 handler 并 reset 后旧副本写 warning 的单元测试；spec 记下 ErrorGroup 分支在 INSERT 写路径上可达、`Reset` 与 `Arc::get_mut` 的对应
- [x] T11 #68 评审：块 D00.1a 改名为 D00.4；`UserVarsReader` 与空的 `UserVars` 替身移到 `variable/uservars.rs`（Go 在 `pkg/sessionctx/variable` 定义），`exprctx` 再导出；`EvalWarnSink` 改用 `OnceLock`（Go 的 `EvalContext` 构造后不再换 handler）
- [ ] T12 D05.2 移植 `PlanCacheParamList` 时：`exprstatic` 的 `Apply` 现在深拷贝参数列表，Go 共享底层的字节；改为 `Arc<[Datum]>` 共享（#68 评审）
