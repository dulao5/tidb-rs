# D05.3 会话执行主流程（tasks）

本块按 VS1 的工作项分部分移植，见 [spec](spec.md)：

- [ ] VS1-14 第 1 部分：`ExecuteStmt` / `runStmt` / `finishStmt`、`LazyTxn` 只读部分、txnManager、回退检查点（VS1 plan §4.6）、KILL 到 coprocessor、中途丢弃的结果集（评审中）
- [ ] VS1-14b 第 2 部分：`PrepareStmt` / `ExecutePreparedStmt` / `DropPreparedStmt` / `resolvePreparedStmt` / `GetSessionPlanCache`，预处理语句四种组合（VS1 plan §4.7）

下面是从其他块移交来的条目。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [x] P1-1 （D00.1）`pkg/expression/sessionexpr/sessionctx.go` 全部 57 个函数（53 个移植、4 个桩：`currentUserProp`（D04.1）与 `sequenceOperator` 的 3 个方法（D06.4）；见 spec §8）：会话背后的 ExprContext / EvalContext（SessionVars、StmtCtx、InfoSchema、sequence、SQL executor），按 N2 随会话构建：`NewEvalContext`、`NewExprContext`、`EvalContext.AllParamValues`、`EvalContext.AppendNote`、`EvalContext.AppendWarning`、`EvalContext.CopyWarnings`、`EvalContext.CtxID`、`EvalContext.CurrentDB`、`EvalContext.CurrentTime`、`EvalContext.ErrCtx`、`EvalContext.GetDefaultWeekFormatMode`、`EvalContext.GetDivPrecisionIncrement`、`EvalContext.GetMaxAllowedPacket`、`EvalContext.GetOptionalPropProvider`、`EvalContext.GetOptionalPropSet`、`EvalContext.GetParamValue`、`EvalContext.GetTiDBRedactLog`、`EvalContext.GetUserVarsReader`、`EvalContext.GetWarnHandler`、`EvalContext.IntoStatic`、`EvalContext.Location`、`EvalContext.RequestDynamicVerification`、`EvalContext.RequestVerification`、`EvalContext.SQLMode`、`EvalContext.Sctx`、`EvalContext.TruncateWarnings`、`EvalContext.TypeCtx`、`EvalContext.WarningCount`、`EvalContext.setOptionalProp`、`ExprContext.AllocPlanColumnID`、`ExprContext.ConnectionID`、`ExprContext.GetBlockEncryptionMode`、`ExprContext.GetCharsetInfo`、`ExprContext.GetDefaultCollationForUTF8MB4`、`ExprContext.GetEvalCtx`、`ExprContext.GetGroupConcatMaxLen`、`ExprContext.GetLastPlanColumnID`、`ExprContext.GetNoopFuncsMode`、`ExprContext.GetPlanCacheTracker`、`ExprContext.GetStaticConvertibleEvalContext`、`ExprContext.GetSysdateIsNow`、`ExprContext.GetWindowingUseHighPrecision`、`ExprContext.IntoStatic`、`ExprContext.IsConstantPropagateCheck`、`ExprContext.IsInNullRejectCheck`、`ExprContext.IsReadonlyUserVar`、`ExprContext.IsUseCache`、`ExprContext.Rng`、`ExprContext.SetSkipPlanCache`、`currentUserProp`、`getStmtTimestamp`、`infoSchemaProp`、`sequenceOperator.GetSequenceID`、`sequenceOperator.GetSequenceNextVal`、`sequenceOperator.SetSequenceVal`、`sequenceOperatorProp`、`sqlExecutorProp`
- [ ] P1-2 （D00.1）`pkg/expression/expropt/sessionvars.go`：`NewSessionVarsProvider`、`SessionVarsPropProvider.Desc`、`SessionVarsPropReader.GetSessionVars`、`SessionVarsPropReader.RequiredOptionalEvalProps`。内置函数如何拿到 SessionVars（N2：EvalContext 是 Send + Sync，SessionVars 属于会话任务）在本块决定
- [ ] P1-3 （D00.1）`pkg/planner/util/debugtrace/base.go` 的 `GetOrInitDebugTraceRoot`、`EnterContextCommon`、`LeaveContextCommon`、`RecordAnyValuesWithNames`：函数体已移植并由 govec `ctx2b` 的 `debugtrace` 记录覆盖（`crates/sessionctx/src/debugtrace.rs` 中标为 `glue:`），只差从 PlanContext 取 `StmtCtx.OptimizerDebugTrace`；接上后改为 `// go:` 标注（converge 中 D00.1 的这 4 个才算移植）
- [ ] P1-4 （D05.2）`pkg/session/cursor/tracker.go`（服务端游标，COM_STMT_FETCH）：`NewTracker`、`cursorHandle.Close`、`cursorHandle.GetState`、`cursorHandle.ID`、`cursorTracker.GetCursor`、`cursorTracker.NewCursor`、`cursorTracker.RangeCursor`、`cursorTracker.remove`
