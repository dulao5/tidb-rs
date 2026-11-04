# N4：表达式求值框架（基础设计 spec，草案）

状态：草案（2026-11-04）。类型：实现例外（基础设计）。计划定稿：P1 之前。
所属块：D08.1（表达式框架）、D08.2（函数注册、类型推导、常量折叠）、D08.3（下推判定与 pb 转换，只涉及接口）；约束 D08.4–D08.10 全部函数族和 D12 中所有求值调用点。
相关：N1（`Arc<Expr>` 与写时复制，`M-EXPR`）、N2（`EvalCtx` = 冻结快照）、N3+A1（async 执行、`TrackedAlloc`）、N5a（列格式）、A4（JSON 函数按本 spec 的统一规则）、A5（SIMD）、ADR 0007 第 3 条。

位置均为 v8.5.8 `pkg/` 下的 `文件:行`；规模的统计命令见文末脚注。

## 1. 要解决的问题

Go 的表达式有两套求值入口：逐行的 `evalXxx(ctx, row)` 和向量化的 `vecEvalXxx(ctx, chunk, result)`。绝大多数函数两套都写了一遍。照搬意味着把约 2.4 万行向量化代码再翻译一遍，并长期维护两份可能不一致的实现。另一方面，优化器规则逐行依赖表达式树的形状（`Column`/`Constant`/`ScalarFunction`、`UniqueID`、`RetType`），这部分不能改。N4 要定的是：规划侧怎么表示，执行侧怎么求值，函数体只写一份时以哪一版为准。

### 1.1 v8.5.8 中的事实

