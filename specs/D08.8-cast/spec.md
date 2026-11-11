# D08.8 CAST：spec

状态：第 1 部分——CAST 到 INT、REAL、DECIMAL；`BuildCastFunction*`、`WrapWithCastAsInt/Real/Decimal`；随之移植 D08.2 的常量折叠（`constant_fold.go`）与 `baseBuiltinCastFunc`、D08.1 的 `PropagateType`。第 2a 部分：RPN 列参数的转换。**第 2b-1 部分（本 PR）**——CAST 到 STRING（INT/REAL/DECIMAL/STRING/hybrid 参数）、`BuildCastCollationFunction`、`WrapWithCastAsString`；`builtin_convert_charset.go`（to_binary/from_binary、`HandleBinaryLiteral`，从 D08.6 移入本块）；D08.2 的 `newBaseBuiltinFuncWithTp`、`newBaseBuiltinCastFunc4String`。第 2b-2 部分：CAST 到 TIME/DURATION/JSON/VECTOR 及从它们出发的签名；第 2b-3 部分：D08.2 第 2 部分其余与 D08.1 2b。

收敛检查（第 2b-1 部分）：D08.8 matched 102/266（块中新增 `builtin_convert_charset.go` 的 16 个函数，全部移植）；D08.2 79/151（`newBaseBuiltinFuncWithTp`、`newBaseBuiltinCastFunc4String`）；D08.1 201/348。govec `cast` 领域 7,380 条（新增 2,820 条，追加在原有记录之后，原有 4,560 条不变）。

收敛检查（第 1 部分）：D08.8 matched 64/250（`builtin_cast.go` 152 + `builtin_cast_vec.go` 98，第 1 部分移植 12 个签名的 `evalXxx`/`vecEvalXxx`/`vectorized`/`Clone`、3 个函数类、`BuildCastFunction*`、`WrapWithCastAs{Int,Real,Decimal}` 等）；D08.2 77/151（第 1 部分 68）；D08.1 198/348（2a 197）；D08.12 2/135（`MaybeOverOptimized4PlanCache`、`containMutableConst`）。govec `cast` 领域 3,760 条，覆盖 D08.8 的 61 个函数，豁免 189 个（`cast_pending.go`）。

## 1. 范围

| 部分 | 内容 |
|---|---|
| 1 | `castAsInt/Real/DecimalFunctionClass.getFunction`；签名 `builtinCast{Int,Real,Decimal,String}As{Int,Real,Decimal}Sig`（12 个，含 `handleOverflow`）；`newBaseBuiltinCastFunc`、`baseBuiltinCastFunc.metadata/cloneFrom`（D08.2）；`BuildCastFunction`、`BuildCastFunction4Union`、`BuildCastFunctionWithCheck`；`WrapWithCastAsInt/Real/Decimal`、`minimalDecimalLenForHoldingInteger`；`setDataTypeDouble`、`floatLength`、`PropagateType`（D08.1）；`CanImplicitEvalInt/Real`；`TryPushCastIntoControlFunctionForHybridType`（临时桩，见 §4）；`constant_fold.go`（D08.2） |
| 1（临时桩） | 函数类会选中的 `builtinCast{Time,Duration,JSON}As{Int,Real,Decimal}Sig`、`builtinCastVectorFloat32AsUnsupportedSig`：只有描述（名字、`tipb` 码、`vectorized`），没有驱动；求值返回 Go 的 `baseBuiltinFunc` 错误 |
| 2a | RPN 列参数按签名读取的求值类型转换（`Column.VecEvalInt/Real/String`） |
| 2b-1 | `castAsStringFunctionClass`；签名 `builtinCast{Int,Real,Decimal,String}AsStringSig`（`builtinCast{Time,Duration,JSON,VectorFloat32}AsStringSig` 为临时桩）；`adjustRetFtForCastString`、`decimalPrecisionToLength`、`padZeroForBinaryType`；`newBaseBuiltinCastFunc4String`、`newBaseBuiltinFuncWithTp`（D08.2；TIME/DURATION/JSON/VECTOR 参数的包装为临时桩）；`WrapWithCastAsString`、`BuildCastCollationFunction`；`builtin_convert_charset.go` 全部 16 个函数（`tidbTo/FromBinaryFunctionClass`、`builtinInternalTo/FromBinarySig`、`BuildTo/FromBinaryFunction`、`HandleBinaryLiteral`、`init`/`convertActionMap`、`isLegacyCharset`、`formatInvalidChars`） |
| 2b-2a | `#[eval_fn]` 支持 Time/Duration/JSON/VectorFloat32 参数；签名 `builtinCast{Time,Duration,JSON}As{Int,Real,Decimal,String}Sig`、`builtinCastVectorFloat32AsStringSig`；`SigExtra`（D08.1 §3.5 (a)） |
| 2b-2b | `castAsTime/Duration/JSON/Array/VectorFloat32FunctionClass` 与它们的签名、从 TIME/DURATION/JSON/VECTOR 出发的签名；`WrapWithCastAsTime/Duration/JSON/VectorFloat32`；`ConvertJSON2Tp` 等 |

