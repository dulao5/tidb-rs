# D09.4 聚合、窗口与 `ONLY_FULL_GROUP_BY` 的计划构建（spec）

状态：待评审（只写 spec，issue #406）。块：D09.4（ROADMAP P3；Go 116 个函数，分布于 4 个文件，另承接 P1 移交的 `grouping_sets.go` 29 个与 `util.go` 4 个函数），crate `tidb-planner`（P1 移交项在 `tidb-expression` 与 `tidb-expr-pushdown`）。

相关：[D09.1 spec](../D09.1-select-builder/spec.md)（SELECT 构建与 VS1-21 已移植部分）、[D09.2 spec](../D09.2-rewriter/spec.md)（表达式改写与 `ByAddr` 约定）、[D09.9 spec](../D09.9-agg-rules/spec.md)（聚合逻辑优化规则）、[D09.14 spec](../D09.14-logical-operators/spec.md)（逻辑算子公共框架）、[D08.11 spec](../D08.11-aggregation/spec.md)（聚合与窗口函数描述）、[D12.3 spec](../D12.3-agg-executors/spec.md)（聚合执行器框架）、[N0](../exceptions/N0-crate-layering/spec.md)（crate 分层）、[N1](../exceptions/N1-plan-representation/spec.md)（计划树与表达式树表示、§4 摘出分派、§5 共享修改审计）、[N13](../exceptions/N13-shared-metadata/spec.md)（共享元数据与 `Arc<FieldType>`）、[mappings.md](../../docs/porting/mappings.md)（全局映射规则）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/planner/core/`，另有说明的除外。

## 1. 这个块在 Go 里做什么

### 1.1 内容

本块负责 SELECT 语句中**聚合（`GROUP BY`、聚合函数、`SELECT DISTINCT`）、`ONLY_FULL_GROUP_BY` 合法性检查、窗口函数（`OVER` / `WINDOW` 子句）、`GROUP BY ... WITH ROLLUP`（数据展开）**的逻辑计划构建，以及对应的三个逻辑算子（`LogicalAggregation`、`LogicalWindow`、`LogicalExpand`）。全部属于只读查询的逻辑计划构建，不涉及数据写入。

| Go 文件 | D09.4 函数数 | 内容 |
|---|--:|---|
| `logical_plan_builder.go` | 64 | `buildSelect` 调用的各阶段构建与解析：GROUP BY 解析与改写（`resolveGbyExprs`、`rewriteGbyExprs`、`gbyResolver`）、`ONLY_FULL_GROUP_BY` 检查（有 / 无 GROUP BY、WHERE 等值依赖与唯一键函数依赖）、HAVING / ORDER BY / 窗口表达式列解析（`havingWindowAndOrderbyExprResolver`、`resolveHavingAndOrderBy`、`resolveWindowFunction`）、聚合函数提取与构建（`extractAggFuncsIn*`、`buildAggregation`、`aggOrderByResolver`）、子查询相关聚合提取（`resolveCorrelatedAggregates`、`correlatedAggregateResolver`、`extractCorrelatedAggFuncs`、`aggColNameResolver`）、`SELECT DISTINCT` 构建与 ORDER BY 冲突检查（`buildDistinct`、`checkOrderByInDistinct`）、ROLLUP 展开构建与 `GROUPING()` 替换（`buildExpand`、`buildExpandFieldName`、`replaceGroupingFunc`、`implicitProjectGroupingSetCols`）、窗口规范解析 / 合并 / 分组 / 排序 / 帧校验与构建（`buildWindowSpecs`、`resolveWindowSpec`、`mergeWindowSpec`、`groupWindowFuncs`、`handleDefaultFrame`、`sortWindowSpecs`、`checkWindowFuncArgs`、`checkOriginWindow*`、`buildProjectionForWindow`、`buildByItemsForWindow`、`buildWindowFunctionFrame*`、`buildWindowFunctions`） |
| `operator/logicalop/logical_aggregation.go` | 25 | `LogicalAggregation` 算子：`Init`、`ExplainInfo`、`ReplaceExprColumns`、`BuildKeyInfo` / `BuildSelfKeyInfo`、`CanPushToCop`、`ExtractFD`、`HasDistinct` / `HasOrderBy` / `IsPartialModeAgg` / `GetGroupByCols` / `GetPotentialPartitionKeys` / `GetUsedCols` / `ResetHintIfConflicted` / `DistinctArgsMeetsProperty` / `CanPullUp` / `getGroupNDVs`、聚合谓词下推辅助（`splitCondForAggregation`、`pushDown*PredicatesForAggregation`），以及 4 个仅旧 cascades 优化器用的方法（豁免） |
| `operator/logicalop/logical_window.go` | 17 | `LogicalWindow`、`WindowFrame`、`FrameBound`：`Init`、`Clone`、`UpdateCmpFuncsAndCmpDataType`、`UpdateCompareCols`、`ToPB`、`ReplaceExprColumns`、`GetPartitionBy` / `GetPartitionByCols` / `GetPartitionKeys` / `GetWindowResultColumns` / `CheckComparisonForTiFlash` / `extractUsedCols` / `GetGroupNDVs`，以及 3 个仅旧 cascades 优化器用的方法（豁免） |
| `operator/logicalop/logical_expand.go` | 10 | `LogicalExpand` 算子：`Init`、`ExtractFD`、`GetUsedCols`、`GenLevelProjections`、`GenerateGroupingMarks`、`TrySubstituteExprWithGroupingSetCol`、`ResolveGroupingFuncArgsInGroupBy`、`GenerateGroupingIDModeBitAnd`、`GenerateGroupingIDIncrementModeNumericSet`、`BuildKeyInfo` |

此外，本块承接 P1 收尾移交的两批表达式侧函数（见 `specs/D09.4-agg-window/tasks.md`）：

- **P1-1（原归 D08.2）**：`pkg/expression/grouping_sets.go` 全部 **29** 个函数（`GroupingSets` / `GroupingSet` / `GroupingExprs` 及 ROLLUP 构造、去重、恢复、可空性调整、基数去重、`ToPB`）。
- **P1-2（原归 D08.12）**：`pkg/expression/util.go` 的 **4** 个函数：GROUP BY / `GROUP_CONCAT(... ORDER BY ...)` 位置项（`ConstructPositionExpr`、`PosFromPositionExpr`），以及窗口函数参数 / 帧边界在 PREPARE 阶段的参数标记检查（`ParamMarkerInPrepareChecker.Enter` / `Leave`）。

### 1.2 执行流程要点（逐条与 v8.5.8 源码核对）

`PlanBuilder.buildSelect`（`logical_plan_builder.go:4346-4680`）按严格顺序串联本块的各个步骤：

1. **展开通配符之后解析 GROUP BY**（`:4448-4453`）：`resolveGbyExprs` 用 `gbyResolver` 把 `GROUP BY` 中的别名和位置项（`GROUP BY 1`、`GROUP BY ?`）解析成对应的选择列表达式，拒绝引用聚合函数或窗口函数（报 `ErrIllegalReference` 或 `ErrWrongGroupField`）。
2. **`ONLY_FULL_GROUP_BY` 检查在改写 GROUP BY 之前**（`:4457-4462`）：当开启 `ONLY_FULL_GROUP_BY`、存在 `FROM` 且未开启新版基于函数依赖的检查开关（`OptimizerEnableNewOnlyFullGroupByCheck`，默认 OFF）时调用 `checkOnlyFullGroupBy`。有 `GROUP BY` 走 `checkOnlyFullGroupByWithGroupClause`（收集 GROUP BY 列/表达式与 WHERE 单值列，对不在其中的 SELECT / ORDER BY 列查 WHERE / JOIN 等值依赖与唯一键 / 主键函数依赖）；无 `GROUP BY` 走 `checkOnlyFullGroupByWithOutGroupClause`（先检查非聚合查询的 `ORDER BY agg(...)` 报 `ErrAggregateOrderNonAggQuery`，再对聚合与普通列混用的情况做单值 / 函数依赖检查）。
3. **改写 GROUP BY 表达式**（`:4464-4469`）：`rewriteGbyExprs` 把解析后的 AST 表达式逐项改写成 `expression.Expression`，返回 `gbyCols` 和 `rollup` 标志。
4. **窗口函数与 HAVING / ORDER BY 列解析**（`:4471-4491`）：先 `resolveWindowFunction`（遍历带窗口标志的 SELECT 字段、全部 `WINDOW` 定义、带窗口标志的 ORDER BY 项，向选择列表追加辅助列与 `aggMapper`），再 `resolveHavingAndOrderBy`（解析 HAVING 与不含窗口的 ORDER BY，在 `inAggFunc` 内将别名列替换为对应的选择列表达式，如 `SELECT a+1 AS b FROM t HAVING SUM(b) < 0` → 聚合 `a+1`）。
5. **相关聚合提取**（`:4497-4500`）：`resolveCorrelatedAggregates` 深入 SELECT / HAVING / ORDER BY 的子查询，提取归属外层查询的聚合函数并追加为外层的辅助字段。
6. **WHERE 与锁之后构建 ROLLUP Expand 与 Aggregation**（`:4550-4581`）：`detectSelectAgg` 为真时先 `extractAggFuncsInSelectFields`；若非纯外层相关聚合，则在 `rollup == true` 时先 `buildExpand`（建下层 `Proj4Expand = true` 的投影 + `LogicalExpand`，设置 `FlagResolveExpand`、记录 `b.currentBlockExpand`），再 `buildAggregation`（设置 7 个逻辑优化标志，若 `tidb_opt_skew_distinct_agg = ON` 再设 `FlagSkewDistinctAgg`；改写每个聚合参数及 `GROUP_CONCAT` 的 `ORDER BY`；对等价聚合去重；为输入计划每一列及 join `FullSchema` 冗余列补 `firstrow` 聚合；调用 `UpdateNotNullFlag4RetType` 调整可空性）。
7. **第一次 Projection 与 HAVING**（`:4586-4597`）：`buildProjection(..., considerWindow = false, ...)` 跳过窗口字段（先放 `0` 占位）；若有 `LogicalExpand`，用 `replaceGroupingFunc` 把非聚合表达式中的原分组表达式替换成 Expand 输出的分组列，并用 `implicitProjectGroupingSetCols` 隐式投影分组列、`gid`（及有重复分组集时的 `gpos`）。之后对 `HAVING` 建 `Selection`。
8. **窗口构建与第二次 Projection**（`:4599-4629`）：`buildWindowSpecs` 解析命名窗口引用链（检测重名、缺窗、环路、继承规则）；`extractWindowFuncs` → `checkWindowFuncArgs` → `groupWindowFuncs`（按 `handleDefaultFrame` 规范化默认帧，含未使用的命名窗口）→ `buildWindowFunctions`（`sortWindowSpecs` 逆字典序稳定排序，每组先建 `buildProjectionForWindow`，对非空组校验帧边界并建 `LogicalWindow`，对每个窗口函数调 `NewWindowFuncDesc` 和 `WrapCastForAggArgs`）；最后若有窗口字段，执行第二次 `buildProjection(..., considerWindow = true, ...)` 填回窗口表达式并更新 `windowMapper` 供 ORDER BY 使用。
9. **DISTINCT 与 ORDER BY 冲突检查**（`:4631-4657`）：`sel.Distinct` 时调 `buildDistinct(p, oldLen)`，建一个全为 `firstrow`、按前 `oldLen` 列分组的 `LogicalAggregation`（设置 `FlagBuildKeyInfo | FlagPushDownAgg`）。随后 `ORDER BY` 在 `ONLY_FULL_GROUP_BY` 下走 `buildSortWithCheck`，当 `sel.Distinct` 时调 `checkOrderByInDistinct` 检查 ORDER BY 表达式/引用列是否出现在 DISTINCT 列表中（上游 issue `12442`）。

### 1.3 调用方（tracker `data/block_edges.tsv`）

| 调用方块 | 引用数 | 用到什么 |
|---|--:|---|
| EXC.B（旧 cascades 优化器） | 42 | `LogicalAggregation` / `LogicalWindow` 的 `Hash64`、`Equals`、`CopyAggHints`、`IsCompleteModeAgg`、`Equal*` 等（不移植） |
| D09.9 聚合逻辑优化规则 | 37 | `LogicalAggregation` 全部字段与 `Init`、`GetGroupByCols`、`GetUsedCols`、`BuildKeyInfo` 等 |
| D10.3 物理计划枚举 | 37 | `LogicalAggregation`（`HasDistinct`、`HasOrderBy`、`CanPushToCop`、`ResetHintIfConflicted`、`DistinctArgsMeetsProperty`、`GetGroupByCols`、`GetPotentialPartitionKeys`）、`LogicalWindow`（`GetPartitionBy`、`GetPartitionKeys`、`CheckComparisonForTiFlash`）、`LogicalExpand` |
| D09.1 SELECT 构建 | 22 | `buildSelect`、`buildProjection`、`buildSortWithCheck` 调用本块全部构建入口 |
| D09.2 表达式改写 | 11 | `funcCallToExpressionWithPlanCtx` 对 `GROUPING()` 调用 `LogicalExpand` 的 `ResolveGroupingFuncArgsInGroupBy`、`GenerateGroupingMarks` |
| D09.6 子查询与去关联 | 9 | `LogicalAggregation.CanPullUp`、`GetGroupByCols`、`Init`、`resolveCorrelatedAggregates` |
| D09.7 列裁剪与投影消除 | 7 | `LogicalAggregation` / `LogicalWindow` / `LogicalExpand` 的类型匹配与 `ReplaceExprColumns` |
| D10.2 `FindBestTask` 与访问路径 | 6 | `LogicalAggregation` / `LogicalWindow` 的 `PreparePossibleProperties`（类型匹配） |
| D10.6 基数估算 | 6 | `LogicalAggregation.getGroupNDVs`、`LogicalWindow.GetGroupNDVs` |
| D09.3 join 构建、D09.8 谓词下推、D09.11 TopN 下推、D09.14 逻辑算子 | 各 5 | `BuildSelfKeyInfo`、`splitCondForAggregation`、`GetPartitionBy`、`GetWindowResultColumns` 等 |
| D09.13 优化器驱动、D10.9 后处理与 EXPLAIN、D09.0、D09.10、D09.16、D10.1、D12.9、D09.5 | 1–4 | `ResolveExpand` 调 `GenLevelProjections`；`plan_to_pb` 调 `FrameBound.ToPB`；`executor/builder.go` 调 `FrameBound.UpdateCompareCols`；`stringer.go` / `collect_column_stats_usage.go` |

### 1.4 sysbench `oltp_read_only` 路径

sysbench 只读基准中有两条聚合查询目前在 `crates/session/src/tests/sbtest_results.rs:483-533` 停在 `blocked-by:D09.4` 并回退到原型（PoC）：

1. **`SELECT SUM(k) FROM sbtest1 WHERE id BETWEEN ? AND ?`**：
   走 `buildSelect` → `checkOnlyFullGroupBy`（无 GROUP BY、无非聚合列，直接放行）→ `resolveHavingAndOrderBy` → `buildSelection` → `detectSelectAgg` 为真 → `extractAggFuncsInSelectFields` → `buildAggregation`（建 `LogicalAggregation`，含 `sum(k)` + 表各列的 `firstrow`）→ `buildProjection` → 逻辑优化（`BuildKeySolver`、`ColumnPruner`、`PPDSolver`、D09.9 三条规则、投影消除）→ D10.1/D10.3 物理聚合 → D12.3/D12.4 执行器。
2. **`SELECT DISTINCT c FROM sbtest1 WHERE id BETWEEN ? AND ? ORDER BY c`**：
   走 `buildSelect` → `checkOnlyFullGroupBy` → `resolveHavingAndOrderBy` → `buildSelection` → `buildProjection` → `buildDistinct`（建 `LogicalAggregation`，`GroupByItems = [c]`，每列一个 `firstrow`）→ `buildSortWithCheck` → `checkOrderByInDistinct`（核对 `c` 在 DISTINCT 列表中）→ 逻辑优化（`BuildKeySolver`、D09.9 聚合下推合并投影）→ D10.1/D10.3 → D12.3/D12.4。

这两条路径全部落在**第 A 部分**，不依赖窗口（第 B 部分）、ROLLUP（第 C 部分）或子查询相关聚合（第 D 部分）。

## 2. 依赖

全部依赖均已用 `grep -rn '// go: <Symbol>' crates/` 在当前仓库核对。

### 2.1 已移植（可直接调用）

| 块 | 用到的符号 / 能力 | Rust 位置 |
|---|---|---|
| D08.1 表达式框架 | `Schema`（`Clone`、`Append`、`Contains`、`ColumnIndex`、`ColumnsIndices`、`Keys` / `UniqueKeys`）、`Column`、`Constant`（`NewZero`、`NewOne`、`NewNullWithFieldType`、`NewUInt64ConstWithFieldType`、`EvalInt`）、`ScalarFunction`（`NewFunctionBase`）、`Column2Exprs`、`ExtractColumns`、`ExtractColumnsFromExpressions`、`ExtractCorColumns`、`SplitCNFItems`、`SplitDNFItems`、`ComposeCNFCondition`、`ComposeDNFCondition`、`EvaluateExprWithNull`、`ExprFromSchema`、`GetUsedList`、`CanonicalHashCode`、`Traverse` / `TraverseAction` | `crates/expression/src/` |
| D08.4 比较函数 | `GetCmpFunction`、`GetAccurateCmpType`、`CompareInt`、`CompareTime`、`CompareDuration`、`CompareReal`、`CompareDecimal` | `crates/expression/src/builtin_compare.rs` |
| D08.5 算术函数 | `Plus`、`Minus`（窗口 `RANGE` 数值帧边界） | `crates/expression/src/builtin_arithmetic.rs` |
| D08.11 聚合描述（第 A + B 部分已合并） | `AggFuncDesc`、`AggFunctionMode`、`NewAggFuncDesc`、`UpdateNotNullFlag4RetType`、`WrapCastForAggArgs`、`WindowFuncDesc` 结构体、`ByItems` | `crates/expression/src/aggregation/`、`byitem.rs` |
| D08.12 表达式工具 | `ColumnSubstitute`、`Contains`、`ExprsHasSideEffects`、`ExprHasSetVarOrSleep`、`ParamMarkerExpression`、`GetIntFromConstant`、`GetStringFromConstant` | `crates/expression/src/util.rs` |
| D08.3 下推转换 | `ExpressionsToPBList`（供 `FrameBound.ToPB`、`GroupingSet.ToPB` / `GroupingSets.ToPB`） | `crates/expr-pushdown/src/expr_to_pb.rs` |
| D09.0 / D09.1 / D09.2 | `PlanBuilder`、`SelectFieldW`、`ByAddr`、`detectSelectAgg`、`detectSelectWindow`、`splitWhere`、`getUintFromNode`、`resolveFromSelectFields`、`getInnerFromParenthesesAndUnaryPlus`、`itemTransformer`、`rewrite` / `rewriteWithPreprocess`、`evalAstExprWithPlanCtx`、`ResolveContext` / `TableNameW` | `crates/planner/src/core/` |
| D09.13 / D09.14 | `LogicalPlan` trait、`BaseLogicalPlan`、`LogicalSchemaProducer`、`LogicalProjection`（已有 `proj4_expand` 字段）、`LogicalSelection`、`LogicalSort`、`pruneByItems`、`ResolveExprAndReplace`、`ResolveColumnAndReplace`、`logicaltrace`、规则标志常量（`FlagBuildKeyInfo`、`FlagPushDownAgg`、`FlagMaxMinEliminate`、`FlagPushDownTopN`、`FlagPredicatePushDown`、`FlagEliminateAgg`、`FlagEliminateProjection`、`FlagSkewDistinctAgg`、`FlagResolveExpand`） | `crates/planner/src/logicalop/`、`core/`、`util/` |
| D10.2 / D10.10 / D13.3 | `compareBool`、`GetMaxSortPrefix`、`SortItem`、`MPPPartitionColumn`、`GetCollateIDByNameForPartition`、`StatsInfo::get_group_ndv4_cols`、`PlanHints.prefer_agg_type` / `prefer_agg_to_cop` | `crates/planner/src/core/find_best_task.rs`、`property/`、`util/`、`hint/` |
| D00.1 / D02.1 / D05.2 | 全部 31 个 `plannererrors` 常量（已逐个核对 0 缺失）；parser 已解析 `AggregateFuncExpr`（含 `GROUP_CONCAT ... ORDER BY`）、`WindowFuncExpr`（保留原始函数名大小写）、`WindowSpec`、`FrameClause` / `FrameBound`、`GroupByClause.rollup`；`SessionVars` 已有 `enable_skew_distinct_agg`、`enable_pipelined_window_exec`、`optimizer_enable_new_only_full_group_by_check`、`alloc_plan_column_id`；`tipb` 已在 workspace 中 | `crates/base`、`crates/parser`、`crates/sessionctx` |

### 2.2 未移植（前置条件、一起移植的辅助函数，或 `blocked-by` 边界）

| 依赖 | 所属块 | 用在哪里 | 处理 |
|---|---|---|---|
| `NewWindowFuncDesc`、`UseDefaultFrame`、`NeedFrame`、`WindowFuncDesc.Clone` | D08.11（第 C 部分） | 第 B 部分 `checkWindowFuncArgs`、`buildWindowFunctions`、`handleDefaultFrame` | **第 B 部分前置条件**（`crates/expression/src/aggregation/window_func.rs` 目前只有类型定义，尚无方法） |
| `SortedExplainExpressionList`、`ExplainAggFunc` | D10.9（P1-1）/ D08.11（第 D 部分） | `LogicalAggregation.ExplainInfo`（`logical_aggregation.go:128,133`） | 未移植前 `LogicalAggregation.explain_info` 返回 `blocked-by:D10.9`（与现有 `LogicalSelection.explain_info` 一致） |
| `pkg/planner/funcdep`（`FDSet`）与 `util.IsNullRejected` | D09.14 / D10.10 | `LogicalAggregation.ExtractFD`、`LogicalExpand.ExtractFD` | 与 `DataSource` / `LogicalSelection` / `LogicalProjection` 现有写法一致，暂作 `blocked-by:D09.14` 桩（默认 `tidb_enable_new_only_full_group_by_check = OFF`，不会调用） |
| `BuildKeySolver` 与 `DataSource` / `LogicalSelection` 的 `BuildKeyInfo` | D09.13 / D09.14 | `buildAggregation` / `buildDistinct` 设置 `FlagBuildKeyInfo`，端到端优化时触发 `BuildKeySolver` | `BuildKeySolver` 在 `optimizer.rs:112` 仍为 `UnportedRule`（`blocked-by:D09.13`）；规划器构建测试不经优化器，端到端聚合链须解除它（§6.4） |
| `LogicalAggregation.PruneColumns` / `PredicatePushDown` / `DeriveStats` / `ExtractColGroups` / `PreparePossibleProperties` / `ExtractCorrelatedCols` | D09.7 / D09.8 / D10.6 / D10.2 / D09.6 | `LogicalAggregation` 进入 `DoOptimize` 后立即被调用（D09.9 也直接调用 `PruneColumns`） | 依赖均已就绪，随第 A 部分一并移植到 `logical_aggregation.rs` 并计入各自块（§3.6、§9.2）；`ExhaustPhysicalPlans` 留作 `blocked-by:D10.3` 桩 |
| `LogicalWindow` / `LogicalExpand` 上同名的规则接口方法（`PruneColumns`、`PredicatePushDown`、`DeriveStats`、`ExtractColGroups`、`PreparePossibleProperties`、`ExtractCorrelatedCols`） | D09.7 / D09.8 / D10.6 / D10.2 / D09.6 | 分别在第 B / C 部分进入优化器时被调用 | 同上，依赖均已就绪，随第 B / C 部分一并移植并计入各自块；`ExhaustPhysicalPlans` 为 `blocked-by:D10.3` 桩 |
| `AggregateFuncExtractor`、`WindowFuncExtractor`（`pkg/planner/core/util.go`） | D09.0 | 第 A 部分 `extractAggFuncsIn*` / `gbyResolver`；第 B 部分 `extractWindowFuncs` | 无出边小工具（各 2 个方法），按宪章第四条 5 随第 A / B 部分移植并计入 D09.0 |
| `extractSingeValueColNamesFromWhere`、`colNameResolver.Enter/Leave`、`allColFromExprNode`、`allColFromAggExprNode`、`getAllByItems`、`restoreByItemText`、`compareItems` | D09.1（tracker 归属，当前在 `logical_plan_builder.rs` 标为 `blocked-by:D09.4` 桩） | 第 A 部分 `ONLY_FULL_GROUP_BY`、第 B 部分窗口排序、第 D 部分相关聚合 | 随对应部分移植并计入 D09.1（消除现有 `blocked-by:D09.4` 桩） |
| `tblInfoFromCol`、`ExtractTableList` | D09.15 | 第 A 部分 `checkOnlyFullGroupByWithGroupClause` / `WithOutGroupClause` | 随第 A 部分移植其读路径（单表、派生表、join 递归，计入 D09.15）；`PlanBuilder` 补存 `resolve_ctx` |
| `ast.ExpressionDeepEqual`、`WindowSpec` / `Expr` 的 flags=0 `Restore` | D02.1 / D02.2 | 第 A 部分 `checkExprInGroupByOrIsSingleValue:3787`；第 B 部分 `restoreByItemText:7160`、`specEqual:7450` | 随第 A / B 部分在 `tidb-parser` 补齐（计入 D02.1 / D02.2） |
| `Expr::Position` 的参数标记形式（`GROUP BY ?`、`GROUP_CONCAT(... ORDER BY ?)`） | D02.2 | 第 A 部分 `gbyResolver.Enter`、`aggOrderByResolver.Enter`、`ConstructPositionExpr`、`PosFromPositionExpr` | Rust AST 位置节点仅有字面整数（`Expr::Position(i64)`），参数标记形式与现有 `ORDER BY ?` 一致暂停在 `blocked-by:D02.2` |
| `findJoinFullSchema`、`buildJoinFuncDepend`、`LogicalJoin` / `LogicalApply` | D09.3 | 第 A 部分 `buildAggregation:381`、`resolveGbyExprs:4115`、`checkOnlyFullGroupBy*` | 单表无 join 时返回空/无依赖；遇到 `Join` 节点时 `buildJoin` 本身先停在 `blocked-by:D09.3`；待 D09.3 合并后接通 |
| `DATE_ADD` / `DATE_SUB`（`addSubDateFunctionClass`） | D08.7（时间函数） | 第 B 部分 `buildWindowFunctionFrameBound:7071-7076`（`RANGE INTERVAL ...` 帧边界） | 内置函数注册表返回 `blocked-by:D08.7` |
| `BuiltinGroupingImplSig`（`GROUPING()` 函数类） | D08.9（杂项函数） | 第 C 部分 `expression_rewriter.rs` 改写 `GROUPING(...)` | 第 C 部分解除 `expression_rewriter.rs:1151` 的 `blocked-by:D09.4` 桩，对 `GROUPING()` 在创建 `BuiltinGroupingImplSig` 处转为 `blocked-by:D08.9` |
| `ResolveExpand` 规则（`rule_resolve_grouping_expand.go`，3 个函数） | D09.13（当前 `optimizer.rs:184` 为 `blocked-by:D09.4` 桩） | 第 C 部分 ROLLUP 展开在逻辑优化最后生成 `LevelExprs` | 随第 C 部分一并移植并计入 D09.13，消除 `optimizer.rs` 桩 |
| 子查询构建、`outerSchemas` / `outerNames` / `correlatedAggMapper` | D09.6 | 第 D 部分相关聚合（`resolveCorrelatedAggregates` 等 10 个函数） | **第 D 部分前置条件**；第 A–C 部分期间保持现有 `resolve_correlated_aggregates` 的 `blocked-by:D09.6` 桩 |
| 表级聚合 hint 常量 `PreferHashAgg` / `PreferStreamAgg`（`pkg/util/hint/hint.go:195-197`） | D13.3 | `buildAggregation:281-284`、`buildDistinct:1971-1974`、`LogicalAggregation.ResetHintIfConflicted:601-610` | 仅 2 个位常量，按宪章第四条 5 补到 `crates/planner/src/hint/hint.rs`（`PlanHints` 已有 `prefer_agg_type` / `prefer_agg_to_cop` 字段）；无表 hint 时 `TableHints()` 仍为空 |
| `intset.FastIntSet` | D00.3 | `grouping_sets.go`（P1-1）、`LogicalExpand`、`LogicalAggregation.ExtractFD` | 照 D08.12 与 D09.9 spec §4.6 先例，用 `BTreeSet<i64>` 替代（L2，有序遍历保证确定性） |
| `plancodec.TypeAgg` / `TypeWindow` / `TypeExpand` | D14.2 | 三个算子的 `Init` | 照 `crates/planner/src/core/initialize.rs:40` 现有做法，写对应字符串常量 `"Aggregation"`、`"Window"`、`"Expand"` |

## 3. 函数清单与划分

tracker `data/func_blocks.tsv` 中 block 为 `D09.4` 的函数共 **116** 个，全部属于只读 SELECT 计划构建与逻辑算子。**归 X 为 0 个**，**豁免（EXC.B / B3，仅旧 cascades 优化器调用）7 个**，其余 109 个按依赖与 sysbench 优先级划分为：已移植（VS1-21）+ A / B / C / D 四部分。

| 类别 | D09.4 函数数 | P1 移交函数数 | 内容 | 前置条件 |
|---|--:|--:|---|---|
| 已移植（VS1-21 完成） | 6 | 0 | `havingWindowAndOrderbyExprResolver`（除 `Leave`）、`resolveHavingAndOrderBy`、`colResolverForOnlyFullGroupBy.Enter` | 已合并（#284） |
| 第 A 部分：聚合、DISTINCT、GROUP BY、`ONLY_FULL_GROUP_BY`、`LogicalAggregation` | 43 | 2（P1-2 位置项） | sysbench `SUM(k)` 与 `DISTINCT c ... ORDER BY c` 所需全部构建与逻辑算子 | 无未满足的前置块（D08.11 A+B 已合并） |
| 第 B 部分：窗口函数与 `LogicalWindow` | 35 | 2（P1-2 参数检查） | `OVER` / `WINDOW`、`WindowFrame` / `FrameBound`、`LogicalWindow` | D08.11 第 C 部分（`window_func.go`） |
| 第 C 部分：ROLLUP 与 `LogicalExpand` | 15 | 29（P1-1 `grouping_sets.go`） | `GROUP BY ... WITH ROLLUP`、`LogicalExpand`、`GroupingSets` | 第 A 部分 |
| 第 D 部分：子查询相关聚合 | 10 | 0 | 子查询中引用外层表的聚合提取与回填 | 第 A 部分、D09.6（子查询） |
| 豁免（EXC.B / B3） | 7 | 0 | 仅旧 cascades 优化器调用的 7 个算子方法 | 不移植 |
| 归 X | 0 | 0 | 无写路径或测试专用钩子 | — |
| **合计** | **116** | **33** | `6 + 43 + 35 + 15 + 10 + 7 = 116`（P1 移交 `2 + 2 + 29 = 33`） | — |

### 3.1 已移植（VS1-21 完成，6 个）

均已在 PR #284 移植于 `crates/planner/src/core/logical_plan_builder.rs`，不再需要本块改写（其内残留的 join / 子查询边界归 D09.3 / D09.6）：

- `havingWindowAndOrderbyExprResolver.pushCurClause`（`:2643`，L1）
- `havingWindowAndOrderbyExprResolver.popCurClause`（`:2648`，L1）
- `havingWindowAndOrderbyExprResolver.Enter`（`:2654`，L1）
- `havingWindowAndOrderbyExprResolver.resolveFromPlan`（`:2679`，L2：join / apply 为 `blocked-by:D09.3`）
- `PlanBuilder.resolveHavingAndOrderBy`（`:2876`，L2：ORDER BY 子查询为 `blocked-by:D09.6`）
- `colResolverForOnlyFullGroupBy.Enter`（`:4023`，L1）

> 注：tracker `data/part_funcs.tsv` 中 `VS1-21` 登记的 6 个函数是前 5 个加上 `havingWindowAndOrderbyExprResolver.Leave`；#284 实际同时移植了 `colResolverForOnlyFullGroupBy.Enter`、`checkOnlyFullGroupBy` 与 `checkOnlyFullGroupByWithOutGroupClause` 的骨架（见 D09.1 spec §2.2 及 `govec/domains/selectbuild.go:95-100`）。因 `Leave`（聚合内列替换）、`checkOnlyFullGroupBy`（GROUP BY 支）、`checkOnlyFullGroupByWithOutGroupClause`（混合聚合函数依赖支）和 `colResolverForOnlyFullGroupBy.Leave`（补显式 `// go:` 标注）都需在第 A 部分补全，故统一计入第 A 部分，不重复计数。

