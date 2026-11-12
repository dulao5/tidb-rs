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

## 1b. 范围（第 2 批，`builtin_op.rs`）

| Go | Rust |
|---|---|
| `logic{And,Or,Xor}FunctionClass.getFunction`、`builtinLogic{And,Or,Xor}Sig`（含 `fallbackEvalInt`） | `LogicFunctionClass` 与 3 个实例（`and or xor`）；AND/OR 的向量版是 `RpnNode::RowFallback`（§3.5） |
| `isTrueOrFalseFunctionClass`（含 `getDisplayName`）、`builtin{Real,Decimal,Int,VectorFloat32}Is{True,False}Sig` | `IsTrueOrFalseFunctionClass` 与 3 个实例（`istrue istrue_with_null isfalse`）；`keepNull` 放在 `SigExtra::IsTrueOrFalse`（D08.1 §3.5 (a)），带 keepNull 的 pb code 为 `...WithNull`；VECTOR 的两个签名 `novec`、pb code 0（Go 注释掉了 `setPbCode`） |
| `unaryNotFunctionClass`、`builtinUnaryNot{Real,Decimal,Int,JSON}Sig` | `UnaryNotFunctionClass`（`not`）；JSON 参数在构建时追加 `errJSONInBooleanContext` warning，签名 `novec` |
| `unaryMinusFunctionClass`（`handleIntOverflow`、`typeInfer`）、`builtinUnaryMinus{Int,Decimal,Real}Sig` | `UnaryMinusFunctionClass`（`unaryminus`）；`constantArgOverflow` 放在 `SigExtra::UnaryMinusDecimal` |
| `wrapWithIsTrue`（D08.1，expression.go）、`logicalOps`（util.go） | `expression::wrap_with_is_true`、`util::is_logical_op` |

IS NULL 随常量传播移植（tasks T3b）。

## 1c. 范围（第 3 批：位运算，`builtin_op.rs`）

| Go | Rust |
|---|---|
| `bit{And,Or,Xor}FunctionClass`、`{left,right}ShiftFunctionClass` 的 `getFunction`；`builtinBit{And,Or,Xor}Sig`、`builtin{Left,Right}ShiftSig` | `BitBinaryFunctionClass` 与 5 个实例（`bitand bitor bitxor leftshift rightshift`）；签名体是 lazy（`args(Int, Int)`）：逐行版在 `args[0]` 为 NULL 时不求 `args[1]`，向量驱动两列都求（§3.2） |
| `bitNegFunctionClass`、`builtinBitNegSig` | `BitNegFunctionClass`（`bitneg`），普通签名体 |

