# D08.8：plan

## 第 1 部分（本 PR）

1. `crates/expression/src/builtin_cast.rs`：3 个函数类、12 个签名、临时桩签名、`BuildCastFunction*`、`WrapWithCastAs{Int,Real,Decimal}`、`PropagateType` 等。
2. `crates/expression/src/constant_fold.rs`：`constant_fold.go`（D08.2）与 `MaybeOverOptimized4PlanCache`/`containMutableConst`（util.go，D08.12）。
3. 框架：`#[eval_fn]` 的 `Decimal` 结果与 `&MyDecimal` 参数，`Args::decimal`，`DecimalView`/`DecimalOut`，常量列的 DECIMAL；`SigView::vector()`；`BuiltinFunc::in_union`；`gofmt::Arg::{Struct, Array}`。
4. govec `cast` 领域（tracker `govec/cast-D08.8-1`）：`cast` 3,060 条（含 60 条 UNSIGNED UNION 负数字符串）、`wrap` 700 条。每条 `cast` 记录：构建结果（`sfDump`：签名名、`tipb` 码、结果类型、collation、参数——含 `PropagateType` 之后的类型；常量参数则是折叠后的常量）、构建时的 warning、`inUnion` 元数据，以及逐行模式每行的值/错误/warning 和向量模式的值或错误与 warning。随机的类型标志（`FlagIgnoreTruncateErr`、`FlagTruncateAsWarning`、`FlagAllowNegativeToUnsigned`）和 7 个错误组的级别。

回放：`cargo test -p tidb-expression --test govec_cast`；额外的种子：`GOVEC_CAST_FILES=a.jsonl.gz,...`。

## 第 2 部分（交接）

1. RPN 的列引用按签名读取的求值类型转换（FLOAT→float64、BIT/ENUM/SET→int），对应 Go `Column.VecEvalInt/Real`；之后 govec 加 FLOAT 与 hybrid 参数。
2. CAST 到 STRING（`newBaseBuiltinCastFunc4String`、`adjustRetFtForCastString`、`HandleBinaryLiteral`/from_binary、`padZeroForBinaryType`）、TIME、DURATION、JSON（含 ARRAY）、VECTOR；其余 `WrapWithCastAs*`、`BuildCastCollationFunction`。
3. 从 TIME/DURATION/JSON/VECTOR 出发的签名（去掉临时桩）；`VectorFloat32AsUnsupported` 需要一个签名有多个求值类型的驱动。
4. 然后 D08.2 第 2 部分其余：`newBaseBuiltinFuncWithTp`/`WithFieldTypes`（用 `derive` 记录经它们回放，Lucky 评审 #80）、注册表的 `get_function`、`NewFunction*`（D08.1 2b），以及 D08.1 2b 的 `EvaluatorSuite`、`chunk_executor`、`VecEvalBool`。
