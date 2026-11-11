# D08.8：plan

## 第 1 部分（本 PR）

1. `crates/expression/src/builtin_cast.rs`：3 个函数类、12 个签名、临时桩签名、`BuildCastFunction*`、`WrapWithCastAs{Int,Real,Decimal}`、`PropagateType` 等。
2. `crates/expression/src/constant_fold.rs`：`constant_fold.go`（D08.2）与 `MaybeOverOptimized4PlanCache`/`containMutableConst`（util.go，D08.12）。
3. 框架：`#[eval_fn]` 的 `Decimal` 结果与 `&MyDecimal` 参数，`Args::decimal`，`DecimalView`/`DecimalOut`，常量列的 DECIMAL；`SigView::vector()`；`BuiltinFunc::in_union`；`gofmt::Arg::{Struct, Array}`。
4. govec `cast` 领域（tracker `govec/cast-D08.8-1`）：`cast` 3,060 条（含 60 条 UNSIGNED UNION 负数字符串）、`wrap` 700 条。每条 `cast` 记录：构建结果（`sfDump`：签名名、`tipb` 码、结果类型、collation、参数——含 `PropagateType` 之后的类型；常量参数则是折叠后的常量）、构建时的 warning、`inUnion` 元数据，以及逐行模式每行的值/错误/warning 和向量模式的值或错误与 warning。随机的类型标志（`FlagIgnoreTruncateErr`、`FlagTruncateAsWarning`、`FlagAllowNegativeToUnsigned`）和 7 个错误组的级别。

回放：`cargo test -p tidb-expression --test govec_cast`；额外的种子：`GOVEC_CAST_FILES=a.jsonl.gz,...`。

## 第 2 部分（交接）

1. ~~RPN 的列引用按签名读取的求值类型转换~~ **已完成（第 2a 部分）**：`SigDrivers` 记录向量驱动读取各参数的求值类型（`#[eval_fn]` 生成）；RPN 按它选择照搬、转换（`Column.VecEvalInt/Real/String` 的转换分支，`RpnNode::ColumnConv`）或在编译时 panic（存储与读取类型不符、Go 也不转换的组合；Lucky 评审 #82）。govec `cast` 追加 800 条 `conv` 记录（FLOAT、BIT(n)、ENUM、SET 列）。

**第 2b 部分的建议顺序**（第 2a 部分之后）：
- 2b-1：CAST 到 STRING，参数为 INT/REAL/DECIMAL/STRING 与 hybrid：`newBaseBuiltinCastFunc4String`、`adjustRetFtForCastString`、`padZeroForBinaryType`，以及 `HandleBinaryLiteral`（需要 `builtin_convert_charset.go` 的内部函数 `to_binary`/`from_binary`，先确认它们所属的块）。BIT 转非 binary 字符集时先包一层到 BINARY 的 CAST（`castAsStringFunctionClass.getFunction` 316–330 行）。`#[eval_fn]` 已支持 String 结果，不需要改宏。
- 2b-2：`#[eval_fn]` 支持 Time/Duration/JSON/VectorFloat32 的参数和结果（`ArgKind`、视图、输出列），然后 CAST 到 TIME/DATETIME/DATE/DURATION/JSON/VECTOR 和从这些类型出发的签名；`VectorFloat32AsUnsupported` 需要一个签名有多个求值类型的驱动。
- 2b-3：第 4 条（D08.2 第 2 部分其余、D08.1 2b）。
2. CAST 到 STRING（`newBaseBuiltinCastFunc4String`、`adjustRetFtForCastString`、`HandleBinaryLiteral`/from_binary、`padZeroForBinaryType`）、TIME、DURATION、JSON（含 ARRAY）、VECTOR；其余 `WrapWithCastAs*`、`BuildCastCollationFunction`。
3. 从 TIME/DURATION/JSON/VECTOR 出发的签名（去掉临时桩）；`VectorFloat32AsUnsupported` 需要一个签名有多个求值类型的驱动。
4. 然后 D08.2 第 2 部分其余：`newBaseBuiltinFuncWithTp`/`WithFieldTypes`（用 `derive` 记录经它们回放，Lucky 评审 #80）、注册表的 `get_function`、`NewFunction*`（D08.1 2b），以及 D08.1 2b 的 `EvaluatorSuite`、`chunk_executor`、`VecEvalBool`。

## 第 2b-1 部分（#86，已合并）

