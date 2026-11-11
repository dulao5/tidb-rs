# D08.2：tasks

- [x] T1 生成器 `gen_registry.py`：函数名常量、`funcs` 条目
- [x] T2 `builtin.rs`：`baseBuiltinFunc` 构造、`BaseFunctionClass`、`BuiltinFuncCache`
- [x] T3 `builtin_registry.rs`：`IsFunctionSupported`、`GetDisplayName`、`GetBuiltinList`、`VerifyArgsWrapper`、`RegisteredBuiltinFunctionNames`
- [x] T4 `collation.rs`：`deriveCollation` 及其辅助函数
- [x] T5 `function_traits.rs`
- [x] T6 govec `funcs` 领域与回放、单元测试；反证
- [ ] T7 第 2 部分：`newBaseBuiltinFuncWithTp`/`WithFieldTypes`、`baseBuiltinCastFunc`、`get_function`、`NewFunction*`（随 D08.8 第一批签名与 D08.1 2b）
- [ ] T8 第 2 部分：常量折叠与传播（随 D08.4/D08.5）
- [ ] T9 第 2 部分：`builtin_func_param.go`（随 REGEXP/ILIKE）
- [ ] T10 第 3 部分：grouping sets（随 D08.3、D08.9）