## 2. 依赖

D08.1（`Expr`、`BuiltinFunc`、`#[eval_fn]`、`RpnProgram`）、D08.2 第 1 部分（`newBaseBuiltinFunc`、`deriveCollation`）、D01.1/D01.4（`convert`、`ProduceDecWithSpecifiedTp`、`ProduceFloatWithSpecifiedTp`）、D01.2（MyDecimal）、D00.1（`EvalContext` 的 type/err context）。

## 3. 设计

### 3.1 `baseBuiltinCastFunc`

Go 的 `baseBuiltinCastFunc` = `baseBuiltinFunc` + `inUnion`。Rust 的 `BuiltinFunc` 加字段 `in_union: Option<bool>`：嵌入 `baseBuiltinCastFunc` 的签名为 `Some`，其余签名为 `None`。`metadata()` 据此返回 `InUnionMetadata`；`newBaseBuiltinCastFunc` 设为 `Some(in_union)`；`bf.baseBuiltinFunc`（`VectorFloat32AsUnsupported`）设回 `None`。

### 3.2 签名体与两个驱动

每个签名一份函数体（Go 的行版本），`#[eval_fn]` 生成行驱动与向量驱动（N4 §3.2）。本部分给宏加了 `Decimal` 结果类型和 `&MyDecimal` 参数类型，`Args` 加了 `decimal`，常量列支持 DECIMAL。

这些签名都有逐行的副作用（`AppendWarning`、`HandleError` 产生 warning），按 D08.1 spec §3.4 不用 `checked` 写法，用默认的单遍驱动：遇到第一个出错的行即返回，这正是 Go 向量版停下的行。

**行版本与向量版本的差异**（D08.1 spec §3.4 (c)）：govec 同时比较两种模式，找到 Go 中 4 处向量版与行版对用户可见的不同（`StringAsDecimal` 和 `DecimalAsInt` 两处登记为 upstream-pitfall #83：错误码随 `tidb_enable_vectorized_expression` 在 1265/1292 之间变化，warning 文字不同）；向量驱动按 Go 的向量版本，函数体通过 `SigView::vector()` 区分：

| 签名 | 行版本 | 向量版本 |
|---|---|---|
| `DecimalAsInt` 溢出的 warning | 参数是 `*MyDecimal`，打印 `String()`：`'9223…'` | 参数是 `types.MyDecimal` 值（`String` 是指针接收者），fmt 按字段打印：`'{%!s(int8=000…031) … [%!s(int32=…) …]}'`。为此 `gofmt::Arg` 加了 `Struct`/`Array`（`printValue` 的结构体、数组分支） |
| `StringAsDecimal` | `len(val) > 1` 才算负数；`FromString` 的 `ErrTruncated` 转成 `ErrTruncatedWrongVal` 再交给 `ec.HandleError`（按错误级别） | `"-"` 也算负数；错误原样交给 `tc.HandleTruncate`（按类型标志）；UNSIGNED UNION 中的负数不写该行（保留 `ResizeDecimal` 的内容，新列为零） |
| `IntAsDecimal` UNSIGNED UNION 中的负数 | `&types.MyDecimal{}` | `dec.FromUint(0)`（内部状态不同） |
| `DecimalAsReal` 溢出 | 错误直接返回 | 交给 `ec.HandleError`（`ToFloat64` 实际不会溢出，按 Go 移植） |

