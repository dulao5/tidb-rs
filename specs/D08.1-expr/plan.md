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
- govec：`expr` 领域新增 `sf` 2,000、`sfpair` 700、`vcol` 400、`sizes` 1 条（共 8,867 条）。Go 用 `NewFunctionBase` 按配方建树，Go 自己选签名；Rust 按 Go 输出的树（签名类型名、返回类型、collation 等）用只有描述的签名重建，比较规划侧方法的结果。39 个配方 Go 建不出来（参数类型不被函数接受），跳过。
- 收敛检查：D08.1 matched 197/348（第 1 部分 141）。

**偏离**（补进 spec §4）：
- `ReHashCode` 的 `grouping` 分支需要 `BuiltinGroupingImplSig`（D08.9），之前 panic（登记的豁免）。
- `ScalarFunction.MemoryUsage`：hash 码按长度计（Go 按 `cap`）；`baseBuiltinFunc.MemoryUsage` 不计 `bufAllocator`（tidb-rs 没有每个签名一个列池，M-POOL）。govec 在调用 `Vectorized()` 之前取 Go 的值。
- `isChildrenVectorized` 每次计算（Go 用 `sync.Once` 缓存；签名建好后参数不变，结果相同）。
- `Clone` 与 Go 一样复制 hash 缓存；`Decorrelate`/`RemapColumn` 返回新节点并清空缓存（Go 原地修改后清空）。
- `emptyScalarFunctionSize` 104、`emptyBaseBuiltinFunc` 152、`onceSize` 12：govec `sizes` 记录 Go 的实测值，回放时核对。

**性能门禁的测量规则**（Lucky 评审 #75）：同一台机器上 Go 与 Rust 交替运行，每个签名在**多个独立进程**中各跑若干次、取最小值（虚拟机或混合大小核的机器上，单个进程偶尔会被调度到能效核，慢 3–4 倍）；每次改动驱动或宏之后，在 amd64 CI（`ubuntu-latest`）上再确认一次。

**第 2 部分 2b（下一个 PR）**：
1. `evaluator.go`（`EvaluatorSuite`、`defaultEvaluator`、`GetOptionalEvalPropsForExpr`）、`chunk_executor.go`（`VectorizedExecute`、`VectorizedFilter(ConsiderNull)`、`evalOneVec/Column/Cell`、`executeToXxx`、`Vectorizable`、`HasGetSetVarFunc` 等）、`expression.go` 的 `VecEvalBool`/`toBool`/`EvalExpr`/`implicitEvalReal`、`vectorized.go` 的 `genVecFromConstExpr`（现在在 `rt::fill_constant`）、`builtin_vectorized.go` 的 `vecEvalIntByRows`/`vecEvalStringByRows`。这些要用真实的签名才能和 Go 对照：和 D08.2（函数注册、`newBaseBuiltinFunc`）、D08.8 CAST 的第一批签名一起做，govec 用真实表达式在 chunk 上求值。
2. `NewFunction*`/`newFunctionImpl`/`typeInferForNull`：需要 D08.2 的 `funcs` 表。
3. 宏：Decimal/Time/Duration/JSON/VectorFloat32 的结果类型；逐行驱动 Concat/Like 慢于 Go 的两项（见门禁表）。
4. 第 3 部分：VECTOR 函数（随 D08.2）。

## 2b 的前提（D08.2 第 1 部分之后）

D08.2 第 1 部分（`specs/D08.2-funcs`）移植了函数注册表 `funcs`（类名、参数个数、显示名）、`newBaseBuiltinFunc`/`newBaseBuiltinFuncWithFieldType`、`newReturnFieldTypeForBaseBuiltinFunc` 和函数的 collation 推导。2b 与 D08.2 第 2 部分一起做，顺序：

1. D08.8 CAST 的第一批签名（`WrapWithCastAs*` 用到的 `castAsXxxFunctionClass.getFunction` 与签名体），以及 `newBaseBuiltinFuncWithTp`/`WithFieldTypes`、`baseBuiltinCastFunc`（D08.2）。
2. `NewFunction*`/`newFunctionImpl`/`typeInferForNull`（D08.1），注册表条目加上 `get_function`；未移植的函数族返回明确的"未实现"错误（临时桩，宪章第四条 3），并在 govec 中跳过。
3. `EvaluatorSuite`、`chunk_executor`、`VecEvalBool`/`toBool`/`EvalExpr`（D08.1）：govec 用 CAST 签名在 chunk 上逐行和向量求值，对照 Go。
4. 常量折叠与传播（D08.2 `constant_fold.go`、`constant_propagation.go`）：需要比较函数（D08.4）。

