# D08.8 CAST：spec

状态：第 1 部分（本 PR）——CAST 到 INT、REAL、DECIMAL；`BuildCastFunction*`、`WrapWithCastAsInt/Real/Decimal`；随之移植 D08.2 的常量折叠（`constant_fold.go`）与 `baseBuiltinCastFunc`、D08.1 的 `PropagateType`。第 2 部分：CAST 到 STRING/TIME/DURATION/JSON/VECTOR，以及从 TIME/DURATION/JSON/VECTOR 出发的签名。

收敛检查（第 1 部分）：D08.8 matched 64/250（`builtin_cast.go` 152 + `builtin_cast_vec.go` 98，第 1 部分移植 12 个签名的 `evalXxx`/`vecEvalXxx`/`vectorized`/`Clone`、3 个函数类、`BuildCastFunction*`、`WrapWithCastAs{Int,Real,Decimal}` 等）；D08.2 77/151（第 1 部分 68）；D08.1 198/348（2a 197）；D08.12 2/135（`MaybeOverOptimized4PlanCache`、`containMutableConst`）。govec `cast` 领域 3,760 条，覆盖 D08.8 的 61 个函数，豁免 189 个（`cast_pending.go`）。

## 1. 范围

| 部分 | 内容 |
|---|---|
| 1 | `castAsInt/Real/DecimalFunctionClass.getFunction`；签名 `builtinCast{Int,Real,Decimal,String}As{Int,Real,Decimal}Sig`（12 个，含 `handleOverflow`）；`newBaseBuiltinCastFunc`、`baseBuiltinCastFunc.metadata/cloneFrom`（D08.2）；`BuildCastFunction`、`BuildCastFunction4Union`、`BuildCastFunctionWithCheck`；`WrapWithCastAsInt/Real/Decimal`、`minimalDecimalLenForHoldingInteger`；`setDataTypeDouble`、`floatLength`、`PropagateType`（D08.1）；`CanImplicitEvalInt/Real`；`TryPushCastIntoControlFunctionForHybridType`（临时桩，见 §4）；`constant_fold.go`（D08.2） |
| 1（临时桩） | 函数类会选中的 `builtinCast{Time,Duration,JSON}As{Int,Real,Decimal}Sig`、`builtinCastVectorFloat32AsUnsupportedSig`：只有描述（名字、`tipb` 码、`vectorized`），没有驱动；求值返回 Go 的 `baseBuiltinFunc` 错误 |
| 2 | `castAsString/Time/Duration/JSON/Array/VectorFloat32FunctionClass` 与它们的签名；`adjustRetFtForCastString`、`decimalPrecisionToLength`、`padZeroForBinaryType`、`newBaseBuiltinCastFunc4String`（D08.2）；`WrapWithCastAsString/Time/Duration/JSON/VectorFloat32`、`BuildCastCollationFunction`；`ConvertJSON2Tp` 等 |

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

## 4. 偏离与豁免

1. **`BuildCastFunction*`/`WrapWithCastAs*` 返回 `Result`**：Go 记日志并返回 `Function == nil` 的节点（之后使用会空指针 panic）；Rust 返回错误。
2. **CAST 到 STRING/TIME/DURATION/JSON/VECTOR**：返回 "not ported yet (D08.8 part 2)" 错误（临时桩，宪章第四条 3）。
3. **TIME/DURATION/JSON/VECTOR 参数的 INT/REAL/DECIMAL 签名**：只有描述，没有驱动（临时桩）。
4. **`TryPushCastIntoControlFunctionForHybridType`**：IF/CASE/ELT 未移植，tidb-rs 建不出这样的节点；遇到时 `unimplemented!`（临时桩，D08.9）。
5. **`CastStringAs{Int,Real,Decimal}` 的前置分支**（hybrid、二进制字面量、`CanImplicitEval*`）：函数类不会对这样的参数选中这些签名（选 `IntAsInt` 等），不可达，未移植（L2）。
6. **`PropagateType` 与类型的原地修改**：Go 直接修改参数节点（及共享它的所有持有者）的 `*FieldType`；Rust 在副本上修改（`with_type_mut`）。`ScalarFunction` 的 `RetType` 与签名的 `tp` 在 Go 中是同一个指针，两者一起改。**移植注意**（Lucky 评审 #82）：调用点 builtin_cast.go:220（`BuildCastFunction` 中对参数调用 `PropagateType`）在 Go 中原地修改的是**共享的** `ScalarFunction`/`Constant` 节点，计划中所有引用同一节点的地方都会看到新类型。按 Lucky 的分析，这一点从 SQL 观察不到；但规划器（P3）的 Go 代码可能依赖这种共享，移植规划器时遇到同一节点被多处引用、又在 CAST 之后读取其类型的地方，要确认是否需要显式地同步修改。
7. **`foldConstant` 的 null-reject 分支**需要 `NewFunctionBase`（D08.1 2b），之前保留原表达式（与 Go 中 `NewFunctionBase` 失败时相同，临时桩）；`ifNullFoldHandler` 修改被丢弃的 IFNULL 节点的 charset/collation，Rust 不做（L2）。
8. **向量版保留 `ResizeDecimal` 内容的情况**（`StringAsDecimal` 的 UNSIGNED UNION 负数）：Rust 写零（新列的内容）；复用的旧列在 Go 中会留下旧值。
9. **FLOAT 与 hybrid（BIT/ENUM/SET）参数在向量驱动中**：RPN 把列原样交给签名（FLOAT 列是 float32、BIT/ENUM/SET 不是 int64），Go 的 `Column.VecEvalReal/Int` 会转换。govec 不生成这类参数；修正放在第 2 部分（RPN 的列引用要按签名读取的求值类型转换）。

没有面向用户的行为差异（`docs/compatibility.md` 不变）。
