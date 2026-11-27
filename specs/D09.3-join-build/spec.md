# D09.3 join 的计划构建与逻辑 join / apply 算子（spec）

状态：待评审（只写 spec，issue #477）。块：D09.3（ROADMAP P3；Go 73 个函数，约 2.3k 行函数体，分布于 3 个文件），crate `tidb-planner`。

相关：[D09.1 spec](../D09.1-select-builder/spec.md)（SELECT 构建，`buildResultSetNode`、`unfoldWildStar`）、[D09.2 spec](../D09.2-rewriter/spec.md)（表达式改写，`findFieldNameFromNaturalUsingJoin`）、[D09.4 spec](../D09.4-agg-window/spec.md)（聚合与 `ONLY_FULL_GROUP_BY`，已留出 join 的接口）、[D09.14 spec](../D09.14-logical-operators/spec.md)（逻辑算子公共框架，已移植 `Conds2TableDual`）、[D09.8 任务表](../D09.8-predicate-pushdown/tasks.md)（谓词下推，尚无 spec）、[D09.9 spec](../D09.9-agg-rules/spec.md)（聚合规则，§4.4 的 `blocked` 传递规则）、[N0](../exceptions/N0-crate-layering/spec.md)（crate 分层）、[N1](../exceptions/N1-plan-representation/spec.md)（计划树表示、§4 摘出分派、§5 共享修改审计）、[N3 / A1](../exceptions/N3-A1-concurrency-memory/spec.md)（并发与内存记账）、[N4](../exceptions/N4-expr-eval/spec.md)（关联列的槽，§3.5）、[N13](../exceptions/N13-shared-metadata/spec.md)（共享元数据与 `Arc<FieldType>`）、[mappings.md](../../docs/porting/mappings.md)（全局映射规则）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/planner/core/`，另有说明的除外。行号都对照过参照树 `scripts/tidb-reference.sh`。

## 1. 这个块在 Go 里做什么

### 1.1 内容

本块把 FROM 子句里的 join 建成逻辑计划，并提供 join 和 apply 两个逻辑算子。全部是只读查询的计划构建，不写数据。

| Go 文件 | D09.3 函数数 | 内容 |
|---|--:|---|
| `logical_plan_builder.go` | 17 | `buildJoin`（两个表源的 join）、`buildLateralJoin`（LATERAL 派生表）、`buildUsingClause` / `buildNaturalJoin` / `coalesceCommonColumns`（USING 和 NATURAL 合并同名列）、`findJoinFullSchema` / `findColFromNaturalUsingJoin`（查找 join 的完整 schema）、`containsLateralTableSource` / `isImmediateLateralTableSource`、`buildJoinFuncDepend`（`ONLY_FULL_GROUP_BY` 用的 ON 等值依赖）、子查询用的 `buildApplyWithJoinType` / `buildSemiApply` / `buildSemiJoin`、集合运算用的 `buildSemiJoinForSetOperator` / `unionJoinFieldType`、物理枚举用的 `hasMPPJoinHints` / `isJoinHintSupportedInMPPMode` |
| `operator/logicalop/logical_join.go` | 46 | `JoinType`（7 种 join 类型）与 `LogicalJoin`：`Init`、条件拆分（`AttachOnConds`、`ExtractOnCondition`、`pushDownConstExpr`）、等值条件整理（`updateEQCond`、`getProj`）、schema（`MergeSchema`、`BuildLogicalJoinSchema`）、key 信息、join hint（`SetPreferredJoinTypeAndOrder` 等）、外连接化简与常量传播辅助（`simplifyOuterJoin`、`outerJoinPropConst`、`DeriveOtherConditions`）、半连接改写（`SemiJoinRewrite`）、函数依赖（`ExtractFD*`）、物理枚举用的取值方法，以及已移植的 `Conds2TableDual`、`AppendAddSelectionTraceStep` |
| `operator/logicalop/logical_apply.go` | 10 | `LogicalApply`（嵌入 `LogicalJoin`，加关联列）：`Init`、`ExplainInfo`、`ReplaceExprColumns`、`findChildFullSchema`、`ExtractFD`、去关联用的 `CanPullUpAgg` / `DeCorColFromEqExpr`、`getGroupNDVs`，以及 cascades 专用的 `Hash64` / `Equals` |

同一文件中属于其他块的方法（不在本块的 73 个里，但第 B、C 部分会一起移植，见 §3.7）：`LogicalJoin` 的 `PredicatePushDown`、`ConstantPropagation`、`ConvertOuterToInnerJoin`（D09.8）、`PruneColumns`（D09.7）、`PushDownTopN` / `pushDownTopNToChild`（D09.11）、`DeriveStats` / `ExtractColGroups`（D10.6）、`PreparePossibleProperties`（D10.2）、`ExtractCorrelatedCols`（D09.6）、`ExhaustPhysicalPlans`（D10.3）；`LogicalApply` 的 `PruneColumns`（D09.7）、`DeriveStats` / `ExtractColGroups`（D10.6）、`ExtractCorrelatedCols`（D09.6）、`ExhaustPhysicalPlans`（D10.3）。

### 1.2 执行流程要点（逐条与 v8.5.8 源码核对）

**`buildJoin`（`logical_plan_builder.go:727-949`）**，按顺序：

1. `joinNode.Right == nil` 时只建左边（`:731-733`，已在 VS1 中移植）。
2. 用 `containsLateralTableSource` 判断右边是否含 LATERAL（`:740`）；置 `FlagPredicatePushDown`，非 LATERAL 时再置 `FlagJoinReOrder`，然后置 `FlagPredicateSimplification`（`:742-748`）。
3. 建左边；LATERAL 时把左边的 schema（有 `FullSchema` 时用它）压进 `outerSchemas` / `outerNames`，`lateralOuterCount++`，用 `defer` 在所有返回路径上弹出（`:756-784`）。再建右边。
4. LATERAL 时，右边直接就是 LATERAL 表源，或右边的计划确实引用了左边的列，就转去 `buildLateralJoin` 建 `LogicalApply`（`:800-815`）；否则当普通 join 继续。
5. 右边是递归 CTE 的递归部分而且是 LEFT JOIN 时报 `ErrCTERecursiveForbiddenJoinOrder`（`:818-820`）。
6. `handleHelper` 弹出两层再合并压回（`:822-824`）；`LogicalJoin.Init`（**此处分配 join 的 plan id**，`:827`），`StraightJoin` 取 `joinNode.StraightJoin || b.inStraightJoin`；schema 是两边 schema 的合并，输出名是两边输出名的拼接（指针拷贝）。
7. join 类型（`:834-848`）：`LEFT JOIN` → `LeftOuterJoin`，置 `FlagEliminateOuterJoin`，**右边各列去掉 NOT NULL 标志**（`util.ResetNotNullFlag`，先复制 `FieldType` 再改）；`RIGHT JOIN` → `RightOuterJoin`，左边各列去掉 NOT NULL；其余（解析器的 `CrossJoin`，即 `JOIN` / `INNER JOIN` / `CROSS JOIN` / 逗号）→ `InnerJoin`。
8. `FullSchema` / `FullNames`（`:850-897`）：**每个 join 都有**。左右两边是 join（或 apply）且有 `FullSchema` 时取它，否则取该边的 schema 和输出名；`RIGHT JOIN` 时交换，使 `FullSchema` 总是"外表在前、内表在后"；外连接时内表一侧去掉 NOT NULL；`FullNames` 逐个**按值复制** `FieldName`（之后 USING 会在副本上标 `Redundant`）。
9. `SetPreferredJoinTypeAndOrder(b.TableHints())`（`:902`），根据表级 join hint 设置 `PreferJoinType` 等。
10. 条件（`:911-946`）：NATURAL → `buildNaturalJoin`；USING → `buildUsingClause`；ON → `curClause = onClause`，`checkColPriv` 临时改为 `reportColumnErrOption`，改写 ON 表达式（改写返回后、检查错误之前就恢复 `checkColPriv`），改写产生了新计划（ON 里有子查询）时报 `errors.New("ON condition doesn't support subqueries yet")`（错误码 1105），按 CNF 拆开；**inner join 的 ON 条件不挂在 join 上，而是在 join 上面建一个 `LogicalSelection`**（`:936-940`，再分配一个 plan id），外连接则 `AttachOnConds`；没有 ON / USING 的 inner join 标 `CartesianJoin = true`。

**`coalesceCommonColumns`（`:1140-1344`）**：两边 schema 先 `Clone`，外连接的内表一侧去掉 NOT NULL；`RIGHT JOIN` 交换左右。USING 先分别检查左、右两边的同名列（`ErrAmbiguous`），NATURAL 统计两边同名列的次数，任一边多于一次报 `ErrAmbiguous`。然后把共有列按左边的顺序挪到最前面（跳过 `_tidb_rowid` 和隐藏列），每个共有列的两侧都追加列权限检查（`appendColNamesToVisitInfo`，视图用视图名，CTE 跳过）；USING 里有找不到的列时遍历 `filter` map 报 `ErrUnknownColumn`（`:1305-1310`）；为每个共有列建 `EQ(左列, 右列)` 条件放在 `OtherConditions` 最前面，并在 `FullNames` 中把内表（`RIGHT JOIN` 时是左表）那一列标为 `Redundant`。join 的 schema 变成"共有列 + 左边其余列 + 右边其余列"。

**`LogicalJoin` 的条件**：`EqualConditions`（两边各一个列的 `=` / `<=>`）、`NAEQConditions`（null-aware 反半连接）、`LeftConditions` / `RightConditions`（只引用一边的条件）、`OtherConditions`（其余）。`ExtractOnCondition`（`logical_join.go:1288-1380`）按以下顺序分类：来自 `IN` 改写的等值条件放 `OtherConditions`；两边各一列的二元函数，按需推导 `IS NOT NULL`（`deriveLeft` / `deriveRight`，用 `IsNullRejected`），是 `=` / `<=>` 时重建为"左列在前"放进 `eqCond`；没有列的常量条件交给 `pushDownConstExpr`（按 join 类型推到一边或两边，外连接时还会追加到 join 自己的 `RightConditions` / `LeftConditions`）；只引用一边的放对应一边；其余放 `OtherConditions`，并按需推导放松后的 DNF 过滤条件。

**等值条件整理 `updateEQCond`（`:1581-1679`）**：谓词下推之后**从后往前**扫描 `OtherConditions`，把两边各一个表达式的 `=` 取出来；表达式不是列时，用 `getProj` 在对应子节点上加（或复用已有的）`LogicalProjection` 并 `AppendExpr`，再建等值条件。因此 `EqualConditions` 中新加的条件与 `OtherConditions` 中的原顺序相反，移植时照抄。

**`LogicalApply`**：`buildApplyWithJoinType` 调 `LogicalApply.Init`（分配 plan id）；`buildSemiApply` 先 `buildSemiJoin`（`LogicalJoin.Init` 分配 plan id），再把这个 join **按值复制**进 `LogicalApply`，只改 TP 和 `self`，**不再分配 plan id**（`:5836-5850`）；`buildSemiJoin` 在 `asScalar` 时为结果列分配一个列 id。

### 1.3 调用方（tracker `data/block_edges.tsv`）

| 调用方块 | 引用数 | 用到什么 |
|---|--:|---|
| EXC.B（新 / 旧 cascades） | 99 | `Shallow`、`ExtractJoinKeys`、`AppendJoinConds`、`ExtractOnCondition`、`DeriveOtherConditions`、`Hash64` / `Equals` 等（不移植） |
| D10.3 物理计划枚举 | 76 | `GetJoinKeys`、`GetNAJoinKeys`、`IsNAAJ`、`PreferAny`、`GetPotentialPartitionKeys`、`hasMPPJoinHints`、`isJoinHintSupportedInMPPMode`、`JoinType` |
| D09.10 join 重排与 join 消除 | 52 | `LogicalJoin` 全部字段、`SetPreferredJoinType`、`MergeSchema`、`AttachOnConds` 等 |
| D09.6 子查询与去关联 | 35 | `buildSemiApply`、`buildApplyWithJoinType`、`LogicalApply`、`Decorrelate`、`ColumnSubstituteAll`、`CanPullUpAgg`、`DeCorColFromEqExpr`、`findJoinFullSchema`、`SemiJoinRewrite` |
| D12.6 hash join 执行器 | 35 | `JoinType`、`IsOuterJoin`、`IsSemiJoin` 等 |
| D10.6 基数估算 | 30 | `GetJoinKeys`、`getGroupNDVs`、`findChildFullSchema` |
| D09.8 谓词下推 | 28 | `simplifyOuterJoin`、`outerJoinPropConst`、`extractOnCondition`、`DeriveOtherConditions`、`updateEQCond`、`SemiJoinRewrite`、`mergeOnClausePredicates`、`addCandidateSelection` |
| D09.9 聚合规则 | 24 | `LogicalJoin` 字段、`BuildLogicalJoinSchema` |
| D09.0、D10.9、D09.2、D09.7、D09.14、D12.7、D10.1、D09.11、D10.2、D09.1、D09.4、DEF.MPP、D09.5、D12.8、D09.13、D13.3、D12.10 | 1–18 | `JoinType.String`、`ExtractUsedCols`、`BuildLogicalJoinSchema`、`findColFromNaturalUsingJoin`、`findJoinFullSchema`、`buildJoinFuncDepend`、`buildSemiJoinForSetOperator`、`unionJoinFieldType` 等 |

本块依赖最多的是 D08.1 表达式框架（149）、D00.1 上下文（144）、D13.3 hint（88）、D09.14 逻辑算子框架（79）、D09.13 规则框架（39）、D10.1（27）、D10.10（26）。

### 1.4 最简单的两表等值 inner join 在 Go 中的路径

`SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.a = t2.a`（两表都没有 join 键上的索引）：

1. 构建：`buildSelect` → `buildResultSetNode` → `buildJoin`：两个 `DataSource`、一个 `LogicalJoin`（`InnerJoin`，没有条件，`CartesianJoin = false`）、上面一个 `LogicalSelection(t1.a = t2.a)`；置 `FlagPredicatePushDown | FlagJoinReOrder | FlagPredicateSimplification`。然后 `buildProjection`。
2. 逻辑优化（`optimizer.go:78` 的 `optRuleList`，只跑置了标志的规则）：列裁剪（`LogicalJoin.PruneColumns`，D09.7）→ 谓词下推（`LogicalSelection.PredicatePushDown` → `LogicalJoin.PredicatePushDown`，D09.8：`ExtractFiltersFromDNFs`、`PropagateConstant`、`extractOnCondition` 把 `t1.a = t2.a` 放进 `EqualConditions`，`updateEQCond`，`SemiJoinRewrite` 对 inner join 直接返回）→ 谓词化简 → **join 重排**（`JoinReOrderSolver`，D09.10：两表也是一个 join 组，`tidb_opt_join_reorder_threshold = 0` 时走贪心算法，会按代价重新组装 join 并可能交换左右）→ 第二次列裁剪、投影消除等。`existsCartesianProduct`（`optimizer.go:1192`）在 `AllowCartesianProduct` 关闭时检查。
3. 物理优化：`LogicalJoin.DeriveStats`（D10.6，`cardinality.EstimateFullJoinRowCount`）、`PreparePossibleProperties`（D10.2）、`ExhaustPhysicalPlans4LogicalJoin`（D10.3：hash join 两种建表方向、merge join、index join 候选）、代价（D10.5）、`attach2Task`（D10.1）；默认 `tidb_hash_join_version = legacy`（`sessionctx/variable/tidb_vars.go:1675`）。
4. 后处理（D10.9）：`ResolveIndices`、EXPLAIN 文本。
5. 执行（D12.0 builder 的 `buildHashJoin`，D12.6 `HashJoinExec` v1 与 joiner），两边各一个 TableReader（D12.1，已移植）。

## 2. 依赖

都已用 `grep -rn '// go: <Symbol>' crates/` 在当前仓库（`6c7bf5d`）核对。

### 2.1 已移植（可直接调用）

| 块 | 用到的符号 / 能力 | Rust 位置 |
|---|---|---|
| D08.1 表达式框架 | `Schema`（`Clone`、`Append`、`Contains`、`ColumnIndex`、`RetrieveColumn`）、`MergeSchema`、`ExprFromSchema`、`ExtractColumns`、`ExtractColumnsFromExpressions`、`ExtractCorColumns`、`SplitCNFItems`、`NewFunction` / `NewFunctionInternal`、`ScalarFuncs2Exprs`、`IsEQCondFromIn`、`Column.Decorrelate` / `ScalarFunction.Decorrelate`、`SortColumns`、`EvaluateExprWithNull` | `crates/expression/src/` |
| D08.12 表达式工具 | `ExtractFiltersFromDNFs`、`DeriveRelaxedFiltersFromDNF`、`BuildNotNullExpr`、`RemoveDupExprs`、`ColumnSubstituteAll`、`ExtractEquivalenceColumns`、`PropagateConstant`、`PropConstOverOuterJoin`、`FoldConstant` | `crates/expression/src/util.rs`、`constant_propagation.rs`、`constant_fold.rs` |
| D01.x 类型 | `AggFieldType`、`TryToFixFlenOfDatetime`、`FieldName.Clone`、`NameSlice.Shallow` | `crates/types/src/` |
| D09.0 / D09.1 / D09.2 | `PlanBuilder`、`handleColHelper.popMap` / `mergeAndPush`、`buildResultSetNode`、`rewrite`（L2：按 schema 和输出名改写）、`appendColNamesToVisitInfo`、`splitWhere`、`in_straight_join` | `crates/planner/src/core/planbuilder.rs`、`logical_plan_builder.rs`、`expression_rewriter.rs` |
| D09.4（第 A、B 部分已合并） | `buildDistinct`、`buildFuncDependCol`、`tblInfoFromCol`、`LogicalAggregation` | `crates/planner/src/core/logical_plan_builder.rs`、`logicalop/logical_aggregation.rs` |
| D09.13 / D09.14 | `LogicalPlan` trait、`BaseLogicalPlan`（`BuildKeyInfo`、`FindBestTask`）、`LogicalSchemaProducer`（`BuildKeyInfo`、`InlineProjection`）、`LogicalSelection`（`Init`、`PredicatePushDown`、`PruneColumns`）、`LogicalProjection`（`Init`、`AppendExpr`）、`AddSelection`、`AppendTableDualTraceStep`、`Conds2TableDual`、`AppendAddSelectionTraceStep`、`logicaltrace`、规则标志常量 | `crates/planner/src/logicalop/`、`core/`、`util/` |
| D13.3 hint | `PlanHints.IfPreferMergeJoin` 等 20 个 `IfPrefer*`、`MatchTableName`、`HintedTable` | `crates/planner/src/hint/hint.rs` |
| D05.2 会话变量 | `optimizer_enable_naaj`、`enable_semi_join_rewrite`、`enable_advanced_join_hint`、`tidb_opt_join_reorder_threshold`、`alloc_plan_column_id` | `crates/sessionctx/src/variable/session_fields.rs` |
| D00.2 错误 | `ErrAmbiguous`、`ErrUnknownColumn`、`ErrCTERecursiveForbiddenJoinOrder`、`ErrInvalidLateralJoin` 等 `plannererrors` 常量 | `crates/base/src/dbterror/plannererrors.rs` |

### 2.2 未移植（随部分一起移植的辅助函数，或 `blocked-by` 边界）

| 依赖 | 所属块 | 用在哪里 | 处理 |
|---|---|---|---|
| `util/null_misc.go` 全部 14 个函数（`IsNullRejected`、`ResetNotNullFlag` 等，约 310 行） | D10.10 | `ExtractOnCondition`、`deriveNotNullExpr`、`simplifyOuterJoin`、`ConvertOuterToInnerJoin`；`buildJoin`、`coalesceCommonColumns`、`BuildLogicalJoinSchema` 的 NOT NULL 处理 | 依赖都已就绪，**随第 A 部分整文件移植**，计入 D10.10 |
| `PlanCtxExtended.GetNullRejectCheckExprCtx`（`pkg/planner/plannersession/context.go:46`） | D09.13 | `IsNullRejected` 用的表达式上下文 | 随第 A 部分移植（表达式侧已有 `is_in_null_reject_check`） |
| `ExtractTableAlias`（`pkg/planner/util/misc.go:316`） | D10.10 | `SetPreferredJoinTypeAndOrder` 取两边的表别名 | 随第 A 部分移植 |
| `infoschema.TableIsView`（`pkg/infoschema/infoschema.go:246`） | D06.3 | `coalesceCommonColumns` 的列权限检查 | 随第 A 部分移植（小函数，依赖就绪） |
| `PreferINLJ` … `PreferRightAsHJProbe` 等 join 偏好位常量（`pkg/util/hint/hint.go:139-192`） | D13.3 | `SetPreferredJoinTypeAndOrder`、`setPreferredJoinTypeFromOneSide`、`containDifferentJoinTypes`、`buildSemiJoin` | 只有常量，照 D09.4 补 `PreferHashAgg` 的做法加进 `crates/planner/src/hint/hint.rs`（`u64`） |
| `PlanBuilder.TableHints`（D13.3） | D13.3 | `buildJoin`、`buildLateralJoin`、apply 构建 | Rust 现在的 `TableHints()` 恒为空 hint（hint 列表在 `blocked-by:D02.2`），不是 nil，所以 `SetPreferredJoinTypeAndOrder` 照 Go 走完整函数体，所有 `IfPrefer*` 返回 false |
| `expression.SortedExplainExpressionList`（D08.12，`pkg/expression/explain.go:213`） | D08.12 | `LogicalJoin.ExplainInfo` | 未移植前 `LogicalJoin.explain_info` 返回 `blocked-by:D10.9`（与 `LogicalSelection` / `LogicalAggregation` 一致） |
| `pkg/planner/funcdep`（`FDSet`） | D09.14 | `LogicalJoin.ExtractFD*`、`LogicalApply.ExtractFD` | 与 `DataSource` / `LogicalSelection` / `LogicalAggregation` 一致，作 `blocked-by:D09.14` 桩（只有 `tidb_enable_new_only_full_group_by_check = ON` 才调用，默认 OFF） |
| `cardinality.EstimateFullJoinRowCount`（`pkg/planner/cardinality/join.go`） | D10.6 | `LogicalJoin.DeriveStats` | D10.6 的 join 部分；第 B 部分开工前确认，未就绪则 `DeriveStats` 留 `blocked-by:D10.6` |
| `constraint.DeleteTrueExprsBySchema` | D09.8 | `LogicalJoin.PredicatePushDown` | 随第 B 部分移植（`crates/planner/src/core/constraint/exprs.rs` 文件头已说明它跟 join 的谓词下推一起来） |
| `ruleutil.BuildKeyInfoPortal`（`rule/util/build_key_info_misc.go:91`，一个函数变量钩子） | D09.13 | `LogicalJoin.PredicatePushDown:321`、D09.9 | 随第 B 部分落地为一个普通函数（不需要钩子：Rust 没有包循环引用的问题），计入 D09.13 |
| `coreusage.ExtractCorColumnsBySchema4LogicalPlan`（D10.10） | D10.10 | `buildJoin` 的 LATERAL 判断、`buildLateralJoin`、`setIsInApplyForCTE` | 随第 C 部分移植 |
| `setIsInApplyForCTE`（`:5855`） | D09.5 | apply 构建 | 随第 C 部分移植，计入 D09.5；Rust 还没有 `LogicalCTE`，CTE 分支随 D09.5 |
| 子查询构建（`expression_rewriter.go` 的 `handleExistSubquery` / `handleInSubquery` 等）、`buildMaxOneRow` | D09.6 | 调用第 C 部分的 `buildSemiApply` / `buildApplyWithJoinType` | **第 C 部分的前置条件**：与 D09.6 同步 |
| 解析器的 LATERAL（`tidb_typed.y:3415` 目前是占位节点）和 `ast::TableSource` 的 `lateral` 字段 | D02.2 | `containsLateralTableSource`、`isImmediateLateralTableSource`、`buildLateralJoin` | 第 A 部分先给 `ast::TableSource` 加 `lateral: bool`（解析器现在总是 false，LATERAL 语法在解析阶段就停在占位节点）；解析器支持 LATERAL 属于 D02.2，是第 C 部分 LATERAL 路径的前置条件 |
| `JoinReOrderSolver` 等 | D09.10 | 每个非 LATERAL 的 join 都置 `FlagJoinReOrder` | 优化器中现有 `JOIN_RE_ORDER_SOLVER` 桩（`blocked-by:D09.10`）保留；见 §3.8 |
| 物理 join 算子、枚举、代价、执行器 | D10.1、D10.3、D10.5、D10.9、D12.6 | 端到端 | 见 §3.8 |

### 2.3 与相关块的先后关系

- **D09.6 子查询**：D09.6 的改写器调用本块第 C 部分的 apply 构建函数，D09.6 的去关联规则调用 `Decorrelate`、`ColumnSubstituteAll`、`CanPullUpAgg`、`DeCorColFromEqExpr`。所以第 C 部分与 D09.6 一起做；D09.6 依赖第 A、B 部分（子查询会产生 semi join，经过谓词下推）。
- **D09.8 谓词下推**：`LogicalJoin.PredicatePushDown` 属于 D09.8，但它调用的 9 个辅助函数都在本块。本 spec 把它们放在第 B 部分，并把 `LogicalJoin.PredicatePushDown` 本身随第 B 部分一起移植（依赖都已就绪，计入 D09.8）。
- **D09.10 join 重排**：需要第 A 部分（`LogicalJoin` 字段、`SetPreferredJoinType`、`AttachOnConds`、`MergeSchema`）、第 B 部分（谓词下推之后才有 `EqualConditions`）和 D10.6 的 join 基数估算（贪心算法按代价选）。
- **D10.3 物理枚举**：需要第 A 部分的取值方法、第 E 部分的两个 MPP hint 函数、D10.1 的物理 join 算子。
- **D12.6 hash join 执行器**：需要 D10.1 的 `PhysicalHashJoin` 和 D10.9 的 `ResolveIndices`；只用到本块的 `JoinType`（第 A 部分）。

## 3. 函数清单与划分

tracker `data/func_blocks.tsv` 中 block 为 `D09.3` 的函数共 **73** 个。**归 X 0 个**；**豁免（EXC.B）4 个**；**已移植 2 个**；其余 67 个按依赖分为 A–E 五部分。

| 类别 | 函数数 | 内容 | 前置条件 |
|---|--:|---|---|
| 已移植（VS1-23） | 2 | `Conds2TableDual`、`AppendAddSelectionTraceStep`（`crates/planner/src/logicalop/logical_join.rs`） | 已合并 |
| 第 A 部分：join 构建与 `LogicalJoin` 基本算子 | 38 | inner / left / right / cross join，ON、USING、NATURAL，`FullSchema`，join hint，条件拆分，key 信息，取值方法，`ExtractFD*` 桩 | 无未满足的前置块（随带的辅助函数见 §2.2） |
| 第 B 部分：逻辑优化用的 `LogicalJoin` 方法 | 11 | 外连接化简、常量传播、等值条件整理、半连接改写、列裁剪辅助 | 第 A 部分 |
| 第 C 部分：`LogicalApply`、子查询用的 apply 构建、LATERAL | 14 | `LogicalApply` 算子，`buildApplyWithJoinType` / `buildSemiApply` / `buildSemiJoin` / `buildLateralJoin`，`Decorrelate`、`ColumnSubstituteAll` | 第 A、B 部分；与 D09.6 同步；LATERAL 另需 D02.2 |
| 第 D 部分：集合运算用的 join | 2 | `buildSemiJoinForSetOperator`、`unionJoinFieldType` | 第 A 部分；随 D09.5 |
| 第 E 部分：物理枚举用的 MPP hint 检查 | 2 | `hasMPPJoinHints`、`isJoinHintSupportedInMPPMode` | 随 D10.3 的 join 部分 |
| 豁免（EXC.B） | 4 | 只有 cascades 优化器调用 | 不移植 |
| **合计** | **73** | `2 + 38 + 11 + 14 + 2 + 2 + 4 = 73` | — |

### 3.1 第 A 部分：join 构建与 `LogicalJoin` 基本算子（38 个）

**`logical_plan_builder.go`（9 个）**：`findJoinFullSchema`（`:652`）、`containsLateralTableSource`（`:683`）、`isImmediateLateralTableSource`（`:711`）、`PlanBuilder.buildJoin`（`:727`，LATERAL 分支在第 C 部分前停在 `blocked-by:D09.3`）、`PlanBuilder.buildUsingClause`（`:1102`）、`PlanBuilder.buildNaturalJoin`（`:1126`）、`PlanBuilder.coalesceCommonColumns`（`:1140`）、`findColFromNaturalUsingJoin`（`:1660`）、`PlanBuilder.buildJoinFuncDepend`（`:3615`）。

**`operator/logicalop/logical_join.go`（29 个）**：

| 分组 | Go 函数（行号） |
|---|---|
| 类型与构造（5） | `init`（`:66`，常量断言）、`JoinType.IsOuterJoin`（`:77`）、`JoinType.IsSemiJoin`（`:83`）、`JoinType.String`（`:88`）、`LogicalJoin.Init`（`:172`） |
| Plan 接口（2） | `ExplainInfo`（`:180`，D10.9 前为桩）、`ReplaceExprColumns`（`:202`） |
| 条件（5） | `AttachOnConds`（`:1090`）、`AppendJoinConds`（`:1096`）、`ExtractOnCondition`（`:1288`）、`pushDownConstExpr`（`:1385`）、`extractOnCondition`（`:1416`） |
| schema 与 key（3） | `BuildKeyInfo`（`:389`）、`MergeSchema`（`:1156`）、`BuildLogicalJoinSchema`（`:2051`） |
| join hint（4） | `SetPreferredJoinTypeAndOrder`（`:1423`）、`SetPreferredJoinType`（`:1568`）、`containDifferentJoinTypes`（`:1875`）、`setPreferredJoinTypeFromOneSide`（`:1914`） |
| 取值方法（6） | `getGroupNDVs`（`:1262`）、`PreferAny`（`:1276`）、`GetJoinKeys`（`:959`）、`GetNAJoinKeys`（`:970`）、`GetPotentialPartitionKeys`（`:980`）、`IsNAAJ`（`:787`） |
| 函数依赖（4，`blocked-by:D09.14` 桩） | `ExtractFD`（`:715`）、`ExtractFDForSemiJoin`（`:798`）、`ExtractFDForInnerJoin`（`:818`）、`ExtractFDForOuterJoin`（`:862`） |

理由：构建期要用到的全部函数，加上不依赖未移植块的取值方法，一次移植，计划构建向量可以一次覆盖。NATURAL / USING 也放在这里，因为 `FullSchema` 每个 join 都有（`:883`），`unfoldWildStar`（`t1.*`）、`buildProjectionField`、表达式改写的列查找在任何 join 上都会读它，拆开反而要多留桩。

### 3.2 第 B 部分：逻辑优化用的 `LogicalJoin` 方法（11 个）

`simplifyOuterJoin`（`:330`）、`LogicalJoin.ExtractUsedCols`（`:1113`）、`addCandidateSelection`（`:1244`）、`LogicalJoin.updateEQCond`（`:1581`）、`LogicalJoin.getProj`（`:1681`）、`LogicalJoin.outerJoinPropConst`（`:1698`）、`mergeOnClausePredicates`（`:1725`）、`LogicalJoin.SemiJoinRewrite`（`:1749`）、`appendTopNPushDownJoinTraceStep`（`:1829`）、`DeriveOtherConditions`（`:1964`）、`deriveNotNullExpr`（`:2009`）。

理由：这些函数只被逻辑优化规则调用（谓词下推、常量传播、列裁剪、TopN 下推），与对应的其他块方法（§3.7）一起移植，才能端到端测试。`SemiJoinRewrite` 对 inner join 直接返回（`:1752-1753`），但 `PredicatePushDown` 对每个 join 都调用它，所以整个函数放在这里；它的改写主体用到的 `LogicalAggregation`、`LogicalProjection` 已就绪，只在 semi join（来自 D09.6 / D09.5）上运行。

### 3.3 第 C 部分：`LogicalApply`、apply 构建与 LATERAL（14 个）

**`logical_plan_builder.go`（4 个）**：`PlanBuilder.buildLateralJoin`（`:954`）、`PlanBuilder.buildApplyWithJoinType`（`:5815`）、`PlanBuilder.buildSemiApply`（`:5836`）、`PlanBuilder.buildSemiJoin`（`:5879`）。

**`operator/logicalop/logical_apply.go`（8 个）**：`LogicalApply.Init`（`:52`）、`ExplainInfo`（`:94`）、`ReplaceExprColumns`（`:99`）、`findChildFullSchema`（`:121`）、`ExtractFD`（`:333`，`blocked-by:D09.14` 桩）、`CanPullUpAgg`（`:375`）、`DeCorColFromEqExpr`（`:387`）、`getGroupNDVs`（`:415`）。

**`operator/logicalop/logical_join.go`（2 个）**：`LogicalJoin.Decorrelate`（`:991`）、`LogicalJoin.ColumnSubstituteAll`（`:1008`）。

理由：调用方全部在 D09.6（子查询改写、去关联）和 LATERAL 路径；单独移植无法测试。apply 的执行器在 D12.10。

### 3.4 第 D 部分：集合运算用的 join（2 个，随 D09.5）

`unionJoinFieldType`（`:1995`，只被 `buildProjection4Union:2059` 调用）、`PlanBuilder.buildSemiJoinForSetOperator`（`:2193`，只被 `INTERSECT` / `EXCEPT` 的构建 `:2268, 2292` 调用）。理由：调用方都在 D09.5，随 D09.5 的实现任务移植，计入 D09.3。

### 3.5 第 E 部分：物理枚举用的 MPP hint 检查（2 个，随 D10.3）

`hasMPPJoinHints`（`:7858`）、`isJoinHintSupportedInMPPMode`（`:7863`），只被 `exhaust_physical_plans.go:2126-2127` 调用。理由：没有出边的小函数，调用方是 D10.3 的 join 枚举，随它移植，计入 D09.3。

### 3.6 豁免（EXC.B：只有 cascades 优化器调用，4 个）

经全仓库 `pkg/` 检索非测试代码的调用方：

1. `LogicalApply.Hash64`（`logical_apply.go:60`）、2. `LogicalApply.Equals`（`:71`）：只由新 cascades 的 memo 经 `base.HashEquals` 调用（例外 B4）。
3. `LogicalJoin.ExtractJoinKeys`（`logical_join.go:1104`）：只有旧 cascades 规则调用（`cascades/transformation_rules.go:1971, 2026`，例外 B3）。
4. `LogicalJoin.Shallow`（`logical_join.go:792`）：只有旧 cascades 规则调用（`transformation_rules.go:999, 1052, 2457, 2515`，例外 B3）。

tidb-rs 不支持 `tidb_enable_cascades_planner=1`（与 D09.4、D09.14 一致），在 Rust 文件中标注 `// go-exception: EXC.B`。