## D08.8 第 1 部分之后（2b 的前提，进展）

- 第 1 步部分完成：CAST 到 INT/REAL/DECIMAL 的签名与 `BuildCastFunction*`、`WrapWithCastAs{Int,Real,Decimal}`、`FoldConstant`（`specs/D08.8-cast`）。框架的变化：`#[eval_fn]` 支持 `Decimal` 结果与 `&MyDecimal` 参数；`SigView::vector()` 让函数体在 Go 的向量版与行版对用户可见地不同时各自跟随（§3.4 (c)，govec 在 CAST 中找到 4 处）；`PropagateType` 已移植。
- 仍待：CAST 到 STRING/TIME/DURATION/JSON（`newBaseBuiltinFuncWithTp` 要包这些 CAST）、RPN 列引用按签名的求值类型转换（FLOAT/hybrid 列，D08.8 plan 第 2 部分第 1 条），然后 `NewFunction*`、EvaluatorSuite、chunk_executor。

## 第 2 部分 2b（完成，D08.8 2b-3c）

- `evaluator.rs`：`EvaluatorSuite`（列表达式用 `ColumnSwapHelper` 换入，其余在构建时编译为 `RpnProgram`）、`GetOptionalEvalPropsForExpr`。
- `chunk_executor.rs`：`Vectorizable`/`checkSequenceFunction`/`HasGetSetVarFunc`/`HasAssignSetVarFunc`、`VectorizedExecute`、`evalOneVec`（BIT/FLOAT/ENUM/SET 结果的转换）/`evalOneColumn`/`evalOneCell`、`executeToXxx`、`VectorizedFilter[ConsiderNull]`/`rowBasedFilter`/`vectorizedFilter`。
- `expression.rs`：`VecEvalBool`、`toBool`、`EvalExpr`、`implicitEvalReal`；`eval/mod.rs`：`RpnProgram::compile_as`。
- govec `cast` 领域追加 `evsuite`/`evfilter`/`evbool`/`evexpr`/`evexec`（tracker `cast2b3c.go`）。
- #95 后续：`function_class_registry` 测试（每个 `impl FunctionClass` 都登记在 `PORTED_FUNCTION_CLASSES`）；`newFunctionImpl` 中 `debug_assert` 签名类型不是 `TypeUnspecified`。
- 仍在 2b 范围之外：`builtin_vectorized.go` 的 `vecEvalIntByRows`/`vecEvalStringByRows`、局部列池（随第一个用到它们的函数族；列池按 M-POOL 不移植）；`Column.EvalVirtualColumn`（随生成列）。


## 门禁重测（2026-11-14，P1 收尾；**未通过**）

Lucky 在 #163 评审中不同意把门禁移交 A5：先做 T7a，再按上面的测量规则重测，然后才能勾掉 P1。指标不放宽。

**方法**：`scripts/eval-gate.sh`（Go 与 Rust 交替运行，各 5 个独立进程，Go 每进程 3 个样本、Rust 为 criterion 的点估计，取最小值；`scripts/eval_gate_summary.py` 逐项对照门槛）。Go 一侧是 v8.5.8 `pkg/expression` 的 `BenchmarkVectorizedBuiltin*Func`，外加 `crates/expression/benches/go/gate_vec_test.go`（距离函数 3 维与 768 维，同一个 `newVectorFloat32RandGener`）；Rust 一侧是 `benches/eval_gate.rs`。测量集比 #75 多了三类：已移植的 PlusInt/LTInt/CastIntAsReal 签名（`-port`，#75 时只有原型）、四个距离函数（3 维、768 维，T7a）、`RpnProgram::compile` 与每 chunk 的堆分配次数。
- arm64：本机 Mac mini（Apple M4 Pro，12 核），go1.25.12，rustc 1.99.1；各轮开始时 load average 2.1–4.6（本机常驻负载约 2）。
- amd64：CI `ubuntu-24.04`（AMD EPYC 7763，4 核），workflow `eval-gate.yml`（run 43491177313，产物 `eval-gate-amd64`）。
- 完整输出：`gate-rerun-arm64.md`、`gate-rerun-amd64.md`。

**门槛**（N4 §5 与负责人确认的口径）：向量驱动每个签名不低于 Go（Go/Rust ≥ 1.0）；Rust 一侧有堆分配的基准差距 ≤ 5%（≥ 0.95，D01.2 spec §5 的口径）；吞吐 ≥ 1.5 倍为目标（逐项列出，不作为通过条件，与 #75 相同）；定长结果每 chunk 除结果列外 0 次堆分配；编译每个节点 ≤ 1 µs。逐行驱动按 #75 不计入门禁，只列出。

