# D08.5 算术、数学与逻辑运算：spec

状态：**第 1 批（本 PR）**——算术运算符 `+ - * / DIV %`（6 个函数类、20 个数值签名和 3 个 VECTOR 签名）与 CEIL / CEILING / FLOOR（2 个函数类、10 个签名）。之后的批次：其余数学函数（ABS、ROUND、TRUNCATE、LOG、POW、SQRT、三角函数、RAND、CONV、CRC32 等）、逻辑运算（AND/OR/XOR/NOT）、位运算、IS TRUE / IS FALSE、一元负号，以及常量传播（D08.2 第 2b 部分）。

收敛检查（第 1 批）：D08.5 matched 152/458；builtin_arithmetic.go 与 _vec.go 的 108 个函数全部移植，builtin_math.go 与 _vec.go 中 CEIL/FLOOR 的 44 个移植，其余 173 个（数学文件中的其他函数）和 builtin_op*.go 的 133 个属于后续批次。0 extra，0 duplicated。govec `cast` 领域 36,464 条（新增 6,600 条，追加在原有 29,864 条之后，原有记录逐字节不变）。

## 1. 范围（第 1 批）

| Go | Rust |
|---|---|
| `arithmetic{Plus,Minus,Multiply,Divide,IntDivide,Mod}FunctionClass.getFunction`、`setType4DivDecimal`、`setType4DivReal`、`setType4ModRealOrDecimal`、`numericContextResultType`、`isConstantBinaryLiteral`、`setFlenDecimal4RealOrDecimal`、`testIfSumOverflowsUll` | `builtin_arithmetic.rs`：`ArithmeticFunctionClass` 与 6 个实例（`funcs` 的 `plus minus mul div intdiv mod`） |
| 20 个签名 `builtinArithmetic{Plus,Minus,Multiply}{Int,Real,Decimal}Sig`、`MultiplyIntUnsigned`、`Divide{Real,Decimal}`、`IntDivide{Int,Decimal}`、`ModInt{UU,US,SU,SS}`、`Mod{Real,Decimal}`（含 `plusUU/US/SU/SS`、`divideUU/US/SU/SS`、`overflowCheck`） | `#[eval_fn]` 函数体与 `SigDesc` |
| `builtinArithmetic{Plus,Minus,Multiply}VectorFloat32Sig` | 同上（`novec`，`vectorized` 为 false，pb code 0，同 Go） |
| `ceilFunctionClass`、`floorFunctionClass`、`getEvalTp4FloorAndCeil`、`setFlag4FloorAndCeil`、`builtin{Ceil,Floor}{Real,IntToInt,IntToDec,DecToInt,DecToDec}Sig` | `builtin_math.rs`：`CeilFloorFunctionClass` 与 3 个实例（`ceil ceiling floor`） |
| Go `math.Mod` | `tidb_base::gostd::math::r#mod`（特殊情况返回 Go 的 NaN，payload 1） |

## 2. 依赖

D08.1（`#[eval_fn]`、`RpnProgram`）、D08.2（`newBaseBuiltinFuncWithTp`）、D08.8（`WrapWithCastAs*`、`PropagateType`）、D01.2（`DecimalAdd/Sub/Mul/Div/Mod`、`Round`、`ToInt/ToUint`）、D01.x 的 `DivInt64` 等、D01.6 的 `VectorFloat32.Add/Sub/Mul`。

## 3. 设计

### 3.1 签名体与 `checked`（D08.1 spec §3.4）

- **`checked`**：`PlusInt`（4 种有无符号组合各一个循环）、`MultiplyInt`、`MultiplyIntUnsigned`、`Plus/Minus/MultiplyReal`。三个条件都满足：(a) Go 的向量版先判断溢出、再看 `result.IsNull(i)`，NULL 行跳过；(b) 出错前没有逐行副作用（只有溢出错误）；(c) 逐行版与向量版报同样的错误。`PlusReal` 在 Go 的逐行版中先求两个参数再看 NULL（`eager`）。
- **不用 `checked`**：`MinusInt`（错误类型取决于 SQL mode 的 `NO_UNSIGNED_SUBTRACTION`，flag 函数读不到上下文）；除法与取模（除零经 `handleDivisionByZeroError` 产生 warning，是逐行副作用）；DECIMAL 的签名（`checked` 只支持 Int/Real 结果）。它们用普通的严格函数体；`ModInt*`、`ModReal` 在 Go 中先求两个参数（`eager`）。
- **T6c (a)**：govec `int`、`real` 来源含"NULL 下藏着出错值"的行（NULL 行的数据为 0：`0 - MinInt64`、无符号 NULL + 负数、DOUBLE NULL + Inf）与"第一个出错行不在第 0 行"（前一半行多为小值）。
- **T6c (c)**：`artryfold` 记录（192 条）走到 `NewFunctionTryFold` 中"折叠产生 warning 时返回未折叠的函数"（warning 级别的除零、截断的字符串参数）；去掉这个分支后 16 条不一致。

