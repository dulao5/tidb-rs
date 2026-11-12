# D08.4 比较函数：spec

状态：**第 1 批**（#98）——比较运算符 `<`、`<=`、`>`、`>=`、`=`、`!=`、`<=>`：`compareFunctionClass`（含参数改写 `refineArgs`）、8 种比较类型 × 7 个运算符共 56 个签名、`CompareXxx`、`GetAccurateCmpType`、`GetCmpFunction`。第 2 批：COALESCE（§1b）。第 3 批：GREATEST/LEAST、INTERVAL、`ResolveType4Between`（§1c），D08.4 全部完成。

收敛检查（第 3 批之后）：D08.4 matched **368/368**（missing 0，extra 0，duplicated 0）。govec `cast` 领域 72,264 条（新增 9,100 条，含评审 #109 的 `gltnull` 400 条，追加在原有 63,164 条之后，原有记录逐字节不变）。

收敛检查（第 2 批之后）：D08.4 matched 286/368（+38：COALESCE 的函数类和签名），缺的 82 个是第 3 批（GREATEST/LEAST、INTERVAL 及辅助函数、`aggregateType`）；D08.9 7/678（builtin_control.go 的类型推导）。第 1 批：D08.4 matched 248/368（`builtin_compare.go` 210 + `builtin_compare_vec.go` 53 + `builtin_compare_vec_generated.go` 105），缺的 120 个都是后续批次的函数；0 extra，0 duplicated。govec `cast` 领域 29,864 条（新增 5,484 条，追加在原有 24,380 条之后，原有记录不变）。

## 1. 范围（第 1 批）

| Go | Rust（`crates/expression/src/builtin_compare.rs`） |
|---|---|
| `compareFunctionClass`（`getFunction`、`refineArgs`、`refineNumericConstantCmpDatetime`、`refineArgsByUnsignedFlag`、`handleDurationTypeComparisonForNullEq`、`generateCmpSigs`、`getDisplayName`） | `CompareFunctionClass` 与 7 个实例 `LT_CLASS`…`NULL_EQ_CLASS`，登记在 `builtin_registry::PORTED_FUNCTION_CLASSES`（`funcs` 的 `lt le gt ge eq ne nulleq`） |
| `builtin{LT,LE,GT,GE,EQ,NE,NullEQ}{Int,Real,Decimal,String,Time,Duration,JSON,VectorFloat32}Sig` 的 `evalInt`、`vecEvalInt`、`vectorized`、`Clone` | `#[eval_fn]` 函数体与 `SigDesc`（`builtinLTIntSig` 等 56 个） |
| `CompareInt/Real/Decimal/StringWithCollationInfo/Time/Duration/JSON/VectorFloat32`、`compareNull`、`genCompareString`、`resOfXX`、`vecResOfXX`、`vecCompareInt` | `compare_*`、`CompareFunc`；`resOfXX`/`vecResOfXX`/`vecCompareInt` 并入函数体与 `checked` 向量驱动 |
| `getBaseCmpType`、`GetAccurateCmpType`、`GetCmpFunction`、`isTemporalColumn`、`tryToConvertConstantInt`、`RefineComparedConstant`、`matchRefineRule3Pattern`、`allowCmpArgsRefining4PlanCache` | 同名 snake_case 函数 |
| util.go（D08.12）：`RemoveMutableConst`、`symmetricOp` | `remove_mutable_const`、`symmetric_op`（被 refineArgs 用到，随本批移植） |

## 1b. 范围（第 2 批：COALESCE）

| Go | Rust |
|---|---|
| `coalesceFunctionClass.getFunction` | `CoalesceFunctionClass` / `COALESCE_CLASS`（`builtin_compare.rs`），登记为 `coalesce` |
| `builtinCoalesce{Int,Real,Decimal,String,Time,Duration,JSON}Sig` 的 `evalXxx`、`vecEvalXxx`、`fallbackEvalXxx`、`vectorized`、`Clone`；`builtinCoalesceVectorFloat32Sig` 的 `evalVectorFloat32`、`Clone`（Go 没有向量版，`novec`） | lazy 函数体（逐行版：第一个不为 NULL 的参数或第一个错误）；向量版是 `RpnNode::RowFallback` 加 `ArgsFallback::EveryArg`（D08.1 spec §3.6），合并由函数体的向量驱动完成；`fallbackEvalXxx` 是 `eval::eval_rows_into` |
| builtin_control.go（D08.9）：`InferType4ControlFuncs`、`setFlenFromArgs`、`setDecimalFromArgs`、`addCollateAndCharsetAndFlagFromArgs`（全部分支）、`maxlen`、`hasNonBinaryStr`、`hasBinaryStr` | 新文件 `builtin_control.rs`（COALESCE 用到，提前移植；IF/IFNULL/CASE 等随 D08.9） |

