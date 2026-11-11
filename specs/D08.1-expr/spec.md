# D08.1 表达式框架：spec

状态：第 1 部分已合并（#73）；第 2 部分 2a（`#[eval_fn]` 与两种驱动、RPN、ScalarFunction 规划侧方法）见 plan.md；2b（EvaluatorSuite、chunk_executor、VecEvalBool、NewFunction*）随 D08.2/D08.8 的第一批签名。

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

### 3.4 `#[eval_fn]` 的 `checked` 写法：使用条件（Lucky 评审 #75）

`checked` 的向量驱动第一遍不提前退出，第二遍只在出错标志置位时找第一个出错行（plan.md "第 2 部分"）。它与 Go 一致的前提是 Go 的向量版正好在"第一个非 NULL 的出错行"停下、此前没有可见的副作用。所以一个签名**只有同时满足下面三条**才能用 `checked`，移植每个函数族时逐个函数核对，并在该签名旁注明依据：

1. **Go 的向量版在会出错的检查之前跳过 NULL 行**。有些 Go 向量版会对 NULL 位下面的无效值做检查（例如先整列计算、再统一处理 NULL），这样的签名不能用 `checked`。
2. **出错之前没有逐行的副作用**：`HandleTruncate`、`AppendWarning`、改写状态等。因此非严格模式下的大部分 CAST 和字符串函数不适用，仍用默认的单遍形式。
3. **Go 的逐行版与向量版报同样的错误**。如果两者不同（Go 的 bug），两个驱动各自跟随对应的 Go 版本（逐行驱动跟 `evalXxx`，向量驱动跟 `vecEvalXxx`），govec 两种模式都比较，并登记在 §4。

   - **`SigView::vector()` 分支只用于小差别**（Lucky 评审 #82）：两个 Go 版本只差几行（打印的参数类型、一个判断条件、错误交给哪个 handler）时，用同一个函数体加 `if f.vector()`；如果控制流或错误路径在结构上不同，就写两个函数体（行驱动和向量驱动各自生成），不要在一个函数体里交织两套逻辑。每个用到 `vector()` 的签名，govec 必须有记录在两种模式下都走到两边的分支（govec 每条记录都在两种模式下求值，所以只需输入能走到差异所在的分支；在签名旁注明是哪类记录）。

每个采用 `checked` 的函数族，govec 必须包含两类记录：**出错的值藏在 NULL 位下面**；**第一个出错行不在第 0 行**（同一批中后面还有出错行）。

### 3.5 带状态的签名的 `Clone`（Lucky 评审 #75）

`SigDesc` 是无状态的描述；有状态的签名（LIKE/ILIKE/REGEXP 的 pattern 缓存、JSON_SCHEMA_VALID 的 schema 缓存、`rand` 的种子、`nonDeterministic` 标记等）在 `BuiltinFunc` 中带自己的状态。Go 每个签名的 `Clone` 对这些状态的处理各不相同（共享、复制或重置）：第一个带状态的签名进来时逐个对照 Go 的 `Clone`，在签名旁注明是哪一种，并在 govec 中加一个"克隆后分别求值再比较"的操作。已核对：`builtinFuncCache`（LIKE、ILIKE、REGEXP、JSON_SCHEMA_VALID）在 Go 的 `Clone` 中是新建的结构体，只复制 `baseBuiltinFunc`，所以克隆后缓存为空——Rust `BuiltinFuncCache::clone` 返回空缓存（D08.2，`builtin_unit.rs` 覆盖）。

## 4. 偏离与豁免

1. **向量化方法不移植**（N4 §3.3 M-EVAL）：`Column`/`CorrelatedColumn`/`Constant` 的 `VecEval*` 由向量驱动（RPN）取代。tracker 豁免（`expr_pending.go`）。例外：`Column.VecEvalInt/Real/String` 中会转换的分支（hybrid 列读成 int/string、FLOAT 列读成 real）已移植（D08.8 第 2 部分），RPN 按父签名读取的求值类型（`SigDrivers::arg_eval_type`）选择：转换（`RpnNode::ColumnConv`）、照搬，或者在编译时 panic——列的存储与读取类型不符、Go 也不转换的组合（例如 FLOAT 列读成 int），Go 的函数类不会构造这种调用，RPN 拒绝而不是让签名误读字节（Lucky 评审 #82）。
   - **读取类型从哪里来**（Lucky 评审 #84）：strict 函数体的参数类型由宏生成（`i64`/`f64`/`&MyDecimal`/`&[u8]`），不会与函数体不一致。`lazy` 函数体默认按每个参数**自己的**求值类型读取（`arg_eval_type` 返回 `None`），目前都是对的；如果 Go 的向量版不经 CAST 就把某个参数按别的类型读（例如把 STRING 参数按 INT 读），要在属性中显式声明 `#[eval_fn(.., lazy, args(Int, String))]`。**变参签名**（CONCAT、COALESCE 等）超出声明列表的参数重复最后一个类型，不回退到参数自己的类型（`SigDrivers::arg_eval_type`，`eval_drivers.rs` 覆盖）。
   - **编译时 panic 的性质**：存储与读取类型不符的组合是不变量被破坏，即 planner 或函数类的 bug，不是用户错误；M-PANIC 在任务边界 `catch_unwind`，只影响这一条语句（Go 在同样情况下会把复制来的存储读错）。