### 3.2 第 A 部分：聚合、DISTINCT、GROUP BY、`ONLY_FULL_GROUP_BY` 与 `LogicalAggregation`（43 个 + P1-2 2 个）

sysbench 聚合链的直接前置。

**`logical_plan_builder.go`（22 个）**：

| 分组 | Go 函数（行号） | 说明 |
|---|---|---|
| 聚合与 DISTINCT 构建（6） | `aggOrderByResolver.Enter`（`:101`）、`aggOrderByResolver.Leave`（`:115`）、`PlanBuilder.buildAggregation`（`:249`）、`PlanBuilder.buildDistinct`（`:1960`）、`PlanBuilder.checkOrderByInDistinct`（`:2441`）、`havingWindowAndOrderbyExprResolver.Leave`（`:2757`） | `Leave` 补全 `inAggFunc` 列替换；`buildAggregation` 中 `rollupExpand` 分支待第 C 部分、`correlatedAggMap` 待第 D 部分、`findJoinFullSchema` 待 D09.3 |
| 聚合提取（3） | `PlanBuilder.extractAggFuncsInExprs`（`:3083`）、`PlanBuilder.extractAggFuncsInSelectFields`（`:3097`）、`PlanBuilder.extractAggFuncsInByItems`（`:3112`） | 基于 `AggregateFuncExtractor` |
| GROUP BY 解析与改写（4） | `gbyResolver.Enter`（`:3456`）、`gbyResolver.Leave`（`:3478`）、`PlanBuilder.resolveGbyExprs`（`:4107`）、`PlanBuilder.rewriteGbyExprs`（`:4148`） | `GROUP BY ?` 停在 `blocked-by:D02.2`；`GROUP BY ... WITH ROLLUP` 在 `buildSelect:4567` 停在 `blocked-by:D09.4`（第 C 部分解除） |
| `ONLY_FULL_GROUP_BY`（9） | `buildFuncDependCol`（`:3565`）、`buildWhereFuncDepend`（`:3598`）、`checkColFuncDepend`（`:3651`）、`checkExprInGroupByOrIsSingleValue`（`:3766`）、`PlanBuilder.checkOnlyFullGroupBy`（`:3808`）、`addGbyOrSingleValueColName`（`:3817`）、`PlanBuilder.checkOnlyFullGroupByWithGroupClause`（`:3858`）、`PlanBuilder.checkOnlyFullGroupByWithOutGroupClause`（`:3947`）、`colResolverForOnlyFullGroupBy.Leave`（`:4047`） | 解除 VS1-21 中关于 `ONLY_FULL_GROUP_BY` 的全部 `blocked-by:D09.4` 桩；join 依赖（`buildJoinFuncDepend`）单表时返回空 |