### 3.7 随本块一并移植的其他块函数（计入各自块）

| 随哪部分 | Go 函数（文件） | 计入块 | 原因 |
|---|---|---|---|
| 第 A 部分 | `util/null_misc.go` 全部 14 个函数；`ExtractTableAlias`（`util/misc.go:316`） | D10.10 | `ExtractOnCondition`、`SetPreferredJoinTypeAndOrder` 的函数体直接调用 |
| 第 A 部分 | `PlanCtxExtended.GetNullRejectCheckExprCtx`（`plannersession/context.go:46`） | D09.13 | `IsNullRejected` 需要 |
| 第 A 部分 | `infoschema.TableIsView`（`infoschema/infoschema.go:246`） | D06.3 | `coalesceCommonColumns` |
| 第 A 部分 | 现有 L2 函数中 join 的分支：`PlanBuilder.unfoldWildStar`（`:4163`）、`PlanBuilder.buildProjectionField`（`:1544`，经 `findColFromNaturalUsingJoin`）、`findFieldNameFromNaturalUsingJoin`（`expression_rewriter.go:2653`）、`havingWindowAndOrderbyExprResolver.resolveFromPlan`（`:2693`）、`buildAggregation`（`:381`）、`resolveGbyExprs`（`:4115`）、`checkOnlyFullGroupByWithGroupClause` / `WithOutGroupClause`（`:3905, 3982`）、`existsCartesianProduct`（`optimizer.go:1192`）、`HasMaxOneRow`（`logical_plans_misc.go:73`）、`canPushToCopImpl` 的 join 分支 | D09.1 / D09.2 / D09.4 / D09.13 / D09.14 | 这些函数现在都假设"没有 join"（注释写着 joins are D09.3）。第 A 部分一合并，join 就会走到它们，不补就是静默偏离；Apply 的分支留到第 C 部分 |
| 第 B 部分 | `LogicalJoin.PredicatePushDown`（`:224`）、`ConstantPropagation`（`:526`）、`ConvertOuterToInnerJoin`（`:731`）；`constraint.DeleteTrueExprsBySchema` | D09.8 | 第 B 部分的辅助函数只为它们服务 |
| 第 B 部分 | `LogicalJoin.PruneColumns`（`:363`）、`rule_eliminate_projection.go:181-184` 的 join / apply 分支 | D09.7 | 列裁剪对每个 join 都运行 |
| 第 B 部分 | `LogicalJoin.PushDownTopN`（`:435`）、`pushDownTopNToChild`（`:1163`） | D09.11 | 用到 `appendTopNPushDownJoinTraceStep` |
| 第 B 部分 | `LogicalJoin.ExtractCorrelatedCols`（`:685`） | D09.6 | 优化器入口对整棵树调用 |
| 第 B 部分 | `LogicalJoin.PreparePossibleProperties`（`:651`） | D10.2 | 物理优化入口 |
| 第 B 部分 | `LogicalJoin.DeriveStats`（`:567`）、`ExtractColGroups`（`:625`），以及 `cardinality/join.go` 中它们用到的函数 | D10.6 | D10.6 的 join 部分已就绪时一起做，否则留 `blocked-by:D10.6` |
| 第 B 部分 | `ruleutil.BuildKeyInfoPortal`、`collectPredicateColumnsForJoin`（`collect_column_stats_usage.go:166`）、`rule_constant_propagation` 的 join 分支 | D09.13 | 谓词下推和统计列收集遇到 join 时调用 |
| 第 C 部分 | `setIsInApplyForCTE`（`:5855`） | D09.5 | apply 构建调用 |
| 第 C 部分 | `coreusage.ExtractCorColumnsBySchema4LogicalPlan` | D10.10 | LATERAL 判断 |
| 第 C 部分 | `LogicalApply.PruneColumns`（`:141`）、`DeriveStats`（`:198`）、`ExtractColGroups`（`:282`）、`ExtractCorrelatedCols`（`:310`），以及第 A 部分各函数中 Apply 的分支 | D09.7 / D10.6 / D09.6 / D09.1 / D09.4 | 同上 |