### 3.3 常量折叠

`FoldConstant`/`foldConstant` 与 IF/IFNULL/CASE/ISNULL 的特殊处理按 Go 移植。Go 原地修改节点（CASE 的参数、`FoldConstant` 结果的 charset/collation、`SetRepertoire`）；Rust 的节点是不可变的 `Arc<Expr>`（M-EXPR），改为在副本上修改（`Arc::make_mut`），持有原节点的其他调用方看不到修改（§4）。

### 3.4 CAST 到 STRING 与 `builtin_convert_charset.go`（第 2b-1 部分）

- **块的归属**：`builtin_convert_charset.go` 原属 D08.6（字符串函数，P5）。`castAsStringFunctionClass` 对二进制字符串参数调用 `HandleBinaryLiteral`（包一层 `from_binary`），`newBaseBuiltinFuncWithTp` 对每个字符串参数也调用它，所以这个文件随 CAST 进入 D08.8（tracker `rules.py`）。
- **`newBaseBuiltinFuncWithTp`**（D08.2）：`to_binary`/`from_binary` 的函数类用它；参数按 `argTps` 包 CAST（INT/REAL/DECIMAL/STRING 已移植），之后才能接着做 `NewFunction*`（第 2b-3 部分）。
- **`from_binary` 的状态**：`cannotConvertStringAsWarning` 是 `BuiltinFunc::extra` 的 `SigExtra::FromBinary`（`inUnion` 是 `SigExtra::Cast`；D08.1 spec §3.5 (a)，Lucky 评审 #86），Go 的 `Clone` 复制它，Rust 的 `Clone` 也复制。
- **签名**：都有逐行的副作用（`ProduceStrWithSpecifiedTp` 的截断 warning、`handleAllowedPacketOverflowed`、`from_binary` 的 `AppendWarning`），按 D08.1 spec §3.4 用默认的单遍驱动。
- **行版本与向量版本的差异**（D08.1 spec §3.4 (c)，govec 两种模式都比较）：`builtinInternalFromBinarySig` 在 `cannotConvertStringAsWarning` 且非严格模式下转换失败时，行版本返回 `Transform` 已产生的部分，向量版本写入原始字节；函数体用 `f.vector()` 区分（"str" 和 "binlit" 记录都有走到这里的行，反证见 PR）。这是 Go 的行为差异，用户能看到（`tidb_enable_vectorized_expression` 切换时 `CAST(binary AS CHAR CHARSET ..)` 的结果不同）。 用户可见：打开向量化时 `CAST(b AS CHAR CHARSET gbk)` 返回非法的原始字节，外层的 `LENGTH`/`HEX` 报 ERROR 1300；已并入 upstream-pitfall #83（Lucky 的复现见该 issue）。

### 3.5 CAST 到 DATETIME/DATE/TIMESTAMP 与 TIME（第 2b-2b-i 部分）

- **函数类**：`castAsTimeFunctionClass`、`castAsDurationFunctionClass` 按参数的求值类型选 14 个签名（`{Int,Real,Decimal,String,Time,Duration,JSON}As{Time,Duration}`）；VECTOR 参数选 `builtinCastVectorFloat32AsUnsupportedSig`。
- **`builtinCastVectorFloat32AsUnsupportedSig`**：Go 的每个 `evalXxx` 都只返回 `cannot cast from vector to <TypeStr(tp)>`，不求值参数（NULL 行也报错），没有向量版。Rust 用 `SigDrivers::Unsupported { err }`：任何结果类型的行驱动都返回这个错误，向量驱动返回 `baseBuiltinFunc.vecEvalXxx` 的错误；RPN 对没有单一结果类型的签名按函数的结果类型选 `VecEvalXxx`（Go 的 `ScalarFunction.VecEvalXxx` 就是这样）。
- **`#[eval_fn]` 的 Time/Duration 结果**：`TimeOut`/`DurationOut`（`ResizeTime`/`ResizeGoDuration` + `MergeNulls`，与 Go 向量版相同）；DURATION 列只存纳秒，读回时用列的声明 fsp。
- **读时钟**：`DurationAsTime` 与 JSON 中 TIME 值转 DATETIME 用 `getStmtTimestamp(ctx)`（`ctx.CurrentTime()`；Go 失败时退回 `gotime.Now()`）。govec 的 castCtx 第 5 个元素是固定的语句时间（Unix 纳秒，UTC，`WithCurrentTime`），回放用同一个值；向量里没有读真实时钟的记录。
- **逐行版与向量版的差异**（D08.1 spec §3.4 (c)，每个驱动跟随各自的 Go 版本；govec 两种模式都比较，反证见 PR）：