### 3.2 Go 的逐行版与向量版的差异（规则 (c)，各自跟随）

| 签名 | 差异 | 用户可见 |
|---|---|---|
| `IntDivideDecimal` | 溢出错误：逐行版打印参数（`StringWithCtx`），向量版打印两个 decimal 值（`num[i].String()`） | 按代码推断可见（`c DIV 0.0001` 溢出时，错误文本随 `tidb_enable_vectorized_expression` 变化）；未在集群上实测 |
| `DivideDecimal` | `ErrTruncated`：逐行版 `tc.HandleTruncate`，参数是 `*MyDecimal`（String）；向量版 `ec.HandleError`，参数是 `types.MyDecimal` 值（按字段打印） | 按代码推断 warning 文本可见（同 #83 的 `DecimalAsInt`）；未实测 |

### 3.3 Go 读到的是包 CAST 之后的参数

`newBaseBuiltinFuncWithTp` 原地替换 `args[i]`（同一个切片），所以 Go 在它之后读 `args[i]`（`setFlenDecimal4RealOrDecimal`、PLUS/MINUS/MOD 的有无符号判断、CEIL/FLOOR 的 `setFlag4FloorAndCeil` 与 flen）读到的是包了 CAST 的参数。Rust 读 `bf.args[i]`。改回读原参数后 309 条不一致。

### 3.4 MOD 读到的原地修改的类型

`arithmeticModFunctionClass.getFunction` 在构建前取 `lhsTp, rhsTp`（指针），构建后才读。构建 `CAST(decimal AS REAL)` 时 `PropagateType` 原地修改常量或函数参数的类型（列会先复制，D08.8 spec §4.6），所以 Go 读到的是修改后的类型，例如 `DECIMAL(30,5) 常量 % 字符串` 的结果 decimal 为 30 而不是 5。Rust 在 `type_seen_after_real_wrap` 中对非列的 DECIMAL 参数复现这一修改（`propagate_type` 作用在副本上）。去掉后 10 条不一致。

## 4. 偏离

1. 第 3.4 节：复现 Go 读到的修改后的类型，而不是共享节点的原地修改（M-EXPR）。
2. `RpnProgram` 的常量填充（`fill_constant`，D08.1）在 0 行时不再求值常量，同 Go `genVecFromConstExpr`（之前会在 0 行输入上报常量的错误）；本批的 govec 记录发现。
3. 两个平台的记录有 5 条 `arbuild` 不同：DOUBLE 的无效运算（Inf/Inf、Inf-Inf、0*Inf）产生硬件默认 NaN，x86 是负号的 `0xFFF8000000000000`，ARM 是 `0x7FF8000000000000`。Go 和 Rust 都用硬件运算，各平台回放自己的文件（tracker `targetDiffs`）。
4. 没有面向用户的差异，`docs/compatibility.md` 不变。

## 5. D08.4 第 1 批的后续（D08.4 T6，Lucky 评审 #98）

- ceil/floor 已移植并登记到 `function_class`，`RefineComparedConstant` 的 `NewFunctionInternal(Ceil/Floor)` 现在能构建，D08.4 spec §4 第 2 条的结果差异消失。
- govec `ceilcmp`（900 条）覆盖走到 Ceil/Floor 的比较：YEAR 列对 DOUBLE、短 DECIMAL、长 DECIMAL、字符串常量，整数列对非整数常量。D08.4 的记录保留 `cmpReachesCeil` 过滤，以保持已合并的记录逐字节不变；`ceilcmp` 不过滤。去掉 ceil/floor 的登记后 1,745 条不一致。