**`operator/logicalop/logical_aggregation.go`（21 个）**：

- `LogicalAggregation.Init`（`:60`）、`ExplainInfo`（`:124`，D10.9 P1-1 前桩）、`ReplaceExprColumns`（`:143`）、`BuildKeyInfo`（`:257`）、`CanPushToCop`（`:381`）、`ExtractFD`（`:397`，D09.14 `funcdep` 前桩）、`HasDistinct`（`:515`）、`HasOrderBy`（`:525`）、`IsPartialModeAgg`（`:546`）、`GetGroupByCols`（`:559`）、`GetPotentialPartitionKeys`（`:570`）、`GetUsedCols`（`:584`）、`ResetHintIfConflicted`（`:601`）、`DistinctArgsMeetsProperty`（`:613`）、`pushDownCNFPredicatesForAggregation`（`:631`）、`pushDownDNFPredicatesForAggregation`（`:656`）、`splitCondForAggregation`（`:685`）、`pushDownPredicatesForAggregation`（`:707`）、`BuildSelfKeyInfo`（`:739`）、`CanPullUp`（`:757`）、`getGroupNDVs`（`:775`）。

**P1-2（D08.12，`pkg/expression/util.go`，2 个）**：`ConstructPositionExpr`（`:1306`）、`PosFromPositionExpr`（`:1311`）。

### 3.3 第 B 部分：窗口函数与 `LogicalWindow`（35 个 + P1-2 2 个）

前置：D08.11 第 C 部分（`window_func.go`）。

**`logical_plan_builder.go`（21 个）**：

- `PlanBuilder.resolveWindowFunction`（`:3154`）、`getWindowName`（`:6866`）、`PlanBuilder.buildProjectionForWindow`（`:6875`）、`PlanBuilder.buildArgs4WindowFunc`（`:6934`）、`PlanBuilder.buildByItemsForWindow`（`:6962`）、`PlanBuilder.buildWindowFunctionFrameBound`（`:7009`）、`PlanBuilder.buildWindowFunctionFrame`（`:7102`）、`PlanBuilder.checkWindowFuncArgs`（`:7117`）、`sortWindowSpecs`（`:7190`）、`PlanBuilder.buildWindowFunctions`（`:7205`）、`PlanBuilder.checkOriginWindowFuncs`（`:7287`）、`PlanBuilder.checkOriginWindowSpec`（`:7310`）、`PlanBuilder.checkOriginWindowFrameBound`（`:7342`）、`extractWindowFuncs`（`:7376`）、`PlanBuilder.handleDefaultFrame`（`:7385`）、`appendIfAbsentWindowSpec`（`:7436`）、`specEqual`（`:7445`）、`PlanBuilder.groupWindowFuncs`（`:7463`）、`resolveWindowSpec`（`:7536`）、`mergeWindowSpec`（`:7556`）、`buildWindowSpecs`（`:7574`）。

**`operator/logicalop/logical_window.go`（14 个）**：

- `WindowFrame.Clone`（`:50`）、`FrameBound.Clone`（`:80`）、`FrameBound.UpdateCmpFuncsAndCmpDataType`（`:94`）、`FrameBound.ToPB`（`:122`）、`FrameBound.UpdateCompareCols`（`:144`）、`LogicalWindow.Init`（`:168`）、`ReplaceExprColumns`（`:176`）、`GetPartitionBy`（`:362`）、`GetWindowResultColumns`（`:429`）、`GetPartitionKeys`（`:434`）、`CheckComparisonForTiFlash`（`:447`）、`extractUsedCols`（`:461`）、`GetPartitionByCols`（`:477`）、`GetGroupNDVs`（`:486`）。

**P1-2（D08.12，`pkg/expression/util.go`，2 个）**：`ParamMarkerInPrepareChecker.Enter`（`:1275`）、`ParamMarkerInPrepareChecker.Leave`（`:1285`）。

### 3.4 第 C 部分：`GROUP BY ... WITH ROLLUP` 与 `LogicalExpand`（15 个 + P1-1 29 个）

前置：第 A 部分。

**`logical_plan_builder.go`（5 个）**：`PlanBuilder.buildExpand`（`:138`）、`buildExpandFieldName`（`:1504`）、`resolveGroupingTraverseAction.Transform`（`:1677`）、`PlanBuilder.replaceGroupingFunc`（`:1711`）、`PlanBuilder.implicitProjectGroupingSetCols`（`:1721`）。