| 签名 | 逐行版 | 向量版 | 用户可见 |
|---|---|---|---|
| `StringAsTime` | `ParseTime` 返回的 `*strconv.NumError`（如 `"2024-1-99999999999999999999"`，Atoi 越界）不是时间错误，`handleInvalidTimeError` 原样返回，语句报错 `strconv.Atoi: parsing ...: value out of range` | 先换成 `ErrIncorrectDatetimeValue`，非严格模式下为 warning + NULL | 是，**已在 v8.5.8 上验证**（Lucky 评审 #91，#83）：非严格模式下 `CAST(s AS DATETIME)` 打开向量化时是 NULL + warning 8034，关闭时整条语句报 ERROR 1105 `strconv.Atoi: ...` |
| `JSONAsDuration` 默认分支 | warning 写 `Truncated incorrect TIME value` | `types.TypeStr(tp)`，即 `time` | 是，**已在 v8.5.8 上验证**（Lucky 评审 #91，#83）：`CAST(j AS TIME)`（j 为数组或对象）的 warning 打开时是 `time`、关闭时是 `TIME` |
| `DurationAsTime` | 参数值自带的 fsp | 结果的声明 fsp | 参数是函数、值的 fsp 与声明不同时（govec "to-fsp"；同类差异 `CAST(TIME(c) AS CHAR)` 已在 v8.5.8 上验证，#83） |
| `DurationAsDuration` | 值的 fsp 等于目标 fsp 时 `RoundFrac` 原样返回 | 从 `UnspecifiedFsp` 取整 | 同上 |
| `DecimalAsDuration`、`StringAsDuration` | `ErrTruncatedWrongVal` 交给 `ec.HandleError` | `tc.HandleTruncate` | 只在错误级别与 truncate 标志不一致时；SQL 中两者都来自 SQL mode，按代码推断看不到（未实测） |

  `TimeAs*` 没有这类 fsp 差异：TIME/DATETIME 值在 chunk 中自带 fsp（`Time` 的打包值），两个驱动读到的是同一个 fsp（Lucky 评审 #91）。

  外层 CAST 读内层 `CAST(json AS TIME(n))` 的值时，内层返回 JSON 值自带的 fsp（`val.GetDuration()`），与声明的 n 不同：这就是 Lucky 在 #88 指出的"值的 fsp ≠ 声明的 fsp"，govec "to-fsp" 300 条覆盖，其中 92 条两种模式的结果不同。

### 3.6 CAST 到 JSON、JSON ARRAY 与 VECTOR（第 2b-2b-ii 部分）