| 事实 | 位置 | 对 Rust 的影响 |
|---|---|---|
| `Expression` 接口同时含逐行 `Eval*`（8 种求值类型）和 `VecExpr` 的 `VecEval*`；另有 `ConstLevel`、`Clone`、`ResolveIndices`、`HashCode` 等规划用方法 | `expression/expression.go:120-147`、`:171-260` | 规划侧接口按 L1 保留；求值入口可以重新组织 |
| `ScalarFunction{FuncName, RetType, Function builtinFunc, hashcode}`；`Vectorized()` = 本函数 `vectorized()` 且所有子节点可向量化 | `scalar_function.go:41-49`、`:125-128` | 规划侧结构照搬；"是否走向量化"的判定要 L1 保留 |
| 签名选择：`newFunctionImpl` 查 `funcs` 表（289 个函数名）得到 `functionClass`，调 `getFunction(ctx, args)` 按参数求值类型返回具体签名，例如 `arithmeticPlusFunctionClass` 按 Real/Decimal/Int 返回三种 sig 并 `setPbCode` | `scalar_function.go:199-291`、`builtin.go:602-607`、`:646`、`builtin_arithmetic.go:170-213` | `getFunction` 逐行 L1；签名在 Rust 中是一个静态描述符，不是对象 |
| 规模：291 个 function class，591 个签名有逐行实现，533 个有 `vecEval*`，其中 520 个 `vectorized()` 返回 true、1 个按条件、12 个返回 false；58 个只有逐行版本（多为 VectorFloat32、锁、信息类函数） | 脚注 1 | 一份函数体要能服务两种求值方式 |
| Go 自己把逐行版本当作参照：向量化函数在子节点不可向量化时整棵树退回逐行；`builtinConvSig`、`builtinExtractDatetimeFromStringSig` 因向量化版本有错而关掉（"TODO: … Then open the vectorized evaluation"、"to fix issue 9716"）；测试框架断言两版结果和 warning 逐条相同 | `chunk_executor.go:225`、`evaluator.go:28-56`、`builtin_math_vec.go:1075-1078`、`builtin_time_vec.go:184-187`、`bench_test.go:1731-1742` | 统一规则以逐行版本为规格（见 3.3） |
| 两版并不总一致：git 历史中至少 15 个修复提交标题含"vectorized … wrong/fix/incompatible"，其中 `8ae81b3852` 修的是关闭向量化时（逐行版）的错 | `git log v8.5.8 --grep=vectoriz` | 两个方向都出过错；需要差分测试兜底 |
| 逐行版本参数是**惰性**的：`PlusInt.evalInt` 先求 `args[0]`，为 NULL 就返回，不再求 `args[1]`；向量化版本先把两个参数整列求完 | `builtin_arithmetic.go:225-268`、`builtin_arithmetic_vec.go:856-893` | 两种模式下"哪个参数的错误/warning 会出现"不同。Rust 要按 Go 在同一场景下用的模式来 |
| 控制函数的向量化版本先整列求值，一旦有 warning 或错误就截断 warning 并退回逐行（`fallbackEval*`）。共 30 个签名：IF/IFNULL/CASE/COALESCE 各 7 种类型 + `LogicAnd`/`LogicOr` | `builtin_control_vec_generated.go:1204-1236` | 这 30 个的净效果等于逐行语义 |
| 常量在向量化时每个 chunk 只求一次（`genVecFromConstExpr` 调一次 `EvalInt` 再填满整列）；逐行时每行求一次。`ParamMarker` 和 `DeferredExpr` 都在求值时才取值 | `vectorized.go:23-46`、`constant.go:143-156`、`:225-230`、`:288-356` | 常量转换产生的 warning 次数在两种模式下不同，Rust 两种驱动各自保持 |
| 父节点可以要求子节点按非自身类型求值：`CastStringAsInt` 对 hybrid（ENUM/SET/BIT）或二进制字面量、`DAYNAME` 调 `args[0].EvalInt`；`Column.EvalInt` 对 hybrid 列转成整数 | `builtin_cast.go:1693-1701`、`:2402-2407`、`expression/column.go:542-559` | 求值节点要带"按什么类型取"的信息，不能假设参数类型 = 子节点 `RetType` |
| 函数体读参数的类型和表达式文本：`args[i].GetType(ctx)` 在 builtin 文件中 127 处（如判断 unsigned），报错文案用 `args[i].StringWithCtx` 49 处 | 脚注 2；`builtin_arithmetic.go:236-262`、`:718-735` | 函数体除了参数值，还要能拿到签名本身（参数表达式、类型、collation） |
| 常量折叠用**逐行** `Eval(ctx, chunk.Row{})`；`unFoldableFunctions` 16 个、`deferredFunctions` 11 个（NOW、RANDOM_BYTES 等，plan cache 下推迟到执行时）；`ConstLevel` 三级 | `constant_fold.go:162-233`、`function_traits.go:48-65`、`:136-160`、`expression.go:152-168` | 逐行驱动是规划期必需的，不只是执行期的退路 |
| 求值期的可变状态：`baseBuiltinFunc.bufAllocator`（`sync.Pool` 中间列）、`childrenVectorizedOnce`；`builtinFuncCache` 以 `ctx.CtxID()` 为键缓存 LIKE 模式、正则、JSON schema；`RAND()` 的 `MysqlRng` 在 Clone 后仍共享（内部有锁）；`CorrelatedColumn.Data *Datum` 由 apply 逐行写入 | `builtin.go:376-391`、`:1089-1130`、`builtin_vectorized.go:28-80`、`builtin_math.go:1058-1072`、`util/mathutil/rand.go:26-30`、`expression/column.go:43-47`、`executor/parallel_apply.go:284`、`join/hash_join_v1.go:1360` | 这就是 N1 6.4 问的"Go 为什么按 worker 复制表达式"：并行 apply（`executor/builder.go:2678`）和 joiner（`join/joiner.go:335-346`）的 `Clone` 是为了关联列；投影 worker 共享同一个 `EvaluatorSuite`（`projection.go:275`、`:471`） |
| 执行期调用点：executor + planner + ranger 中非测试代码约 204 处逐行 `Eval*`，executor 中 16 处 `VecEval*`/`VectorizedFilter`/`VectorizedExecute`；`tidb_enable_vectorized_expression` 默认 ON | 脚注 3；`sessionctx/variable/tidb_vars.go:1453` | 两种入口都要有 |
| `EvalContext` 是窄接口：`CtxID/SQLMode/TypeCtx/ErrCtx/Location/CurrentTime/…/GetParamValue`，外加 `WarningCount/TruncateWarnings`（上面 IF 的回退用它） | `exprctx/context.go:63-95`、`exprstatic/evalctx.go:281-308` | 直接对应 N2 的冻结快照 |
| 下推：`canFuncBePushed`、`scalarExprSupportedByTiKV`（注释："Rust use the llvm math functions, which have different precision with Golang"，多数字符串函数被注释掉不下推）、`ExpressionsToPBList`。Go 侧 526 个签名有 pb code，其中 166 个在 TiKV `tidb_query_expr` 的映射表里没有 | `infer_pushdown.go:45`、`:160-190`、`expr_to_pb.go:37`；脚注 4 | 下推只需要"签名 → pb code"和判定逻辑，与根节点怎么求值无关 |
| `SLEEP` 在函数体内阻塞，每 10ms 检查一次 KILL | `builtin_miscellaneous_vec.go:357-379` | 与 N3 的 async 任务池冲突，需单独处理 |

