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

## 第 2b-1 部分（本 PR）

1. `builtin_cast.rs`：`castAsStringFunctionClass`、4 个 CAST 到 STRING 的签名（另 4 个为临时桩）、`adjustRetFtForCastString`、`decimalPrecisionToLength`、`padZeroForBinaryType`、`WrapWithCastAsString`、`BuildCastCollationFunction`；`BuildCastFunctionWithCheck` 的 STRING 分支（BIT 参数的 flen、`isExplicitCharset`）。
2. `builtin_convert_charset.rs`（新文件，块 D08.6 → D08.8）：`to_binary`/`from_binary` 的函数类与签名、`BuildTo/FromBinaryFunction`、`HandleBinaryLiteral`、`convertActionMap`。`BuiltinFunc::cannot_convert_string_as_warning`。
3. `builtin.rs`（D08.2）：`newBaseBuiltinFuncWithTp`（TIME/DURATION/JSON/VECTOR 参数为临时桩）、`newBaseBuiltinCastFunc4String`。`Expr::charset_and_collation`。
4. `#[eval_fn]`：`args(Int, String, ..)` 选项（lazy 函数体声明向量驱动读取参数的类型），`SigDrivers::arg_eval_type` 对变参签名重复最后一个类型（Lucky 评审 #84；D08.1 spec §4.1）。
5. govec `cast` 领域追加 2,820 条（原有 4,560 条不变）：`str` 1,600（INT/REAL/DECIMAL/YEAR/字符串/FLOAT/hybrid 参数 → CHAR/BINARY/VARCHAR，7 种字符集，随机的显式字符集、`max_allowed_packet`、严格模式）、`dbl` 120（FLOAT → DOUBLE，0.1f、16777217f、次正规数、float32 最大值，逐位比较）、`binlit` 500（`HandleBinaryLiteral` 与 to_binary/from_binary 的求值）、`collcast` 300、`wrapstr` 300。castCtx 可选地带 `max_allowed_packet` 与 SQL mode。

## 第 2b-2、2b-3 部分（交接）

- 2b-2：`#[eval_fn]` 支持 Time/Duration/JSON/VectorFloat32 的参数与结果（`ArgKind`、视图、输出列）；CAST 到 TIME/DATETIME/DATE/DURATION/JSON（含 ARRAY）/VECTOR 与从这些类型出发的签名（去掉临时桩，包括 `*AsString`）；`WrapWithCastAsTime/Duration/JSON/VectorFloat32`；`VectorFloat32AsUnsupported` 需要一个签名有多个求值类型的驱动。之后 `newBaseBuiltinFuncWithTp` 的临时桩分支去掉。
- 2b-3：`newBaseBuiltinFuncWithFieldTypes`；注册表的 `get_function`、`NewFunction*`（`derive` 记录经包 CAST 的构造函数再跑一遍，Lucky 评审 #80）；常量传播；D08.1 2b 的 `EvaluatorSuite`、`chunk_executor`、`VecEvalBool`、`toBool`、`EvalExpr`。