**`operator/logicalop/logical_expand.go`（10 个）**：`LogicalExpand.Init`（`:68`）、`ExtractFD`（`:178`，D09.14 `funcdep` 前桩）、`GetUsedCols`（`:190`）、`GenLevelProjections`（`:199`）、`GenerateGroupingMarks`（`:254`）、`TrySubstituteExprWithGroupingSetCol`（`:298`）、`ResolveGroupingFuncArgsInGroupBy`（`:312`）、`GenerateGroupingIDModeBitAnd`（`:357`）、`GenerateGroupingIDIncrementModeNumericSet`（`:383`）、`BuildKeyInfo`（`:397`）。

**P1-1（D08.2，`pkg/expression/grouping_sets.go`，全部 29 个）**：`NewGroupingSets`、`GroupingSets.Merge`、`GroupingSets.MergeOne`、`GroupingSets.TargetOne`、`GroupingSets.NeedCloneColumn`、`GroupingSet.IsEmpty`、`GroupingSet.AllColIDs`、`GroupingSet.ExtractCols`、`GroupingSet.Clone`、`GroupingSet.StringWithCtx`、`GroupingSet.MemoryUsage`、`GroupingSet.ToPB`、`GroupingSets.IsEmpty`、`GroupingSets.AllSetsColIDs`、`GroupingSets.StringWithCtx`、`GroupingSets.ToPB`、`newGroupingSet`、`GroupingExprs.IsEmpty`、`GroupingExprs.SubSetOf`、`GroupingExprs.IDSet`、`GroupingExprs.Clone`、`GroupingExprs.StringWithCtx`、`GroupingExprs.MemoryUsage`、`RollupGroupingSets`、`AdjustNullabilityFromGroupingSets`、`DeduplicateGbyExpression`、`RestoreGbyExpression`、`GroupingSets.DistinctSize`、`GroupingSets.DistinctSizeWithThreshold`。

### 3.5 第 D 部分：子查询中的相关聚合（10 个，前置 D09.6）

**`logical_plan_builder.go`（10 个）**：`PlanBuilder.extractCorrelatedAggFuncs`（`:3122`）、`correlatedAggregateResolver.Enter`（`:3214`）、`resolveSelect`（`:3234`）、`collectFromTableRefs`（`:3309`）、`collectFromSelectFields`（`:3328`）、`collectFromGroupBy`（`:3339`）、`collectFromWhere`（`:3353`）、`Leave`（`:3369`）、`PlanBuilder.resolveCorrelatedAggregates`（`:3383`）、`aggColNameResolver.Enter`（`:4055`）。

理由：这 10 个函数仅在 SELECT / HAVING / ORDER BY 含子查询时运行（`aggColNameResolver.Enter` 仅被 `allColFromAggExprNode` 调用，后者仅在 `resolveCorrelatedAggregates:3417` 调用），依赖 D09.6 的子查询上下文（`outerSchemas`、`outerNames`、`correlatedAggMapper`）。

### 3.6 豁免（EXC.B / B3：仅旧 cascades 优化器调用，7 个）

经全仓库 `pkg/` 检索，以下 7 个函数在非测试代码中**只**被 `pkg/planner/cascades/` 调用，按已确认的例外 B（EXC.B / B3，与 D09.14、D08.11 一致）不移植，在对应 Rust 文件中标注 `// go-exception: EXC.B`：

1. `LogicalAggregation.Hash64`（`logical_aggregation.go:68`）：实现 cascades `base.HashEquals`。
2. `LogicalAggregation.Equals`（`logical_aggregation.go:87`）：实现 cascades `base.HashEquals`。
3. `LogicalAggregation.CopyAggHints`（`logical_aggregation.go:535`）：仅 `pkg/planner/cascades/transformation_rules.go:469,475,2081,2425` 调用。
4. `LogicalAggregation.IsCompleteModeAgg`（`logical_aggregation.go:552`）：仅 `pkg/planner/cascades/transformation_rules.go:1585,2059,2180,2338` 调用。
5. `LogicalWindow.EqualPartitionBy`（`logical_window.go:367`）：仅 `pkg/planner/cascades/transformation_rules.go:2553` 调用。
6. `LogicalWindow.EqualOrderBy`（`logical_window.go:384`）：仅 `pkg/planner/cascades/transformation_rules.go:2554` 调用。
7. `LogicalWindow.EqualFrame`（`logical_window.go:398`）：仅 `pkg/planner/cascades/transformation_rules.go:2555` 调用。

### 3.7 随本块一并移植的其他块函数（计入各自块）

以下函数属于其他块，但其依赖已就绪且被本块直接调用（或在 `DoOptimize` 对本块算子运行既有规则时立即可达），按宪章第四条 5 / 第九条 2 随对应部分移植并计入各自块：

| 随哪部分 | Go 函数（文件） | 计入块 | 原因 |
|---|---|---|---|
| 第 A 部分 | `AggregateFuncExtractor.Enter` / `Leave`（`core/util.go:54,63`） | D09.0 | 无出边，`extractAggFuncsIn*` 与 `gbyResolver` 直接调用 |
| 第 A 部分 | `extractSingeValueColNamesFromWhere`（`:3825`）、`colNameResolver.Enter/Leave`（`:4077,4085`）、`allColFromExprNode`（`:4095`） | D09.1 | tracker 归在 D09.1，目前在 `logical_plan_builder.rs` 为 `blocked-by:D09.4` 桩 |
| 第 A 部分 | `PlanBuilder.tblInfoFromCol`（`:3541`）、`ExtractTableList`（`:7632`，读路径） | D09.15 | `checkOnlyFullGroupBy*` 查找表主键 / 唯一键必需 |
| 第 A 部分 | `ExpressionDeepEqual`（`pkg/parser/ast/expressions.go:1512`） | D02.1 | `checkExprInGroupByOrIsSingleValue:3787` 比较 `GROUP BY` 复杂表达式 |
| 第 A 部分 | `LogicalAggregation` 的 `PruneColumns`（`:171`）、`PredicatePushDown`（`:164`）、`DeriveStats`（`:282`）、`ExtractColGroups`（`:309`）、`PreparePossibleProperties`（`:327`）、`ExtractCorrelatedCols`（`:354`） | D09.7 / D09.8 / D10.6 / D10.2 / D09.6 | `LogicalAggregation` 建成后 `DoOptimize` 立即调用它们，且 D09.9 需要 `PruneColumns`；全部依赖已就绪 |
| 第 B 部分 | `WindowFuncExtractor.Enter` / `Leave`（`core/util.go:82,91`）、`getAllByItems`（`:7147`）、`restoreByItemText`（`:7158`）、`compareItems`（`:7168`） | D09.0 / D09.1 | 后 3 个目前在 `logical_plan_builder.rs` 为 `blocked-by:D09.4` 桩 |
| 第 B 部分 | `LogicalWindow` 的 `PruneColumns`（`:215`）、`PredicatePushDown`（`:197`）、`DeriveStats`（`:261`）、`ExtractColGroups`（`:285`）、`PreparePossibleProperties`（`:302`）、`ExtractCorrelatedCols`（`:319`） | D09.7 / D09.8 / D10.6 / D10.2 / D09.6 | 与 `LogicalAggregation` 同理 |
| 第 C 部分 | `ResolveExpand.Optimize` / `Name` / `genExpand`（`core/rule_resolve_grouping_expand.go:80,89,93`）；`LogicalExpand` 的 `PruneColumns`（`:95`）、`PredicatePushDown`（`:78`）、`ExtractCorrelatedCols`（`:146`） | D09.13 / D09.7 / D09.8 / D09.6 | 消除 `optimizer.rs:184` 的 `blocked-by:D09.4` 桩 |
| 第 D 部分 | `allColFromAggExprNode`（`:4062`） | D09.1 | 目前为 `blocked-by:D09.4` 桩，仅 `resolveCorrelatedAggregates` 调用 |

## 4. Go → Rust 的结构映射

### 4.1 文件与类型

| Go 文件 / 类型 | Rust 文件 / 类型 | 说明 |
|---|---|---|
| `pkg/planner/core/operator/logicalop/logical_aggregation.go`（`LogicalAggregation`） | `crates/planner/src/logicalop/logical_aggregation.rs` | `LogicalKind::Aggregation`；嵌入 `base: BaseLogicalPlan`、`sp: LogicalSchemaProducer`；`agg_funcs: Vec<AggFuncDesc>`（按值，沿用已确认的 D08.11 决定点 4 与 D09.9 决定点 5）、`group_by_items: Vec<ExprRef>`、`prefer_agg_type: u64`、`prefer_agg_to_cop: bool`、`possible_properties: Vec<Vec<Arc<Column>>>`、`input_count: f64`、`no_cop_push_down: bool` |
| `pkg/planner/core/operator/logicalop/logical_window.go`（`LogicalWindow`、`WindowFrame`、`FrameBound`） | `crates/planner/src/logicalop/logical_window.rs` | `LogicalKind::Window`；嵌入 `base`、`sp`；`window_func_descs: Vec<WindowFuncDesc>`（按值）、`partition_by: Vec<SortItem>`、`order_by: Vec<SortItem>`、`frame: Option<WindowFrame>`。`FrameBound.cmp_funcs: Vec<Option<CompareFunc>>`、`cmp_data_type: tipb::RangeCmpDataType`、`calc_funcs: Vec<ExprRef>`、`compare_cols: Vec<ExprRef>`；`FrameBound.ToPB` 在 `tidb-planner` 中直接调 `tidb_expr_pushdown::expressions_to_pb_list`（`tidb-planner` 已依赖 `tidb-expr-pushdown` 与 `tipb`） |
| `pkg/planner/core/operator/logicalop/logical_expand.go`（`LogicalExpand`） | `crates/planner/src/logicalop/logical_expand.rs` | `LogicalKind::Expand`；嵌入 `base`、`sp`；`distinct_group_by_col: Vec<Arc<Column>>`、`distinct_gby_col_names: NameSlice`、`distinct_gby_exprs: Vec<ExprRef>`、`distinct_size: isize`、`rollup_grouping_sets: GroupingSets`、`rollup_id2_gids: BTreeMap<isize, BTreeSet<u64>>`、`rollup_grouping_ids: Vec<u64>`、`level_exprs: Vec<Vec<ExprRef>>`、`extra_grouping_col_names: Vec<String>`、`grouping_mode: tipb::GroupingMode`、`gid: Option<Arc<Column>>`、`gid_name: Option<Arc<FieldName>>`、`gpos: Option<Arc<Column>>`、`gpos_name: Option<Arc<FieldName>>` |
| `pkg/planner/core/logical_plan_builder.go` | `crates/planner/src/core/logical_plan_builder.rs`（或按子模块 `logical_plan_builder/` 拆分，保持文件头 `//! go-file: pkg/planner/core/logical_plan_builder.go`） | 在 `PlanBuilder<'ast>` 上增加：`resolve_ctx: Option<ContextRef>`（供 `tblInfoFromCol`）、`current_block_expand: Option<PlanId>`（第 C 部分，保存 arena 下标而非裸指针）、`outer_block_expand: Vec<Option<PlanId>>`（第 D 部分）、`window_specs: BTreeMap<String, WindowSpecW<'ast>>`（第 B 部分）、`correlated_agg_mapper: HashMap<ByAddr<'ast, ast::AggregateFuncExpr>, Arc<CorrelatedColumn>>`（第 D 部分） |
| `pkg/expression/grouping_sets.go` | `crates/expression/src/grouping_sets.rs`（27 个函数） + `crates/expr-pushdown/src/grouping_sets.rs`（`GroupingSet.ToPB`、`GroupingSets.ToPB` 2 个自由函数，按 N0 规则 6 与 D08.11 §4.8 相同） | `GroupingExprs(Vec<ExprRef>)`、`GroupingSet(Vec<GroupingExprs>)`、`GroupingSets(Vec<GroupingSet>)`；`intset.FastIntSet` 用 `BTreeSet<i64>`（L2） |
| `pkg/expression/util.go`（P1-2 4 个函数） | `crates/expression/src/util.rs` | `ParamMarkerInPrepareChecker`、`construct_position_expr`、`pos_from_position_expr` |

### 4.2 固定写法（N1 §4，映射规则 `M-PLAN` / `M-SELF` / `M-IFACE`）

1. **算子实现与嵌入**：三个算子均在 `impl LogicalPlan` 中用 `logical_plan_boilerplate!(<Kind>, producer);` 生成 `kind()`、`as_any()`、`base()`、`schema_producer()` 访问器。`LogicalKind` 新增 `Aggregation`、`Window`、`Expand` 三个变体。
2. **摘出分派**：`LogicalPlan` 的 `prune_columns`、`predicate_push_down`、`derive_stats`、`extract_col_groups`、`prepare_possible_properties`、`build_key_info` 等均沿用 `logical_dispatch!` 摘出分派。
   - **摘出期间不按 `this` 回查 arena**：`LogicalAggregation.PruneColumns` 调用 `logicaltrace::append_column_prune_trace_step`、`append_function_prune_trace_step`、`append_group_by_items_prune_trace_step` 及 `prune_by_items` 时传 `&self`（`&dyn LogicalPlan`），`LogicalExpand.PruneColumns` 同理。
   - **`PlanCtx::split()`**：本块所有函数均不需要 `split()`。
   - **重入检查**：本块任何规则方法均不会在单次分派中两次摘出同一个 `PlanId`。