### 1.2 TiKV `tidb_query_expr` 的情况（已 vendored，`third_party/tikv/tidb_query_expr`）

| 事实 | 位置（TiKV `components/`） | 影响 |
|---|---|---|
| RPN 节点 `FnCall{func_meta, args_len, field_type, metadata}` / `Constant` / `ColumnRef{offset}`；函数指针签名 `fn(&mut EvalContext, output_rows, &[RpnStackNode], &mut RpnFnCallExtra, &metadata) -> Result<VectorValue>` | `tidb_query_expr/src/types/expr.rs:12-30`、`function.rs:36-56` | 绑定 TiKV 的 `EvalContext`、`VectorValue`、`LazyBatchColumnVec` |
| `RpnFnCallExtra` 只有 `ret_field_type`；参数类型差异靠构建时选不同的单态化版本 | `function.rs:66-69` | Go 函数体要读参数类型和参数表达式，需要扩展 |
| 所有参数先整列求值再调函数（RPN 后序）；`case_when`、`if_condition` 也是如此，没有 Go 的"有 warning 就退回逐行" | `types/expr_eval.rs:205-245`、`impl_control.rs:34-36`、`:88-90` | 与 Go 的 30 个控制函数语义不同 |
| 函数体与 Go 有已知差异，代码里自己写着：`cast_string_as_int` "TODO: in TiDB, if … Hybrid() … return res from EvalInt() directly"；`cast_decimal_as_uint` "TODO: here TiDB round before call is_negative()"（Go 先 `Round` 再判断） | `impl_cast.rs:344-346`、`:434`；对照 Go `builtin_cast.go:1495-1511`、`:1693-1701` | 函数体不能复用（ADR 0007 第 2 条已定） |
| 框架本身的非测试代码：`expr_eval.rs` 约 330 行、`expr_builder.rs` 约 512 行、`function.rs` 385 行、`expr.rs` 148 行、宏 `rpn_function.rs` 约 1,729 行；373 个 `#[rpn_fn]` | 脚注 4 | 若列格式用 Go Chunk（N5a），求值循环和构建器都得重写，宏也要大改 |

## 2. 方案比较

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| a. 照搬 Go | `trait BuiltinFunc` 有 8 个 `eval_*` 和 8 个 `vec_eval_*`，591 个签名各自实现 | 逐行对照最直接 | 约 2.4 万行向量化代码重写一遍；两版不一致的风险照搬；每次上游同步要对两份 |
| b. 规划侧与执行侧都用 TiKV RPN | 规划侧也用 tipb/RPN 形状 | 一套表示 | 优化器规则依赖 Go 树形状，L1 对照全部失效。**否决** |
| c. 规划侧按 Go；执行侧编译成 RPN；函数体一份，宏生成逐行和向量两种驱动 | 见第 3 节 | 函数体只按 Go 逐行版写一次；两种驱动分别复现 Go 两种模式的可见行为；执行侧无共享可变状态 | 宏和驱动要自己写；向量化的性能靠单态化而不是手写循环，个别热点函数要靠 A5 |
| c'. 同 c，但执行侧直接依赖 TiKV 的 RPN 框架（L3） | 用 `tidb_query_expr` 的构建器、求值器、`#[rpn_fn]` | 少写框架代码 | 框架绑定 TiKV 的列类型和 `EvalContext`，N5a 若选 Go Chunk 就接不上；缺惰性参数、参数类型、回退逐行这三样；等于 fork 一个 TiKV 仍在改的模块 |

