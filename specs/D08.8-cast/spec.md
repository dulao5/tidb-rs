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
| 2b-2 | `castAsTime/Duration/JSON/Array/VectorFloat32FunctionClass` 与它们的签名、从 TIME/DURATION/JSON/VECTOR 出发的签名；`WrapWithCastAsTime/Duration/JSON/VectorFloat32`；`ConvertJSON2Tp` 等 |

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

## 4. 偏离与豁免

1. **`BuildCastFunction*`/`WrapWithCastAs*` 返回 `Result`**：Go 记日志并返回 `Function == nil` 的节点（之后使用会空指针 panic）；Rust 返回错误。
2. **CAST 到 TIME/DURATION/JSON/VECTOR**：返回 "not ported yet (D08.8 part 2b-2)" 错误（临时桩，宪章第四条 3）。`newBaseBuiltinFuncWithTp` 对这些求值类型的参数同样返回这个错误。
3. **TIME/DURATION/JSON/VECTOR 参数的 INT/REAL/DECIMAL/STRING 签名**：只有描述，没有驱动（临时桩）。
4. **`TryPushCastIntoControlFunctionForHybridType`**：IF/CASE/ELT 未移植，tidb-rs 建不出这样的节点；遇到时 `unimplemented!`（临时桩，D08.9）。
5. **`CastStringAs{Int,Real,Decimal}` 的前置分支**（hybrid、二进制字面量、`CanImplicitEval*`）：函数类不会对这样的参数选中这些签名（选 `IntAsInt` 等），不可达，未移植（L2）。
6. **`PropagateType` 与类型的原地修改**：Go 直接修改参数节点（及共享它的所有持有者）的 `*FieldType`；Rust 在副本上修改（`with_type_mut`）。`ScalarFunction` 的 `RetType` 与签名的 `tp` 在 Go 中是同一个指针，两者一起改。**移植注意**（Lucky 评审 #82）：调用点 builtin_cast.go:220（`BuildCastFunction` 中对参数调用 `PropagateType`）在 Go 中原地修改的是**共享的** `ScalarFunction`/`Constant` 节点，计划中所有引用同一节点的地方都会看到新类型。按 Lucky 的分析，这一点从 SQL 观察不到；但规划器（P3）的 Go 代码可能依赖这种共享，移植规划器时遇到同一节点被多处引用、又在 CAST 之后读取其类型的地方，要确认是否需要显式地同步修改。
7. **`foldConstant` 的 null-reject 分支**需要 `NewFunctionBase`（D08.1 2b），之前保留原表达式（与 Go 中 `NewFunctionBase` 失败时相同，临时桩）；`ifNullFoldHandler` 修改被丢弃的 IFNULL 节点的 charset/collation，Rust 不做（L2）。
8. **向量版保留 `ResizeDecimal` 内容的情况**（`StringAsDecimal` 的 UNSIGNED UNION 负数）：Rust 写零（新列的内容）；复用的旧列在 Go 中会留下旧值。
9. ~~FLOAT 与 hybrid（BIT/ENUM/SET）参数在向量驱动中~~：第 2a 部分已修正（RPN 按签名读取的求值类型转换）。FLOAT 加宽成 DOUBLE 的逐位比较见 "dbl" 记录（Lucky 评审 #84）。
10. **`adjustRetFtForCastString` 中 Go 的 `intest.Assert` 分支**（ENUM/SET 带 `EnumSetAsIntFlag`、未知类型）：按 Go 的生产构建直接返回（D08.1 spec §4 第 6 条）。
11. **`newBaseBuiltinFuncWithTp` 不检查 nil 上下文**（引用不会为空）；参数个数与类型个数不同时与 Go 一样 panic。

没有面向用户的行为差异（`docs/compatibility.md` 不变）。
