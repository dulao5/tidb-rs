# D08.1 表达式框架：spec

状态：第 1 部分（规划侧节点：Column、CorrelatedColumn、Constant、Schema，逐行求值，错误与上下文辅助函数）PR 评审中；第 2 部分（ScalarFunction 与求值驱动，N4 §3.2–3.4）待做。

收敛检查（第 1 部分）：D08.1 matched 141/348（6 个 Go 文件中缺 68 个，其余 7 个文件属于第 2、3 部分）；D08.2 的 `collation.go` 11 个。govec `expr` 领域 5,766 条，豁免 220 个（`expr_pending.go`）。

## 1. 范围

tracker 中 D08.1 共 348 个函数（Go 函数体 4,490 行），分布在 14 个 Go 文件中。设计按 [N4](../exceptions/N4-expr-eval/spec.md)（规划侧与 Go 同形；执行侧编译成 RPN；函数体一份、两种驱动）。分三部分：

| 部分 | Go 文件 | 函数数 | 内容 |
|---|---|--:|---|
| 1 | `errors.go`、`context.go` | 3 + 9 | 错误变量，`handle*Error`，`typeCtx` 等上下文辅助函数 |
| 1 | `column.go` | 83 | `Column`、`CorrelatedColumn`：逐行求值、`String`、`HashCode`、`ResolveIndices`、`RemapColumn`、`Coercibility`/`Repertoire` 等 |
| 1 | `constant.go` | 51 | `Constant`、`ParamMarker`、`New*` 构造：逐行求值（含参数标记）、`HashCode`、`Equal`、`ConstLevel`、`StringWithCtx` |
| 1 | `schema.go` | 23 | `Schema`、`KeyInfo`、`MergeSchema` 等 |
| 1 | `expression.go`（部分） | 约 12 | `Expr` 枚举、`ConstLevel`、`IsBinaryLiteral`、`ExprNotNull`、`EvalBool`、`IsEQCondFromIn`、`ColumnInfos2ColumnsAndNames`、`TableInfo2SchemaAndNames`、`CNFExprs` |
| 2 | `scalar_function.go`、`expression.go` 其余、`evaluator.go`、`chunk_executor.go`、`vectorized.go`、`builtin_vectorized.go`、`vs_helper.go` | 约 140 | `ScalarFunction`、`BuiltinFunc`/`SigDesc`、`#[eval_fn]` 宏与逐行/向量驱动、RPN、`EvaluatorSuite`、`VectorizedFilter`；N4 §5 的性能门禁 |
| 3 | `builtin_vec.go`、`builtin_vec_vec.go` | 40 | 向量（VECTOR 类型）函数：随 D08.2 的函数注册一起做（它们是普通的 function class，只是被划进了本块） |

`collation.go` 中的 `collationInfo`（D08.2）是 `Column`、`Constant` 的嵌入字段，第 1 部分一并移植其中 11 个方法与 `deriveCoercibilityFor{Column,Constant}`，converge 记入 D08.2。

## 2. 依赖

- `tidb-sessionctx`：`exprctx::{EvalContext, BuildContext, ParamValues}`、`errctx`（D00.1）。
- `tidb-types`：`Datum`、`FieldType`、转换与比较（D01.1、D01.4）；`tidb-chunk`：`Row`（D01.7）；`tidb-codec`：`EncodeInt`、`HashCode`（D01.8）；`tidb-model`：`TableInfo`、`ColumnInfo`（D06.1）；`tidb-parser`：`CIStr`。
- 不依赖未移植的块：`BuildSimpleExpr`（虚拟生成列的表达式，P3 planner 设置）在 Go 中本来就是包级函数变量，Rust 中是注册回调（未注册时 panic，与 D05.2 `hook_state` 相同）。

## 3. 设计

### 3.1 节点表示（N1 M-EXPR、N4 §3.1）

```rust
pub enum Expr { Column(Column), CorrelatedColumn(CorrelatedColumn), Constant(Constant), ScalarFunction(ScalarFunction) }
pub type ExprRef = Arc<Expr>;
```

- Go 的 `switch e.(type)` 写成 `match`（M-IFACE）；`Expression` 接口的方法是 `Expr` 上的方法，按变体分派。
- `Schema.Columns`、`KeyInfo` 是 `Vec<Arc<Column>>`：Go 在计划节点间共享 `*Column`，修改前 `Clone()`；Rust 不可变共享、写时复制。列的身份仍是 `UniqueID`。
- `hashcode` 缓存：`OnceLock<Vec<u8>>`（N4 §3.5）。`Clone` 复制缓存；`CleanHashCode` 取 `&mut self` 清空。
- `collationInfo` 中 `coer`、`coerInit` 在 Go 中是原子变量（`Coercibility()` 会在共享的节点上惰性初始化）：Rust 同样用 `AtomicI32`/`AtomicBool`，所以 `&self` 即可设置。

### 3.2 逐行求值