**建议：方案 c。** TiKV 的 RPN 设计（后序节点、构建期选单态化函数、列引用不复制）作为设计参照，代码不依赖 `tidb_query_expr`。理由见 3.6。

## 3. 设计

### 3.1 规划侧：与 Go 同形

```rust
pub enum Expr {                         // M-EXPR：Arc<Expr> 共享，Arc::make_mut 写时复制
    Column(Column),                     // ret_type, id, unique_id, index, virtual_expr, collation_info ...
    CorrelatedColumn(CorrelatedColumn), // 不再带 Data 指针，见 3.5
    Constant(Constant),                 // value, ret_type, deferred_expr, param_marker
    ScalarFunction(ScalarFunction),     // func_name, ret_type, function: BuiltinFunc
}
pub struct BuiltinFunc {
    pub sig: &'static SigDesc,          // 对应 Go 的具体 sig 类型，如 builtinArithmeticPlusIntSig
    pub base: BaseBuiltinFunc,          // args: Vec<Arc<Expr>>, tp, collation, pb_code ...
    pub meta: SigMeta,                  // Go sig 结构体里的其余字段（inUnion、rng、常量参数等）
}
```

- 用枚举而不是 trait 对象：Go 只有四种节点，大量 `switch x := e.(type)` 可以逐行写成 `match`（`M-IFACE`）。
- `functionClass` 与 `getFunction` 逐行 L1；`getFunction` 末尾的 `sig := &builtinXxxSig{bf}; sig.setPbCode(...)` 对应 `BuiltinFunc { sig: &SIG_PLUS_INT, base: bf, meta }`。`pb_code` 是 `SigDesc` 的常量字段。
- 规划侧在 v8.5.8 外部只有 1 处对签名做类型断言（`expression_rewriter.go` 对 `BuiltinGroupingImplSig`），由 `SigMeta` 的访问器替代。
- `ConstLevel`、`FoldConstant`、`specialFoldHandler`、`deferredFunctions`、`MaybeOverOptimized4PlanCache`、`ResolveIndices`、`HashCode` 全部 L1。折叠时调用 3.2 的逐行驱动。

### 3.2 一份函数体，两种驱动

函数体按 Go 逐行版本（`evalXxx`）写，参数通过访问器取。宏 `#[eval_fn]` 为每个签名生成两个驱动：

| 驱动 | 参数如何得到 | 对应 Go | 用在哪里 |
|---|---|---|---|
| 逐行 `eval_row` | 访问器被调用时才对子树求值（树遍历），取第几个参数、取不取都由函数体决定 | `evalXxx`：惰性、左到右、遇 NULL 提前返回 | 常量折叠、规划期求值、`Eval*` 的 204 个调用点、Go 判为不可向量化的表达式、`tidb_enable_vectorized_expression=OFF` |
| 向量 `eval_vec` | 先把所有参数按 RPN 整列求出，访问器只是按行下标读已有的列 | `vecEvalXxx`：参数整列求值，常量每 chunk 求一次 | Go 用 `VecEval*` 的地方（`EvaluatorSuite`、`VectorizedFilter`、分组检查等），判定逻辑 L1 |

- 默认（严格）签名的简写：`fn plus_int(ctx, sig, a: i64, b: i64) -> Result<Option<i64>>`。宏在逐行驱动里展开为"求 a，NULL 则返回；求 b，NULL 则返回；调用函数体"，正是 Go 逐行版的固定开头；在向量驱动里展开为"合并空位图，对非空行调用函数体"。
- 需要控制参数求值的签名（IF、CASE、COALESCE、IN、AND/OR、`CastStringAsInt` 的 hybrid 分支等）写成 `#[eval_fn(lazy)]`，函数体拿到 `args` 访问器，代码与 Go 逐行版一一对应。
- 30 个有 `fallbackEval` 的控制函数加 `#[eval_fn(lazy, vec = eager_then_row_fallback)]`：向量驱动先整列求参数并记下 warning 数；有新 warning 或错误时截断并对该 chunk 改用逐行驱动。这与 Go 完全同构（包括 `RAND()` 在分支里被整列求值时随机数序列的推进）。
- 每个签名在 `SigDesc` 中带 `vectorized: Always | Never | Fn`（照搬 Go 的 `vectorized()`，含 `Benchmark` 的条件和 12 个关闭的）。所以 Rust 只在 Go 会走向量化的地方走向量驱动，两种模式的可见差异（哪个参数的错误出现、常量转换 warning 的次数和顺序）都与 Go 一致。
- 字符串求值类型用 `&[u8]`，不用 `&str`（chunk 中可以是任意字节）。
- 中间列用 `TrackedAlloc` 分配（`M-MEM`），挂在算子的 tracker 下；Go 的 `bufAllocator`/`sync.Pool` 删除（`M-POOL`）。

