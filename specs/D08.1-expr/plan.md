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

## 第 2 部分：N4 §5 门禁（2026-11-11，未通过，暂停移植，等决定）

**做了什么**：框架的最小可用版本已经写好（不是一次性原型，第 2 部分继续用）。
- 新 crate `tidb-expression-macros`（rank 0）：`#[eval_fn(Int|Real|String[, lazy])]`，函数体一份，生成逐行驱动 `__row` 和向量驱动 `__vec`。严格签名的参数类型 `i64`/`f64`/`&[u8]`；`lazy` 签名拿 `args: &mut impl Args` 自己求参数；字符串结果写进驱动给的 `out: &mut Vec<u8>`（向量驱动整块复用一个缓冲区，不按行分配）。
- `tidb-expression::eval`：`SigDesc`、`SigDrivers`、`BuiltinFunc`（缓存 collator，Go 的 `ctor`）、`SigView`（`RowSig` 逐行取参数类型，`VecSig` 每个 chunk 取一次）、`RpnProgram`/`RpnState`（后序 RPN，列引用直接借用输入 chunk 的列，中间列放池里复用，常量按 Go `genVecFromConstExpr` 每 chunk 求一次再填满）。
- `ScalarFunction` 改为持有 `function: BuiltinFunc`，`eval_int`/`eval_real`/`eval_string` 不再是桩。
- `tidb-chunk::Column::fixed_parts_mut`（glue）：结果列的空位图与数据同时可写。
- `tests/eval_drivers.rs`：两种驱动逐行一致（值、NULL、错误、嵌套调用、池复用）。

**基准**（`cargo bench -p tidb-expression --bench eval_gate`；Go：`go test -c` 后 `-test.bench BenchmarkVectorizedBuiltin<X>Func`，同一台 Mac mini M 系列，同一时间段交替运行；每次 1024 行，输入分布与 Go 的 generator 相同；ns/批）：

| 签名 | Go 向量 | Rust 向量驱动 | 比值 | Rust 手写循环¹ | Go 逐行 | Rust 逐行驱动 |
|---|--:|--:|--:|--:|--:|--:|
| PlusInt 有符号+有符号 | 1,460 | 1,700 | 0.86× | 1,690 | 13,000 | 10,200 |
| PlusInt 无符号+无符号 | 750 | 910 | 0.82× | 950 | 10,300 | 7,650 |
| PlusInt 有符号+无符号 | 1,010 | 1,630 | 0.62× | 1,210 | 10,600 | 7,400 |
| PlusInt 无符号+有符号 | 1,000 | 1,160 | 0.86× | 1,240 | 10,500 | 7,560 |
| LTInt | 1,900 | 2,370 | 0.80× | 2,010 | 14,000 | 10,200 |
| CastIntAsReal | 1,080 | 830 | 1.30× | — | 8,700 | 5,900 |
| Concat（3 个字符串） | 35,500 | 14,100 | 2.52× | — | 47,000 | 82,000 |
| Like | 53,700 | 122,000 | 0.44× | — | 72,000 | 149,000 |

¹ 同一份数据上按 Go `plusXX`/`vecCompareInt` 的形状手写的 Rust 循环（类型标志提到循环外，不经过框架）。Go 的数字还包含把两个参数列各复制一次（`Column.VecEvalInt` 的 `CopyReconstruct`），Rust 直接借用。

**结论**：
1. 门禁"任一签名不低于 Go"没有达到：8 项中 6 项低于 Go。
2. **框架本身的开销小**：生成的向量驱动与手写循环基本持平（PlusInt 三种 ±5%，有符号+无符号 慢 35%，LT 慢 18%）。差距主要在这类分支密集的整数循环上 rustc 生成的代码不比 Go 的手写向量版好，不是"一份函数体 + 宏"这个设计造成的。
3. **Like 慢 2.3 倍是函数体用到的已移植代码**：逐行驱动也是 Go 逐行版的 2 倍（149 vs 72 µs）。瓶颈在 `tidb-types` 的 `compile_pattern`/`do_match`（D01.9 的移植）与每行新建 pattern，与驱动无关。
4. 字符串（Concat）快 2.5 倍：输出缓冲区复用、不复制参数列。

**过程中做过的改进**（都已保留）：热点小函数加 `#[inline]`（跨 crate 不内联时差 30–50%）；参数类型每 chunk 读一次放进 `Copy` 的 `VecSig`，函数体对两种驱动各单态化一份（之前每行取类型，慢 2 倍）；按空位图每 8 行一块，整块非 NULL 时不逐行测位；RPN 的栈与中间列复用，调用时不分配。

**可选方案**（请决定）：
- (a) **接受现状、继续移植**：把门禁改为"框架开销 ≤ 手写循环的 1.2 倍"，与 Go 的差距记为性能债（#53），在 A5 SIMD / 热点签名专项中处理；Like 的差距另开 D01.9 的性能项。
- (b) **继续调宏**：对严格的定长签名生成"先整列做无分支计算、再单独扫一遍检查溢出"的两段式循环（Go 没这样写，但可见行为相同：第一个溢出行报错）；预计能追平或超过 Go，代价是宏更复杂，约 1–2 天。
- (c) **热点签名允许第二份向量函数体**（N4 §3.3 只允许"Go 内部不一致"时保留第二份）：需要修改 N4。不建议。

建议 (a)，并在 D08.4/D08.5 移植时对 PlusInt/LT 这类热点做一次 (b) 的试点。