- 结果类型都加 `UnsignedFlag`（Go 在 `setPbCode` 之后改 `sig.tp`，与 `RetType` 是同一个对象，D08.1 §3.2）。
- 移位：Go 的 `uint64(a) << uint64(b)` 在移位数 ≥ 64 时为 0，负的移位数转成很大的 uint64，也是 0；Rust 的 `<<` 会溢出，所以先判断移位数。`>>` 是逻辑右移（按 uint64）。
- pb code：BitAnd 3118、BitOr 3119、BitXor 3120、BitNeg 3121、LeftShift 3129、RightShift 3130（go-tipb）。
- 不用 `checked` 写法：这几个运算不会出错（§3.4 不适用）。
- 两个平台有 8 条 `bitop` 记录不同：参数是 NaN 的 DOUBLE，经 `CAST(.. AS SIGNED)`（`builtinCastRealAsIntSig`）转成整数时随平台不同（M-F2I），登记在 tracker `targetDiffs`，各平台回放自己的文件。

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
| `IntDivideDecimal` | 溢出错误：逐行版打印参数（`StringWithCtx`），向量版打印两个 decimal 值（`num[i].String()`） | **用户可见，已在 v8.5.8 上验证**（[#83](https://github.com/dulao5/tidb-rs/issues/83#issuecomment-6814585483)）：`c DIV 0.0001` 溢出时，向量版的错误信息里是行里的数据，逐行版只写列名 |
| `DivideDecimal` | `ErrTruncated`：逐行版 `tc.HandleTruncate`，参数是 `*MyDecimal`（String）；向量版 `ec.HandleError`，参数是 `types.MyDecimal` 值（按字段打印） | **用户可见，已在 v8.5.8 上验证**（[#83](https://github.com/dulao5/tidb-rs/issues/83#issuecomment-6814585483)）：warning 文本随 `tidb_enable_vectorized_expression` 变化 |
| `LogicXor`（第 2 批） | 逐行版在 `args[0]` 为 NULL 时不求 `args[1]`；向量版两列都求（`MergeNulls`）。`args[1]` 在这些行上的 warning 或错误只在向量化打开时出现 | **用户可见，已在 v8.5.8 上验证**（[#83](https://github.com/dulao5/tidb-rs/issues/83#issuecomment-6816966507)）：关闭向量化时 `SELECT a XOR (b DIV 0)` 只有 1 条 warning、严格模式的 `INSERT ... SELECT` 成功；打开后变成 3 条 warning，INSERT 报 `ERROR 1365 Division by 0`。govec `logicfb` 中 4 条记录两种模式不同 |
| `Bit{And,Or,Xor}`、`{Left,Right}Shift`（第 3 批） | 同 `LogicXor`：逐行版在 `args[0]` 为 NULL 时不求 `args[1]`，向量版两列都求（`MergeNulls`） | 按负责人 2026-11-12 的决定（关闭向量化是历史负债），只分析开关为 ON 时逐行版能否走到：能走到的地方是不能向量化的表达式和按行求值的执行器（#83 的第二类）；常量折叠时参数都是常量，`args[1]` 的错误或 warning 在它自己折叠时就已产生，看不出差别（按代码推断）。未在集群上实测。govec `bitfb` 覆盖（反证：逐行版也求 `args[1]` 时报 28 条不一致） |

### 3.3 Go 读到的是包 CAST 之后的参数

`newBaseBuiltinFuncWithTp` 原地替换 `args[i]`（同一个切片），所以 Go 在它之后读 `args[i]`（`setFlenDecimal4RealOrDecimal`、PLUS/MINUS/MOD 的有无符号判断、CEIL/FLOOR 的 `setFlag4FloorAndCeil` 与 flen）读到的是包了 CAST 的参数。Rust 读 `bf.args[i]`。改回读原参数后 309 条不一致。

### 3.4 MOD 读到的原地修改的类型

`arithmeticModFunctionClass.getFunction` 在构建前取 `lhsTp, rhsTp`（指针），构建后才读。构建 `CAST(decimal AS REAL)` 时 `PropagateType` 原地修改常量或函数参数的类型（列会先复制，D08.8 spec §4.6），所以 Go 读到的是修改后的类型，例如 `DECIMAL(30,5) 常量 % 字符串` 的结果 decimal 为 30 而不是 5。Rust 在 `builtin_cast::type_after_real_wrap`（第 2 批从 `builtin_arithmetic.rs` 挪到 `propagate_type` 旁边并改名，Lucky 评审 #100）中对非列的 DECIMAL 参数复现这一修改（`propagate_type` 作用在副本上）。去掉后 10 条不一致。

### 3.5 AND / OR 的向量版：`RpnNode::RowFallback`（第 2 批）

Go 的 `builtinLogic{And,Or}Sig.vecEvalInt` 自己求两个参数：先求 `args[0]`（出错直接返回），再求 `args[1]`；如果 `args[1]` 出错或增加了 warning，就把 warning 截断到 `args[0]` 之前，整个 chunk 改用逐行版（`fallbackEvalInt`，逐行版有短路）。RPN 是后序的，参数在调用之前就已求完，所以这两个签名编译成 `RpnNode::RowFallback`：两个参数各是一个子程序，节点按 Go 的顺序求、比较 warning 数、截断并逐行回退；否则由函数体（`lazy`）的向量驱动合并两列，与 Go 的合并循环等价。`builtin_op::falls_back_to_rows` 按签名判断。反证：回退不看 warning 17 条、不截断 warning 30 条不一致。

## 4. 偏离

1. 第 3.4 节：复现 Go 读到的修改后的类型，而不是共享节点的原地修改（M-EXPR）。
2. `RpnProgram` 的常量填充（`fill_constant`，D08.1）在 0 行时不再求值常量，同 Go `genVecFromConstExpr`（之前会在 0 行输入上报常量的错误）；本批的 govec 记录发现。
3. 两个平台的记录有 5 条 `arbuild` 不同：DOUBLE 的无效运算（Inf/Inf、Inf-Inf、0*Inf）产生硬件默认 NaN，x86 是负号的 `0xFFF8000000000000`，ARM 是 `0x7FF8000000000000`。Go 和 Rust 都用硬件运算，各平台回放自己的文件（tracker `targetDiffs`）。同类问题见 #27。
4. 没有面向用户的差异，`docs/compatibility.md` 不变。
5. （第 2 批）`unaryMinusFunctionClass.getFunction` 在构建前取 `args[0].GetType()`：一元负号不会把 DECIMAL 参数包成 REAL，所以 `PropagateType` 改不到它，直接用构建前的类型（§3.4 的情况不会发生）。
6. （第 2 批）`isTrueOrFalseFunctionClass` 的 `unexpected types.EvalType %v` 与 `unaryNotFunctionClass` 的 `%s is not supported for unary not operator` 是普通错误（`Error::plain`），文字用 `EvalType.String()`，同 Go。

## 移植检查表（Lucky 评审 #100）

- 在包 CAST（`newBaseBuiltinFuncWithTp`）**之前**取了类型指针、包完之后才读的，Go 看到的是修改后的类型吗？（`PropagateType` 只改非列的 DECIMAL 参数，见 §3.4、`builtin_cast::type_after_real_wrap`）

## 5. D08.4 第 1 批的后续（D08.4 T6，Lucky 评审 #98）

- ceil/floor 已移植并登记到 `function_class`，`RefineComparedConstant` 的 `NewFunctionInternal(Ceil/Floor)` 现在能构建，D08.4 spec §4 第 2 条的结果差异消失。
- govec `ceilcmp`（900 条）覆盖走到 Ceil/Floor 的比较：YEAR 列对 DOUBLE、短 DECIMAL、长 DECIMAL、字符串常量，整数列对非整数常量。D08.4 的记录保留 `cmpReachesCeil` 过滤，以保持已合并的记录逐字节不变；`ceilcmp` 不过滤。去掉 ceil/floor 的登记后 1,745 条不一致。