### 3.3 统一规则：两版不一致时以谁为准

1. **逐行版本（`evalXxx`）是规格。** 函数体只从它移植。理由：Go 自己在向量化不可用时退回它；向量化版本有错时 Go 的做法是关掉向量化（3 处证据见 1.1）；常量折叠和规划期只用它。
2. 向量化版本（`vecEvalXxx`）不移植，只作为三种用途的参照：(a) 它是否改变"参数是否被求值"（如上面的惰性/整列差异）——由驱动复现；(b) 它是否有回退逐行的逻辑——由 `vec = eager_then_row_fallback` 复现；(c) 它在每个 chunk 只做一次的事情（如常量参数预处理）——函数体内用签名状态的懒缓存实现（见 3.5），不改变可见行为。
3. govec 对每个签名分别跑 Go 的 `evalXxx` 和 `vecEvalXxx`，Rust 两种驱动分别比对。若 Go 两版在同一输入上值或错误码不同（第 1 条之外的"真分歧"），登记为"Go 内部不一致"，默认按逐行版实现、开 issue，并把该签名的 `vectorized` 标为待评审。
4. 向量化文件中的非求值辅助函数（如 `doSleep` 在 `builtin_miscellaneous_vec.go`）照常 L1 移植，tracker 中按函数归属。
5. 本规则作为新的映射规则 `M-EVAL` 写入 `docs/porting/mappings.md`；A4 §5.7 已改为引用本条，无需再改。

### 3.4 执行侧：编译成 RPN

- 执行器构建时，把 `Arc<Expr>` 编译成 `Arc<RpnProgram>`：后序节点 `ColumnRef { index, as: EvalType }`、`Constant { expr: Arc<Expr>, as: EvalType }`、`Call { sig: &'static SigDesc, func: Arc<Expr>, nargs, state_slot }`。`as` 是父节点要求的求值类型，构建时由 `getFunction` 已确定的静态信息算出（hybrid、二进制字面量、`CanImplicitEvalInt/Real`），对应 Go 中 `args[0].EvalInt` 这类调用。
- `ColumnRef` 直接借用输入 chunk 的列和 `sel`，不复制（Go 的 `Column.VecEvalInt` 会 `CopyReconstruct`，`expression/column.go:335-354`；hybrid 列仍按 Go 逐行转换）。列格式见 N5a。
- `Constant` 节点在向量驱动下每 chunk 调一次常量的取值逻辑（含 `ParamMarker`、`DeferredExpr`），结果作为标量参与计算，不物化成整列；逐行驱动下每行调一次。与 Go 的 warning 次数一致。
- 程序本身不可变、`Send + Sync`，可在 worker 间共享；每个 worker 持有一个 `RpnState`（中间列、签名缓存、关联列取值）。
- RPN 只是向量驱动的内部形式；逐行驱动直接遍历 `Arc<Expr>`，两者调同一个函数体。

### 3.5 可变状态的去处（回答 N1 6.4）

| Go 中的可变状态 | Rust 中的去处 | 规则 |
|---|---|---|
| `bufAllocator`、`childrenVectorizedOnce` | 删除；是否可向量化在编译时算出 | `M-POOL` |
| `builtinFuncCache`（按 `CtxID` 缓存 LIKE 模式、正则、指令、JSON schema） | `RpnState` 中每个签名一个 `OnceCell`；`RpnState` 本身按执行创建，天然不跨语句、不跨会话，不再需要 `CtxID` 键 | L2（`M-EVAL`） |
| `RAND()` 的 `MysqlRng`（Clone 后共享，内部加锁） | `SigMeta` 中 `Arc<Mutex<MysqlRng>>`，共享方式与 Go 相同 | L1 |
| `CorrelatedColumn.Data`（apply 逐行写入） | 关联列编译成"关联槽"读取；apply 执行器持有槽并在每行写入，内层执行器构建时拿到槽的句柄 | L2 |
| 表达式 `hashcode` 等惰性缓存 | `OnceLock`，只在规划侧 | L1 |