### 向量驱动（门禁；ns 或 µs / 1024 行）

| 签名 | 门槛 | arm64 Go | arm64 Rust | arm64 比值 | amd64 Go | amd64 Rust | amd64 比值 | 结果 |
|---|---|--:|--:|--:|--:|--:|--:|:-:|
| PlusInt-SS | ≥ 1.0× | 1,380 ns | 477 ns | 2.89× | 2,107 ns | 1,172 ns | 1.80× | 通过 |
| PlusInt-UU | ≥ 1.0× | 738 ns | 459 ns | 1.61× | 1,551 ns | 1,316 ns | 1.18× | 通过 |
| PlusInt-SU | ≥ 1.0× | 980 ns | 669 ns | 1.47× | 2,007 ns | 1,676 ns | 1.20× | 通过 |
| PlusInt-US | ≥ 1.0× | 985 ns | 669 ns | 1.47× | 1,866 ns | 1,683 ns | 1.11× | 通过 |
| LTInt | ≥ 1.0× | 1,803 ns | 370 ns | 4.87× | 3,020 ns | 755 ns | 4.00× | 通过 |
| CastIntAsReal | ≥ 1.0× | 1,069 ns | 265 ns | 4.03× | 1,842 ns | 516 ns | 3.57× | 通过 |
| Concat | ≥ 0.95×（含分配） | 35.59 µs | 13.30 µs | 2.67× | 88.44 µs | 18.40 µs | 4.81× | 通过 |
| Like | ≥ 0.95×（含分配） | 54.72 µs | 41.00 µs | 1.33× | 100.98 µs | 62.23 µs | 1.62× | 通过 |
| PlusInt-SS-port | ≥ 1.0× | 1,380 ns | 1,097 ns | 1.26× | 2,107 ns | 1,737 ns | 1.21× | 通过 |
| PlusInt-UU-port | ≥ 1.0× | 738 ns | 454 ns | 1.63× | 1,551 ns | 1,387 ns | 1.12× | 通过 |
| PlusInt-SU-port | ≥ 1.0× | 980 ns | 674 ns | 1.45× | 2,007 ns | 1,527 ns | 1.31× | 通过 |
| PlusInt-US-port | ≥ 1.0× | 985 ns | 672 ns | 1.47× | 1,866 ns | 1,521 ns | 1.23× | 通过 |
| LTInt-port | ≥ 1.0× | 1,803 ns | 370 ns | 4.87× | 3,020 ns | 749 ns | 4.03× | 通过 |
| CastIntAsReal-port | ≥ 1.0× | 1,069 ns | 1,178 ns | 0.91× | 1,842 ns | 1,566 ns | 1.18× | arm64 未通过，amd64 通过 |
| VecL1Distance-d3 | ≥ 1.0× | 23.20 µs | 17.20 µs | 1.35× | 28.20 µs | 54.16 µs | 0.52× | arm64 通过，amd64 未通过 |
| VecL2Distance-d3 | ≥ 1.0× | 10.20 µs | 19.53 µs | 0.52× | 25.77 µs | 61.24 µs | 0.42× | 未通过 |
| VecCosineDistance-d3 | ≥ 1.0× | 11.47 µs | 25.05 µs | 0.46× | 32.77 µs | 67.44 µs | 0.49× | 未通过 |
| VecNegativeInnerProduct-d3 | ≥ 1.0× | 10.15 µs | 16.50 µs | 0.62× | 24.96 µs | 53.89 µs | 0.46× | 未通过 |
| VecL1Distance-d768 | ≥ 1.0× | 3,320.63 µs | 408.32 µs | 8.13× | 5,608.72 µs | 792.37 µs | 7.08× | 通过 |
| VecL2Distance-d768 | ≥ 1.0× | 755.18 µs | 634.23 µs | 1.19× | 1,021.95 µs | 797.88 µs | 1.28× | 通过 |
| VecCosineDistance-d768 | ≥ 1.0× | 769.42 µs | 640.15 µs | 1.20× | 1,034.96 µs | 805.33 µs | 1.29× | 通过 |
| VecNegativeInnerProduct-d768 | ≥ 1.0× | 705.59 µs | 613.36 µs | 1.15× | 1,019.82 µs | 790.91 µs | 1.29× | 通过 |

### 其余指标