3. **plan id 与列 id 分配顺序严格照 Go**：
   - `buildExpand`：先 `LogicalProjection::init`（`Proj4Expand`）→ 循环为每个 distinct 分组表达式 `alloc_plan_column_id` → `LogicalExpand::init` → `gid` `alloc_plan_column_id` → 若有重复分组集则 `gpos` `alloc_plan_column_id`（`logical_plan_builder.go:145,170,202,216,228`）。
   - `buildAggregation`：在改写参数循环**之前**调 `LogicalAggregation::init`（`:280`），等价去重失败时才为聚合函数分配列 id（`:354`）。
   - `buildDistinct`：`LogicalAggregation::init`（`:1970`，`offset` 用 `child.query_block_offset()` 而非 `b.get_select_offset()`！）。
   - `buildWindowFunctions`：对每个 `sortWindowSpecs` 产出的窗口，**即便 `len(funcs) == 0`（未使用的命名窗口）**也会先调 `buildProjectionForWindow`（分配一个 `LogicalProjection` 的 plan id 及非列排序项的列 id，随后被丢弃，`:7222-7234`），必须照搬以保持后续算子的 plan id 和列 id 与 Go 一致。
4. **`LogicalPlan` 的 `build_key_info` trait 方法**：目前 `crates/planner/src/base.rs` 尚未把 `build_key_info` 提升为 `LogicalPlan` trait 方法（各算子是固有方法）；本块可按现有风格为 `LogicalAggregation` / `LogicalExpand` 提供 `build_key_info` 方法，并在 `BuildKeySolver`（D09.13）接入时统一挂到 trait 分派。

### 4.3 不可变 AST（`&'ast ast::SelectStmt`）下 Go 就地改写 AST 的承载（L2）

Go 的 `PlanBuilder` 在构建期间有多处直接改写传入的 `*ast.SelectStmt` 并在结束时恢复。Rust 中语句 AST 为只读借用 `&'ast ast::SelectStmt`（D09.1 spec §3 第 2 条，且 `ByAddr<'ast, T>` 依赖 AST 节点地址稳定），本块按以下四处分别映射，**不修改 AST 且保持语义与 Go 100% 等价**：

1. **`havingWindowAndOrderbyExprResolver.Leave` 替换聚合参数内的列（`logical_plan_builder.go:2865-2867`）**：
   - Go：`if a.inAggFunc { return a.selectFields[index].Expr, true }`，原地把 `HAVING SUM(b)` 中的 `b` 换成 `a + 1`。
   - Rust：在 `PlanBuilder<'ast>` 上增加按 `ByAddr<'ast, ast::ColumnName>` 索引的**聚合内列表达式替换表** `agg_col_replacements: HashMap<ByAddr<'ast, ast::ColumnName>, usize>`（值为 `select_fields` 下标，或直接 `FieldExpr<'ast>` 的克隆/引用）。`buildAggregation` 改写 `aggFunc.Args` 时，对 `ExpressionRewriter` 在遇到 `AstExpr::Column` 且命中替换表时，直接递归改写被替换的表达式（注意：被替换后的表达式中若还有列，按 Go 逻辑直接从底层计划 `p` 解析，不再二次查 `agg_col_replacements`）。如此既不改 AST，又保持外层 `AggregateFuncExpr` 地址不变（`aggMapper` 的 `ByAddr` 键有效）。
2. **`gbyResolver` 替换别名与位置项并标记 `UseAsValueInGbyByClause`（`:3463, 3502, 3504, 3532, 4139-4143`）**：
   - Go：当 `!resolver.isParam` 时写回 `item.Expr = retExpr`（后续 `checkOnlyFullGroupByWithGroupClause` 读的是写回后的 `sel.GroupBy.Items`！），同时返回 `gbyExprs` 供 `rewriteGbyExprs`；若别名指向 `ParamMarkerExpr`，置 `isParam.UseAsValueInGbyByClause = true` 防止再次被当作位置项。
   - Rust：`resolve_gby_exprs` 返回两份视图：`gby_exprs: Vec<ResolvedExpr<'ast>>`（传给 `rewrite_gby_exprs`）和 `resolved_gby_items: Vec<ResolvedExpr<'ast>>`（即 `!is_param` 时替换、`is_param` 时保留原 `item.expr`，传给 `check_only_full_group_by_with_group_clause` 与 `havingWindowAndOrderbyExprResolver.gby_items`），同时在 `gbyResolver` 内用 `HashSet<ByAddr<'ast, ast::ParamMarkerExpr>>` 记 `use_as_value_in_gby`，完全复现 Go 行为而不改 AST。
3. **`aggOrderByResolver` 替换 `GROUP_CONCAT(... ORDER BY 1)` 位置项（`:108, 132-133`）**：
   - Rust：`agg_order_by_resolver` 不改 AST，直接返回替换后的 `&'ast ast::Expr`（位置命中时返回 `&true_args[pos - 1]`，否则原节点）交给 `b.rewrite`。
4. **窗口规范的合并与默认帧调整（`buildWindowSpecs`、`mergeWindowSpec`、`handleDefaultFrame`，`:7385-7591`）**：
   - Go：复制 `ast.WindowSpec` 值结构体并在副本上改写 `PartitionBy`、`OrderBy`、`Ref`、`Frame`（`mergeWindowSpec` 对未命名内联窗口 `OVER (w)` 直接改写 `windowFunc.Spec`，`:7470-7476`）。
   - Rust：定义构建器本地结构 `WindowSpecW<'ast>`，借用原始的 `name`、`ref_`、`partition_by: Option<&'ast PartitionByClause>`、`order_by: Option<&'ast OrderByClause>`，以及 `frame: Option<CowFrame<'ast>>`（借用原 `FrameClause` 或持有 `handleDefaultFrame` 新建的默认 `FrameClause`）。`groupedWindow` 与 `orderedSpec` 以 `WindowSpecId(usize)`（在本地 `Vec<WindowSpecW<'ast>>` 中的下标）代替 Go 的 `*ast.WindowSpec` 指针作为键。

### 4.4 错误、panic 与 `defer` 顺序

- **`defer` 恢复状态（`M-DEFER`）**：以下四处必须在正常返回与错误返回时都恢复 `b.check_col_priv`（用 RAII guard 或 `let res = ...; self.check_col_priv = origin; res`）：
  - `buildAggregation`（`:262-266`）
  - `resolveGbyExprs`（`:4126-4130`）
  - `checkWindowFuncArgs`（`:7118-7122`）
  - `buildWindowFunctions`（`:7209-7213`）
  - 以及第 D 部分 `extractCorrelatedAggFuncs`（`:3126-3130`）和 `correlatedAggregateResolver.resolveSelect`（`:3237-3239` 恢复 `outerCTEs`）。
- **错误忽略处对 `blocked` 的传递**：与 D09.9 spec §4.4 一致，Go 中忽略错误继续的地方，若错误是 `tidb_base::blocked::blocked_by(&e).is_some()` 必须向上返回、不得吞掉：
  - `addGbyOrSingleValueColName`（`:3818-3821`）、`colNameResolver.Leave`（`:4087-4090`）、`checkColFuncDepend`（`:3675, 3713`）、`tblInfoFromCol`（`:3555-3558`）、`LogicalAggregation.CanPullUp`（`:763-766`）、`restoreByItemText`（`:7161-7164`）、`LogicalAggregation.DeriveStats`（`:294`）。
- **报错顺序**：严格按 Go 的校验顺序：
  - `checkOriginWindowFuncs`：`IGNORE NULLS` → `DISTINCT` → `FROM LAST` → `checkOriginWindowSpec`（`GROUPS` → start `UNBOUNDED FOLLOWING` → end `UNBOUNDED PRECEDING` → start/end 反转 → start 边界 → end 边界）；
  - `buildSelect`：`resolveGbyExprs` → `checkOnlyFullGroupBy` → `rewriteGbyExprs` → `resolveWindowFunction` → `resolveHavingAndOrderBy` → `resolveCorrelatedAggregates` → `buildSelection(WHERE)` → `buildExpand` / `buildAggregation` → `buildProjection` → `buildSelection(HAVING)` → `buildWindowSpecs` → `checkWindowFuncArgs` → `groupWindowFuncs` → `buildWindowFunctions` → `buildDistinct` → `buildSortWithCheck`。
- **`LogicalAggregation.IsPartialModeAgg`**：照 Go 索引 `self.agg_funcs[0]`（调用方 `BuildKeyInfo:262` 先判非空）。

### 4.5 并发、内存记账（A1）与落盘

- **并发**：规划期同步运行在单会话 task 上，无 goroutine / channel / 锁（`M-GO`、`M-CH`、`M-LOCK` 不适用）。三个算子在语句级 arena 内独占，`LogicalWindow` 的 `FrameBound` 等会进入物理计划（后续 D10.1），需满足 `Send + Sync`。
- **内存记账（A1，`M-MEM`）**：规划器构建不对逻辑算子调用 `memory.Tracker`。`GroupingSet.MemoryUsage` / `GroupingExprs.MemoryUsage`（`grouping_sets.go:279, 406`）为计划缓存估算大小，照 `crates/expression/src/size.rs` 的 Go 64 位结构大小常量移植（L2，同 D08.11 spec §4.5）。
- **落盘**：不涉及 `blocked-by:D01.10`。

### 4.6 其他映射规则

- **`M-MAPORDER`**（共 4 处 Go 遍历 map 的位置）：
  1. `checkOnlyFullGroupByWithGroupClause`（`:3910`）：遍历 `notInGbyOrSingleValueColNames` 返回第一个不满足函数依赖的错误。Rust 改用保留首次插入顺序的列表（或 `IndexMap` / `Vec` 去重，同一列被 ORDER BY 覆盖时照 Go 更新 `ErrExprLoc`），取确定顺序；govec 对多列同时违规的用例按结构检查错误码或在生成器中构造单列违规。
  2. `buildWindowSpecs`（`:7584`）：遍历 `specsMap` 调 `resolveWindowSpec`。Rust 按 `sel.window_specs` 的声明顺序遍历（`BTreeMap` 查表 + 按输入切片顺序解析）。
  3. `groupWindowFuncs`（`:7523`）：遍历 `b.windowSpecs` 追加未使用的命名窗口。Rust 按 `sel.window_specs` 的声明顺序追加。
  4. `resolveCorrelatedAggregates`（`:3418`，第 D 部分）：遍历 `colMap` 追加外层辅助列。Rust 按 `p.output_names()` 中的列下标顺序追加，保证列顺序和列 id 确定。
- **`M-INT`**：`offset`、`pos`、`DistinctSize`、`idx` 等 Go `int` 统一为 `isize`（切片下标处 `as usize`）。`LogicalExpand.GenerateGroupingIDModeBitAnd`（`logical_expand.go:366-373`）和 `GenerateGroupingMarks`（`:272-282`）中的 `res = res << 1` 仅在 `GroupingMode_ModeBitAnd`（≤ 64 个 grouping sets，即最多 63 个 distinct 列）运行，不会溢出；写 `res = res.wrapping_shl(1)` 以防边界测试（见 §8 第 1 条）。
- **`M-NAME`**：`WindowFrame.Clone`、`FrameBound.Clone`、`GroupingSet.Clone`、`GroupingExprs.Clone` 按 D08.11 先例写为固有方法 `clone_frame`、`clone_bound`、`clone_set`、`clone_exprs`（同时 `#[derive(Clone)]`），`StringWithCtx` 为 `string_with_ctx`。
- **`M-FMA` / `M-F2I`**：tracker `data/go_fma_sites.tsv` 中本块 5 个文件均无位置；本块无浮点转整数。
- **`Restore` 的 flags=0 模式**：Go 的 `restoreByItemText`（`:7160`）与 `specEqual`（`:7450`）调用 `format.NewRestoreCtx(0, &sb)`，而 `crates/parser/src/restore.rs` 目前硬编码了 `DefaultRestoreFlags`（反引号 + 大写关键字 + 单引号字符串）。第 B 部分在 `crates/parser/src/restore.rs` 为 `Expr` / `WindowSpec` 增加 flags=0（无反引号、小写关键字、双引号字符串）模式，确保 `sortWindowSpecs` 排序与 `specEqual` 去重和 Go 完全一致。

### 4.7 共享修改的审计（N1 §5，映射规则 `M-FT` / `M-SHARE`）

对本块全部 4 个 Go 文件中属于 D09.4 的行范围，加上 P1 移交的 `pkg/expression/grouping_sets.go` 和 `pkg/expression/util.go`（4 个函数），运行 `specs/templates/plan.md` §4 的两个 grep 并读代码补充，逐处结论如下（S / V0 / V1 定义见 D09.7 试点报告附录 B）：