结论：Rust 不需要为 worker 复制表达式。Go 中 `innerFilter.Clone()`、`baseJoiner.Clone()` 处改为"为该 worker 新建 `RpnState`"（和关联槽）。

### 3.6 为什么不依赖 TiKV 的 RPN 框架

1. 求值循环和构建器都以 `VectorValue`/`LazyBatchColumnVec`、TiKV `EvalContext`、`tipb::Expr` 为输入；N5a 若选 Go Chunk，这些代码没有一行能原样用。
2. 宏缺三样 Go 函数体需要的能力：惰性参数、访问参数类型与参数表达式、整列求值后按 warning 回退逐行。补齐等于重写 `rpn_function.rs` 的大部分。
3. 宏和求值器总共约 3,100 行非测试代码，自写成本可控；不依赖它就不需要 L3 ADR，`third_party/tikv/tidb_query_expr` 可以在 PoC 的 `query-smoke` 冒烟测试移除后删掉（ADR 0007 第 3 条据此定稿）。

### 3.7 上下文、错误与 warning

- `EvalCtx<'a>`：`&'a` N2 冻结快照（sql_mode、`TypeCtx`/`ErrCtx` 的级别、时区、div_precision_increment、当前时间、参数值、可选属性）+ `&'a mut WarnBuf`。`HandleError`、`HandleTruncate` 按 L1 决定"报错还是 warning"。
- `WarnBuf` 是本次求值调用（一个 chunk 或一次规划期求值）的局部 warning 列表，结束时一次性追加到 `StmtCtx`。`WarningCount`/`TruncateWarnings` 只作用于局部列表。Go 中并行 worker 共享 `StmtCtx` 时 `TruncateWarnings` 可能截掉别的 worker 的 warning，Rust 中不会；单线程时顺序与 Go 相同。
- collation：`getFunction` 时确定并存入 `BaseBuiltinFunc`（L1），函数体通过 `sig.collator()` 取。
- 函数体返回 `Result<Option<T>>`：`Err` 对应 Go 的 `err != nil`，`Ok(None)` 对应 `isNull=true`。Go 中"返回 NULL 同时返回 nil 错误"的分支保持原样。

### 3.8 下推

- `canFuncBePushed`、`scalarExprSupportedByTiKV`、`ExpressionsToPBList` 等 L1 移植（D08.3），只依赖 `SigDesc.pb_code` 和规划侧树，不依赖求值框架。
- 根节点不从 tipb 构建 RPN；根节点与 TiKV 的结果一致性按 ADR 0007 的结论，用对照测试检查，不共享代码。

### 3.9 阻塞型函数

`SLEEP`、`GET_LOCK` 等在 `SigDesc` 中标 `blocking`。含这类签名的程序由执行器放到阻塞池执行（`M-BLOCK`），函数体内 10ms 检查 KILL 的逻辑 L1 保留。见待定 3。

## 4. 对 L1 移植的影响（示例）

Go（`builtin_arithmetic.go:718-735`）：

```go
func (s *builtinArithmeticDivideRealSig) evalReal(ctx EvalContext, row chunk.Row) (float64, bool, error) {
	a, isNull, err := s.args[0].EvalReal(ctx, row)
	if isNull || err != nil { return 0, isNull, err }
	b, isNull, err := s.args[1].EvalReal(ctx, row)
	if isNull || err != nil { return 0, isNull, err }
	if b == 0 { return 0, true, handleDivisionByZeroError(ctx) }
	result := a / b
	if math.IsInf(result, 0) {
		return 0, true, types.ErrOverflow.GenWithStackByArgs("DOUBLE", fmt.Sprintf("(%s / %s)", s.args[0].StringWithCtx(ctx, errors.RedactLogDisable), s.args[1].StringWithCtx(ctx, errors.RedactLogDisable)))
	}
	return result, false, nil
}
```

Rust：

