# D08.12 表达式工具（spec）

状态：第 1 批（本 PR）。基线：Go TiDB v8.5.8（`8b857efa2`）。块成员以 tidb-rs-tracker `data/func_blocks.tsv` 为准（135 个函数：`util.go`、`explain.go`、`extension.go`、`helper.go`）。

## 1. 范围

### 1a. 第 1 批（本 PR）

`util.go` 中依赖都已移植的表达式树工具，放在 `crates/expression/src/util.rs`（与 D08.2 常量传播时移植的 `ColumnSubstitute*` 等在同一个文件）：

| 类别 | Go 函数 |
|---|---|
| 过滤 | `Filter`、`FilterOutInPlace` |
| 提取列 | `ExtractDependentColumns`/`extractDependentColumns`、`ExtractAllColumnsFromExpressionsInUsedSlices`/`extractColumnsSlices`、`ExtractCorColumns`、`ExtractColumnsFromExpressions`、`extractColumnsAndCorColumns`/`ExtractColumnsAndCorColumnsFromExpressions`、`ExtractColumnSet`/`extractColumnSet` |
| 等价与常量等值 | `ExtractEquivalenceColumns`/`extractEquivalenceColumns`、`ExtractConstantEqColumnsOrScalar`/`extractConstantEqColumnsOrScalar`、`FindUpperBound` |
| NOT 下推 | `pushNotAcrossArgs`、`pushNotAcrossExpr`、`PushDownNot`、`GetExprInsideIsTruth`、`ContainOuterNot`/`containOuterNot`（`oppositeOp` 写成 match） |
| 去掉无损 CAST | `noPrecisionLossCastCompatible`、`unwrapCast`、`eliminateCastFunction`、`EliminateNoPrecisionLossCast` |
| DNF | `ExtractFiltersFromDNFs`/`extractFiltersFromDNF`、`DeriveRelaxedFiltersFromDNF` |
| 判断 | `Contains`、`IsRuntimeConstExpr`、`CheckNonDeterministic`、`CheckFuncInExpr`、`IsImmutableFunc`、`ContainVirtualColumn`、`ContainCorrelatedColumn`、`ProjectionBenefitsFromPushedDown`/`jsonUnquoteFunctionBenefitsFromPushedDown`、`ExprsHasSideEffects`/`ExprHasSetVarOrSleep`、`MaybeOverOptimized4PlanCacheForMultiExpression` |
| 行与参数 | `GetRowLen`、`CheckArgsNotMultiColumnRow`、`GetFuncArg` |
| 常量 | `DatumToConstant`、`GetStringFromConstant`、`GetIntFromConstant`、`GetUint64FromConstant` |
| 显示 | `GetFormatBytes`、`GetFormatNanoTime`、`ExprsToStringsForDisplay` |

另移植 D08.1 的 `extractBinaryOpItems`、`FlattenDNFConditions`、`FlattenCNFConditions`（`expression.rs`），去掉 CAST 和 DNF 提取要用。

此前随其他块移植的 14 个 D08.12 函数（`ColumnSubstitute*`、`cowExprRef.*`、`checkCollationStrictness`、`SetExprColumnInOperand`、`BuildNotNullExpr`、`IsMutableEffectsExpr`、`RemoveDupExprs`、`ConstExprConsiderPlanCache`、`MaybeOverOptimized4PlanCache`、`containMutableConst`、`RemoveMutableConst`、`DisableParseJSONFlag4Expr`）的 govec 覆盖登记到 cprop / cmpbuild。

### 1b. 不在第 1 批（登记在 tracker `govec/domains/cast_pending_d0812.go`）

