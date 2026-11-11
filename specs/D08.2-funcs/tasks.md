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
- [ ] T8b 常量传播（随 D08.4/D08.5）
- [ ] T9 第 2 部分：`builtin_func_param.go`（随 REGEXP/ILIKE）
- [ ] T10 第 3 部分：grouping sets（随 D08.3、D08.9）