`LogicalJoin` / `LogicalApply` 的 `ExhaustPhysicalPlans` 保持 `blocked-by:D10.3`。

### 3.8 让最简单的两表等值 inner join 在新执行栈上跑通的最小块序列

目标语句：`SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.a = t2.a`，两表在 join 键上没有可用的索引。下面每一步合并后，join 语句都还在执行之前停在下一个 `blocked`（1235），由路由器回退到原型，不会执行两次。

1. **D09.3 第 A 部分**（本块）：join 构建与 `LogicalJoin`，连同 §3.7 中第 A 部分的其他块函数。之后计划构建与 Go 一致，语句停在逻辑优化的第一个 join 方法。
2. **D09.3 第 B 部分**（本块）+ 随带的 D09.8 / D09.7 / D09.11 / D09.6 / D10.2 / D09.13 方法：列裁剪、谓词下推（得到 `EqualConditions`）、常量传播。
3. **D10.6 的 join 部分**：`LogicalJoin.DeriveStats`、`cardinality/join.go`。可以与第 B 部分同一个 PR。
4. **D09.10 的两表贪心路径**：`JoinReOrderSolver.Optimize`、`extractJoinGroup`、贪心算法及其公共部分、`joinorder.CheckAndGenerateLeadingHint`。每个非 LATERAL 的 join 都置 `FlagJoinReOrder`，所以即使两张表也必须有它（外连接还需要 `OuterJoinEliminator`，不在最小序列里）。
5. **D10.1 + D10.3 + D10.5 的 join 部分**：`PhysicalHashJoin`、`PhysicalMergeJoin`、`PhysicalIndexJoin` 的定义与 `attach2Task`；`ExhaustPhysicalPlans4LogicalJoin` 的全部候选（hash join 两个方向、merge join、index join），因为 EXPLAIN 要与 Go 一致，候选不能少；代价 ver2；以及本块第 E 部分。
6. **D10.9 的 join 部分**：`ResolveIndices`、EXPLAIN 文本，`optimizeByShuffle` 的 merge join 分支（`crates/planner/src/core/plan.rs:133` 现在跳过）。
7. **D12.6 的 hash join v1**（`tidb_hash_join_version` 默认 `legacy`）+ D12.0 builder 的 `buildHashJoin`。merge join（D12.8）、index join（D12.7）在执行器构建时停在 `blocked`，仍可回退。
8. 维护者用 `tools/ab` 做端到端对比（§6.4）。