| 部分 | 原因 / 去处 |
|---|---|
| `explain.go`（20 个） | 第 2 批：EXPLAIN 中表达式的打印 |
| `helper.go`（7 个） | 第 2 批：时间函数（D08.7）用的当前时间辅助函数 |
| `extension.go`（16 个） | tidb-rs 不支持 `pkg/extension`（与 D08.2 `GetBuiltinList` 一致），永久豁免 |
| `ParamMarkerExpression`、`ParamMarkerInPrepareChecker.*`、`ConstructPositionExpr`、`PosFromPositionExpr` | 第 2 批：要用 parser driver 的 `ParamMarkerExpr` |
| `SubstituteCorCol2Constant` | 第 2 批：读 `CorrelatedColumn.Data`，tidb-rs 中值在 `EvalState`（D08.1 spec §3.2），签名由规划器的调用方决定 |
| `ExecBinaryParam`、`binary*`（7 个） | 第 2 批：二进制协议参数，随协议层 D03.1 |
| `SQLDigestTextRetriever.*`、`NewSQLDigestTextRetriever` | 要用 statements_summary 上的 SQLExecutor（D08.9 / D14.2） |
| `PopRowFirstArg` | 要用 ROW()（D08.9） |
| `getValidPrefix` / `locateStringWithCollation` / `timeZone2int` | 分别是 CONV（D08.5 T5b）、LOCATE/INSTR（D08.6）、时间函数（D08.7）的辅助函数 |

## 2. 依赖

D08.1（表达式框架、`wrap_with_is_true`、CNF/DNF 组合）、D08.2（`new_function_internal`、函数集合 `unFoldableFunctions`/`mutableEffectsFunctions`）、D08.4 / D08.5（比较、逻辑、算术、IS NULL 签名，用于 govec 构建表达式）、D08.8（`build_cast_function_with_check`）、`tidb_types::collate::compatible_collate`、`tidb_base::gostd::strconv`、`tidb_base::gofmt::strconv_quote`。

## 3. 设计

- 返回 `[]*Column` 的函数返回 `Vec<Arc<Column>>`，列复制进新的 `Arc`（与已有的 `extract_columns` 相同）；`ExtractCorColumns` 返回 `Vec<Arc<CorrelatedColumn>>`，保留 slot。
- Go 的 nil Expression 用 `Option<ExprRef>` 表示（`PushDownNot`、`EliminateNoPrecisionLossCast`、`DeriveRelaxedFiltersFromDNF`）；Go 把 nil 再传给 `NewFunctionInternal` 时会 panic，Rust 同样 panic。
- 追加结果的函数（`Filter`、`Extract*`）接收并返回 `Vec`，与 Go 的 append 语义一致。

## 4. 偏离

1. **`ExtractColumnSet` 用 `BTreeSet<i64>`**：`intset.FastIntSet`（D00.3）还没有移植。值与 `SortedArray()` 相同（升序）。移植 D00.3 后替换。
2. **`extractFiltersFromDNF` 提取项的顺序**：Go 遍历 map 收集提取项，顺序随机；Rust 按它们在第一个 DNF 项中首次出现的顺序（M-MAPORDER）。govec 比较时双方都排序。
3. **`ExtractAllColumnsFromExpressionsInUsedSlices`**：Go 用不稳定的 `slices.SortFunc` 再 `CompactFunc`，同一 UniqueID 的多个列保留哪一个不确定（例如函数参数中被 `PropagateType` 改过类型的副本）；Rust 用稳定排序保留第一个。govec 只比较 UniqueID。
4. **`GetUint64FromConstant` 不写 warning 日志**。
5. **`FilterOutInPlace`** 返回剩下的 `Vec`，而不是对输入切片重新切片。
6. **原地修改**：Go 在构建或改写函数时会原地修改共享的 `*FieldType`（例如 JSON CAST 的 ParseToJSONFlag），共享这个类型的列都会看到；tidb-rs 改副本（D08.1 spec §4）。govec 把这类记录标为 `known_deviation`，按 cprop 的规则计数（`util` 和 `cprop` 共用 2% 的上限）。

没有面向用户的差异。

## 5. 验证

govec `cast` 领域的 `util` 操作（tidb-rs-tracker `govec/domains/cast_d0812.go`），回放在 `crates/expression/tests/cast_util/mod.rs`。新增配方：`["corcol", i]`（关联列，UniqueID 101+i）、`["vcol", i, recipe]`（虚拟列，UniqueID 201+i）。