- **函数类**：`castAsJSONFunctionClass`（7 个签名 `{Int,Real,Decimal,String,Time,Duration,JSON}AsJSON`；STRING 参数给结果类型加 `ParseToJSONFlag`；VECTOR 参数选 `builtinCastVectorFloat32AsUnsupportedSig`，其 `evalJSON` 现在可达）、`castAsArrayFunctionClass`（`verifyArgs` 要求 JSON 参数；YEAR/JSON/FLOAT/DECIMAL 元素、非 utf8mb4/binary 字符集、未指定长度的字符串返回 `ErrNotSupportedYet`；签名 `castJSONAsArrayFunctionSig`）、`castAsVectorFloat32FunctionClass`（`StringAsVectorFloat32`、`VectorFloat32AsVectorFloat32`、其余参数 `UnsupportedAsVectorFloat32`）。`BuildCastFunctionWithCheck` 对 `tp.IsArray()` 选数组的函数类；CAST 到 JSON 不折叠（与 Go 相同）。
- **`#[eval_fn]` 的 JSON/VectorFloat32 结果**：与 STRING 一样逐行追加（`ReserveJSON`/`ReserveVectorFloat32` + `AppendJSON`/`AppendNull`，Go 向量版的写法）。7 个 `*AsJSON` 有向量版；`castJSONAsArray` 与 3 个 VECTOR 结果的签名在 Go 中没有向量版（`novec`），向量求值返回 base 的错误。`UnsupportedAsVectorFloat32` 不求值参数（`lazy`），任何行都报 `cannot cast from <TypeStr(arg)> to vector`。
- **`fakeSctx`**：`cast as array` 与 `ConvertJSON2Tp` 忽略 SQL mode、返回所有错误；Rust 用 `types.StrictContext`（`StrictFlags`、UTC、丢弃 warning），错误上下文直接返回错误。
- **`ConvertJSON2Tp`**：Go 返回 `any`（string/int64/uint64/float64/Time/Duration）；Rust 返回 `JSONValue`（这些值正是 `CreateBinaryJSON` 的输入）。
- **逐行版与向量版**：govec 两种模式比较，这次没有发现新的差异（`*AsJSON` 的向量版与逐行版逐行相同）。

## 4. 偏离与豁免

