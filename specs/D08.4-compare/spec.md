# D08.4 比较函数：spec

状态：**第 1 批（本 PR）**——比较运算符 `<`、`<=`、`>`、`>=`、`=`、`!=`、`<=>`：`compareFunctionClass`（含参数改写 `refineArgs`）、8 种比较类型 × 7 个运算符共 56 个签名、`CompareXxx`、`GetAccurateCmpType`、`GetCmpFunction`。第 2 批及以后：COALESCE、GREATEST/LEAST、INTERVAL 及其辅助函数（`aggregateType`、`resolveType4Extremum` 等，120 个函数）。

收敛检查（第 1 批）：D08.4 matched 248/368（`builtin_compare.go` 210 + `builtin_compare_vec.go` 53 + `builtin_compare_vec_generated.go` 105），缺的 120 个都是后续批次的函数；0 extra，0 duplicated。govec `cast` 领域 29,264 条（新增 4,884 条，追加在原有 24,380 条之后，原有记录不变）。

## 1. 范围（第 1 批）

| Go | Rust（`crates/expression/src/builtin_compare.rs`） |
|---|---|
| `compareFunctionClass`（`getFunction`、`refineArgs`、`refineNumericConstantCmpDatetime`、`refineArgsByUnsignedFlag`、`handleDurationTypeComparisonForNullEq`、`generateCmpSigs`、`getDisplayName`） | `CompareFunctionClass` 与 7 个实例 `LT_CLASS`…`NULL_EQ_CLASS`，登记在 `builtin_registry::PORTED_FUNCTION_CLASSES`（`funcs` 的 `lt le gt ge eq ne nulleq`） |
| `builtin{LT,LE,GT,GE,EQ,NE,NullEQ}{Int,Real,Decimal,String,Time,Duration,JSON,VectorFloat32}Sig` 的 `evalInt`、`vecEvalInt`、`vectorized`、`Clone` | `#[eval_fn]` 函数体与 `SigDesc`（`builtinLTIntSig` 等 56 个） |
| `CompareInt/Real/Decimal/StringWithCollationInfo/Time/Duration/JSON/VectorFloat32`、`compareNull`、`genCompareString`、`resOfXX`、`vecResOfXX`、`vecCompareInt` | `compare_*`、`CompareFunc`；`resOfXX`/`vecResOfXX`/`vecCompareInt` 并入函数体与 `checked` 向量驱动 |
| `getBaseCmpType`、`GetAccurateCmpType`、`GetCmpFunction`、`isTemporalColumn`、`tryToConvertConstantInt`、`RefineComparedConstant`、`matchRefineRule3Pattern`、`allowCmpArgsRefining4PlanCache` | 同名 snake_case 函数 |
| util.go（D08.12）：`RemoveMutableConst`、`symmetricOp` | `remove_mutable_const`、`symmetric_op`（被 refineArgs 用到，随本批移植） |

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
2. **`RefineComparedConstant` 的 Ceil/Floor**：`int 非常量 <,<=,>,>= 非 int 常量` 时 Go 用 `NewFunctionInternal(ctx, ast.Ceil/Floor, ..)` 把常量取整。数学函数（D08.5）移植前 Rust 构建不出 `ceil`/`floor`，`new_function_internal` 返回 `None`，常量不改写（比较结果相同，只是没有取整；与 Go 不同的是参数形式，例如 `a < 1.5` 不变成 `a < 2`）。govec 不生成这类记录（`cmpCeilPath`）。**D08.5 移植 ceil/floor 后去掉这个过滤。**
3. **`CompareFunc` 是枚举**（§3.2）。
4. **NULL 安全等于的 Duration 向量读取用 fsp 0**：比较只看纳秒，与 Go 相同。
5. 没有面向用户的差异，`docs/compatibility.md` 只补了 #97 的链接（D08.1 的条目）。