| Go 位置 | 写入 / 共享内容 | 结论 | 处理（含是否进入客户端列定义） |
|---|---|---|---|
| `logical_plan_builder.go:374, 392`（`buildAggregation`） | `newCol := col.Clone(); newCol.RetType = newFunc.RetTp` | **S** | 改刚 `Clone()` 的新列字段，之后共享 `Arc<FieldType>` |
| `logical_plan_builder.go:399, 403`（`buildAggregation`） | `aggFunc.UpdateNotNullFlag4RetType(...)`（内部先 `Clone` 再 `DelFlag`）后 `schema4Agg.Columns[i].RetType = aggFunc.RetTp` | **S** | 改新建 `schema4Agg` 的列指针；写时复制结果一致 |
| `logical_plan_builder.go:1983, 1988`（`buildDistinct`） | `plan4Agg.SetSchema(child.Schema().Clone())`；`col.RetType = plan4Agg.AggFuncs[i].RetTp` | **S** | 改 `Clone()` 后的新列指针，不改 `child` 的 `FieldType` |
| `logical_plan_builder.go:214`（`buildExpand`） | `tp := NewFieldType(TypeLonglong); tp.SetFlag(Unsigned \| NotNull)` | **S** | 刚新建的 `FieldType` |
| `logical_plan_builder.go:2751`（`resolveFromPlan`） | `sf.Expr.SetType(col.GetStaticType())` | **S** | 刚新建的 AST 节点，Rust AST 不存此字段 |
| **`pkg/expression/grouping_sets.go:506`（`AdjustNullabilityFromGroupingSets`），经 `logical_plan_builder.go:192-193`（`buildExpand`）调用** | `expandSchema := proj.Schema().Clone()` 后原地 `schema.Columns[idx].RetType.SetFlag(... &^ NotNullFlag)`。Go 中 `expandSchema`、`proj.Schema()`（`Proj4Expand`）、`distinctGbyCols`、`newGbyItems`（→ `LogicalAggregation.GroupByItems`）及 `rollupGroupingSets` 共享同一个 `*FieldType` 指针 | **V1**（**进入客户端列定义**！`replaceGroupingFunc` 把 `SELECT a ... GROUP BY a WITH ROLLUP` 中的 `a` 换成 `expand.DistinctGroupByCol[i]`，若不模拟，非空列 `a` 的 ROLLUP 结果在客户端列定义中仍标 `NOT NULL` 且 `GroupByItems` 类型与 Go 不同） | **按 `M-FT` 显式模拟**（`// Go aliasing (M-FT): grouping_sets.go:506, clears NotNullFlag on shared FieldType of Proj4Expand schema, DistinctGroupByCol, RollupGroupingSets and newGbyItems`）：在 `build_expand` 调完 `adjust_nullability_from_grouping_sets` 后，按 `UniqueID` 将被清除的 `NotNullFlag` 同步写回 `proj.schema_mut()`、`distinct_gby_cols`、`new_gby_items` 与 `rollup_grouping_sets`。配回归测试 `rollup_adjust_nullability_syncs_gby_cols` |
| `logical_plan_builder.go:7251, 7260`（`buildWindowFunctions` → `NewWindowFuncDesc`） | `schema := np.Schema().Clone()` 后，对 `first_value` / `last_value` / `nth_value`，`window_func.go:74,76` 原地 `DelFlag(NotNullFlag)` 会同时清掉 `schema` 中对应源列的 `NOT NULL` | **疑似 V1**（已登记于 D08.11 spec §5 及决定点 6，可能进入客户端列定义） | 第 B 部分随 D08.11 第 C 部分用计划 / 列定义向量确认；确认可见则在 `build_window_functions` 中按 `UniqueID` 同步清 `schema` 对应列的 `NotNullFlag`（`// Go aliasing (M-FT): window_func.go:76 & logical_plan_builder.go:7251`），配回归测试 |
| `logical_plan_builder.go:7268`（`buildWindowFunctions` → `WrapCastForAggArgs`） | 时间分支共享 `a.RetTp` 经 `WrapWithCastAsTime` 修改 | **V1**（已由 D08.11 spec §5 模拟处理） | 由 D08.11 内部完成，本块直接调用 |
| `logical_plan_builder.go:1699-1703`（`resolveGroupingTraverseAction.Transform`） | 原地改 `ScalarFunction.GetArgs()[i]` | **S**（`M-EXPR`） | 输入 `newExpr` 刚由 `rewrite` 建出，独占；Rust 经 `Arc::make_mut` 修改 |
| `logical_plan_builder.go:148, 240, 1984`（`OutputNames`） | `buildExpand` 往 `p.OutputNames()` 上 `append`；`buildDistinct` 共享 `child.OutputNames()` | **S** | `buildExpand` 先克隆 `Vec<Arc<FieldName>>` 再 `push`（避免 Go slice 底层数组别名）；`buildDistinct` 克隆 `NameSlice` |
| `logical_plan_builder.go:178, 239, 416, 1983, 6888, 7279`（`SetSchema`） | 均为新建或 `Clone()` 后的 `Schema` | **S** | 无未克隆 schema 共享 |

### 4.8 对其他块的要求

- **D08.11（第 C 部分）**：第 B 部分需要 `NewWindowFuncDesc`、`UseDefaultFrame`、`NeedFrame`、`WindowFuncDesc.Clone`；第 D 部分（`ExplainAggFunc`）与 D10.9 P1-1 合并后解除 `LogicalAggregation.ExplainInfo` 桩。
- **D09.9（聚合规则）**：第 A 部分交付 `LogicalAggregation`（含 `PruneColumns`、`PredicatePushDown`、`BuildKeyInfo`）和 `buildAggregation` / `buildDistinct` 后，D09.9 第 1 部分即可开工并替换 `optimizer.rs` 中的 4 个聚合规则桩。
- **D09.13（`BuildKeySolver`）**：`buildAggregation` 与 `buildDistinct` 都置 `FlagBuildKeyInfo`；端到端运行 sysbench 聚合语句前必须解除 `optimizer.rs:112` 的 `BuildKeySolver` 桩（以及 `DataSource` / `LogicalSelection` 的 `BuildKeyInfo`）。
- **D10.1 / D10.3（物理聚合算子与枚举）**：本块只建逻辑计划；物理计划阶段 `LogicalAggregation.exhaust_physical_plans` 仍返回 `blocked-by:D10.3`，待物理聚合（`PhysicalHashAgg` / `PhysicalStreamAgg`）和 D10.3 枚举合并后打通端到端。
- **D09.11（`DeriveTopNFromWindow`）**：第 B 部分引入 `LogicalWindow` 后，`crates/planner/src/logicalop/logical_selection.rs:191` 中 `LogicalSelection.DeriveTopN` 原先不可达的窗口分支即可接通。

## 5. 边界桩

### 5.1 本块消除的现有 `blocked-by:D09.4` 桩

按当前仓库 `grep -rn 'D09\.4' crates/` 与 tracker `data/stubs.tsv`：

- **第 A 部分消除**：`logical_plan_builder.rs` 中 `build_select_inner`（GROUP BY 支）、`build_select_rest`（`buildAggregation`、`buildDistinct`）、`check_only_full_group_by`、`check_only_full_group_by_with_out_group_clause`、`check_order_by_in_distinct`、`extract_singe_value_col_names_from_where`、`all_col_from_expr_node`（含 `colNameResolver`）。
- **第 B 部分消除**：`logical_plan_builder.rs` 中 `build_select_inner`（窗口支）、`compare_items`（含 `getAllByItems`、`restoreByItemText`）。
- **第 C 部分消除**：`expression_rewriter.rs:1151`（`GROUPING()` 从 `D09.4` 转为更精确的 `D08.9`）、`optimizer.rs:184-188`（`ResolveExpand`）。
- **第 D 部分消除**：`logical_plan_builder.rs` 中 `all_col_from_agg_expr_node`。
- **归其他块、本块不消除（但本块完成后这些桩应改标实际阻塞块）**：`optimizer.rs` 的 4 条聚合规则（实属 **D09.9**，现标 `D09.4`）、`rule_inject_extra_projection.rs:100` 的 `InjectProjBelowAgg`（实属 **D10.1**，现标 `D09.4`）。

### 5.2 本块保留或新增的边界桩

| Rust 位置 | Go 位置 | 类型 | 调用时行为 |
|---|---|---|---|
| `LogicalAggregation.explain_info` | `logical_aggregation.go:124-140` | 临时 `blocked-by:D10.9` | 返回 1235 阻塞错误（与 `LogicalSelection.explain_info` 一致），待 D10.9 P1-1 与 D08.11 第 D 部分解除 |
| `LogicalAggregation.extract_fd`、`LogicalExpand.extract_fd` | `logical_aggregation.go:397`、`logical_expand.go:178` | 临时 `blocked-by:D09.14` | 返回 1235（`pkg/planner/funcdep` 未移植，默认开关关闭时不可达；与 `DataSource` / `LogicalSelection` / `LogicalProjection` 一致） |
| `LogicalAggregation` / `LogicalWindow` / `LogicalExpand` 的 `exhaust_physical_plans` | `logical_aggregation.go:349`、`logical_window.go:314`、`logical_expand.go:141` | 临时 `blocked-by:D10.3` | 返回 1235，新栈物理优化前由路由器回退到 PoC；待 D10.3 物理枚举解除 |
| `gby_resolver.enter`、`agg_order_by_resolver.enter` 对顶层 `?`（`GROUP BY ?`、`GROUP_CONCAT(... ORDER BY ?)`） | `logical_plan_builder.go:103-111, 3461-3470` | 临时 `blocked-by:D02.2` | 返回 1235（Rust AST `Expr::Position` 尚无参数标记形式，与现有 `itemTransformer` 对 `ORDER BY ?` 一致） |
| `build_select_rest` 中 `rollup == true`（仅第 A–B 部分期间） | `logical_plan_builder.go:4567-4572` | 临时 `blocked-by:D09.4` | 返回 1235，第 C 部分解除 |
| `build_window_function_frame_bound` 的 `INTERVAL` 分支调 `date_add` / `date_sub` | `logical_plan_builder.go:7071-7079` | 临时 `blocked-by:D08.7`（由 `NewFunctionBase` 注册表直接返回，本块不另写桩） | 返回 1235，待 D08.7 解除 |
| `expression_rewriter.rs` 改写 `GROUPING(...)` 建 `BuiltinGroupingImplSig`（第 C 部分） | `expression_rewriter.go:2456-2461` | 临时 `blocked-by:D08.9` | 替换原 `blocked-by:D09.4` 桩；无 `GROUPING()` 的 ROLLUP 查询正常构建 |
| `resolve_correlated_aggregates`（第 A–C 部分期间） | `logical_plan_builder.go:3383-3440` | 临时 `blocked-by:D09.6`（已有） | 无子查询直接返回 `Ok(())`，有子查询返回 1235；第 D 部分随 D09.6 解除 |
| `build_aggregation` / `resolve_gby_exprs` / `check_only_full_group_by*` 中的 join 支 | `logical_plan_builder.go:381, 4115, 3905, 3982` | 依赖 D09.3（单表不可达，不另增桩；`buildJoin` 先停在 `blocked-by:D09.3`） | D09.3 合并后接通 `findJoinFullSchema` 与 `buildJoinFuncDepend` |

**棘轮说明**（`scripts/check_stub_ratchet.py`）：第 A 部分删掉 `logical_plan_builder.rs` 中 7 处 `blocked-by:D09.4` 桩，新增 `LogicalAggregation` 的 `explain_info`（D10.9）、`extract_fd`（D09.14）、`exhaust_physical_plans`（D10.3）与 `rollup`（D09.4，细化）、`GROUP BY ?` / `GROUP_CONCAT ORDER BY ?`（D02.2），属 port-block §3 允许的两种细化 / 新纳入算子情形；各部分 PR 正文逐条列出实际脚本计数。

## 6. 测试计划

### 6.1 要移植的 Go 单测

| Go 测试 | Go 位置 | 处理 |
|---|---|---|
| `TestGroupSetsTargetOne`、`TestGroupSetsTargetOneCompoundArgs`、`TestGroupingSetsMergeOneUnitTest`、`TestRollupGroupingSets`、`TestGroupingSetsMergeUnitTest`、`TestDistinctGroupingSets` | `pkg/expression/grouping_sets_test.go:29-313` | 不依赖存储，全部随第 C 部分移植到 `crates/expression/src/grouping_sets.rs`（或 `tests/`） |
| `TestGroupByWhenNotExistCols`（`:471`）、`TestWindowFunction`（`:3284`）、`TestWindowParallelFunction`（`:3307`）、`TestWindowLogicalPlanAmbiguous`（`:3817`）、`TestRollupExpand`（`:3876`）、`TestResolvingCorrelatedAggregate`（`:3701`） | `pkg/planner/core/logical_plans_test.go` | 依赖 `testkit`（模拟存储），将其中的单表 SQL 语句提取进入 govec 生成器，按部分做逻辑计划级逐项对比；多表 / 子查询语句分别等 D09.3 / D09.6 |
| `TestLogicalExpandBuildKeyInfo` | `pkg/planner/core/operator/logicalop/logicalop_test/logical_operator_test.go:152` | 含 join 与 mock store，改写成 `LogicalExpand::build_key_info` 单元测试（验证 `Keys` / `UniqueKeys` 清空）+ govec 单表 ROLLUP 用例 |

### 6.2 govec 向量

**扩展现有领域 `selectbuild`**（tracker `govec/domains/selectbuild.go`，已声明 `Blocks: ["D09.1", "D09.4", "D10.2", "D09.0", "D09.2"]` 且 `selectbuild_exempt.go` 已列全部未覆盖的 D09.4 函数）：

- **输出序列化扩展**：在 `selectbuild` 的计划节点快照中新增：
  - `LogicalAggregation`：`aggs`（名、参数、`distinct`、`mode`、`order_by`、`ret_tp`）、`gby`、`prefer_agg_type`、`prefer_agg_to_cop`、`schema`、`output_names`；
  - `LogicalWindow`（第 B 部分）：`funcs`（名、参数、`ret_tp`）、`partition_by`、`order_by`、`frame`（`tp`、`start`/`end` 含 `unbounded`/`num`/`calc_funcs`/`cmp_data_type`/`is_explicit_range`）、`schema`、`output_names`；
  - `LogicalExpand`（第 C 部分）：`distinct_gby_cols`、`distinct_gby_exprs`、`rollup_grouping_sets`、`grouping_mode`、`distinct_size`、`gid`/`gpos`、`schema`、`output_names`。