1. **`BuildCastFunction*`/`WrapWithCastAs*` 返回 `Result`**：Go 记日志并返回 `Function == nil` 的节点（之后使用会空指针 panic）；Rust 返回错误。
2. ~~CAST 到 JSON/VECTOR 的临时桩~~：第 2b-2b-ii 部分已移植（§3.6），`cast_not_ported` 与 `newBaseBuiltinFuncWithTp` 的桩分支已删除。
3. ~~TIME/DURATION/JSON/VECTOR 参数的 INT/REAL/DECIMAL/STRING 签名只有描述~~：第 2b-2a 部分已移植。`builtinCastVectorFloat32AsUnsupportedSig` 在 Go 中定义了 `evalInt`…`evalJSON`，没有 `evalVectorFloat32`：`SigDrivers::Unsupported` 的 VectorFloat32 行驱动因此返回 base 的错误（不可达：CAST 到 VECTOR 选别的签名；Lucky 评审 #91）。
4. **`TryPushCastIntoControlFunctionForHybridType`**：IF/CASE/ELT 未移植，tidb-rs 建不出这样的节点；遇到时 `unimplemented!`（临时桩，D08.9）。
5. **`CastStringAs{Int,Real,Decimal}` 的前置分支**（hybrid、二进制字面量、`CanImplicitEval*`）：函数类不会对这样的参数选中这些签名（选 `IntAsInt` 等），不可达，未移植（L2）。
6. **`PropagateType` 与类型的原地修改**：Go 直接修改参数节点（及共享它的所有持有者）的 `*FieldType`；Rust 在副本上修改（`with_type_mut`）。`ScalarFunction` 的 `RetType` 与签名的 `tp` 在 Go 中是同一个指针，两者一起改。**移植注意**（Lucky 评审 #82）：调用点 builtin_cast.go:220（`BuildCastFunction` 中对参数调用 `PropagateType`）在 Go 中原地修改的是**共享的** `ScalarFunction`/`Constant` 节点，计划中所有引用同一节点的地方都会看到新类型。按 Lucky 的分析，这一点从 SQL 观察不到；但规划器（P3）的 Go 代码可能依赖这种共享，移植规划器时遇到同一节点被多处引用、又在 CAST 之后读取其类型的地方，要确认是否需要显式地同步修改。
7. **`foldConstant` 的 null-reject 分支**需要 `NewFunctionBase`（D08.1 2b），之前保留原表达式（与 Go 中 `NewFunctionBase` 失败时相同，临时桩）；`ifNullFoldHandler` 修改被丢弃的 IFNULL 节点的 charset/collation，Rust 不做（L2）。
8. **向量版保留 `ResizeDecimal` 内容的情况**（`StringAsDecimal` 的 UNSIGNED UNION 负数）：Rust 写零（新列的内容）；复用的旧列在 Go 中会留下旧值。
9. ~~FLOAT 与 hybrid（BIT/ENUM/SET）参数在向量驱动中~~：第 2a 部分已修正（RPN 按签名读取的求值类型转换）。FLOAT 加宽成 DOUBLE 的逐位比较见 "dbl" 记录（Lucky 评审 #84）。
10. **`adjustRetFtForCastString` 中 Go 的 `intest.Assert` 分支**（ENUM/SET 带 `EnumSetAsIntFlag`、未知类型）：按 Go 的生产构建直接返回（D08.1 spec §4 第 6 条）。
11. **`newBaseBuiltinFuncWithTp` 不检查 nil 上下文**（引用不会为空）；参数个数与类型个数不同时与 Go 一样 panic。
12. **向量驱动读 DURATION 参数的 fsp**（2b-2a）：Go 的向量版用参数的声明类型 `b.args[0].GetType(ctx).GetDecimal()` 读 `GoDurations`，逐行版用 `EvalDuration` 返回值自带的 fsp；`DurationView` 按 Go 的向量版取声明的 fsp，行驱动用返回值的 fsp（各自跟随，§3.4 (c)；列参数两者相同，govec 未见差异）。`builtinCastDurationAs{Real,Decimal}Sig` 的向量版在循环**之前**对声明的 fsp 做一次 `CheckFsp`，所以全 NULL 或 0 行时也会报错；Rust 在第一个非 NULL 行检查（只在声明的 fsp 非法时有差别，SQL 构造不出这样的类型）。
13. **签名没有自己的向量版时的向量求值**：Go 中 `ScalarFunction.VecEvalXxx` 调用签名的 `vecEvalXxx`，不看 `vectorized()`；签名没有定义 `vecEvalXxx` 时到达 `baseBuiltinFunc.vecEvalXxx` 并报错（执行器按 `Vectorizable` 改用逐行求值，属于 D08.1 2b）。2b-2a 曾用 `SigDesc.vectorized == false` 判断；2b-2b-i 按 Lucky 在 #88 的意见改为由驱动决定：没有向量版的签名写 `#[eval_fn(.., novec)]`，向量驱动返回 base 的错误，`vec_eval` 不再看 `vectorized`（D08.1 spec §3.5）。`VectorFloat32AsString` 现在是 `novec`，govec 的记录不变。
14. **TIME/DURATION/JSON/VECTOR 类型的常量**：govec 的 `sfDump`/`d2DatumInOf` 已能输出这四种 Datum；"to-const" 400 条覆盖 CAST 到 DATETIME/DATE/TIMESTAMP/TIME 的常量参数（构建时折叠，比较折叠后的常量）。向量驱动的 `fill_constant` 只在未折叠时用到，CAST 的常量参数总会被折叠，所以这条路径等以后的函数族覆盖。
15. **`WrapWithCastAsTime` 的 `tp`**：Go 修改调用方传入的 `*FieldType`（设置 decimal、flen 和 binary charset）；Rust 按值接收（L2）。`newBaseBuiltinFuncWithTp` 每次新建这个类型，观察不到差别。
16. **`getStmtTimestamp`**（helper.go，D08.12）：这里是边界桩，只做 `ctx.CurrentTime()` 与失败时的 `gotime.Now()`；Go 中 `intest` 构建下的随机时区与 `injectNow` failpoint 只在测试中出现。

17. **govec 测不出的两处**（第 2b-2b-ii 部分的反证）：`TimeAsJSON` 把 DATETIME/TIMESTAMP 的 fsp 设成 6——JSON 的二进制只存打包的时间值（不含 fsp），之后读出时用读取方给的 fsp，所以这一步从值上观察不到；`convertJSON2Tp` 对 DATE 目标截掉时分秒——只接受 `JSONTypeCodeDate` 的值，而 DATE 值本来就没有时分秒，结果相同。两处都照 Go 移植。
18. **`ConvertJSON2Tp` 返回 `JSONValue`**（L2，见 §3.6）；`fakeSctx` 是 `types.StrictContext`（L2）。

没有面向用户的行为差异（`docs/compatibility.md` 不变）。