| 指标 | 门槛 | arm64 | amd64 | 结果 |
|---|---|--:|--:|:-:|
| 定长结果每 chunk 的堆分配（20 个用例，向量驱动） | 0 | 0 | 0 | 通过 |
| `RpnProgram::compile` 每个节点（66 个节点） | ≤ 1 µs | 9.4 ns | 18.5 ns | 通过 |
| 吞吐 ≥ 1.5 倍（目标） | — | 22 项中 8 项 | 22 项中 7 项 | 目标未全部达到（#75 时 Like 也是 1.4×） |
| sysbench `oltp_point_select` 不低于 PoC 基线 | — | — | — | 本次未测（这是端到端指标，不在本门禁的微基准中） |

### 逐行驱动（不计入门禁，#75）

| 签名 | 门槛 | arm64 Go | arm64 Rust | arm64 比值 | amd64 Go | amd64 Rust | amd64 比值 | 结果 |
|---|---|--:|--:|--:|--:|--:|--:|:-:|
| PlusInt-SS | ≥ 1.0× | 12.88 µs | 8,827 ns | 1.46× | 26.99 µs | 24.39 µs | 1.11× | ≥ Go |
| PlusInt-UU | ≥ 1.0× | 10.35 µs | 8,822 ns | 1.17× | 23.04 µs | 24.34 µs | 0.95× | arm64 ≥ Go，amd64 低于 Go |
| PlusInt-SU | ≥ 1.0× | 10.56 µs | 8,821 ns | 1.20× | 23.36 µs | 24.66 µs | 0.95× | arm64 ≥ Go，amd64 低于 Go |
| PlusInt-US | ≥ 1.0× | 10.43 µs | 8,820 ns | 1.18× | 23.60 µs | 24.36 µs | 0.97× | arm64 ≥ Go，amd64 低于 Go |
| LTInt | ≥ 1.0× | 13.78 µs | 10.76 µs | 1.28× | 28.62 µs | 24.74 µs | 1.16× | ≥ Go |
| CastIntAsReal | ≥ 1.0× | 8,811 ns | 6,221 ns | 1.42× | 17.68 µs | 15.89 µs | 1.11× | ≥ Go |
| Concat | ≥ 0.95×（含分配） | 47.09 µs | 83.39 µs | 0.56× | 107.81 µs | 122.08 µs | 0.88× | 低于 Go |
| Like | ≥ 0.95×（含分配） | 72.80 µs | 93.05 µs | 0.78× | 153.40 µs | 129.00 µs | 1.19× | arm64 低于 Go，amd64 ≥ Go |
| PlusInt-SS-port | ≥ 1.0× | 12.88 µs | 10.73 µs | 1.20× | 26.99 µs | 22.56 µs | 1.20× | ≥ Go |
| PlusInt-UU-port | ≥ 1.0× | 10.35 µs | 8,153 ns | 1.27× | 23.04 µs | 20.61 µs | 1.12× | ≥ Go |
| PlusInt-SU-port | ≥ 1.0× | 10.56 µs | 8,210 ns | 1.29× | 23.36 µs | 21.23 µs | 1.10× | ≥ Go |
| PlusInt-US-port | ≥ 1.0× | 10.43 µs | 8,199 ns | 1.27× | 23.60 µs | 21.22 µs | 1.11× | ≥ Go |
| LTInt-port | ≥ 1.0× | 13.78 µs | 10.79 µs | 1.28× | 28.62 µs | 22.16 µs | 1.29× | ≥ Go |
| CastIntAsReal-port | ≥ 1.0× | 8,811 ns | 6,422 ns | 1.37× | 17.68 µs | 14.28 µs | 1.24× | ≥ Go |
| VecL1Distance-d3 | ≥ 0.95×（含分配） | 29.64 µs | 39.89 µs | 0.74× | 45.88 µs | 85.03 µs | 0.54× | 低于 Go |
| VecL2Distance-d3 | ≥ 0.95×（含分配） | 16.37 µs | 40.38 µs | 0.41× | 37.75 µs | 90.98 µs | 0.41× | 低于 Go |
| VecCosineDistance-d3 | ≥ 0.95×（含分配） | 16.90 µs | 40.05 µs | 0.42× | 43.08 µs | 96.89 µs | 0.44× | 低于 Go |
| VecNegativeInnerProduct-d3 | ≥ 0.95×（含分配） | 16.44 µs | 40.10 µs | 0.41× | 37.60 µs | 84.19 µs | 0.45× | 低于 Go |
| VecL1Distance-d768 | ≥ 0.95×（含分配） | 3,478.28 µs | 506.37 µs | 6.87× | 5,354.44 µs | 998.69 µs | 5.36× | ≥ Go |
| VecL2Distance-d768 | ≥ 0.95×（含分配） | 662.79 µs | 752.78 µs | 0.88× | 778.52 µs | 1,008.30 µs | 0.77× | 低于 Go |
| VecCosineDistance-d768 | ≥ 0.95×（含分配） | 682.77 µs | 767.07 µs | 0.89× | 789.89 µs | 1,017.50 µs | 0.78× | 低于 Go |
| VecNegativeInnerProduct-d768 | ≥ 0.95×（含分配） | 621.68 µs | 729.25 µs | 0.85× | 773.94 µs | 999.55 µs | 0.77× | 低于 Go |