```rust
// go: pkg/expression/builtin_arithmetic.go (*builtinArithmeticDivideRealSig).evalReal L1 [M-EVAL]
#[eval_fn(sig = DivideReal)]   // 严格签名：a、b 的取值与 NULL 提前返回由宏生成，对应前四行
fn divide_real(ctx: &mut EvalCtx, sig: &BuiltinFunc, a: f64, b: f64) -> Result<Option<f64>> {
    if b == 0.0 {
        handle_division_by_zero_error(ctx)?;          // 非严格模式下变成 warning，返回 NULL
        return Ok(None);
    }
    let result = a / b;
    if result.is_infinite() {
        return Err(ErrOverflow.gen(("DOUBLE", format!("({} / {})",
            sig.arg(0).string_with_ctx(ctx, REDACT_DISABLE), sig.arg(1).string_with_ctx(ctx, REDACT_DISABLE)))));
    }
    Ok(Some(result))
}
// builtin_arithmetic_vec.go 中的 vecEvalReal 不移植，由宏生成的向量驱动代替。
```

惰性签名（Go `builtin_cast.go:1693-1705`）：

```rust
// go: (*builtinCastStringAsIntSig).evalInt L1 [M-EVAL]
#[eval_fn(sig = CastStringAsInt, lazy)]
fn cast_string_as_int(ctx: &mut EvalCtx, sig: &BuiltinFunc, args: &mut Args) -> Result<Option<i64>> {
    if sig.arg_type(0).hybrid() || is_binary_literal(sig.arg(0)) {
        return args.int(ctx, 0);          // 编译期已把该参数节点的 as 定为 Int
    }
    if can_implicit_eval_int(sig.arg(0)) { return args.int(ctx, 0); }
    let Some(val) = args.string(ctx, 0)? else { return Ok(None) };
    ...
}
```

执行器调用点（Go `executor/projection.go:227`）：`e.evaluatorSuit.Run(e.evalCtx, e.enableVectorizedExpression, e.childResult, chk)` → `self.evaluator_suite.run(&mut self.eval_state, &ctx.eval, self.enable_vectorized_expression, &self.child_result, chk)?`，`EvaluatorSuite` 的分组与判定逻辑 L1，只多一个 per-worker 的 `eval_state` 参数。

## 5. 验收（指标均为初拟，先测 Go 基线再定）

| 项目 | 指标 |
|---|---|
| 函数级对照 | govec 对 591 个签名生成向量（复用 `builtin_*_test.go` 与 `bench_test.go` 的 `vecExprBenchCases` 输入，加随机输入）。每条记录值、NULL、错误码与文案、warning 列表（含顺序）；Rust 逐行驱动对 Go `evalXxx`、向量驱动对 Go `vecEvalXxx`（Go 中 `vectorized()` 为 false 的签名两边都对 `evalXxx`）全部一致 |
| 模式覆盖 | `tests/integrationtest/t/expression/*.test` 在 `tidb_enable_vectorized_expression` ON 和 OFF 下各跑一遍，结果与 warning 与 Go 一致 |
| 折叠与 plan cache | `constant_fold` 相关单测、plan cache 中含 `?` 与 `NOW()` 的用例：EXPLAIN 与结果一致 |
| 静态 | `Expr: Send + Sync`、`RpnProgram: Send + Sync` 静态断言；`RpnState` 不是 `Sync` |
| 性能 | Go `BenchmarkVectorizedBuiltin*Func` 对应的输入，Rust 向量驱动吞吐 ≥ Go 1.5 倍，任一签名不低于 Go；定长类型签名每 chunk 除结果列外 0 次堆分配；表达式编译每个节点 ≤ 1µs，sysbench `oltp_point_select` QPS 不低于 PoC 基线 |
| 内存 | 中间列全部经 `TrackedAlloc` 记账；语句结束后为 0 |

## 6. 待定项（需要项目负责人决定）

1. **"Go 内部不一致"的默认处理**：建议按逐行版实现并开 issue（3.3 第 3 条）。也可以选择"按 Go 默认配置下用户看到的版本（向量化版）"，那样该签名要保留两份函数体。
2. **是否依赖 TiKV RPN 框架**：建议不依赖（3.6），ADR 0007 第 3 条据此定稿为"不使用，删除 vendored `tidb_query_expr`"。
3. **阻塞型函数**：放阻塞池（建议），还是把逐行驱动做成 async（会让所有函数体沾上 async，不建议）。
4. **SIMD**：本 spec 只保证单态化循环；某个签名需要手写 SIMD 时按 A5 单独申请，并用本 spec 的 govec 向量做等价性门禁。

