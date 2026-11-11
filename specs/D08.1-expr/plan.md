# D08.1：plan

## 第 1 部分（本 PR）

1. 新 crate `tidb-expression`（`crates/expression`，rank 8，layers.toml 中已规划）。
2. `errors.rs`、`context.rs`、`collation.rs`（collationInfo 部分，D08.2）、`column.rs`、`constant.rs`、`schema.rs`、`expression.rs`、`scalar_function.rs`（过渡形态，见 spec §3.3）。
3. govec `expr` 领域（tracker `govec/expr-D08.1`）：节点的逐行求值、`HashCode`、`String`/`StringWithCtx`、`ConstLevel`、`Equal`、`Coercibility`/`Repertoire`、`MemoryUsage`、Schema 的各个操作；`TableInfo2SchemaAndNames` 用 model 领域中真实集群导出的表结构做输入。Rust 回放 `tests/govec_expr.rs`。

回放：`cargo test -p tidb-expression --test govec_expr`；额外的种子：`GOVEC_EXPR_FILES=a.jsonl.gz,b.jsonl.gz`。

## 第 2 部分（交接）

见 spec §1 的表；先做 N4 §5 的门禁（宏原型 + 5 个热点签名的基准），再移植 `scalar_function.go` 与驱动。

## 第 1 部分完成情况

- crate `tidb-expression`（rank 8）：`errors.rs`、`context.rs`、`collation.rs`（D08.2 的 collationInfo）、`column.rs`、`constant.rs`、`schema.rs`、`expression.rs`、`scalar_function.rs`（过渡形态）、`size.rs`。
- 第 1 部分没有移植、留给第 2 部分的 D08.1 函数（6 个已开始的文件中）：`expression.go` 中构造函数调用的部分（`ComposeCNFCondition`、`EvaluateExprWithNull`、`wrapWithIsTrue`、`VecEvalBool`/`toBool`、`EvalExpr`、`Split*`/`Flatten*`、`BuildOption` 系列、`NewValuesFunc`、`PropagateType`、`StringifyExpressionsWithCtx`、`Assignment`）；`Column.EqualByExprAndID`、`ResolveIndicesByVirtualExpr`、`GetUsedList`（需要 `ScalarFunction.Equal`）；`wrapEvalAssert` 与 `assertionEvalContext`（需要 `builtinFunc`）。
- 第 2 部分开始前先做 N4 §5 的门禁：宏原型 + 5 个热点签名与 Go `BenchmarkVectorizedBuiltin*` 对比。

## 第 2 部分：N4 §5 门禁（2026-11-11，通过）

门禁仍按 N4 §5 与 Go 比较，不放宽。第一轮（宏只有逐行调用函数体的一种循环）有 6 项低于 Go；Lucky 在 #75 评审中指出差距来自循环的形状（逐行提前返回、在循环内判断符号组合），不是 rustc。按评审意见给 `#[eval_fn]` 加了 `checked` 形式后通过。

**框架**（第 2 部分继续用）：
- 新 crate `tidb-expression-macros`（rank 0）：`#[eval_fn(Int|Real|String[, lazy][, eager][, checked(..)])]`，函数体一份，生成逐行驱动 `__row` 和向量驱动 `__vec`。
  - 默认形式：严格签名（`i64`/`f64`/`&[u8]` 参数），向量驱动按空位图每 8 行一块调用函数体。
  - `lazy`：函数体拿 `args: &mut impl Args` 自己求参数（IF、CASE、CONCAT 这类）。
  - `eager`：逐行驱动先求出全部参数再看 NULL（Go 的 `CompareInt` 这类）。
  - **`checked(nflags = N, flags = f, err = e)`**：函数体是 `fn name<const FL: u8>(a, ..) -> (值, 是否出错)`，`FL` 是签名的标志组合（如两个参数的 unsigned 标志），`e(ctx, f, a, ..)` 是构造 Go 错误的冷路径函数。向量驱动每个 chunk 读一次标志，选择为该组合单态化的循环；循环不提前退出，写出每一行的值，把非 NULL 行的出错标志 OR 起来；只有置位时才第二遍扫描，找出第一个非 NULL 的出错行并返回它的错误——这正是 Go 向量版停下的那一行和那个错误。逐行驱动对每行做同样的判断。只用于没有逐行副作用（warning、状态）的函数体，其余仍用默认形式。