其他块需要先做的部分：D02.2（解析器去掉 `FULL OUTER JOIN` 的合成，见 §7 第 7 条；与最小序列无关，但第 A 部分要为它留显式桩）、D12.0 的 `table_ids` 改法（§4.7）。

## 4. Go → Rust 的结构映射

### 4.1 文件与类型

| Go 文件 / 类型 | Rust 文件 / 类型 | 说明 |
|---|---|---|
| `operator/logicalop/logical_join.go`（`JoinType`） | `crates/planner/src/logicalop/logical_join.rs` | `#[repr(u8)] enum JoinType { InnerJoin = 0, …, AntiLeftOuterSemiJoin = 6 }`，取值与 Go 相同（`conflict_detector.go` 依赖取值）；Go `init()` 中 `intest.Assert` 的检查写成编译期常量断言（L2，同 `crates/types/src/vector.rs:25` 的先例）。`String()` 按 `M-NAME` 写成固有方法 `string()` |
| `LogicalJoin` | 同上 | `LogicalKind::Join`；`base: BaseLogicalPlan`、`sp: LogicalSchemaProducer`；`join_type`、`reordered`、`cartesian_join`、`straight_join`、`hint_info: Option<Arc<PlanHints>>`、`prefer_join_type: u64`、`prefer_join_order: bool`、`left_prefer_join_type` / `right_prefer_join_type: u64`、`equal_conditions: Vec<ExprRef>`（元素都是 `ScalarFunction`，取用处断言）、`na_eq_conditions`、`left_conditions` / `right_conditions` / `other_conditions: Vec<ExprRef>`、`left_properties` / `right_properties: Vec<Vec<Arc<Column>>>`、`default_values: Vec<Datum>`、`full_schema: Option<Schema>`、`full_names: NameSlice`、`prefer_correlate`、`equal_cond_out_cnt: f64`、`from_decorrelated_apply` |
| `operator/logicalop/logical_apply.go`（`LogicalApply`） | `crates/planner/src/logicalop/logical_apply.rs` | `LogicalKind::Apply`；`join: LogicalJoin`（嵌入）、`cor_cols: Vec<CorrelatedColumn>`、`no_decorrelate`、`is_lateral` |
| `logical_plan_builder.go` 的本块函数 | `crates/planner/src/core/logical_plan_builder.rs`（保持文件头 `//! go-file: pkg/planner/core/logical_plan_builder.go`） | `PlanBuilder<'ast>` 增加 `outer_schemas` / `outer_names` / `lateral_outer_count`（第 C 部分，可能由 D09.6 先加） |
| `pkg/parser/ast` `TableSource.Lateral` | `crates/parser/src/ast.rs` `TableSource.lateral: bool` | 第 A 部分加字段，解析器现在总是 false（§2.2） |