2. **cascades 的 `Hash64`/`Equals`**：`base.HashEquals` 只被 cascades 规划器使用，`pkg/planner/cascades/base` 是 EXC.B（非默认路径，不移植）。`Column`、`CorrelatedColumn`、`Constant`、`collationInfo` 的 8 个方法豁免。
3. **`MemoryUsage`**：Go 用 `unsafe.Sizeof` 估算，Rust 结构体大小不同。移植公式，固定部分取 Go 在 64 位目标上的值（`Column` 168、`Constant` 192、`Schema` 72，govec 生成器实测），govec 比较数值。`hashcode` 的容量在 Go 中由 `append` 的扩容决定，Rust 的 `Vec` 不同：只在算过 `HashCode` 之后才可能差几个字节（`Column` 固定为 9，与 Go 相同）；`Schema.MemoryUsage` 的切片容量同理，govec 不比较。
   - **关联列**：值不在节点中（§3.2），`CorrelatedColumn.MemoryUsage` 计入零值 Datum，Go 计入当前值。govec 的 `corr` 记录带 `dmem`，回放比较"Rust 的值 − 零值 + dmem"。`memory_usage` 不带 `EvalState` 参数：Go 在规划期估算时读到的 `Data` 本来就是过期的值（Apply 还没写或上一行的值），带上 state 也不会更准（Lucky 评审 #73）。
4. **`Clone` 不深拷贝 `RetType`**：Go 的 `Constant.Clone` 复制 `RetType`，`Column.Clone` 共享 `*FieldType`。Rust 中 `FieldType` 按值存放，两者都复制。**移植注意**：Go 中 `Column.Clone` 之后再原地修改 `RetType` 的调用方（修改会影响共享它的原节点），移植时要逐个检查，必要时显式修改两处（Lucky 评审 #73）。
5. **值与错误同时返回**的分支只保留错误（§3.2）。
6. `intest.Assert*`：只在 Go 的 intest 构建中生效。`ParamMarker` 读参数失败时的 `AssertNoError` 去掉（tidb-rs 的测试是 debug 构建，`debug_assert!` 会让它 panic，和 Go 的生产行为不同）；其余写成 `debug_assert!`。
7. **`Constant.GetType` 返回 `Option`**：参数读不到时 Go 记日志并返回 nil `*FieldType`，Rust 返回 `None`；Go 解引用它的调用方在 Rust 中 `expect`（同样 panic）。
8. **`ColumnInfos2ColumnsAndNames`**：`BuildSimpleExpr` 是注册的回调（§2），参数是列信息而不是 ast 节点（解析生成列表达式也由 planner 一侧完成）。
9. **关联列在 `ExtractCorColumnsBySchema` 之前的状态**：Go 中新建的关联列各有自己的 Datum，统一之前同一外层列的两个关联列互不影响；Rust 按外层列的 `UniqueID` 取槽，从创建起就共享。规划期在统一之前不会写 `Data`，所以不可观察。
10. **`ScalarFunction.MemoryUsage`**：hash 码按长度计（Go 按 `cap`）；`baseBuiltinFunc.MemoryUsage` 不计 `bufAllocator`（M-POOL，没有每签名的列池）；`isChildrenVectorized` 每次计算（Go 用 `sync.Once` 缓存）。
11. **`ReHashCode` 的 `grouping` 分支**：等 `BuiltinGroupingImplSig`（D08.9），之前 panic（登记的豁免）。
12. **`Decorrelate`/`RemapColumn`/`resolveIndices*`**：Go 原地修改克隆出来的节点；Rust 返回参数替换后的新节点，hash 缓存的处理与 Go 相同（`Decorrelate`、`RemapColumn` 清空，`resolveIndices` 保留）。