- pb code：CoalesceInt 4201 … CoalesceJson 4207（go-tipb）；VECTOR 签名没有 pb code（Go 注释掉了 `setPbCode`），`vectorized()` 为 false。
- TIME 的结果用 `b.tp` 的 fsp（`res.SetFsp`），DURATION 的结果 `Fsp = b.tp.GetDecimal()`；向量版读 DURATION 列按 fsp 0（列中只存纳秒，结果列也只存纳秒）。
- **逐行版和向量版的差异**：向量版求出全部参数，任何一个参数出错或产生 warning 都会改用逐行版并截断 warning，所以结果、错误和 warning 都和逐行版一致；govec `coal`/`coalfb` 的 2,100 条记录在 Go 中两种模式的结果都相同。按代码推断唯一的例外是参数有副作用时：向量版会对每一行求所有参数（例如 `COALESCE(c, RAND())` 会多推进 RAND 的序列），逐行版遇到第一个非 NULL 的参数就停。**用户可见，已在 v8.5.8 上验证**（[#83](https://github.com/dulao5/tidb-rs/issues/83#issuecomment-6832072399)）：`RAND(seed)` 是确定的，`coalesce(a, rand(1))` 关闭向量化得到 0.4054, 5, 0.8716…，打开得到 0.4054, 5, 0.1418…；Lucky 补的 300 条这类记录 Rust 两种模式都和 Go 一致，下次重新生成 cast 向量时加入（T5c）。另外，Duration 结果的 fsp 只在 COALESCE 是最外层时看不到：`CAST(COALESCE(t0,t3) AS CHAR)` 的 2,000 条记录中，去掉 `v.fsp = fsp` 报 433 条不一致，也在 T5c 中加入。

## 1c. 范围（第 3 批：GREATEST/LEAST、INTERVAL、`ResolveType4Between`）

| Go | Rust（`builtin_compare.rs`） |
|---|---|
| `greatestFunctionClass` / `leastFunctionClass.getFunction`、`aggregateType`、`resolveType4Extremum`、`unsupportedJSONComparison`、`fixFlenAndDecimalForGreatestAndLeast` | `ExtremumFunctionClass`（`GREATEST_CLASS` / `LEAST_CLASS`），登记为 `greatest`、`least` |
| `builtin{Greatest,Least}{Int,Real,Decimal,String,Time,Duration,CmpStringAsTime,VectorFloat32}Sig` 的 `evalXxx`、`vecEvalXxx`、`vectorized`、`Clone`；`doTimeConversionForGL`、`getAccurateTimeTypeForGLRet` | lazy 函数体；`cmpAsDate` 是 `SigExtra::Extremum`；Date/CmpStringAsDate 是同一 Go 类型的另一个 `SigDesc`（pb code 不同） |
| `intervalFunctionClass.getFunction`、`builtinInterval{Int,Real}Sig` 的 `evalInt`、`vecEvalInt`、`linearSearch`、`binSearch`、`vectorized`、`Clone` | `IntervalFunctionClass`（`INTERVAL_CLASS`），`hasNullable` 是 `SigExtra::IntervalInt/IntervalReal` |
| `ResolveType4Between` | `resolve_type4_between`（pub，规划器的 BETWEEN 改写用） |

- pb code：GreatestInt 4215 … LeastTime 4224，IntervalInt 4225、IntervalReal 4226，GreatestDuration 7067、GreatestCmpStringAsTime 7068、LeastDuration 7069、LeastCmpStringAsTime 7070、Greatest/LeastCmpStringAsDate 7073/7074、Greatest/LeastDate 7075/7076；VECTOR 签名没有 pb code，`vectorized()` 为 false（`novec`）。
- **flen/decimal 在包装 CAST 之后计算**：Go 把 `args` 传给 `newBaseBuiltinFuncWithTp`，后者把 CAST 包装写回同一个切片，`fixFlenAndDecimalForGreatestAndLeast` 看到的是包装后的参数（例如 JSON 参数包装成 `CAST(.. AS CHAR)` 后 flen 为 4294967295）。去掉这一点有 3,975 条不一致。
- **`getRetTp` 的副作用**：Go 的 `sig.getRetTp().SetFlenUnderLimit(flen)` 和 `newFunctionImpl` 中的 `f.getRetTp()` 会把 flen ≥ 65536 的字符串类型改成 MEDIUMBLOB/LONGBLOB。`new_function_impl` 原来用 `f.tp.clone()`，现在调用 `get_ret_tp()`（对所有函数生效；之前已移植的函数族在 cast/expr/funcs 向量上 0 条不一致）。**以后的调用方也要调用 `get_ret_tp()`**：D08.9 的 `TryPushCastIntoControlFunctionForHybridType`（`sf.RetType = f.getRetTp()`）和 PB → 表达式（`distsql_builtin.go:1168`），见 tasks T8。
- **`newFunctionImpl` 的回调不是 Clone**：Go 把节点本身传给 `defaultScalarFunctionCheck`；Rust 之前用 `sf.clone()`（即 Go 的 `Clone`），会丢掉 `IntervalIntSig.hasNullable`。改为逐字段复制。

### 1c.1 逐行版与向量版的差异（打开向量化时能否走到逐行版）

| 签名 | 差异 | Rust | 打开向量化时可见吗 |
|---|---|---|---|
| Greatest/Least Int/Real/Decimal/Duration/Time | 逐行版遇到第一个 NULL 参数就返回（后面的参数不求），向量版求出全部参数列再 `MergeNulls` | lazy 函数体：行驱动短路，向量驱动先求全部参数列，两个版本的 warning/错误与 Go 一致 | 只有参数有 warning 或副作用时可见（同 COALESCE，§1b） |
| Greatest/Least String | collation 下相等（例如 `'a'` 与 `'A'`，`_general_ci`）时，逐行版保留**前一个**（`CompareString(v, max) > 0` 才替换），向量版取**后一个**（`CompareString(src, arg) > 0 ? src : arg`） | 函数体按 `SigView::vector()` 分支 | **可见，已在 v8.5.8 验证**：`c='a'`（`utf8mb4_general_ci`），`greatest(c,'A')` 向量化打开时返回 `A`；常量折叠 `greatest('a' collate utf8mb4_general_ci,'A')` 返回 `a`；`UPDATE .. SET d = greatest(c,'A')` 返回 `a`；关闭向量化返回 `a`。#83 的候选条目 |
| Greatest/Least Time | 结果转换成 DATE/DATETIME 出错时，逐行版 `handleInvalidTimeError`（warning + NULL），向量版直接返回错误 | `SigView::vector()` 分支 | 理论上可见（同上三条路径）；govec 3 条记录覆盖，未在 SQL 上构造 |
| Greatest/Least Time：**向量版在结果为 NULL 的行上也会报错** | 向量版合并完所有参数后对**每一行**做 `Convert`（builtin_compare_vec.go:838），变成 NULL 的行里仍留着 NULL 参数之前的极值，无效日期照样报错；逐行版在遇到 NULL 参数时直接返回 NULL，不做转换 | 函数体在向量驱动中遇到 `i > 0` 的 NULL 参数时，先转换已有的值，出错就返回错误（`args[0]` 为 NULL 时 Go 的槽位是零值时间，转换不会失败）。govec `gltnull` 400 条，去掉这一处有 66 条不一致（Lucky 评审 #109） | **可见，已在 v8.5.8 验证**（[#83](https://github.com/dulao5/tidb-rs/issues/83#issuecomment-6835656191)）：`d='1000-03-00'`、`dt=NULL`，默认严格模式下 `insert into gt2 select greatest(d,dt) from gt`：关闭向量化插入 NULL，打开报 ERROR 1292 |
| Greatest/Least CmpStringAsTime | 向量版逐个参数：求第 `j` 个参数列，再逐行转换，然后才求第 `j+1` 个；逐行版逐行 | `ArgsFallback::OwnArgs`（D08.1 spec §3.6）：签名自己按 Go 的顺序求参数，warning 顺序和第一个错误与 Go 相同。改成"先求全部参数列"有 20 条不一致 | warning 顺序不同、第一个错误可能不同 |
| Interval Int/Real | 向量版只把 `args[0]` 求成列，其余参数在每行的查找中逐行求 | `ArgsFallback::OwnArgs` | 与逐行版相同 |

- **INTERVAL 的 upstream 问题（已在 v8.5.8 验证）**：`builtinIntervalIntSig.Clone` 不复制 `hasNullable`（`builtinIntervalRealSig.Clone` 复制）。规划器的 `ResolveIndices` 会 Clone 表达式，所以从 SQL 来的 INTERVAL(int) 实际总是二分查找：`a=10, b=NULL, x=1` 时 `interval(5, a, b, x)` 返回 **3**（打开、关闭向量化都是），常量折叠 `interval(5, 10, NULL, 1)` 返回 0，`interval(5.5, a, b, x)` 返回 0。Rust 照搬（`BuiltinFunc::clone_from_base` 对 `SigExtra::IntervalInt` 重置 `has_nullable`；govec `glclone` 记录覆盖，去掉重置有 13 条不一致）。建议登记为 upstream-pitfall。

## 2. 依赖

D08.1（`#[eval_fn]`、`RpnProgram`、`Args`）、D08.2（`newBaseBuiltinFuncWithTp`、collation 推导、`MaybeOverOptimized4PlanCache`）、D08.8（`WrapWithCastAs*`）、D08.12 的 `DisableParseJSONFlag4Expr`、D01.x 的比较原语（`types::compare_int`、`compare_binary_json`、`MyDecimal::compare`、`Time/Duration::compare`、`VectorFloat32::compare`、collator）。

## 3. 设计

### 3.1 签名体

- **整数比较**（`LT/LE/GT/GE/EQ/NE Int`）：`CompareInt` 先求两个参数再看 NULL（`eager`）。函数体没有出错行，所以用 `checked` 形式（D08.1 §3.4）：标志位是两个参数的 unsigned 标志，向量驱动按组合各编一个循环，对应 Go 的 `vecCompareInt` 四个分支加 `vecResOfXX`。§3.4 的三个条件都满足：(a) 没有可能出错的检查；(b) 没有逐行副作用；(c) Go 的逐行版与向量版结果相同。
- **其他类型**（Real/Decimal/String/Time/Duration/JSON）：`eager` 的严格函数体，`v <op> 0`。String 用签名缓存的 collator（Go 的 `types.CompareString(.., b.collation)` 每行按名字取 collator，结果相同）。Real 用 Go `cmp.Compare` 的 NaN 规则（`tidb_types::datum::cmp_f64`，本 PR 改为 `pub`）。
- **VECTOR**：Go 没有向量版（`vectorized()` 为 false），函数体标 `novec`。
- **NULL 安全等于**（`NullEQ*`）：`lazy` 函数体，自己求两个参数并处理 NULL（两边 NULL 为 1，一边 NULL 为 0）。为此 `eval::Args` 增加 `time`、`duration`、`json`、`vector_float32` 四个方法（行驱动调用 `EvalXxx`，向量驱动读列；Duration 列按 fsp 0 读，比较只看纳秒）。第一个参数先转成拥有的值（`Own`），以便借用第二个参数。

### 3.2 `CompareFunc`

Go 的 `CompareFunc` 是函数值，`genCompareString` 闭包捕获 collation。Rust 用枚举 `CompareFunc { Int, Real, .., String(collation), .. }` 加 `call`。`GetCmpFunction` 推导 collation 失败时 Go 读空指针 panic（`coll, _ :=` 后读 `coll.Collation`），Rust 同样 panic（M-PANIC）。

### 3.3 `refineArgs`

逐行照 Go 移植，包括 plan cache 的判断、`RemoveMutableConst`、`<=>` 的 duration 改写、datetime 列对数值常量、int 列对非 int 常量（`RefineComparedConstant`）、YEAR、unsigned 标志改写为 `0/1` 常量。Go 原地修改 YEAR 常量（调用方传入的节点）和 `RemoveMutableConst` 的常量；Rust 改的是副本（共享节点先复制，M-EXPR，§4 第 1 条）。

### 3.4 T6c（D08.1 tasks.md）的验收

- (a) `checked`：整数比较采用 `checked`。比较没有出错行，"NULL 下藏着出错值"和"第一个出错行不在第 0 行"两类记录对它没有意义；NULL 行下的值（govec 的 NULL 列值随机）在两个驱动都被忽略，记录覆盖了 NULL。
- (b) 带状态的签名：比较签名没有状态（Go 的 `Clone` 只复制 `baseBuiltinFunc`），不适用；留给第一个带状态的函数族（LIKE/REGEXP，D08.6）。
- (c) `newFunctionImpl` 的折叠：`NewFunction`（fold 1）现在经 `funcs` 走到，govec `newfunc` 记录（来源 `cmp`）覆盖：不折叠时 287 条不一致。`NewFunctionTryFold`（fold -1）"折叠产生 warning 时返回未折叠的函数"这一支，比较函数族走不到：常量参数的 warning 在构建 CAST 包装（`WrapWithCastAs*` 折叠常量）时就产生了，早于 TryFold 记录的 warning 数，比较本身求值不产生 warning。`tryfold` 记录（200 条）固定了这个行为；该分支留给第一个求值会产生 warning 的函数族（算术，D08.5）。
- (d) `DisableParseJSONFlag4Expr`：`generateCmpSigs` 对 JSON 比较调用它。govec `json-parse` 记录：`CAST('[1]' AS JSON) <op> '[1]'` 等 6 个字符串 × 7 个运算符 × 不折叠/折叠，`=` 的结果为 0（v8.5.8 行为）。去掉调用后 176 条不一致。

## 4. 偏离

1. **原地修改的常量**：Go 的 `refineArgs` 把 YEAR 常量的值原地改成调整后的年份（调用方持有的同一个节点也变了），`RemoveMutableConst` 原地清掉 ParamMarker/DeferredExpr。Rust 改副本，调用方的节点不变（M-EXPR）。SQL 看不到：调用方只用返回的参数。
2. **`RefineComparedConstant` 的 Ceil/Floor**（Lucky 评审 #98；**D08.5 第 1 批已移植 ceil/floor，下面的结果差异已消失**，govec `ceilcmp` 覆盖，见 specs/D08.5-arith/spec.md §5）。`int 非常量 <,<=,>,>= 非 int 常量`，在常量能转换成 int 类型（不出错、不溢出）但与原值不相等时，Go 用 `NewFunctionInternal(ctx, ast.Ceil/Floor, ..)` 把常量取整后再转换。数学函数（D08.5）移植前 Rust 构建不出 `ceil`/`floor`，`new_function_internal` 返回 `None`，常量不改写，**比较结果可能与 Go 不同**：Lucky 去掉 govec 的过滤后有 368 条不一致，其中 62 条结果值不同、59 条在 YEAR 列上。例如 v8.5.8 中 `y < 2e0`（y=1977）返回 1，因为常量取整后按 YEAR 转换成 2002；Rust 不取整，返回 0。v8.5.8 本身也不一致：`y < 2.0` 按 2 比较，`y < 2`、`'2'`、`2e0` 按 2002 比较，见 upstream-pitfall [#99](https://github.com/dulao5/tidb-rs/issues/99)。
   - govec 只跳过真正走到 Ceil/Floor 的记录（`cmpReachesCeil`：在 Go 中执行到 Ceil/Floor 之前的同样步骤来判断）；之前的分支（NULL 常量、转换出错、溢出改写为 0/1、`int_col < 2.0` 这类相等的情况）都生成了记录（来源 `refine` 600 条）。
   - **在 D08.5 移植 ceil/floor 之前，任何 SQL 路径都不能构建这类比较**（tidb-rs 目前没有 SQL 路径用到 `compareFunctionClass`，接入规划器/执行器时要检查这一点）。
   - D08.5 的任务（plan.md）：移植 ceil/floor，去掉 `cmpReachesCeil` 过滤，补 YEAR 列的记录（DOUBLE、短 DECIMAL、长 DECIMAL、字符串常量）。
3. **`CompareFunc` 是枚举**（§3.2）。
4. **NULL 安全等于的 Duration 向量读取用 fsp 0**：比较只看纳秒，与 Go 相同（函数体中有注释）。
5. **第 3 批没有新的偏离**：§1c.1 中逐行版与向量版的差异 Rust 两个驱动都按 Go 实现（与关闭/打开向量化的 Go 一致），不是 tidb-rs 的偏离，所以没有写入 `docs/compatibility.md`；String 的 collation 相等规则和 INTERVAL 的 Clone 问题作为 #83 / upstream-pitfall 候选。
6. 没有面向用户的差异，`docs/compatibility.md` 只补了 #97 的链接（D08.1 的条目）。