1. `builtin_cast.rs`：`castAsStringFunctionClass`、4 个 CAST 到 STRING 的签名（另 4 个为临时桩）、`adjustRetFtForCastString`、`decimalPrecisionToLength`、`padZeroForBinaryType`、`WrapWithCastAsString`、`BuildCastCollationFunction`；`BuildCastFunctionWithCheck` 的 STRING 分支（BIT 参数的 flen、`isExplicitCharset`）。
2. `builtin_convert_charset.rs`（新文件，块 D08.6 → D08.8）：`to_binary`/`from_binary` 的函数类与签名、`BuildTo/FromBinaryFunction`、`HandleBinaryLiteral`、`convertActionMap`。`BuiltinFunc::cannot_convert_string_as_warning`。
3. `builtin.rs`（D08.2）：`newBaseBuiltinFuncWithTp`（TIME/DURATION/JSON/VECTOR 参数为临时桩）、`newBaseBuiltinCastFunc4String`。`Expr::charset_and_collation`。
4. `#[eval_fn]`：`args(Int, String, ..)` 选项（lazy 函数体声明向量驱动读取参数的类型），`SigDrivers::arg_eval_type` 对变参签名重复最后一个类型（Lucky 评审 #84；D08.1 spec §4.1）。
5. govec `cast` 领域追加 2,820 条（原有 4,560 条不变）：`str` 1,600（INT/REAL/DECIMAL/YEAR/字符串/FLOAT/hybrid 参数 → CHAR/BINARY/VARCHAR，7 种字符集，随机的显式字符集、`max_allowed_packet`、严格模式）、`dbl` 120（FLOAT → DOUBLE，0.1f、16777217f、次正规数、float32 最大值，逐位比较）、`binlit` 500（`HandleBinaryLiteral` 与 to_binary/from_binary 的求值）、`collcast` 300、`wrapstr` 300。castCtx 可选地带 `max_allowed_packet` 与 SQL mode。

## 第 2b-2a 部分（本 PR）

1. `#[eval_fn]`：严格参数类型增加 `Time`、`Duration`、`&BinaryJSON`、`&VectorFloat32`（行驱动调 `eval_time/duration/json/vector_float32`；向量驱动用 `TimeView`、`DurationView`（按参数声明的 fsp，Go 向量版的读法）、`JsonView`、`VectorView`）；`SigDrivers::args` 对应 `ETDatetime/ETDuration/ETJson/ETVectorFloat32`。结果类型仍为 Int/Real/Decimal/String（Time/Duration/JSON/Vector 结果在 2b-2b）。
2. RPN：`fill_constant` 支持 Time/Duration/JSON/Vector（Go `genVecFromConstExpr` 的对应分支）；`column_read` 把 `ETTimestamp` 与 `ETDatetime` 视为同一存储。`vec_eval` 对 `vectorized() == false` 的签名返回 `baseBuiltinFunc.vecEvalXxx` 的错误（spec §4 第 13 条）。
3. 13 个签名：`builtinCast{Time,Duration,JSON}As{Int,Real,Decimal,String}Sig`、`builtinCastVectorFloat32AsStringSig`。逐行版与向量版的小差别用 `vector()`：`TimeAsReal` 只有向量版把 `ErrOverflow` 交给 `HandleError` 并置 NULL（`ToFloat64` 在时间数上走不到）。没有发现新的对用户可见的差异。
4. `SigExtra`（Lucky 评审 #86）：`BuiltinFunc::extra` 取代 `in_union`、`cannot_convert_string_as_warning` 两个字段；D08.1 spec §3.5 对 98 个 Go 签名结构体的附加字段分类。
5. govec `cast` 追加 2,500 条（原有 7,380 条不变）：`from` 2,400（DATE/DATETIME/TIMESTAMP(fsp)、TIME(fsp)、JSON、VECTOR 列 → INT/REAL/DECIMAL/STRING/YEAR，随机 type flags、SQL mode、`max_allowed_packet`；YEAR 目标总是带 `FlagCastTimeToYearThroughConcat`，避开读时钟的路径）、`from-fsp` 100（fsp 未指定的 TIME 列，覆盖 `CheckFsp`）。

## 第 2b-2b-i 部分（本 PR）

