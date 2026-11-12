# D08.2：tasks

- [x] T1 生成器 `gen_registry.py`：函数名常量、`funcs` 条目
- [x] T2 `builtin.rs`：`baseBuiltinFunc` 构造、`BaseFunctionClass`、`BuiltinFuncCache`
- [x] T3 `builtin_registry.rs`：`IsFunctionSupported`、`GetDisplayName`、`GetBuiltinList`、`VerifyArgsWrapper`、`RegisteredBuiltinFunctionNames`
- [x] T4 `collation.rs`：`deriveCollation` 及其辅助函数
- [x] T5 `function_traits.rs`
- [x] T6 govec `funcs` 领域与回放、单元测试；反证
- [x] T7a `baseBuiltinCastFunc`（`newBaseBuiltinCastFunc`、`metadata`、`cloneFrom`；随 D08.8 第 1 部分）
- [ ] T7b `newBaseBuiltinFuncWithTp`/`WithFieldTypes`（要 CAST 到 STRING/TIME 等，D08.8 第 2 部分；`derive` 记录经它们回放）、`get_function`、`NewFunction*`
- [x] T8a 常量折叠 `constant_fold.go`（随 D08.8 第 1 部分；IF/IFNULL/CASE/ISNULL 的处理已移植，随这些函数在 govec 中覆盖；null-reject 分支等 `NewFunctionBase`）
- [x] T8b 常量传播 `constant_propagation.go`（`PropagateConstant`、`PropConstOverOuterJoin`；govec `cprop`；spec §4 第 7 条）
- [ ] T8c（planner / 第一条经过 `PropagateConstant` 的 SQL 路径）SQL 测试固定 tidb-rs 对 YEAR 原地修改偏离的结果（#104，`docs/compatibility.md`）：表 `yy(y1 YEAR, y2 YEAR)`，行按 #104 的复现；`y1 = y2 AND y2 < 1.0` 只返回 0000（v8.5.8 还返回 1990、2000），`y1 = y2 AND y2 >= 0.5` 与 v8.5.8 相同
- [ ] T8d（D08.6，交给 D08.6 的任务列表）govec 配方覆盖 `ColumnSubstituteImpl` 的 #53730 `LENGTH` 分支（替换进 `LENGTH(col)` 的参数时检查 collation/类型），LENGTH 移植后加
- [ ] T8e（plan cache 负责人）参数标记：`ConstExprConsiderPlanCache`、`MaybeOverOptimized4PlanCache` + `SetSkipPlanCache` 在常量传播中的路径（跳过 plan cache 是正确性保护）；需要 `ParamMarker` 常量与 plan cache 上下文后用 govec 覆盖
- [ ] T9 第 2 部分：`builtin_func_param.go`（随 REGEXP/ILIKE）
- [ ] T10 第 3 部分：grouping sets（随 D08.3、D08.9）
