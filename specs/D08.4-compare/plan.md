# D08.4 比较函数：plan

## 第 1 批（本 PR）

见 spec §1。验证：govec `cast` 领域的 `cmpbuild`（`name(lhs, rhs)` 用 `NewFunctionBase` 或 `NewFunction` 构建，记录改写后的参数与签名、`GetAccurateCmpType`、`GetCmpFunction` 的逐行结果，再用 `EvalExpr` 关闭/打开向量化求值）与 `newfunc`（常量折叠）记录：

| 来源 | 条数 | 内容 |
|---|--:|---|
| `sys` | 3,000 | 1–3 列的输入（数值/字符串、FLOAT 与 hybrid、时间/duration/JSON/VECTOR、YEAR，NULL，`Sel()`），参数是列、特殊常量（整数边界、2019/69/70、1.5、`'10:00:00'`、`'[1]'`…、NULL 常量）、CAST、嵌套比较 |
| `same` | 500 | 同类型的两列（逐行 CompareFunc、相等值、NULL 安全等于） |
| `sign` | 400 | 有符号与无符号 BIGINT 列和常量（CompareInt 的 unsigned 标志、refineArgsByUnsignedFlag） |
| `json-parse` | 84 | `CAST(s AS JSON) <op> s`（DisableParseJSONFlag4Expr） |
| `tryfold` | 200 | 常量比较的 NewFunction / NewFunctionTryFold，构建时有 warning |
| `cmp` | 700 | `newfunc`：两个常量或列的比较，5 种入口 |

## 后续批次（交接）

1. **第 2 批**：COALESCE（`coalesceFunctionClass` 与 8 个签名，含 `fallbackEvalXxx`）、`aggregateType`。
2. **第 3 批**：GREATEST/LEAST（含 `CmpStringAsTime`、`resolveType4Extremum`、`fixFlenAndDecimalForGreatestAndLeast`、`getAccurateTimeTypeForGLRet`、`doTimeConversionForGL`、`unsupportedJSONComparison`）、INTERVAL（`binSearch`/`linearSearch`）、`ResolveType4Between`。
3. **D08.5 移植 ceil/floor 后**：去掉 govec 生成器的 `cmpCeilPath` 过滤，补 `int 列 <op> 1.5` 一类记录（spec §4 第 2 条）。
4. **常量传播（D08.2 第 2b 部分，`constant_propagation.go`）**：需要 `LogicAnd`/`LogicOr`/`IsNull` 等逻辑函数（D08.5）与比较函数；在 D08.5 第一批之后做。