### 4.2 固定写法（N1 §4，映射规则 `M-PLAN` / `M-SELF` / `M-IFACE`）

1. **算子与嵌入**：`LogicalJoin` 用 `logical_plan_boilerplate!(Join, producer)`。`LogicalApply` 嵌入的是整个 `LogicalJoin`，宏的 `base()` / `schema_producer()` 访问器指向 `self.join.base` / `self.join.sp`；另提供 `join()` / `join_mut()`。
2. **Go 的类型断言不会把 apply 当成 join**：Go 中 `p.(*LogicalJoin)` 对 `*LogicalApply` 不成立。Rust 的 `match p.kind()` 中 `Join` 与 `Apply` 分开写，只在 Go 显式经过嵌入字段（`ap.LogicalJoin.SetPreferredJoinTypeAndOrder(...)`、`la.LogicalJoin.ExplainInfo()`）或经过 `self` 调用 `LogicalJoin` 方法（如 `SemiJoinRewrite` 中的 `p.Self().(*LogicalApply)`，`:1755`）的地方，才通过 `join()` 访问。`SemiJoinRewrite` 等需要 `Self()` 的方法接收 `this: PlanId` 并按 `kind()` 判断（`M-SELF`）。
3. **trait 方法要全部显式写出**：Go 中 `LogicalJoin` 重写的每个 `LogicalPlan` 接口方法，第 A 部分都在 Rust 中显式实现，没移植的返回所属块的 `blocked(...)`（`PredicatePushDown` / `ConstantPropagation` / `ConvertOuterToInnerJoin` → D09.8，`PruneColumns` → D09.7，`PushDownTopN` → D09.11，`DeriveStats` / `ExtractColGroups` → D10.6，`PreparePossibleProperties` → D10.2，`ExtractCorrelatedCols` → D09.6，`ExhaustPhysicalPlans` → D10.3，`ExtractFD` → D09.14），不能落到 `BaseLogicalPlan` 的默认实现（那会静默偏离）。第 B 部分把依赖就绪的这些桩换成实现。
4. **摘出分派**：`PredicatePushDown` 等改树的方法用 `logical_dispatch!` 摘出 `LogicalJoin`；`getProj` 改的是子节点（另一个 `PlanId`），经 `ctx.arena` 访问，摘出期间不回查 `this`。本块不需要 `PlanCtx::split()`；一次分派中不会两次摘出同一个节点。
5. **plan id 与列 id 的分配顺序照 Go**：`buildJoin` 中先建左右子树，再 `handleHelper`，再 `LogicalJoin::init`，inner join 带 ON 时改写 ON 之后再 `LogicalSelection::init`；`buildSemiJoin` 先 `LogicalJoin::init`，`asScalar` 时再 `alloc_plan_column_id`；`buildSemiApply` 把这个 join 的值移进 `LogicalApply`，只改 TP，**不调用 `alloc_plan_id`**；`buildLateralJoin` 在提取关联列之后 `LogicalApply::init`；`getProj` 新建投影时在谓词下推当中分配 plan id。
6. **`FullNames` 按值复制**：`buildJoin:889-897`、`buildLateralJoin` 逐个复制 `FieldName`（`Arc::new(FieldName::clone(&n))`），因为 USING 之后要在副本上设 `Redundant`；输出名 `OutputNames` 则共享子节点的 `Arc<FieldName>`（Go 是指针拷贝）。
7. **`coalesceCommonColumns` 的原地挪动**：Go 用 `copy` 在两份 `Clone` 出来的列切片和 `Shallow` 出来的名字切片上原地挪动（`:1255-1267`），`RIGHT JOIN` 时 `lColumns` 与 `rsc.Columns` 是同一个底层数组，之后用 `lsc.Columns[i]` / `rsc.Columns[i]` 取共有列（`:1323`）。Rust 用 `Vec::rotate_right` 在同样的两份 `Vec` 上做相同的挪动，交换时交换的是对这两份 `Vec` 的引用，保证 `lc` 总是原左表的列、`rc` 总是原右表的列（L1，逐行对照）。

### 4.3 错误顺序与 `defer`

- **`buildJoin` 的报错顺序**：左边的错误 → 右边的错误 → （LATERAL 的错误）→ 递归 CTE 在 LEFT JOIN 右边（`:818-820`，Rust 还没有 `LogicalCTETable`，此分支随 D09.5）→ USING：左边同名列、右边同名列（`ErrAmbiguous`）→ NATURAL：右边扫描时左边计数大于 1、再查右边计数（`ErrAmbiguous`）→ USING 找不到列（`ErrUnknownColumn`）→ 建 `EQ` 的错误 → ON：改写的错误 → ON 里有子查询（1105）。
- **`checkColPriv` 的恢复**：ON 子句（`:923-927`）在改写返回后、检查错误之前恢复，Rust 写成 `let res = self.rewrite(...); self.check_col_priv = origin; let (expr, plan) = res?;`；`coalesceCommonColumns`（`:1233-1237`）用 `defer`，Rust 用 drop guard（`M-DEFER`），配一个"`ErrUnknownColumn` 返回后 `check_col_priv` 已恢复"的测试。
- **LATERAL 的 `defer`**（`:776-780`）：弹出 `outer_schemas` / `outer_names` 并 `lateral_outer_count -= 1`，用 drop guard，配一个"右边构建出错后栈已恢复"的测试（第 C 部分）。
- **Go 忽略错误的地方**：`LogicalJoin.PredicatePushDown:322-325` 在 `SemiJoinRewrite` 出错时返回原计划、不报错。与 D09.9 spec §4.4 一致（负责人 2026-11-24 在 PR #340 确认）：错误是 `blocked` 时向上返回，其他错误照 Go 吞掉（只有在命中未移植块的桩时才有差异，见 §7 第 3 条）。
- **`ON condition doesn't support subqueries yet`**：Go 是 `errors.New`，客户端看到 1105。Rust 按 `M-ERR` 建一个 1105 错误，文案相同。D09.6 之前，ON 里的子查询先在改写器停在 `blocked-by:D09.6`。

### 4.4 并发与内存记账（A1）

- 计划构建同步运行在会话的 task 上，没有 goroutine、channel、锁（`M-GO`、`M-CH`、`M-LOCK` 不适用）。`LogicalJoin` / `LogicalApply` 在语句级 arena 中独占。
- **内存记账（A1，`M-MEM`）**：Go 的计划构建不调用 `memory.Tracker`，本块没有 `MemoryUsage` 方法（物理 join 的 `MemoryUsage` 在 D10.1，用于计划缓存）。join 的内存记账全部在执行器：hash join 的哈希表、行容器和落盘由 D12.6 按 A1 的 `TrackedAlloc` 记到语句的 tracker 树上，见 §4.7。