1. `#[eval_fn]`：结果类型增加 `Time`、`Duration`（`TimeOut`/`DurationOut`）；`novec` 选项（没有自己向量版的签名，D08.1 spec §3.5，Lucky 评审 #88）。`SigDrivers::Unsupported { err }`（`VectorFloat32AsUnsupported`：任何结果类型的行版本只返回错误）。`vec_eval` 不再看 `SigDesc.vectorized`；`VectorFloat32AsString` 改为 `novec`。
2. `castAsTimeFunctionClass`、`castAsDurationFunctionClass` 与 14 个签名（spec §3.5，逐行版与向量版的 6 处差异见那里的表）；`WrapWithCastAsTime/Duration`；`newBaseBuiltinFuncWithTp` 对 DATETIME/TIMESTAMP/DURATION 参数包 CAST；`BuildCastFunctionWithCheck` 的 DATETIME/TIMESTAMP/DURATION 分支。
3. govec `cast` 追加 4,000 条（原有 9,880 条不变）：`to` 3,000（INT/YEAR、REAL、DECIMAL、时间形态的字符串、DATE/DATETIME/TIMESTAMP、TIME、各种 type code 的 JSON（含 `CreateBinaryJSON` 的 DATE/DATETIME/TIMESTAMP/TIME 值）、VECTOR 列 → DATETIME/DATE/TIMESTAMP/TIME(fsp)，VECTOR 也到 INT/REAL/DECIMAL；随机 SQL mode（含 NO_ZERO_DATE）、四个固定的语句时间）、`to-const` 400（常量参数，构建时折叠）、`to-fsp` 300（`CAST(CAST(json AS TIME(n)) AS ..)`：内层值的 fsp 与声明不同，Lucky 评审 #88）、`wrap-time` 300（`WrapWithCastAsTime/Duration`）。castCtx 可选的第 5 个元素是语句时间；`sfDump` 输出 Time/Duration/JSON/Vector 常量。
4. tracker `cast2b2.go` 开头的注释：设 concat 标志的是 DURATION（TIME 类型）列（Lucky 评审 #88）。

## 第 2b-2b-ii 部分（完成）

CAST 到 JSON、JSON ARRAY、VECTOR（spec §3.6），`WrapWithCastAsJSON/VectorFloat32`、`ConvertJSON2Tp`；`cast_not_ported` 与 `newBaseBuiltinFuncWithTp` 的桩分支已删除；Lucky 在 #91 的 3 条已处理（spec §3.5 的表、§4 第 3 条、`TimeAs*` 的说明）。converge：D08.8 265/266（只剩 `inCastContext.String`，规划器的会话键，D09）。

**留给以后的覆盖**（Lucky 评审 #91）：值的 fsp 与声明的 fsp 不同的最常见来源是函数结果，例如 `TIME(varchar)`、`ADDTIME`、`SEC_TO_TIME`；移植这些函数（D08.7）时，在 govec 中加上以它们为参数的 CAST 记录（`DurationAsTime`、`DurationAsDuration`、`DurationAsString` 等两种模式各自跟随 Go）。

## 第 2b-2b-ii、2b-3 部分（交接）

- **2b-2b-ii**：`#[eval_fn]` 支持 JSON/VectorFloat32 **结果**（`ReserveJSON`/`ReserveVectorFloat32`，NULL 合并）；`castAsJSON/Array/VectorFloat32FunctionClass` 与 CAST 到 JSON/VECTOR 的签名（`builtinCast*AsJSONSig`、`castJSONAsArray`、`StringAsVectorFloat32`、`VectorFloat32AsVectorFloat32`、`UnsupportedAsVectorFloat32`）；`WrapWithCastAsJSON/VectorFloat32`、`ConvertJSON2Tp`、`convertJSON2Tp`；之后去掉 `newBaseBuiltinFuncWithTp` 的临时桩分支和 `cast_not_ported`；`VectorFloat32AsUnsupported.evalJSON` 随之覆盖。
- **原 2b-2b 描述（已部分完成）**：`#[eval_fn]` 支持 Time/Duration/JSON/VectorFloat32 **结果**（输出列：`ResizeTime`/`ResizeGoDuration`/`ReserveJSON`/`ReserveVectorFloat32`，NULL 合并）；`castAsTime/Duration/JSON/Array/VectorFloat32FunctionClass` 与 CAST 到这些类型的签名（`builtinCast*AsTime/Duration/JSON/VectorFloat32Sig`、`castJSONAsArray`、`VectorFloat32AsUnsupported`——需要一个签名有多个求值类型的驱动）；`WrapWithCastAsTime/Duration/JSON/VectorFloat32`、`ConvertJSON2Tp`；之后去掉 `newBaseBuiltinFuncWithTp` 的临时桩分支和 `cast_not_ported`。`DurationAsTime` 读当前时间（`getStmtTimestamp` 失败时 `gotime.Now()`）：govec 必须固定语句时间戳，或按 `<clock>` 规则屏蔽。govec 的 `sfDump`/`d2DatumInOf` 补上 Time/Duration/JSON/Vector 种类，然后加常量参数的记录（spec §4 第 14 条）。
- **2b-3**：同下（原交接）。

