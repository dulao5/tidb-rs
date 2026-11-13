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


## 门禁重测（2026-11-14，P1 收尾；修复后**通过**）

Lucky 在 #163 评审中不同意把门禁移交 A5：先做 T7a，再按上面的测量规则重测，然后才能勾掉 P1。指标不放宽。

**方法**：`scripts/eval-gate.sh`（Go 与 Rust 交替运行，各 5 个独立进程，Go 每进程 3 个样本、Rust 为 criterion 的点估计，取最小值；`scripts/eval_gate_summary.py` 逐项对照门槛）。Go 一侧是 v8.5.8 `pkg/expression` 的 `BenchmarkVectorizedBuiltin*Func`，外加 `crates/expression/benches/go/gate_vec_test.go`（距离函数 3 维与 768 维，同一个 `newVectorFloat32RandGener`）；Rust 一侧是 `benches/eval_gate.rs`。测量集比 #75 多了三类：已移植的 PlusInt/LTInt/CastIntAsReal 签名（`-port`，#75 时只有原型）、四个距离函数（3 维、768 维，T7a）、`RpnProgram::compile` 与每 chunk 的堆分配次数。

**门槛**（N4 §5 与负责人确认的口径）：向量驱动每个签名不低于 Go（Go/Rust ≥ 1.0）；Rust 一侧有堆分配的基准差距 ≤ 5%（≥ 0.95，D01.2 spec §5 的口径）；吞吐 ≥ 1.5 倍为目标（逐项列出，不作为通过条件，与 #75 相同）；定长结果每 chunk 除结果列外 0 次堆分配；编译每个节点 ≤ 1 µs。逐行驱动按 #75 不计入门禁，只列出。sysbench `oltp_point_select` 是端到端指标，由 weekly 构建测量，不在本门禁中。

### 第一次测量（T7a 之后、修复之前）：未通过

完整输出：`gate-rerun-arm64-before.md`、`gate-rerun-amd64-before.md`（amd64：CI run 43491177313）。向量驱动低于 Go 的项：3 维距离函数（arm64：L2 0.52×、COSINE 0.46×、NEGATIVE_INNER_PRODUCT 0.62×；amd64：四个都在 0.42–0.52×）；移植后的 CastIntAsReal（arm64 0.91×，amd64 1.18×）。逐行驱动的距离函数每行 2 次分配（Go 0 次）。

原因（`sample` 剖析，arm64）：3 维时每行的固定开销占主导——约一半时间在 `Column::get_vector_float32` → `get_bytes` → `zero_copy_deserialize_vector_float32` → `peek_bytes_as_vector_float32`，其余在 `check_identical_dims`、`elements`、`Elements::iter`，都是没有内联的跨 crate 小函数（带出错路径的 `errorf` 让它们变大；fat LTO 也没有内联）。CastIntAsReal 的移植是默认形式，每行读 unsigned 标志和 `inUnion`，循环不能向量化。逐行驱动的分配来自 `Column::eval_vector_float32` 返回自有值（从行复制）。

### 修复（同一 PR，行为不变）

1. **距离函数**：lazy 函数体的向量驱动对 `args(VectorFloat32, ..)` 中的 VECTOR 参数每 chunk 取一次视图（`Column::var_len_view` 取出 offsets 与 data，`rt::VectorView`、`VecArgs::with_vectors`），行内不再经过列的缓冲区；`Column::get_vector_float32`、`peek_bytes_as_vector_float32`、`zero_copy_deserialize_vector_float32`、`VectorFloat32::len/elements`、`Elements::iter`、`check_identical_dims` 与各距离方法加 `#[inline]`，出错路径移到 `#[cold]` 函数（文案与 panic 不变）。
2. **CastIntAsReal**：改为 `checked(nflags = 3)`（参数 unsigned、结果 unsigned、`inUnion`），标志每 chunk 读一次（Go 的 `vecEvalReal` 也在循环外读）。满足 spec §3.4 三条：(a) 没有可能出错的检查，NULL 行的值被空位图屏蔽；(b) 没有逐行副作用（无 warning、无状态）；(c) 行版与向量版计算同一个值。
3. **逐行驱动不复制**：`Column::eval_vector_float32` / `Expr::eval_vector_float32` 返回借用行的 `VectorFloat32<'r>`（Go 的 `row.GetVectorFloat32` 也不复制）；距离函数等 VECTOR 函数的逐行驱动每行 0 次分配。