- **op 与覆盖（按部分解除 `selectbuild_exempt.go` 登记并加入 `Coverage`）**：
  - **第 A 部分**：
    - sysbench 聚合语句（`SELECT SUM(k) ... WHERE id BETWEEN ? AND ?`、`SELECT DISTINCT c ... ORDER BY c`）；
    - 既有 `selectbuild` 中原先 `EXPECTED_BLOCKED` 的 15 条 `D09.4` 记录转为正常比对通过（不含窗口/ROLLUP 的那些）；
    - 聚合与去重：单 / 多聚合函数、等价聚合合并、带 `DISTINCT`、带 `GROUP_CONCAT(... ORDER BY 1, a DESC SEPARATOR ';')`、`HAVING` 聚合内别名替换（`SELECT a+1 AS b FROM t HAVING SUM(b) < 0`）、`ORDER BY` 聚合与非聚合混合；
    - `GROUP BY`：列、别名、位置项（`GROUP BY 1`、越界报错、引用聚合/窗口报错）、表达式、带 `HAVING` / `ORDER BY` / `LIMIT`；
    - `SELECT DISTINCT` + `ORDER BY`（`ONLY_FULL_GROUP_BY` 开 / 关：合法表达式匹配、引用列匹配、非法列 / 聚合报错 `12442`）；
    - `ONLY_FULL_GROUP_BY`（有 / 无 GROUP BY）：单值 WHERE（`a = 1`、`a = -1`、`1 = a`）、主键 / NOT NULL 唯一键函数依赖、可空唯一键（不放行）、WHERE 列等值传递（`a = b AND b = 1`）、复杂表达式 `ExpressionDeepEqual`、`ANY_VALUE()`；
    - 逻辑算子单测 / op（`LogicalAggregation` 的 `ReplaceExprColumns`、`BuildKeyInfo` / `BuildSelfKeyInfo`、`splitCondForAggregation` / `pushDown*PredicatesForAggregation`、`CanPullUp`、`CanPushToCop`、`HasDistinct` / `HasOrderBy` / `DistinctArgsMeetsProperty` / `ResetHintIfConflicted` / `GetGroupByCols` / `GetUsedCols` / `GetPotentialPartitionKeys` / `getGroupNDVs`）；
    - 随机单表聚合 / GROUP BY / DISTINCT 查询生成器（约 500+ 条，按结构排除 `M-MAPORDER` 多列同时违规的 `ONLY_FULL_GROUP_BY` 报错，见 §4.6）。
  - **第 B 部分**：
    - 命名窗口引用链（单层、多层、重名、缺窗、环路、继承 frame / partition / order 报错）、未使用命名窗口（验证 plan id 与列校验）；
    - `ROWS` / `RANGE` 帧边界（`UNBOUNDED`、`CURRENT ROW`、数字偏移、负偏移报错、非法顺序报错、`GROUPS` / `IGNORE NULLS` / `DISTINCT` / `FROM LAST` 报错）、默认帧（有无 ORDER BY、`UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING`、`EnablePipelinedWindowExec` 开 / 关、忽略帧的 Note warning）；
    - 窗口函数与聚合函数混合（`SELECT SUM(a), ROW_NUMBER() OVER (ORDER BY SUM(a))`）、ORDER BY 窗口函数、HAVING 禁用窗口函数及窗口别名报错；
    - `SELECT a, first_value(a) OVER () FROM t`（`a NOT NULL`）验证 D08.11 §5 疑似 V1；
    - `FrameBound.UpdateCompareCols` / `ToPB` / `CheckComparisonForTiFlash` 等方法覆盖。
  - **第 C 部分**：
    - `GROUP BY a, b WITH ROLLUP`、带语义等价重复项（`GROUP BY a+1, 1+a WITH ROLLUP` → 产生 `gpos`）、带非空列验证 `M-FT` 模拟（`AdjustNullabilityFromGroupingSets`）、带 `ORDER BY` 隐式投影分组列；
    - 为 `pkg/expression/grouping_sets.go` 29 个函数在 `expr`（或新 op `grouping_sets`）补齐向量覆盖，并从 tracker `govec/domains/funcs_pending.go` 删除这 29 条移交豁免。
    - 同时删除 `govec/domains/cast_pending_d0812.go` 中 4 条 P1-2 豁免（第 A 部分删 `ConstructPositionExpr` / `PosFromPositionExpr`，第 B 部分删 `ParamMarkerInPrepareChecker.Enter` / `Leave`）。
  - **第 D 部分**：等 D09.6 合并后补子查询相关聚合向量。
- 向量双架构要求：amd64 与 arm64 两份向量均须通过 CI `scripts/govec.sh --check` 逐字节校验，amd64 向量由 CI runner 产物生成。

### 6.3 Rust 单元测试

- `agg_col_replacements`（不可变 AST 下 `SELECT a+1 AS b FROM t HAVING SUM(b) < 0` 聚合参数正确改写且不重复替换）。
- `rollup_adjust_nullability_syncs_gby_cols`（§4.7 V1 模拟回归测试：非空列 ROLLUP 后 `Proj4Expand`、`LogicalExpand`、`DistinctGroupByCol`、`GroupByItems` 的 `NotNullFlag` 均被清除）。
- `build_window_functions` 的 `M-FT` 回归测试（若第 B 部分实测确认 `window_func.go:76` 对输出列可见）。
- `defer` 守卫测试：各函数提前报错返回后 `PlanBuilder.check_col_priv` 均恢复原值。
- `PosFromPositionExpr` / `ParamMarkerInPrepareChecker` 单元测试。

### 6.4 端到端与下游衔接

- **第 A 部分完成后**：`selectbuild` 计划构建全绿，但完整 SQL 端到端执行（`sbtest_results.rs`）还需按顺序接通：**D09.13 `BuildKeySolver`** → **D09.9 第 1 部分**（聚合优化规则）→ **D10.1 / D10.3**（物理聚合算子与枚举，替换 `exhaust_physical_plans` 桩）→ **D12.3 / D12.4**（`executorBuilder` 分派 `PhysicalHashAgg` / `PhysicalStreamAgg`，执行器主体已在 #375/#376/#382/#386/#393 合并）。全部接通后由维护者运行 `tools/ab` 对 sysbench `SUM(k)` 与 `DISTINCT c ... ORDER BY c` 做 EXPLAIN 与结果对比，并把 `sbtest_results.rs:483-533` 改为新栈直接执行断言。

## 7. 与 Go 的已知差异

1. **AST 不可变，四处就地修改改为构建器本地覆盖**（`logical_plan_builder.go:108, 133, 2866, 3463, 3502, 3504, 3532, 4140, 7470-7571`）：见 §4.3，纯内部承载差异（L2），对计划、结果、报错无可见影响。
2. **四个 map 遍历改为确定顺序**（`logical_plan_builder.go:3910, 7523, 7584, 3418`）：按 `M-MAPORDER`（§4.6）。仅在 Go 自身行为不确定（多列同时违反 `ONLY_FULL_GROUP_BY`、多个窗口规范同时报错、多个未用命名窗口排序键相同）时，Rust 稳定返回 Go 可能返回的其中一种结果。
3. **Go 忽略错误的位置遇到 `blocked` 向上返回**（`logical_plan_builder.go:3555, 3675, 3713, 3818, 4087, 7161`；`logical_aggregation.go:294, 763`）：仅在命中尚未移植块的边界桩时生效，确保不静默偏离 Go（与 D09.9 spec §4.4 一致）。
4. **7 个 cascades 专用方法不移植**（`logical_aggregation.go:68, 87, 535, 552`；`logical_window.go:367, 384, 398`）：见 §3.6（EXC.B / B3），tidb-rs 不含旧 cascades 优化器，无用户可见影响。
5. **`intset.FastIntSet` 改为 `BTreeSet<i64>`**（`grouping_sets.go`、`logical_expand.go`）：无可见差异（L2）。
6. **`GroupingSet.MemoryUsage` / `GroupingExprs.MemoryUsage`**：按 Go 64 位结构尺寸常量计算（`grouping_sets.go:279, 406`，L2），数值与 Go 64 位一致。
7. **临时边界差异（随对应块消失）**：`GROUP BY ?` / `GROUP_CONCAT(... ORDER BY ?)`（`blocked-by:D02.2`）、`LogicalAggregation.ExplainInfo`（`blocked-by:D10.9`）、`ExtractFD`（`blocked-by:D09.14`）、窗口 `RANGE INTERVAL`（`blocked-by:D08.7`）、`GROUPING()` 函数体（`blocked-by:D08.9`）、物理聚合枚举（`blocked-by:D10.3`）。

## 8. 读代码发现的可能的 Go 问题

以下 7 条均在阅读 Go TiDB v8.5.8 源码时发现，**尚未在运行中的 v8.5.8 上复现**。按 port-block §1，在实现对应部分时先在 v8.5.8 上复现并开英文 `upstream-pitfall` issue（标 `go-bug-kept` 或 `go-bug-fixed`）；若复现不成立则删除对应条目。

1. **`buildExpand` 与 `DistinctSize` 的 64 阈值条件不一致，导致 `GenLevelProjections` 越界 panic（疑似可复现的 panic）**
   - **位置**：`logical_plan_builder.go:205-210`、`operator/logicalop/logical_expand.go:202, 235-237, 389`、`pkg/expression/grouping_sets.go:557-560, 586, 615`。
   - **机制**：`buildExpand` 按 `len(expand.RollupGroupingSets) > 64` 决定启用 `tipb.GroupingMode_ModeNumericSet`（`:206-208`）；但 `DistinctSize()` 调用 `DistinctSizeWithThreshold(64)`，仅在去重后的 `len(distinctGroupingIDsPos) > 64` 时才分配 `RollupGroupingIDs` 与 `RollupID2GIDS`，否则返回 `nil`（`grouping_sets.go:586, 615`）。当 `GROUP BY ... WITH ROLLUP` 包含 ≥ 64 项且存在语义重复项，使得总 grouping sets 数 > 64 但去重后 ≤ 64 时，`ResolveExpand` 调用 `GenLevelProjections` → `GenerateGroupingIDIncrementModeNumericSet(0)` 读取 `p.RollupGroupingIDs[0]`（`logical_expand.go:389`），触发 `index out of range [0] with length 0` panic（被优化器恢复成 1105）。
   - **v8.5.8 复现方法**：建表 `CREATE TABLE t(a INT);`，执行 `EXPLAIN SELECT COUNT(*) FROM t GROUP BY a, a, ... (共 64 个 a) WITH ROLLUP;`（或任意 64 项含重复列的 ROLLUP），对比 MySQL 8.0 行为。若复现成立且 MySQL 8.0 正常执行，实现第 C 部分时可按宪章第九条 3 处理。
2. **`GroupingSets.MergeOne` 中间插入漏检前驱子集关系（疑似逻辑错误）**
   - **位置**：`pkg/expression/grouping_sets.go:110-142`。
   - **机制**：从右向左查找插入位置时，对 `j < len(oneNewGroupingSet)-1` 且 `!targetOne.SubSetOf(cur)` 的情况直接落入第 136–141 行将 `targetOne` 插入 `j` 与 `j+1` 之间，未检查 `cur.SubSetOf(targetOne)`。例如已有 `[[a], [a, b, c]]`，合并 `[b]` 会得到 `[[a], [b], [a, b, c]]`，破坏"每个前驱是后继的子集"的前缀不变量。
   - **v8.5.8 复现方法**：在 `grouping_sets_test.go` 中对 `GroupingSets{{{a}, {a, b, c}}, {{b}}}.Merge()` 调用并检查结果是否满足前缀包含关系；再验证是否可由 3-stage multi-distinct agg（`task.go:1804`）经 SQL 触发错误结果。
3. **`checkOnlyFullGroupByWithOutGroupClause` 覆盖 `firstOrderByAggColIdx` 且 HAVING 沿用最后 SELECT 列下标**
   - **位置**：`logical_plan_builder.go:3952-3971, 4027-4029`。
   - **机制**：(a) `colResolverForOnlyFullGroupBy.Enter` 在 `orderByClause` 每次遇到聚合都覆盖 `c.firstOrderByAggColIdx = c.exprIdx`（`:4028`），记录的实为**最后一个**而非第一个聚合；(b) 遍历 `sel.Having`（`:3957-3959`）未更新 `curClause` 与 `exprIdx`，沿用最后一个 SELECT 列的 `exprIdx`，导致 HAVING 中的违规列在 `ErrMixOfGroupFuncAndFields` 中报出最后一条 SELECT 列的序号。
   - **v8.5.8 复现方法**：在默认 `ONLY_FULL_GROUP_BY` 下执行 `CREATE TABLE t(a INT PRIMARY KEY, b INT, c INT);` 后跑 `SELECT a FROM t ORDER BY COUNT(a), SUM(b);`（观察是否报 `Expression #2` 而非 `#1`）和 `SELECT a, 1 FROM t WHERE a = 1 HAVING SUM(b) > 0 AND c > 0;`（观察 `ErrMixOfGroupFuncAndFields` 的序号是否为 `2`）。仅影响报错序号，按宪章第一条 2 照 Go 移植。