- `SigView`：`RowSig`（逐行取参数类型）/`VecSig`（每 chunk 取一次）；`scratch()` 提供本次驱动调用内的工作对象（逐行驱动每行一个、向量驱动每 chunk 一个，对应 Go 逐行版每行新建、向量版每次调用新建的对象，例如 LIKE 的 `WildcardPattern`）。
- `RpnProgram`/`RpnState`、`BuiltinFunc`（`baseBuiltinFunc` 字段，缓存 collator）、`ScalarFunction` 持有 `BuiltinFunc`。
- `tests/eval_drivers.rs`：两种驱动逐行一致；`checked` 形式在 NULL 位下藏着溢出值、出错行在不同位置、行数不是 8 的倍数时，两种驱动返回同一个错误（反证：第二遍扫描不跳过 NULL 行、或者从后往前找，测试都会失败）。

**基准**（同一台 Mac mini（arm64），Go 与 Rust 交替运行；每批 1024 行，输入分布与 Go 的 generator 相同；ns/批；Go 原始数据在 `gate-go-bench.txt`）：

| 签名 | Go 向量 | Rust 向量驱动 | 比值 | Go 逐行 | Rust 逐行驱动 |
|---|--:|--:|--:|--:|--:|
| PlusInt 有符号+有符号 | 1,450 | 482 | 3.0× | 13,070 | 9,460 |
| PlusInt 无符号+无符号 | 770 | 468 | 1.6× | 10,480 | 9,390 |
| PlusInt 有符号+无符号 | 1,000 | 673 | 1.5× | 11,100 | 9,460 |
| PlusInt 无符号+有符号 | 1,000 | 675 | 1.5× | 10,860 | 9,560 |
| LTInt | 1,920 | 376 | 5.1× | 14,100 | 11,170 |
| CastIntAsReal | 1,090 | 268 | 4.1× | 8,960 | 6,330 |
| Concat（3 个字符串） | 37,700 | 13,360 | 2.8× | 49,640 | 85,290 |
| Like | 59,300 | 41,630 | 1.4× | 78,700 | 94,360 |

- 向量驱动 8 项全部高于 Go（门禁：不低于 Go）；N4 §5 "吞吐 ≥ 1.5 倍"的目标，Like 1.4× 略低，其余达到。
- 逐行驱动：Concat、Like 低于 Go。Concat 的逐行驱动对每个参数返回新分配的 `Vec<u8>`（`eval_string`）；Like 逐行驱动每行新建 pattern（与 Go 逐行版相同）。逐行驱动用在常量折叠和不可向量化的表达式上，记为第 2 部分的后续项，不阻塞门禁。
- **Like**：Go 的 `builtinLikeSig` 只在 pattern 和 escape 参数是常量时缓存编译结果（`patternCache`）；基准里 pattern 是列，Go 也每行编译。差距原因是 `tidb-types` 的 `[]rune` 转换没有一次分配到位、pattern 每行重新分配缓冲区：`to_runes` 改为按准确长度一次分配（ASCII 走快路径），`DerivedBinPattern::compile` 复用缓冲区（`compile_pattern_inner_into`，只影响分配，L2）；签名一侧用 `scratch()` 让向量驱动整块复用一个 pattern（Go 向量版同样如此）。常量 pattern 的缓存（`isMemorizedPattern`）在移植 LIKE 签名时按 N4 §3.5 放进 `RpnState` 的签名缓存。
- 这 5 个函数体只是门禁原型，正式移植在 D08.4/D08.5/D08.6/D08.8；移植时用 govec 对照 Go 的值、错误和 warning。

## 第 2 部分 2a（本 PR）完成情况与交接