### 4.5 其他映射规则

- **`M-MAPORDER`（1 处）**：`coalesceCommonColumns:1306` 遍历 `filter` map，USING 中有多个找不到的列时 Go 报哪一个不确定。Rust 按 USING 列表的顺序报第一个找不到的列（Go 也可能报它）；govec 只放一个找不到的列的用例。
- **`M-INT`**：Go `uint` 的 `PreferJoinType` 等用 `u64`（与 `hint.rs` 的常量一致）；`isJoinHintSupportedInMPPMode` 的 `bits.OnesCount` 用 `u64::count_ones`。
- **`M-NAME`**：`JoinType.String` → `string()`。
- **`M-FMA` / `M-F2I`**：tracker `data/go_fma_sites.tsv` 中本块 3 个文件没有位置；`getGroupNDVs` 等没有浮点转整数。
- **AST**：本块不修改 AST（`buildSemiJoin:5881` 改的是调用方传入的条件切片，不是 AST；`coalesceCommonColumns` 改的是本地的 `filter` map）。Rust 用返回新 `Vec` 的方式，没有可见差异。

### 4.6 共享修改的审计（N1 §5，映射规则 `M-FT` / `M-SHARE`）

对 `logical_plan_builder.go` 的本块行范围和 `logical_join.go`、`logical_apply.go` 全文运行 `specs/templates/plan.md` §4 的两个 grep，并读代码补充：

| Go 位置 | 写入 / 共享内容 | 结论 | 处理 |
|---|---|---|---|
| `logical_plan_builder.go:829, 1005, 5821`（`MergeSchema` 后 `ResetNotNullFlag`）；`coalesceCommonColumns:1144-1146`；`logical_join.go:2061-2065` | `ResetNotNullFlag`（`util/null_misc.go:469`）先复制列和 `FieldType` 再去掉 NOT NULL | **S** | 改的是副本；Rust 用 `Arc::make_mut` |
| `logical_plan_builder.go:1331, 1333` | `FullNames[i].Redundant = true` | **S** | `FullNames` 是 `buildJoin` 按值复制出来的 |
| `logical_plan_builder.go:2006-2023`（`unionJoinFieldType`） | 改 `AggFieldType` 新建的类型 | **S** | 第 D 部分 |
| **`logical_plan_builder.go:2204`（`buildSemiJoinForSetOperator`）** | `joinPlan.SetSchema(leftPlan.Schema())`，**不 Clone**，join 与左边的 `LogicalAggregation`（`buildDistinct`）共享同一个 `*Schema` | **待第 D 部分判定**（推断为 V0：之后改 schema 的规则对 join 都是整体换新 schema，见 `MergeSchema`） | 默认写时复制；第 D 部分实现时按 N1 §5 逐处核对 `INTERSECT` / `EXCEPT` 在列裁剪前后的计划，若 EXPLAIN 对照有差异再显式模拟 |
| `logical_plan_builder.go:5895, 5902`（`buildSemiJoin`） | 外表 schema `Clone` 后追加新列 | **S** | — |
| `logical_join.go:1691`（`getProj`） | 新投影的 schema 是子节点 schema 的 `Clone` | **S** | — |
| `logical_join.go:1809-1825`（`SemiJoinRewrite`） | 新建或 `Clone` 的 schema | **S** | — |
| `logical_join.go:2044`（`Conds2TableDual`） | `dual.SetSchema(p.Schema())` 共享 | 已移植（VS1 plan 的审计表：共享期间不写） | 不变 |
| **`logical_apply.go:104`（`LogicalApply.ReplaceExprColumns`）** | `coCol.Column = *dst`：`CorCols` 中的 `*CorrelatedColumn` 与内层子树表达式中的关联列是**同一个对象**，改一处，内层所有引用都跟着变 | **依赖共享可见性**（`M-SHARE` 的第二类，需要改设计） | Rust 的关联列在表达式中按值保存（N4 §3.5 用槽 `CorrSlot` 代替 `Data` 指针）。第 C 部分：`replace_expr_columns` 改 `cor_cols` 的同时，**按 `CorrSlot` 同步改写内层子树中所有同一个槽的关联列**（经 `Arc::make_mut` 写时复制），标 `// Go aliasing (M-SHARE): logical_apply.go:104, CorCols share *CorrelatedColumn with inner expressions`，配回归测试（投影消除替换外表列之后，内层条件的关联列也被替换，`ResolveIndices` 结果与 Go 相同） |
| `logical_plan_builder.go:5882`（`buildSemiJoin`） | `expr.Decorrelate(...)`：Go 的 `ScalarFunction.Decorrelate` 原地改参数 | **S**（`M-EXPR`） | 条件刚由改写器建出，独占；Rust 的 `Decorrelate` 已按写时复制移植 |
| `logical_plan_builder.go:832-833`（输出名） | 输出名切片拷贝的是子节点 `*FieldName` 指针，本块不写这些 `FieldName` | **S** | 共享 `Arc<FieldName>` |

没有需要显式模拟的 `M-FT` 位置（`grep 'Go aliasing (M-FT)'` 不新增）；新增的 `M-SHARE` 模拟只有 `logical_apply.go:104` 一处。

### 4.7 对其他块的要求

- **D09.4**：第 A 部分接通 `findJoinFullSchema`（`buildAggregation`、`resolveGbyExprs`）、`buildJoinFuncDepend`（`checkOnlyFullGroupBy*`）和 `resolveFromPlan` 的 join 分支，删掉这些地方"单表、没有 join"的注释和 L2 理由。
- **D09.13**：`optimizer.rs` 中标为 `D09.3` 的两个规则桩归属有误：`ConvertOuterToInnerJoin`（`rule_outer_to_inner_join.go`）在 tracker 属于 **D09.8**，而且 v8.5.8 中**没有任何地方置 `FlagConvertOuterToInnerJoin`**（只在 `rule/logical_rules.go:31` 定义），这个规则从不运行；`OuterJoinEliminator`（`rule_join_elimination.go`）属于 **D09.10**。第 A 部分把两者改标为 `blocked-by:D09.8` / `blocked-by:D09.10`（数量不变）。
- **D09.10**：贪心 / DP 重排需要的 `LogicalJoin` 方法全部在第 A 部分；`SetPreferredJoinType` 发出的 hint 冲突 warning 照 Go。
- **D10.3 / D10.1**：第 E 部分随 D10.3 的 join 部分移植；物理 join 的候选必须全部枚举（含 index join 和 merge join），即使它们的执行器还没移植。
- **D12.0 / D12.6**：`crates/executor/src/builder.rs:1295, 1311` 的 TODO：`stmt_ctx_mut()` 用 `Arc::get_mut`，在已有执行器持有 `StatementContext` 时会 panic。多子节点的执行器（join、union、apply）合并前必须先改（例如把 `table_ids` 改成语句内的可追加结构），否则第二个 TableReader 构建时会 panic。hash join 的哈希表、行容器、落盘按 A1 记账；是否等 N6（哈希 join 数据结构的例外，原则同意、尚无 spec）由 D12.6 的 spec 决定（D12.3 的先例是不等）。
- **D02.2**：解析器把 `FULL OUTER JOIN` 合成为 `JoinType::FullJoin`（`crates/parser/src/lexer.rs:641-643`、`grammar/tidb_typed.y:3491`），而 v8.5.8 的 `parser.y:10107-10115` 中 `JoinType` 只有 `LEFT` / `RIGHT`，Go 对 `t1 FULL OUTER JOIN t2` 报语法错误（`FULL` 只能作别名，后面的 `OUTER JOIN` 不合语法；按 `parser.y` 推断，未在集群上执行）。建议维护者开 issue 由 D02.2 修正解析器；在那之前，第 A 部分的 `build_join` 对 `FullJoin` 显式返回 `blocked("D02.2", ...)`，**不能落进"其余都是 inner join"的分支**。另外 LATERAL 的解析（第 C 部分前置）也在 D02.2。

## 5. 边界桩

### 5.1 本块消除的现有桩

按当前仓库 `grep -rn 'D09\.3' crates/` 与 tracker `data/stubs.tsv`（4 行）：

- **第 A 部分**：`logical_plan_builder.rs` 的 `build_join`（`stubs.tsv`：`build_join`）、`build_projection_field`（`stubs.tsv`：`build_projection_field`）；`optimizer.rs` 的 `CONVERT_OUTER_TO_INNER_JOIN`、`OUTER_JOIN_ELIMINATOR` 两行**改标**为 D09.8 / D09.10（§4.7，tracker 中块名随之改）。
- 同时更新的注释与断言：`logical_plan_builder.rs:12, 641-659, 854-890, 1448, 1613, 1975-1998, 3168, 3259, 3328, 3381, 3434, 3477, 3565-3573` 的 L2 理由和注释；`expression_rewriter.rs:2285-2288`；`optimizer.rs:1150-1151`；`rule_eliminate_projection.rs:196`（第 B 部分）；`collect_column_stats_usage.rs:425`（第 B 部分）；`rule/rule_constant_propagation.rs:3`（第 B 部分）；`plan.rs:133`（D10.9）；`logical_plans_misc.rs:45, 209`；`crates/planner/tests/govec_selectbuild.rs:91`、`govec_findbest.rs:91`、`govec_optdriver.rs:105` 的 `("D09.3", 6)`（第 A 部分之后这些记录推进到下游的块，按实际改为对应块名与条数）。

### 5.2 本块保留或新增的边界桩