4. **`checkOnlyFullGroupByWithGroupClause` 同列在 ORDER BY 覆盖 SELECT 的报错位置**
   - **位置**：`logical_plan_builder.go:3874-3896`。
   - **机制**：`notInGbyOrSingleValueColNames` 以 `*types.FieldName` 为键，先写 SELECT 再写 ORDER BY，同一列既在 SELECT 表达式又在 ORDER BY 表达式中违规时，后者覆盖前者的 `ErrExprLoc`，导致本应报 `SELECT list` 改为报 `ORDER BY`。
   - **v8.5.8 复现方法**：`SELECT b + 1 FROM t GROUP BY a ORDER BY b + 2;`（`a` 非主键/唯一键）。仅影响报错文案，照 Go 移植。
5. **`buildWindowFunctionFrameBound` 对非字面量常量表达式构造 `Constant` 时使用了零值 `FieldType`**
   - **位置**：`logical_plan_builder.go:7038-7042`。
   - **机制**：`evalAstExprWithPlanCtx` 求出 `Datum` 后，`:7042` 将 `RetType` 设为 `boundClause.Expr.GetType()`；当 `boundClause.Expr` 是二元运算等复合常量表达式（如 `1 + 1`）时，AST 节点的 `GetType()` 仍是未推导的零值（`TypeUnspecified`），随后被传给 `expr.EvalInt` 和 `NewFunctionBase(Plus/Minus/DateAdd/DateSub)`。
   - **v8.5.8 复现方法**：在 v8.5.8 执行 `SELECT SUM(a) OVER (ORDER BY a RANGE BETWEEN 1 + 1 PRECEDING AND CURRENT ROW) FROM t;`（及 `INTERVAL (1 + 1) DAY`），观察是否报错或产生异常类型/结果，并与 MySQL 8.0 对比。
6. **`LogicalExpand.ResolveGroupingFuncArgsInGroupBy` 报错使用 0-based 序号**
   - **位置**：`operator/logicalop/logical_expand.go:346`。
   - **机制**：传给 `ErrFieldInGroupingNotGroupBy` 的参数为 `fmt.Sprintf("#%d", argIdx)`（从 `#0` 起），而同文件的 `logical_plan_builder.go:3925` 与 MySQL 为 1-based。
   - **v8.5.8 复现方法**：`SELECT GROUPING(b) FROM t GROUP BY a WITH ROLLUP;`。仅影响报错文案，照 Go 移植。
7. **`LogicalWindow.EqualFrame` 未校验 `CalcFuncs` 切片长度**
   - **位置**：`operator/logicalop/logical_window.go:415-424`。
   - **机制**：遍历 `p.Frame.Start.CalcFuncs`（及 `End`）并按同下标访问 `newWindow.Frame.Start.CalcFuncs[i]`，未先比较切片长度。仅由旧 cascades 调用（EXC.B，不移植），对 tidb-rs 无影响，仅记录。

## 9. 决定点

按宪章第九条分为两部分。

### 9.1 需要负责人决定

无。

> 本块严格遵循已确认的基础设计（N0、N1、N13）与已确认的兄弟 spec（D08.11、D09.9），不引入打算长期保留的用户可见差异，也不涉及范围变更或外部资源。实现阶段若 §8 第 1、2、5 条在 v8.5.8 上复现出结果错误或 panic，按宪章第九条 3（可复现结果错误/panic 的默认修正流程）或第九条 1（有争议或仅元数据影响时）另行处理。

### 9.2 维护者已定（宪章第九条 2）

1. **拆分与顺序**：按 §3 分为 A（聚合 / DISTINCT / GROUP BY / `ONLY_FULL_GROUP_BY` / `LogicalAggregation`，43+2 个）、B（窗口 / `LogicalWindow`，35+2 个）、C（ROLLUP / `LogicalExpand` + `grouping_sets.go`，15+29 个）、D（子查询相关聚合，10 个）四部分，优先实施第 A 部分以解锁 sysbench 聚合链与 D09.9 第 1 部分。理由：第 A 部分零未满足前置块，B / D 分别受限于 D08.11 第 C 部分与 D09.6。
2. **cascades 专用方法豁免**：§3.6 的 7 个方法（`Hash64`、`Equals`、`CopyAggHints`、`IsCompleteModeAgg`、`EqualPartitionBy`、`EqualOrderBy`、`EqualFrame`）按已确认的 EXC.B / B3 不移植。理由：全仓库仅 `pkg/planner/cascades/` 调用，与 D09.14、D08.11 处理一致。
3. **聚合与窗口函数描述按值持有**：`LogicalAggregation.agg_funcs: Vec<AggFuncDesc>`、`LogicalWindow.window_func_descs: Vec<WindowFuncDesc>`。理由：沿用负责人已确认的 D08.11 决定点 4 与 D09.9 决定点 5。
4. **不可变 AST 的改写承载**：用构建器本地的 `agg_col_replacements`、`ResolvedExpr<'ast>`、`WindowSpecW<'ast>` 替代 Go 对 AST 的原地修改（§4.3）。理由：保持 `&'ast ast::SelectStmt` 只读和 `ByAddr<'ast, T>` 键身份，无用户可见差异。
5. **同算子 / 直接依赖的其他块函数提前随部分移植**：§3.7 所列的算子规则方法（`PruneColumns`、`PredicatePushDown`、`DeriveStats`、`ExtractColGroups`、`PreparePossibleProperties`、`ExtractCorrelatedCols`）与无出边 / 仅本块调用的辅助函数（`AggregateFuncExtractor`、`WindowFuncExtractor`、`extractSingeValueColNamesFromWhere`、`colNameResolver`、`allColFromExprNode`、`getAllByItems`、`restoreByItemText`、`compareItems`、`tblInfoFromCol` 读路径、`ExpressionDeepEqual`、`ResolveExpand`）随对应部分移植并计入各自块；`LogicalAggregation.ExplainInfo`（D10.9）、`ExtractFD`（D09.14）、`ExhaustPhysicalPlans`（D10.3）暂作边界桩。理由：依赖已就绪且 `DoOptimize` 立即可达，避免出现不可用的半成品算子。
6. **P1-1 `grouping_sets.go` 的 `ToPB` 放在 `tidb-expr-pushdown`**：`GroupingSet.ToPB` / `GroupingSets.ToPB` 按 N0 规则 6 放入 `tidb-expr-pushdown`，其余 27 个放入 `tidb-expression/src/grouping_sets.rs`。理由：`tidb-expression` 不依赖 `tipb` 和 `ExpressionsToPBList`，与 D08.11 §4.8 一致。
7. **`GROUP BY ?` 与 `GROUP_CONCAT(... ORDER BY ?)` 暂作 `blocked-by:D02.2`**：`PosFromPositionExpr` 先实现字面数字路径，参数标记形式与现有 `ORDER BY ?` 一致停在 `D02.2` 边界。理由：Rust AST `Expr::Position(i64)` 尚未扩展参数节点。
8. **`AdjustNullabilityFromGroupingSets` 按 `M-FT` 显式模拟**：按 §4.7 在 `build_expand` 同步清除 `Proj4Expand`、`DistinctGroupByCol`、`RollupGroupingSets`、`newGbyItems` 的 `NotNullFlag` 并配回归测试。理由：影响发给客户端的列定义，属已确认 N1 §3.3 / `M-FT` 的规定行为。
9. **`tidb-parser` 补 flags=0 的 `Restore` 模式**：在 `crates/parser/src/restore.rs` 为 `Expr` / `WindowSpec` 提供无反引号、小写关键字、双引号字符串的恢复入口（不改现有 `DefaultRestoreFlags`）。理由：保证第 B 部分 `sortWindowSpecs` 排序与 `specEqual` 和 Go 一致。
10. **map 遍历顺序按 `M-MAPORDER` 确定化**：§4.6 四处改用输入声明序 / 首次出现序 / schema 列序。理由：Go 本身结果不确定，取确定结果符合 `M-MAPORDER`。
11. **govec 扩展 `selectbuild` 领域**：在已有 `selectbuild` 领域扩展聚合 / 窗口 / ROLLUP 输出快照与用例，按部分从 `selectbuild_exempt.go`、`cast_pending_d0812.go`、`funcs_pending.go` 移出豁免；`grouping_sets.go` 29 个函数在 `expr` 领域新增 op 覆盖。理由：`selectbuild` 已声明 `D09.4` 并钉住了 15 条 `D09.4` 边界记录，直接延续可自然解除边界并避免重复建域。
12. **§8 疑似 Go 问题延至实现阶段复现并登记**：在实现各部分时于 v8.5.8 实测复现，确认后开英文 `upstream-pitfall` issue 并按宪章第一条 2 / 第九条 3 决定 `go-bug-kept` 或 `go-bug-fixed`。理由：当前只写 spec，不改动代码或运行集群。

## 本文所用 ID

- D00.1：上下文接口骨架（`sessionctx`、`exprctx` 等）
- D00.3：通用工具（`util/*`，含 `intset`）
- D01.10：落盘容器与磁盘 chunk
- D02.1 / D02.2：parser 与 AST（含 `Restore`、`ExpressionDeepEqual`、位置节点）
- D05.2：`SessionVars` 与 `StatementContext`
- D08.1：表达式框架（`Expression`、`Column`、`Constant`、`ScalarFunction`、`Schema`）
- D08.2：表达式构建辅助（含 P1-1 移交的 `grouping_sets.go`）
- D08.3：表达式下推与 protobuf 转换（crate `tidb-expr-pushdown`）
- D08.4：比较内置函数
- D08.5：算术、数学与逻辑内置函数
- D08.7：时间与日期内置函数（含 `DATE_ADD` / `DATE_SUB`）
- D08.9：控制流、信息、杂项内置函数（含 `GROUPING()`、`ANY_VALUE()`）
- D08.11：聚合与窗口函数描述（`expression/aggregation`）
- D08.12：表达式工具函数（含 P1-2 移交的 4 个函数）
- D09.0：计划构建器公共部分（`planbuilder.go`、`util.go`）
- D09.1：SELECT 逻辑计划构建
- D09.2：表达式改写（`expression_rewriter.go`）
- D09.3：join 构建与逻辑 join 算子
- D09.4：本块，聚合、窗口与 `ONLY_FULL_GROUP_BY` 的计划构建及逻辑算子
- D09.5：集合运算（UNION 等）与 CTE
- D09.6：子查询与去关联
- D09.7：列裁剪与投影消除
- D09.8：谓词下推与简化
- D09.9：聚合相关逻辑优化规则（消除、下推、skew、max/min）
- D09.10：join 重排（JoinReorder）
- D09.11：TopN / Limit 下推与窗口 TopN 推导
- D09.13：优化器驱动、规则框架、key-info 与 `ResolveExpand` 规则
- D09.14：逻辑算子公共部分与函数依赖（`funcdep`）
- D09.15：写路径与视图的计划构建（含 `tblInfoFromCol`、`ExtractTableList`）
- D09.16：其他语句的计划构建（`SELECT ... INTO` 等）
- D10.1：物理算子定义与 task
- D10.2：`FindBestTask` 与访问路径
- D10.3：物理计划枚举（`exhaust_physical_plans`）
- D10.6：基数估算（`DeriveStats`）
- D10.9：后处理与 EXPLAIN 文本输出
- D10.10：物理属性与优化器工具（`planner/util`）
- D12.3：聚合执行器框架（hash / stream agg）
- D12.4：聚合函数实现（`aggfuncs`）
- D12.9：其他执行器（含窗口执行器等）
- D13.3：优化器 hint 框架
- D14.2：plan 编码与慢日志摘要
- N0：crate 分层设计（规则 6 为跨 crate 自由函数上移）
- N1：计划树和表达式树的表示与所有权（§4 摘出分派、§5 共享修改审计）
- N13：长期共享元数据与 `Arc<FieldType>` 写时复制
- M-PLAN、M-SELF、M-IFACE、M-EXPR、M-FT、M-SHARE、M-DEFER、M-MEM、M-GO、M-CH、M-LOCK、M-INT、M-MAPORDER、M-NAME、M-FMA、M-F2I：`docs/porting/mappings.md` 中的全局映射规则（依次为：计划表示、struct 嵌入、接口、表达式共享、`FieldType` 共享与修改、长期共享元数据、`defer`、内存记账、goroutine、channel、锁、定宽整数、map 遍历顺序、与 Rust trait 同名的方法、浮点乘加、浮点转整数）
- A1：基于 `TrackedAlloc` 的内存记账设计
- L1 / L2：来源标注等级（L1 按映射规则逐行对应；L2 有理由的 Rust 化改写）
- EXC.B / B3：例外 B（cascades 优化器专用接口 / 仅旧 cascades 优化器调用的代码，不移植）
- X：不在 tidb-rs 范围内的代码（写路径、mock store、测试专用钩子等）
- S / V0 / V1：N1 §5 共享修改审计结论分类（S：只改本处新建或已复制对象；V0：共享但无可见影响；V1：共享且改变计划、结果或客户端列定义）
- VS1 / VS1-21：第一条纵向切片及其 SELECT 构建任务
- P1 / P3：ROADMAP 的第一阶段（基础表达式）/ 第三阶段（规划器）
- PoC：原型实现（新栈遇到 `blocked` 错误 1235 时的回退目标）