govec `cast` 回放：本机 arm64 与 CI 的 amd64、arm64 都通过；`tidb-types`、`tidb-chunk`、`tidb-expression` 的测试全部通过。

### 修复后的测量：通过

- arm64：本机 Mac mini（Apple M4 Pro，12 核），go1.25.12，rustc 1.99.1；各轮开始时 load average 1.8–2.6（第 1 轮 14.9：刚结束编译；取多进程最小值）。
- amd64：CI `ubuntu-24.04`（这次是 AMD EPYC 9V74，4 核；第一次测量是 EPYC 7763），workflow `eval-gate.yml`（run 43500047141，产物 `eval-gate-amd64`）。
- 完整输出：`gate-rerun-arm64.md`、`gate-rerun-amd64.md`。

#### 向量驱动（门禁；每 1024 行）

| 签名 | 门槛 | arm64 Go | arm64 Rust | arm64 Go/Rust | amd64 Go | amd64 Rust | amd64 Go/Rust | 结果 |
|---|---|--:|--:|--:|--:|--:|--:|:-:|
| PlusInt-SS | ≥ 1.0× | 1,377 ns | 477 ns | 2.89× | 1,635 ns | 958 ns | 1.71× | 通过 |
| PlusInt-UU | ≥ 1.0× | 736 ns | 458 ns | 1.61× | 1,210 ns | 1,099 ns | 1.10× | 通过 |
| PlusInt-SU | ≥ 1.0× | 975 ns | 666 ns | 1.46× | 1,550 ns | 1,399 ns | 1.11× | 通过 |
| PlusInt-US | ≥ 1.0× | 976 ns | 666 ns | 1.47× | 1,639 ns | 1,349 ns | 1.21× | 通过 |
| LTInt | ≥ 1.0× | 1,782 ns | 370 ns | 4.82× | 2,421 ns | 606 ns | 3.99× | 通过 |
| CastIntAsReal | ≥ 1.0× | 1,065 ns | 264 ns | 4.03× | 1,388 ns | 420 ns | 3.30× | 通过 |
| Concat | ≥ 0.95×（含分配） | 35.46 µs | 13.26 µs | 2.67× | 61.95 µs | 13.97 µs | 4.43× | 通过 |
| Like | ≥ 0.95×（含分配） | 55.25 µs | 40.86 µs | 1.35× | 74.92 µs | 41.66 µs | 1.80× | 通过 |
| PlusInt-SS-port | ≥ 1.0× | 1,377 ns | 1,079 ns | 1.28× | 1,635 ns | 1,358 ns | 1.20× | 通过 |
| PlusInt-UU-port | ≥ 1.0× | 736 ns | 454 ns | 1.62× | 1,210 ns | 1,207 ns | 1.00× | 通过 |
| PlusInt-SU-port | ≥ 1.0× | 975 ns | 658 ns | 1.48× | 1,550 ns | 1,205 ns | 1.29× | 通过 |
| PlusInt-US-port | ≥ 1.0× | 976 ns | 674 ns | 1.45× | 1,639 ns | 1,210 ns | 1.36× | 通过 |
| LTInt-port | ≥ 1.0× | 1,782 ns | 369 ns | 4.83× | 2,421 ns | 609 ns | 3.97× | 通过 |
| CastIntAsReal-port | ≥ 1.0× | 1,065 ns | 265 ns | 4.02× | 1,388 ns | 422 ns | 3.29× | 通过 |
| VecL1Distance-d3 | ≥ 1.0× | 23.27 µs | 6,974 ns | 3.34× | 20.07 µs | 12.14 µs | 1.65× | 通过 |
| VecL2Distance-d3 | ≥ 1.0× | 10.34 µs | 6,850 ns | 1.51× | 20.63 µs | 11.90 µs | 1.73× | 通过 |
| VecCosineDistance-d3 | ≥ 1.0× | 11.42 µs | 7,171 ns | 1.59× | 22.07 µs | 15.06 µs | 1.47× | 通过 |
| VecNegativeInnerProduct-d3 | ≥ 1.0× | 10.14 µs | 6,804 ns | 1.49× | 20.34 µs | 11.37 µs | 1.79× | 通过 |
| VecL1Distance-d768 | ≥ 1.0× | 3,244.25 µs | 338.41 µs | 9.59× | 4,712.30 µs | 640.16 µs | 7.36× | 通过 |
| VecL2Distance-d768 | ≥ 1.0× | 736.96 µs | 555.85 µs | 1.33× | 884.41 µs | 636.06 µs | 1.39× | 通过 |
| VecCosineDistance-d768 | ≥ 1.0× | 758.82 µs | 580.85 µs | 1.31× | 893.12 µs | 659.86 µs | 1.35× | 通过 |
| VecNegativeInnerProduct-d768 | ≥ 1.0× | 707.57 µs | 538.60 µs | 1.31× | 877.60 µs | 630.04 µs | 1.39× | 通过 |