| Rust 位置 | Go 位置 | 类型 | 调用时行为 |
|---|---|---|---|
| `build_join` 的 LATERAL 分支（第 A–B 部分期间） | `logical_plan_builder.go:756-815` | 临时 `blocked-by:D09.3`（第 C 部分解除；解析器现在到不了这里） | 1235 |
| `build_join` 对 `ast::JoinType::FullJoin` | 无（Go 语法不接受） | 临时 `blocked-by:D02.2` | 1235，D02.2 修正解析器后删除 |
| `LogicalJoin.explain_info` | `logical_join.go:180` | 临时 `blocked-by:D10.9` | 1235，与 `LogicalSelection` 一致 |
| `LogicalJoin.extract_fd*`（4 个）、`LogicalApply.extract_fd` | `logical_join.go:715, 798, 818, 862`、`logical_apply.go:333` | 临时 `blocked-by:D09.14` | 1235（默认开关关闭时不可达） |
| `LogicalJoin` 的 `predicate_push_down`、`constant_propagation`、`convert_outer_to_inner_join`（D09.8）、`prune_columns`（D09.7）、`push_down_top_n`（D09.11）、`derive_stats` / `extract_col_groups`（D10.6）、`prepare_possible_properties`（D10.2）、`extract_correlated_cols`（D09.6）（仅第 A 部分期间） | `logical_join.go` 各处 | 临时，标各自的块 | 1235；第 B 部分换成实现 |
| `LogicalJoin` / `LogicalApply` 的 `exhaust_physical_plans` | `logical_join.go:680`、`logical_apply.go:305` | 临时 `blocked-by:D10.3` | 1235 |
| `buildJoin` 中递归 CTE 在 LEFT JOIN 右边的检查、`setIsInApplyForCTE` 的 CTE 分支 | `logical_plan_builder.go:818-820, 5859-5866` | 不可达（Rust 没有 `LogicalCTETable` / `LogicalCTE`），随 D09.5 | 不另加桩，L2 理由写明 |
| `buildUsingClause` / `buildNaturalJoin` 中 `inUpdateStmt || inDeleteStmt` 分支 | `logical_plan_builder.go:1112-1115, 1132-1135` | 不可达：UPDATE / DELETE 的计划构建属于写路径（D09.15），只读阶段由 Go TiDB 承担，Rust 的 `PlanBuilder` 没有这两个标志 | 不另加桩，L2 理由写明 |

**棘轮说明**（`scripts/check_stub_ratchet.py`）：第 A 部分删掉 `build_join`、`build_projection_field` 两个整函数桩，新纳入 `LogicalJoin` 类型，它超出本部分的 trait 方法写成桩（port-block §3 允许的第 2 种情况），另有 LATERAL 和 `FullJoin` 两个细化桩（第 1 种情况）；各部分的 PR 正文逐条列出脚本实际报告的净增数。

## 6. 测试计划

### 6.1 要移植的 Go 单测

| Go 测试 | Go 位置 | 处理 |
|---|---|---|
| `TestJoinPredicatePushDown`（`:232`）、`TestOuterWherePredicatePushDown`（`:273`）、`TestSimplifyOuterJoin`（`:322`）、`TestAntiSemiJoinConstFalse`（`:361`）、`TestDeriveNotNullConds`（`:392`）、`TestDupRandJoinCondsPushDown`（`:522`）、`TestConflictedJoinTypeHints`（`:3645`）、`TestSimplyOuterJoinWithOnlyOuterExpr`（`:3673`） | `logical_plans_test.go` | 依赖 `testkit` 的模拟存储，照 D09.4 的做法把其中的 SQL 提取进 govec 生成器，按部分做逻辑计划逐项对比（第 B 部分；含 hint 的用例等 D02.2 支持 hint 列表） |
| `TestJoinReOrder`（`:671`）、`TestOuterJoinEliminator`（`:3207`） | `logical_plans_test.go` | 属于 D09.10，本块只提供算子 |
| `TestLateralJoin*`（13 个，`:29-793`） | `lateral_join_test.go` | 第 C 部分，等 D02.2 解析 LATERAL；改成 govec 计划向量和会话测试 |
| `TestLogicalApplyClone`（`:73`） | `operator/logicalop/logicalop_test/logical_operator_test.go` | 第 C 部分，改写成 Rust 单元测试 |
| `hash64_equals_test.go` 中 join / apply 的部分 | `operator/logicalop/logicalop_test/` | EXC.B，不移植 |

### 6.2 govec 向量

- **扩展现有领域 `selectbuild`**（tracker `govec/domains/selectbuild.go`，`Blocks` 中加 `D09.3`）：
  - 计划节点快照加 `LogicalJoin`：`join_type`、`equal` / `na_equal` / `left` / `right` / `other` 条件、`cartesian_join`、`straight_join`、`prefer_join_type` / `left_prefer_join_type` / `right_prefer_join_type` / `prefer_join_order`、`schema`（含 NOT NULL 标志）、`output_names`、`full_schema`、`full_names`（含 `redundant`）；第 C 部分加 `LogicalApply` 的 `cor_cols`、`no_decorrelate`、`is_lateral`。
  - **第 A 部分**的用例：现有 `EXPECTED_BLOCKED` 中 6 条 `D09.3` 记录转为正常比对（`SELECT * FROM t2 JOIN t3`、`SELECT * FROM t2, t3` 等）；`JOIN` / `INNER JOIN` / `CROSS JOIN` / 逗号 / `STRAIGHT_JOIN`（含 `SELECT STRAIGHT_JOIN`）；`LEFT` / `RIGHT [OUTER] JOIN` 的 NOT NULL 清除（客户端列定义）；ON 条件（等值、非等值、只引用一边、常量、多个 AND）；USING（单列、多列、找不到的列只放一个、左右各自同名列报 `ErrAmbiguous`）；NATURAL（`_tidb_rowid`、三表时同名列多于一次报错）；嵌套 join 的 `FullSchema`（`SELECT t1.*, t2.* FROM t1 JOIN t2 USING (b)`、`SELECT t2.a FROM t1 JOIN t2 USING (a) WHERE t2.a > 0`）；`ONLY_FULL_GROUP_BY` 加 join（`buildJoinFuncDepend` 的三种 join 类型）；`HAVING` / `ORDER BY` 引用 USING 的冗余列；ON 里有子查询（D09.6 前为 `blocked`）；列权限（列权限检查的 visitInfo 顺序，若领域能表达）。
  - 逻辑算子方法的 op：`ExtractOnCondition`（四种分类、`deriveLeft` / `deriveRight`、`IN` 改写的等值条件）、`pushDownConstExpr`（7 种 join 类型）、`BuildKeyInfo`、`BuildLogicalJoinSchema`、`GetJoinKeys` / `GetNAJoinKeys` / `GetPotentialPartitionKeys` / `IsNAAJ` / `PreferAny` / `getGroupNDVs`、`containDifferentJoinTypes` / `setPreferredJoinTypeFromOneSide`（按位输入，不需要 hint 语法）、`JoinType.String`。
  - 随第 A 部分移植的 `util/null_misc.go`：在 `planutil` 领域加 `IsNullRejected` 的 op（各类表达式、`IS NULL`、`IN`、`COALESCE` / `IF` / `IFNULL`、常量）。
- **扩展 `logicalrules`**（第 B 部分，`Blocks` 加 `D09.3`）：谓词下推、外连接化简、常量传播、列裁剪、TopN 下推在 join 上的结果计划；`updateEQCond` 的非列等值键（产生 `getProj` 投影）、`OtherConditions` 从后往前取出的顺序；`SemiJoinRewrite`（`tidb_opt_enable_sem_join_rewrite` 开 / 关，等 D09.6 有 semi join 后补）；`TestJoinPredicatePushDown` 等 Go 单测的 SQL。
- **第 C 部分**：apply 与 LATERAL 的构建，等 D09.6 / D02.2 后补；`ReplaceExprColumns` 同步关联列的回归用例。
- 向量双架构：amd64 与 arm64 两份都由 tracker CI `scripts/govec.sh --check` 逐字节校验，amd64 来自 CI 产物。

### 6.3 Rust 单元测试

- `coalesce_common_columns_right_join_keeps_left_as_lc`：`RIGHT JOIN ... USING` 时生成的 `EQ` 左参数是原左表的列，冗余标记落在原左表那一列（§4.2 第 7 条）。
- `check_col_priv_restored_after_using_error`、`check_col_priv_restored_after_on_error`（§4.3）。
- `full_join_is_blocked_not_inner`：`ast::JoinType::FullJoin` 返回 `blocked("D02.2")`。
- `join_trait_methods_are_explicit`：第 A 部分每个 Go 重写的接口方法都返回实现或所属块的 `blocked`，不落到 `BaseLogicalPlan` 默认实现。
- `semi_apply_reuses_join_plan_id`（第 C 部分）、`lateral_outer_schema_stack_restored_on_error`（第 C 部分）、`apply_replace_expr_columns_syncs_inner_corr_cols`（第 C 部分，§4.6）。
- `update_eq_cond_reverse_order`（第 B 部分）。

### 6.4 端到端验收

按 `docs/process/agent-workflow.md` 的规定（2026-11-24），凡是让原来回退的语句改走新栈执行的 PR，合并前要有一次 `tools/ab` 结果（fallback 与 strict 各一轮），由 Maya 在本机跑：

- **第 A、B 部分**：join 语句仍在执行之前停在 `blocked`（下一个未移植的块），由路由器回退到原型。验收：会话测试确认回退之后会话状态、warning 数与 Go 相同（第 A 部分的 `SetPreferredJoinTypeAndOrder`、第 B 部分的 `SetPreferredJoinType` 可能写 hint warning，回退时不能残留）；`tools/ab` 对现有工作负载不退化（宪章第七条 4）。
- **§3.8 序列全部完成后**：维护者用 `tools/ab` 对 `SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.a = t2.a` 及其变体（`LEFT JOIN`、多条件 ON、带 WHERE、带 ORDER BY / LIMIT、两表都没有 join 键索引）做 EXPLAIN 和结果对比；为了让计划可比，Go 端的表与 Rust 一样没有收集统计信息（Rust 现在只有 pseudo 统计）。index join、merge join 被选中时执行器构建停在 `blocked` 并回退，结果仍由原型给出。

## 7. 与 Go 的已知差异