- 签名：`eval_int(&self, ctx: &dyn EvalContext, st: &EvalState, row: Row) -> Result<Option<i64>, Error>`，`Ok(None)` = Go 的 `isNull`（N4 §3.7）。其余类型同理；字符串是 `Vec<u8>`（N4 §3.2：任意字节）。
- Go 中"既返回值又返回错误"的分支（如 `Constant.EvalInt` 字符串转换出错时返回截断值和错误）只保留错误：调用方在 `err != nil` 时不读值（第 2 部分的驱动会逐处核对）。
- **关联列**：N4 §3.1/§3.5 规定 `CorrelatedColumn` 不带 `Data` 指针。Go 中 `Data *Datum` 由 Apply 逐行写入；`Clone` 和 `RemapColumn` 共享同一个指针；规划结束时 `ExtractCorColumnsBySchema`（`planner/core`）把同一个外层列的所有关联列的 `Data` 指向同一个 Datum，所以 Apply 每行只写一次，所有引用它的节点都能读到；并行 Apply 的每个 worker 克隆后拿到新的 Datum（在 v8.5.8 上验证过，Lucky 评审 #73）。Rust 中 `CorrelatedColumn` 带槽号 `CorrSlot`，**取值是创建时外层列的 `UniqueID`**（不再用全局计数器），`Clone`/`RemapColumn` 保留槽号；值在 `EvalState` 中按槽号查找，每个 worker 一个 `EvalState`。这样得到的正是 `ExtractCorColumnsBySchema` 统一之后的共享关系，表达式本身不可变（L2）。
- 求值上下文用 `&dyn EvalContext`（`exprctx` 的 trait）；`typeCtx(ctx)` 等辅助函数是一行转发。

### 3.3 `ScalarFunction` 的过渡形态

第 1 部分只定义 `ScalarFunction { func_name, ret_type, args }`，用于 `match` 和读取参数（`ExprReferenceSchema`、`IsEQCondFromIn`、`GcColumnExprIsTidbShard`）。求值、`Equal`、`String`、`HashCode` 等是**临时桩**（宪章第四条 3），调用时 panic 并指向第 2 部分。依赖 `ScalarFunction.Equal` 的 `Column.EqualByExprAndID`、`resolveIndicesByVirtualExpr`、`GetUsedList` 留到第 2 部分。

## 4. 偏离与豁免

1. **向量化方法不移植**（N4 §3.3 M-EVAL）：`Column`/`CorrelatedColumn`/`Constant` 的 24 个 `VecEval*` 由第 2 部分的向量驱动取代。tracker 豁免（`expr_pending.go`）。
2. **cascades 的 `Hash64`/`Equals`**：`base.HashEquals` 只被 cascades 规划器使用，`pkg/planner/cascades/base` 是 EXC.B（非默认路径，不移植）。`Column`、`CorrelatedColumn`、`Constant`、`collationInfo` 的 8 个方法豁免。
3. **`MemoryUsage`**：Go 用 `unsafe.Sizeof` 估算，Rust 结构体大小不同。移植公式，固定部分取 Go 在 64 位目标上的值（`Column` 168、`Constant` 192、`Schema` 72，govec 生成器实测），govec 比较数值。`hashcode` 的容量在 Go 中由 `append` 的扩容决定，Rust 的 `Vec` 不同：只在算过 `HashCode` 之后才可能差几个字节（`Column` 固定为 9，与 Go 相同）；`Schema.MemoryUsage` 的切片容量同理，govec 不比较。
   - **关联列**：值不在节点中（§3.2），`CorrelatedColumn.MemoryUsage` 计入零值 Datum，Go 计入当前值。govec 的 `corr` 记录带 `dmem`，回放比较"Rust 的值 − 零值 + dmem"。`memory_usage` 不带 `EvalState` 参数：Go 在规划期估算时读到的 `Data` 本来就是过期的值（Apply 还没写或上一行的值），带上 state 也不会更准（Lucky 评审 #73）。
4. **`Clone` 不深拷贝 `RetType`**：Go 的 `Constant.Clone` 复制 `RetType`，`Column.Clone` 共享 `*FieldType`。Rust 中 `FieldType` 按值存放，两者都复制。**移植注意**：Go 中 `Column.Clone` 之后再原地修改 `RetType` 的调用方（修改会影响共享它的原节点），移植时要逐个检查，必要时显式修改两处（Lucky 评审 #73）。
5. **值与错误同时返回**的分支只保留错误（§3.2）。
6. `intest.Assert*`：只在 Go 的 intest 构建中生效。`ParamMarker` 读参数失败时的 `AssertNoError` 去掉（tidb-rs 的测试是 debug 构建，`debug_assert!` 会让它 panic，和 Go 的生产行为不同）；其余写成 `debug_assert!`。
7. **`Constant.GetType` 返回 `Option`**：参数读不到时 Go 记日志并返回 nil `*FieldType`，Rust 返回 `None`；Go 解引用它的调用方在 Rust 中 `expect`（同样 panic）。
8. **`ColumnInfos2ColumnsAndNames`**：`BuildSimpleExpr` 是注册的回调（§2），参数是列信息而不是 ast 节点（解析生成列表达式也由 planner 一侧完成）。
9. **关联列在 `ExtractCorColumnsBySchema` 之前的状态**：Go 中新建的关联列各有自己的 Datum，统一之前同一外层列的两个关联列互不影响；Rust 按外层列的 `UniqueID` 取槽，从创建起就共享。规划期在统一之前不会写 `Data`，所以不可观察。
