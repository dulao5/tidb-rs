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