## 第 2b-3 部分（原交接，含 2b-2 的旧描述）

- 2b-2：`#[eval_fn]` 支持 Time/Duration/JSON/VectorFloat32 的参数与结果（`ArgKind`、视图、输出列）；CAST 到 TIME/DATETIME/DATE/DURATION/JSON（含 ARRAY）/VECTOR 与从这些类型出发的签名（去掉临时桩，包括 `*AsString`）；`WrapWithCastAsTime/Duration/JSON/VectorFloat32`；`VectorFloat32AsUnsupported` 需要一个签名有多个求值类型的驱动。之后 `newBaseBuiltinFuncWithTp` 的临时桩分支去掉。
- 2b-3：`newBaseBuiltinFuncWithFieldTypes`；注册表的 `get_function`、`NewFunction*`（`derive` 记录经包 CAST 的构造函数再跑一遍，Lucky 评审 #80）；常量传播；D08.1 2b 的 `EvaluatorSuite`、`chunk_executor`、`VecEvalBool`、`toBool`、`EvalExpr`。

## 第 2b-3a 部分（本 PR）

1. **结果类型只有一份**（Lucky 评审 #94）：`ScalarFunction` 去掉 `ret_type` 字段，`ret_type()` 返回签名的 `tp`，`ret_type_mut()` 原地修改；`with_type_mut` 改用它。D08.1 spec §3.2 写明规则。
2. `DisableParseJSONFlag4Expr`（D08.12，`util.rs`）与单元测试 `disable_parse_json_flag_reaches_the_signature`。它的调用方（JSON 函数、比较）还没有移植，所以没有 govec 记录。
3. `newBaseBuiltinFuncWithFieldTypes`（D08.2）。
4. `NewFunction`、`NewFunctionBase`、`NewFunctionTryFold`、`NewFunctionWithInit`、`NewFunctionInternal`、`newFunctionImpl`、`typeInferForNull`、`defaultScalarFunctionCheck`（D08.1）；`builtin_registry::FunctionClass` 与 `function_class`（目前没有已移植的 `funcs` 函数类）。
5. govec `cast` 追加 2,300 条（原有 17,380 条不变）：`withtps` 1,300（`newBaseBuiltinFuncWithTp`/`WithFieldTypes`，叶子参数覆盖各种类型与字符集，collation 推导经过包 CAST 的构造函数，Lucky 评审 #80）、`newfunc` 700（五个入口 × cast/to_binary/from_binary/未知函数名，有无当前库）、`nullinfer` 300。

## 第 2b-3b、2b-3c 部分（交接）

- **#95 评审的后续**：(1) 加一个检查，保证已移植的函数类都登记到了 `builtin_registry::function_class`；(2) 移植 `DisableParseJSONFlag4Expr` 的第一个调用方（JSON 函数、JSON 比较、`MEMBER OF`）时补 govec 记录，包括 v8.5.8 上 `CAST('[1]' AS JSON) = '[1]'` 返回 0（D08.12 交接清单）；(3) 可选：`get_function` 之后 `debug_assert!` 签名类型不是 `TypeUnspecified`。

- **2b-3b**：常量传播（`constant_propagation.go`，D08.2）与 `builtin_func_param.go`、`grouping_sets.go`、`simple_rewriter.go`（依赖允许的部分，否则登记豁免）。常量传播要构建比较函数（`NewFunction(ast.EQ, ..)`），而比较函数族（D08.4）还没有移植：先确认最小依赖，必要时与 D08.4 的第一批签名一起做。
- **2b-3c**（完成）：D08.1 2b 的求值入口（EvaluatorSuite、chunk_executor、VecEvalBool/toBool/EvalExpr），govec `ev*` op；#95 后续 (1)(3) 已做（`function_class_registry` 测试、`TypeUnspecified` 的 `debug_assert`）。
- **第一个 `funcs` 函数族移植时**：在 `builtin_registry::function_class` 中登记；给 govec `newfunc` 加上这些函数名的记录，覆盖 `fold` 0/1/-1 与 `TryFold` 截断 warning（目前测不到，D08.1 spec §4 第 14 条）。