**已完成**：
- `ScalarFunction`（`scalar_function.go`）：结构改为 `func_name`、`ret_type`、`function: BuiltinFunc` 和两个 hash 缓存；`Clone`、`GetArgs`、`Vectorized`、`String`/`StringWithCtx`、`GetType`/`GetStaticType`、`Equal`、`IsCorrelated`、`ConstLevel`（`unFoldableFunctions`）、`Decorrelate`、`Traverse`、`Eval` 与 8 个 `EvalXxx`、`VecEvalXxx`（统一入口 `vec_eval`）、`HashCode`/`ReHashCode`、`CanonicalHashCode`/`simpleCanonicalizedHashCode`、`CleanHashCode`、`ResolveIndices`(`ByVirtualExpr`)、`RemapColumn`、`GetSingleColumn`、collationInfo 的 9 个方法、`MemoryUsage`、`ScalarFuncs2Exprs`、`ExpressionsSemanticEqual`。**`part2_stub!` 已全部删除。**
- `BuiltinFunc` = `baseBuiltinFunc`（D08.2 的 `builtin.go`）：`args`、`tp`、`pbCode`、`ctor`、`collationInfo`；`equal`、`cloneFrom`、`MemoryUsage`、`getArgs`、`getRetTp`、`vectorized`、`isChildrenVectorized`、`collator`、`SetCollator`、`PbCode`；`evalXxx` 未实现时返回 Go 的 `baseBuiltinFunc.evalXxx() should never be called` 错误。`SigDrivers` 覆盖 8 种求值类型（宏目前生成 Int/Real/String，其余类型随第一个用到的函数族加上），另有 `SigDrivers::None`（只用于规划的签名和测试）。
- `Column.EqualByExprAndID`、`Column.ResolveIndicesByVirtualExpr`、`GetUsedList`；`Expr::resolve_indices_by_virtual_expr` 分派。
- 评审 #73 的后续：关联列的槽按外层列 `UniqueID`（spec §3.2、§4 第 9 条）。
- govec：`expr` 领域新增 `sf` 2,000、`sfpair` 700、`vcol` 400、`sizes` 1 条（共 8,866 条）。Go 用 `NewFunctionBase` 按配方建树，Go 自己选签名；Rust 按 Go 输出的树（签名类型名、返回类型、collation 等）用只有描述的签名重建，比较规划侧方法的结果。39 个配方 Go 建不出来（参数类型不被函数接受），跳过。
- 收敛检查：D08.1 matched 197/348（第 1 部分 141）。

**偏离**（补进 spec §4）：
- `ReHashCode` 的 `grouping` 分支需要 `BuiltinGroupingImplSig`（D08.9），之前 panic（登记的豁免）。
- `ScalarFunction.MemoryUsage`：hash 码按长度计（Go 按 `cap`）；`baseBuiltinFunc.MemoryUsage` 不计 `bufAllocator`（tidb-rs 没有每个签名一个列池，M-POOL）。govec 在调用 `Vectorized()` 之前取 Go 的值。
- `isChildrenVectorized` 每次计算（Go 用 `sync.Once` 缓存；签名建好后参数不变，结果相同）。
- `Clone` 与 Go 一样复制 hash 缓存；`Decorrelate`/`RemapColumn` 返回新节点并清空缓存（Go 原地修改后清空）。
- `emptyScalarFunctionSize` 104、`emptyBaseBuiltinFunc` 152、`onceSize` 12：govec `sizes` 记录 Go 的实测值，回放时核对。

**第 2 部分 2b（下一个 PR）**：
1. `evaluator.go`（`EvaluatorSuite`、`defaultEvaluator`、`GetOptionalEvalPropsForExpr`）、`chunk_executor.go`（`VectorizedExecute`、`VectorizedFilter(ConsiderNull)`、`evalOneVec/Column/Cell`、`executeToXxx`、`Vectorizable`、`HasGetSetVarFunc` 等）、`expression.go` 的 `VecEvalBool`/`toBool`/`EvalExpr`/`implicitEvalReal`、`vectorized.go` 的 `genVecFromConstExpr`（现在在 `rt::fill_constant`）、`builtin_vectorized.go` 的 `vecEvalIntByRows`/`vecEvalStringByRows`。这些要用真实的签名才能和 Go 对照：和 D08.2（函数注册、`newBaseBuiltinFunc`）、D08.8 CAST 的第一批签名一起做，govec 用真实表达式在 chunk 上求值。
2. `NewFunction*`/`newFunctionImpl`/`typeInferForNull`：需要 D08.2 的 `funcs` 表。
3. 宏：Decimal/Time/Duration/JSON/VectorFloat32 的结果类型；逐行驱动 Concat/Like 慢于 Go 的两项（见门禁表）。
4. 第 3 部分：VECTOR 函数（随 D08.2）。