1. **一处 map 遍历改为确定顺序**（`logical_plan_builder.go:1306`）：USING 中有多个找不到的列时，Rust 报 USING 列表中第一个，Go 报哪一个不确定（`M-MAPORDER`，§4.5）。
2. **4 个 cascades 专用方法不移植**（`logical_apply.go:60, 71`；`logical_join.go:792, 1104`）：见 §3.6，没有用户可见影响。
3. **Go 忽略错误的地方遇到 `blocked` 向上返回**（`logical_join.go:322-325`）：只在命中未移植块的桩时生效。
4. **`JoinType` 的 `init()` 断言改为编译期断言**（`logical_join.go:66-74`）：L2，没有可见差异。
5. **`BuildKeyInfoPortal` 从函数变量钩子改为普通函数**（`rule/util/build_key_info_misc.go:91`）：Go 用钩子只是为了避开包的循环引用，Rust 没有这个问题；L2，没有可见差异。
6. **`LogicalApply.ReplaceExprColumns` 的关联列共享改为显式同步**（`logical_apply.go:104`）：见 §4.6，结果与 Go 相同。
7. **临时边界差异（随对应块消失）**：`FULL OUTER JOIN`（Rust 解析器接受、Go 语法错误；`blocked-by:D02.2`，§4.7）、LATERAL（`blocked-by:D02.2` / 第 C 部分）、ON 里的子查询（`blocked-by:D09.6`，Go 报 1105）、`LogicalJoin.ExplainInfo`（`blocked-by:D10.9`）、`ExtractFD*`（`blocked-by:D09.14`）、物理 join 枚举（`blocked-by:D10.3`）、join 重排（`blocked-by:D09.10`）。

## 8. 读代码发现的可能的 Go 问题（均未复现）

以下几条只读了 v8.5.8 的代码，**没有在 v8.5.8 上用 SQL 复现**（本次没有集群），不算问题。由对应部分的实现任务先复现：复现成立的按宪章第九条 3 或第 1 款处理并开 `upstream-pitfall` issue；不成立的删掉。

1. **LATERAL 的 ON 子句不切换列权限检查方式**（`logical_plan_builder.go:1069-1072` 对照 `:922-927`）：`buildJoin` 改写 ON 时把 `checkColPriv` 临时设为 `reportColumnErrOption`，`buildLateralJoin` 没有。缺少列权限的用户执行 `... JOIN LATERAL (...) ON <列>` 时，报错可能与普通 `JOIN ... ON` 不同。复现：一个只有部分列权限的用户，分别执行两种写法，比较错误码和文案。第 C 部分。
2. **`ExtractUsedCols` 只在子节点是 `*LogicalJoin` 时查 `FullSchema`**（`logical_join.go:1124-1134`），而 `findJoinFullSchema`（`logical_plan_builder.go:652`）和 `findChildFullSchema`（`logical_apply.go:121`）会穿过 `LogicalSelection` 并认 `LogicalApply`。当 USING join 的冗余列被上层引用、而左子节点是 apply（LATERAL 叠在 USING join 上）时，列裁剪可能把这一列分给错误的一边或丢掉。复现：`SELECT t2.a FROM (t1 JOIN t2 USING (a)) JOIN LATERAL (SELECT t2.a AS x) d ON TRUE JOIN t3 ON ...` 一类语句，看是否报错或结果错误。第 C 部分。
3. **`USING (a, a)` 中重复的列名不报错**（`logical_plan_builder.go:1102-1106, 1305`）：`filter` map 去重后只有一项，`len(filter) == commonLen`，不报错。是否与 MySQL 8.0 不同需要对照（不确定 MySQL 的行为）。第 A 部分。

## 9. 决定点

按宪章第九条分两部分。

### 9.1 需要负责人决定（宪章第九条 1）

无。本块照 Go 移植；§7 的差异都没有用户可见的影响，或者是随对应块消失的临时边界；§8 的几条都没有复现；`FULL OUTER JOIN` 是 Rust 解析器与 Go 不一致，修正方向是回到 Go 的行为，不需要负责人决定。

### 9.2 维护者已定（宪章第九条 2；以下均为提议，待 Lucky / Maya 确认）

1. **拆分与顺序**：按 §3 分为 A（38）、B（11）、C（14）、D（2，随 D09.5）、E（2，随 D10.3）五部分，先做 A、B。理由：A 没有未满足的前置块，A + B 是 §3.8 最小序列中本块负责的部分；C 必须与 D09.6 一起测试。
2. **NATURAL / USING 放在第 A 部分**：理由：每个 join 都有 `FullSchema`，拆开要在 `unfoldWildStar` 等处多留桩（§3.1）。
3. **cascades 专用的 4 个方法按 EXC.B 不移植**（§3.6）。理由：与 D09.4、D09.14 一致。
4. **随本块移植的其他块函数**（§3.7）：`util/null_misc.go` 整文件、`ExtractTableAlias`、`TableIsView`、`GetNullRejectCheckExprCtx` 随第 A 部分；`LogicalJoin` 的谓词下推、列裁剪等方法随第 B 部分；apply 的方法与 `setIsInApplyForCTE` 随第 C 部分，各自计入所属块。理由：依赖已就绪，且不一起做就无法测试或会静默偏离（宪章第九条 2 "依赖都已就绪的小块或辅助函数提前移植"）。
5. **第 A 部分补全现有 L2 函数中 join 的分支**（§3.7 第 4 行、§4.7 D09.4）。理由：第 A 部分合并后 join 会走到这些函数，它们现在假设没有 join。
6. **`LogicalJoin` 的接口方法全部显式写出，没移植的返回所属块的 `blocked`**（§4.2 第 3 条）。理由：避免落到 `BaseLogicalPlan` 的默认实现；属于 port-block §3 棘轮的第 2 种情况。
7. **`optimizer.rs` 中两个规则桩改标为 D09.8 / D09.10**（§4.7）。理由：按 tracker 的归属；`ConvertOuterToInnerJoin` 在 v8.5.8 中从不运行。
8. **`FullJoin` 显式停在 `blocked-by:D02.2`，并建议开 issue 让 D02.2 修正解析器**（§4.7）。理由：不能被当成 inner join。
9. **`ast::TableSource` 加 `lateral: bool`**（§2.2）。理由：让 `containsLateralTableSource` 按 L1 移植；解析器支持 LATERAL 之前字段总是 false，LATERAL 语法在解析阶段就停住。
10. **`LogicalApply.ReplaceExprColumns` 按 `CorrSlot` 同步内层关联列**（§4.6）。理由：Go 依赖 `*CorrelatedColumn` 的共享可见性，Rust 的关联列按值保存（N4 §3.5）。
11. **`hint_info` 用 `Option<Arc<PlanHints>>` 保存设置偏好时的快照**。理由：Rust 现在的表级 hint 恒为空；hint 列表在 D02.2 / D13.3 支持后，如果之后的代码需要经 `HintInfo` 修改 hint 的状态，再按 N13 重新评估。
12. **`USING` 找不到多个列时按 USING 列表顺序报第一个**（`M-MAPORDER`，§4.5）。
13. **govec 扩展 `selectbuild`、`logicalrules`、`planutil` 三个领域**（§6.2），不新建领域。理由：计划构建、逻辑规则、`planner/util` 已有对应领域，join 只是新的节点类型和用例。
14. **§8 的几条由对应部分的实现任务先在 v8.5.8 上复现，复现成立再开 issue**。理由：本次没有集群；与 #343 的做法一致。

## 本文所用 ID

- A1：统一内存模型（`TrackedAlloc` 记账），并入 N3
- B3 / B4：例外 B 的两项（旧 cascades 优化器不移植 / 新 cascades 优化器暂不移植）
- D00.1：上下文接口骨架
- D00.2：常量、错误码与配置
- D01.x：类型与值（`FieldType`、`FieldName` 等）
- D02.2：parser 扩展（AST 节点、hint 列表、LATERAL 等）
- D05.2：`SessionVars` 与 `StatementContext`
- D06.3：infoschema
- D08.1：表达式框架
- D08.12：表达式工具函数（含 explain 文本）
- D09.0：计划构建器公共部分
- D09.1：SELECT 逻辑计划构建
- D09.2：表达式改写
- D09.3：本块，join 的计划构建与逻辑 join / apply 算子
- D09.4：聚合、窗口与 `ONLY_FULL_GROUP_BY` 的计划构建
- D09.5：集合运算、CTE
- D09.6：子查询与去关联
- D09.7：列裁剪与投影消除
- D09.8：谓词下推、谓词简化、外连接转内连接
- D09.9：聚合相关规则
- D09.10：join 重排与 join 消除
- D09.11：TopN / Limit 下推
- D09.13：优化器驱动、规则框架
- D09.14：逻辑算子公共部分与函数依赖
- D09.15：写路径（UPDATE / DELETE / INSERT）的计划构建
- D10.1：物理算子定义与 task
- D10.2：`FindBestTask` 与访问路径
- D10.3：物理计划枚举（`ExhaustPhysicalPlans`）
- D10.5：代价模型 ver2
- D10.6：基数估算
- D10.9：后处理、EXPLAIN 输出
- D10.10：物理属性与优化器工具（`planner/util`）
- D12.0：执行器框架与构建
- D12.1：读取类执行器
- D12.6：hash join 执行器与 joiner
- D12.7：index join 执行器
- D12.8：merge join 执行器
- D12.10：apply、CTE 等执行器
- D13.3：hint 处理
- DEF.MPP：TiFlash / MPP（推迟）
- EXC.B：例外 B（cascades 等非默认路径，不移植）
- L1 / L2：来源标注等级（L1 按映射规则逐行对应；L2 有理由的 Rust 化改写）
- M-DEFER、M-ERR、M-EXPR、M-FMA、M-F2I、M-FT、M-GO、M-CH、M-IFACE、M-INT、M-LOCK、M-MAPORDER、M-MEM、M-NAME、M-PLAN、M-SELF、M-SHARE：`docs/porting/mappings.md` 中的全局映射规则（依次为：`defer`、错误类型、表达式共享、浮点乘加、浮点转整数、`FieldType` 共享与修改、goroutine、channel、接口、定宽整数、锁、map 遍历顺序、内存记账、与 Rust trait 同名的方法、计划表示、struct 嵌入、长期共享元数据）
- N0：crate 分层
- N1：计划树和表达式树的表示（§4 摘出分派、§5 共享修改审计）
- N3：并发、取消与错误边界（含 A1）
- N4：表达式求值（§3.5 关联列的槽）
- N6：哈希聚合、哈希 join、排序的数据结构（例外，原则同意，尚无 spec）
- N13：长期共享元数据与 `Arc<FieldType>` 写时复制
- S / V0 / V1：N1 §5 共享修改审计的结论（S：只改本处新建或已复制的对象；V0：共享但没有可见影响；V1：共享且改变计划、结果或客户端列定义）
- VS1 / VS1-23：第一条纵向切片及其逻辑算子与基本规则任务
- X：不在 tidb-rs 范围内的代码（写路径等）