### 未通过的项、原因与修复建议（待决定，未改指标）

1. **3 维距离函数（arm64：L2、COSINE、NEGATIVE_INNER_PRODUCT；amd64：四个都低于 Go，0.42–0.62×）**。768 维时四个都高于 Go（1.15–8.1×），所以差距在每行的固定开销，不在求和循环。`sample` 剖析（arm64，COSINE 3 维向量驱动）：约一半时间在 `Column::get_vector_float32` → `get_bytes` → `zero_copy_deserialize_vector_float32` → `peek_bytes_as_vector_float32`，其余在 `check_identical_dims`、`VectorFloat32::elements` 和距离函数本身——这些都是跨 crate 的小函数，没有内联（release 没有 LTO；加 `lto = "fat"`、`codegen-units = 1` 后剖析中仍是函数调用）。实验（未提交）：给 `tidb-chunk` 的 `get_bytes`/`get_vector_float32` 和 `tidb-types` 的 `peek_bytes_as_vector_float32`/`zero_copy_deserialize_vector_float32`/`len`/`elements`/`check_identical_dims`/距离函数加 `#[inline]`，arm64 上 L2 19.5 → 10.7 µs、COSINE 25.1 → 12.3 µs、NIP 16.5 → 10.5 µs、L1 17.2 → 10.8 µs，与 Go（10.2、11.5、10.2、23.2 µs）基本持平，仍略低于 Go。**建议**：(a) 上述 `#[inline]`（只影响代码生成，L2）；(b) 距离函数的向量驱动不再每行经过 `VecArgs` 和完整的反序列化检查：每 chunk 用 `VectorView`（严格签名已有）一次取出两列的偏移，行内只读维度字和元素切片（列中的值是 `append_vector_float32` 写入的，格式已检查过；格式错误时仍按 Go panic）。(a)+(b) 后重测 3 维；amd64 的差距更大，以 amd64 结果为准。
2. **CastIntAsReal（移植后的签名，arm64 0.91×；amd64 1.18× 通过）**。原型用 `checked` 形式，标志组合每 chunk 读一次（arm64 265 ns，4.0×）；D08.8 的移植是默认形式，函数体每行读 `f.arg_type(0)`、`f.tp` 的 unsigned 标志和 `in_union`，循环里有分支、不能向量化（1,178 ns）。**建议**：按 spec §3.4 改为 `checked(nflags = 3, flags = …, err = …)`（参数 unsigned、结果 unsigned、`inUnion`）：这个签名没有出错行、没有逐行副作用，行版与向量版结果相同，满足三条使用条件；govec `cast` 回放核对。PlusInt 的移植（arm64 SS 1.26×，原型 2.89×）同理：溢出判断照 Go 写成比较，改为 `overflowing_add` 的等价形式可以让循环向量化（不是门禁项，吞吐目标项）。
3. **逐行驱动的距离函数（不计入门禁，但 Rust 有分配而 Go 没有：每行 2 次）**：`Column::eval_vector_float32` 返回 `VectorFloat32<'static>`，从行复制一份（Go 的 `row.GetVectorFloat32` 不复制）。T7a 去掉的是函数体里的那一次复制（原来每行 3 次）。**建议**：`Expr::eval_vector_float32` 的返回值借用行（`VectorFloat32<'r>`，常量与函数结果仍是自有值），`RowArgs::vector_float32_pair` 就能不复制。若"含分配的基准差距 ≤ 5%"也适用于逐行驱动，768 维 L2/COSINE/NIP（0.77–0.89×）和 Concat/Like（#75 已知）都要按此处理。

**结论**：D08.1 的 T6c（门禁验收）仍未完成，P1 暂不能勾掉。T7a 已完成。
