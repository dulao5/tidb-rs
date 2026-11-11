# D08.2：plan

## 第 1 部分（本 PR）

1. `tools/expr-gen/gen_registry.py`：从 Go 源码生成 `crates/parser/src/ast/funcs.rs`（`tidb_parser::ast::funcs`，`pkg/parser/ast/functions.go` 的 345 个函数名常量；parser 自己也用，所以放在 tidb-parser，tidb-expression 以 `ast_funcs` 再导出，#80 评审）和 `builtin_registry_gen.rs`（`funcs` 的 308 个条目）。D08.1 中手写的 `ast_func` 模块改为这里的再导出。
2. `builtin.rs`：`BaseFunctionClass`、`newBaseBuiltinFunc`、`newBaseBuiltinFuncWithFieldType`、`newReturnFieldTypeForBaseBuiltinFunc`、`adjustNullFlagForReturnType`、`setDecimalAndFlenFor*`、`BuiltinFuncCache`；`BuiltinFunc::base`/`with_sig`（`eval/mod.rs`）。
3. `builtin_registry.rs`：注册表查询。
4. `collation.rs`：函数的 collation 推导。
5. `function_traits.rs`：函数集合与 `IsDeferredFunctions`；`unFoldableFunctions` 从 `scalar_function.rs` 移来。
6. govec `funcs` 领域（tracker `govec/funcs-D08.2`），Rust 回放与单元测试。

回放：`cargo test -p tidb-expression --test govec_funcs`；额外的种子：`GOVEC_FUNCS_FILES=a.jsonl.gz,b.jsonl.gz`。

## 第 2 部分（交接）

与 D08.1 2b、D08.8 CAST 的第一批签名一起（D08.1 plan.md "2b 的前提"）：

1. `newBaseBuiltinFuncWithTp`、`newBaseBuiltinFuncWithFieldTypes`（`WrapWithCastAs*`、`BuildCastFunction`）、`baseBuiltinCastFunc`（`metadata` 返回 `tipb.InUnionMetadata`，`BuiltinFunc` 需要加 `in_union`）。
2. 注册表条目加 `get_function`；`NewFunction*`。
3. 常量折叠 `constant_fold.go`（`FoldConstant` 与 IF/IFNULL/CASE/ISNULL 的特殊处理）、常量传播 `constant_propagation.go`：需要比较、逻辑函数（D08.4、D08.5）真实求值。
4. `builtin_func_param.go`：随 REGEXP/ILIKE。

## 第 3 部分

`grouping_sets.go`：随 D08.3（`ToPB`）与 GROUPING 函数（D08.9）。
