# D00.1：tasks

- [x] T1 `contextutil`：warn.go、plancache.go、context.go
- [x] T2 errctx
- [x] T3 stmtctx 核心（warnings、错误级别、类型标志、下推标志、行数计数、stmt cache、stale TSO、ReferenceCount、ReservedRowIDAlloc）
- [x] T4 govec `stmtctx` 领域与回放；单元测试；反证
- [ ] T5 ranger 类型（types.go、points.go 的 Full*Range、ranger/context）
- [ ] T6 exprctx、exprstatic、expropt、sessionexpr（随 D08.1）
- [ ] T7 planctx、core/base、debugtrace、sessionctx/context.go、distsql/context
- [ ] T8 stmtctx 剩余：digest（D02.2）、内存/磁盘 tracker（D12.0）、ExecDetails（D14.0）、used-stats（D11）、LogicalPlanBuildState（P3）
- [ ] T9 D05.2 `session.rs` 的 StatementContext 替身换成 `stmtctx::StatementContext`