#### 其余指标

| 指标 | 门槛 | arm64 | amd64 | 结果 |
|---|---|--:|--:|:-:|
| 定长结果每 chunk 的堆分配（20 个用例，向量驱动） | 0 | 0 | 0 | 通过 |
| `RpnProgram::compile` 每个节点（66 个节点） | ≤ 1 µs | 9.5 ns | 14.7 ns | 通过 |
| 吞吐 ≥ 1.5 倍（目标，不作为通过条件） | — | 22 项中 12 项 | 22 项中 11 项 | 目标未全部达到 |
| sysbench `oltp_point_select` 不低于 PoC 基线 | — | — | — | 由 weekly 构建测量 |

amd64 上 PlusInt-UU-port 为 1.00×（1,207 vs 1,210 ns），刚好不低于 Go（第一次测量 1.12×）；它是 D08.5 的移植，溢出判断照 Go 写成比较、没有向量化（原型用 `overflowing_add` 的等价形式为 1.10–1.61×）。不是本 PR 改动的签名，记为余量很小的项。

#### 逐行驱动（不计入门禁，#75）

| 签名 | 门槛 | arm64 Go | arm64 Rust | arm64 Go/Rust | amd64 Go | amd64 Rust | amd64 Go/Rust | 结果 |
|---|---|--:|--:|--:|--:|--:|--:|:-:|
| PlusInt-SS | ≥ 1.0× | 12.77 µs | 8,823 ns | 1.45× | 17.09 µs | 19.89 µs | 0.86× | arm64 ≥ Go / amd64 低于 Go |
| PlusInt-UU | ≥ 1.0× | 10.34 µs | 8,821 ns | 1.17× | 16.89 µs | 19.89 µs | 0.85× | arm64 ≥ Go / amd64 低于 Go |
| PlusInt-SU | ≥ 1.0× | 10.59 µs | 8,815 ns | 1.20× | 17.01 µs | 19.91 µs | 0.85× | arm64 ≥ Go / amd64 低于 Go |
| PlusInt-US | ≥ 1.0× | 10.53 µs | 8,815 ns | 1.19× | 17.14 µs | 19.95 µs | 0.86× | arm64 ≥ Go / amd64 低于 Go |
| LTInt | ≥ 1.0× | 13.47 µs | 10.41 µs | 1.29× | 17.51 µs | 18.55 µs | 0.94× | arm64 ≥ Go / amd64 低于 Go |
| CastIntAsReal | ≥ 1.0× | 8,733 ns | 5,903 ns | 1.48× | 10.77 µs | 11.10 µs | 0.97× | arm64 ≥ Go / amd64 低于 Go |
| Concat | ≥ 0.95×（含分配） | 47.67 µs | 82.82 µs | 0.58× | 72.63 µs | 87.41 µs | 0.83× | 低于 Go |
| Like | ≥ 0.95×（含分配） | 72.80 µs | 92.14 µs | 0.79× | 114.21 µs | 86.59 µs | 1.32× | arm64 低于 Go / amd64 ≥ Go |
| PlusInt-SS-port | ≥ 1.0× | 12.77 µs | 10.14 µs | 1.26× | 17.09 µs | 16.99 µs | 1.01× | ≥ Go |
| PlusInt-UU-port | ≥ 1.0× | 10.34 µs | 7,898 ns | 1.31× | 16.89 µs | 16.60 µs | 1.02× | ≥ Go |
| PlusInt-SU-port | ≥ 1.0× | 10.59 µs | 7,935 ns | 1.34× | 17.01 µs | 17.16 µs | 0.99× | arm64 ≥ Go / amd64 低于 Go |
| PlusInt-US-port | ≥ 1.0× | 10.53 µs | 8,013 ns | 1.31× | 17.14 µs | 17.25 µs | 0.99× | arm64 ≥ Go / amd64 低于 Go |
| LTInt-port | ≥ 1.0× | 13.47 µs | 10.41 µs | 1.29× | 17.51 µs | 16.31 µs | 1.07× | ≥ Go |
| CastIntAsReal-port | ≥ 1.0× | 8,733 ns | 6,560 ns | 1.33× | 10.77 µs | 10.80 µs | 1.00× | arm64 ≥ Go / amd64 低于 Go |
| VecL1Distance-d3 | ≥ 1.0× | 30.20 µs | 14.76 µs | 2.05× | 28.14 µs | 29.53 µs | 0.95× | arm64 ≥ Go / amd64 低于 Go |
| VecL2Distance-d3 | ≥ 1.0× | 16.48 µs | 14.72 µs | 1.12× | 28.59 µs | 29.38 µs | 0.97× | arm64 ≥ Go / amd64 低于 Go |
| VecCosineDistance-d3 | ≥ 1.0× | 16.92 µs | 15.20 µs | 1.11× | 30.89 µs | 33.62 µs | 0.92× | arm64 ≥ Go / amd64 低于 Go |
| VecNegativeInnerProduct-d3 | ≥ 1.0× | 16.39 µs | 14.42 µs | 1.14× | 28.52 µs | 29.31 µs | 0.97× | arm64 ≥ Go / amd64 低于 Go |
| VecL1Distance-d768 | ≥ 1.0× | 3,139.61 µs | 343.54 µs | 9.14× | 4,369.78 µs | 666.32 µs | 6.56× | ≥ Go |
| VecL2Distance-d768 | ≥ 1.0× | 654.07 µs | 574.38 µs | 1.14× | 670.04 µs | 670.64 µs | 1.00× | arm64 ≥ Go / amd64 低于 Go |
| VecCosineDistance-d768 | ≥ 1.0× | 671.89 µs | 597.47 µs | 1.12× | 678.28 µs | 677.56 µs | 1.00× | ≥ Go |
| VecNegativeInnerProduct-d768 | ≥ 1.0× | 612.34 µs | 554.85 µs | 1.10× | 665.28 µs | 664.63 µs | 1.00× | ≥ Go |

距离函数的逐行驱动现在每行 0 次分配；amd64 上 3 维仍略低于 Go（0.92–0.97×），768 维持平（1.00×）。整数签名的原型在 amd64 上逐行低于 Go（0.86–0.97×，原型不是移植后的签名；移植后的签名 0.99–1.07×）。Concat、Like 的逐行驱动与 #75 相同，仍低于 Go（#75 记为后续项）。

**结论**：N4 §5 门禁在 arm64 与 amd64 上都通过，D08.1 T6c 的门禁验收完成。