## 7. 与其他 spec 的关系

| spec | 交互 |
|---|---|
| N1 | 规划侧 `Arc<Expr>` 不可变共享；本 spec 证明执行期不需要复制表达式（3.5），N1 6.4 可据此关闭 |
| N2 | `EvalCtx` 是冻结快照的视图；字段清单 = `exprstatic.EvalContext` 字段；warning 经 `WarnBuf` 汇入 `StmtCtx` |
| N3+A1 | 求值是同步 CPU 工作，在 async `Next` 内按 chunk 调用，检查点仍在 chunk 边界（`M-KILL`）；中间列用 `TrackedAlloc`；阻塞函数走 `M-BLOCK` |
| N5a | 向量驱动读写 Go 格式的 `Column`（N5a 3.4 规定接口） |
| A4 | JSON 函数按 3.3 只写一份函数体；A4 §5.6 的常量路径缓存放在 `RpnState` 的签名缓存中 |
| ADR 0007 | 第 2 条（函数体不复用）不变；第 3 条按待定 2 定稿 |

## 附：我不确定的地方

- 1.1 中"15 个修复提交"是按提交标题关键字筛的，没有逐个看补丁；v8.5.8 中两版是否还有未发现的分歧，要等 govec 跑完全部签名才知道。
- "惰性参数"的判断是从抽查的 PlusInt、DivideReal、CastStringAsInt、IF 读出来的；591 个签名中逐行版不按"左到右、遇 NULL 返回"开头的究竟有多少（即需要写成 `lazy` 的数量），没有统计。
- 向量驱动对每行调用函数体，性能是否能达到"≥ Go 1.5 倍"未测；Go 的部分向量化实现是手写的整列循环（如 `plusSS`），单态化后能否同样被自动向量化，需要基准。
- `RAND()` 在分支中、`SLEEP` 在过滤条件中这类"有副作用的函数遇上整列求值"的边界，只按代码推理与 Go 同构，没有用实际查询验证。

---

脚注（统计命令，均在 `tidb-v858/pkg/expression` 下、只计非测试文件 `F=$(ls *.go | grep -v _test.go)`）：

1. 签名：`cat $F | grep -oE '^func \([a-z]+ \*?builtin[A-Za-z0-9_]*\) eval(Int|Real|Decimal|String|Time|Duration|JSON|VectorFloat32)\(' | sort -u`（591）；同法 `vecEval…`（533）；`vectorized()` 的返回值用 awk 取下一行统计（520 true / 10 false / 1 条件 / 2 处 TODO 后 return false）；function class：`grep -oE '^type [a-zA-Z0-9_]*FunctionClass struct'`（291）；`funcs` 表条目 289。行数：`wc -l $F` 合计 71,867；`builtin*.go` 58,732；其中 `builtin*vec*.go` 23,719（含生成代码 6,239）。
2. `cat builtin*.go(非测试) | grep -oE 'args\[[0-9a-z]+\]\.GetType\(ctx\)' | wc -l` = 127；`grep -c 'StringWithCtx(ctx'` = 49。
3. `grep -rE '\.(Eval|EvalInt|EvalReal|EvalString|EvalDecimal|EvalTime|EvalDuration|EvalJSON)\(' executor planner util/ranger | grep -v _test.go | wc -l` = 204；`grep -rE '\.(VecEval[A-Za-z]*|VectorizedFilter|VectorizedExecute|VecEvalBool)\(' executor | grep -v _test.go | wc -l` = 16。
4. Go pb code：`grep -oE 'setPbCode\(tipb\.ScalarFuncSig_[A-Za-z0-9_]+' | sort -u`（526）；TiKV：`grep -oE 'ScalarFuncSig::[A-Za-z0-9_]+' tidb_query_expr/src/lib.rs | sort -u`（510），`comm -23` 得 166；`#[rpn_fn` 计数 373；框架行数取各文件 `#[cfg(test)]` 之前的行数。
