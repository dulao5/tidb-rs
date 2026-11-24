# D09.9 聚合相关的逻辑优化规则（spec）

状态：**待确认**（只有 spec，没有代码；2026-11-24，issue #334）。块：D09.9（聚合相关规则：下推、消除、skew 改写、max/min 消除，ROADMAP P3），crate `tidb-planner`。相关：[D09.14 spec](../D09.14-logical-operators/spec.md)（逻辑算子与基本规则，写法照它）、[D09.13 spec](../D09.13-optimizer/spec.md)（优化器驱动与规则表）、[D10.2 spec](../D10.2-find-best-task/spec.md) §4 第 2 条（访问路径的指针共享）、[N1 计划表示](../exceptions/N1-plan-representation/spec.md)、[映射规则](../../docs/porting/mappings.md)；同批的 spec：聚合函数描述 D08.11（`specs/D08.11-aggregation/spec.md`，#333）、聚合执行器 D12.3（#335）、聚合函数实现 D12.4（#336）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/planner/core/`，另有说明的除外。

## 1. 这个块在 Go 里做什么

四条逻辑改写规则，都只改写 SELECT 的逻辑计划，不读写数据：

| 规则（Go 类型，`Name()`） | Go 文件 | 作用 |
|---|---|---|
| `AggregationEliminator`（`aggregation_eliminate`） | `rule_aggregation_elimination.go` | GROUP BY 的列覆盖子节点的唯一键时，聚合换成投影（`min(b) ... group by pk` → `b`，`count(b)` → `if(isnull(b), 0, 1)` 等）；聚合函数的 DISTINCT 参数覆盖唯一键时去掉 DISTINCT |
| `SkewDistinctAggRewriter`（`skew_distinct_agg_rewrite`） | `rule_aggregation_skew_rewrite.go` | `tidb_opt_skew_distinct_agg = ON` 时，把"有 GROUP BY、恰好一个 DISTINCT 聚合"的聚合拆成两层（下层按分组列 + DISTINCT 列分组，上层再聚合），`count` 拆成 `sum` + `count`，外面加投影把 `sum` 的结果转回 BIGINT |
| `MaxMinEliminator`（`max_min_eliminate`） | `rule_max_min_eliminate.go` | 没有 GROUP BY、只有 max/min 的聚合：一个函数时在下面加 `Selection(not(isnull(x)))` + `Sort` + `Limit 1`；多个函数且每个列都能用索引保序时，拆成多个单函数聚合，用笛卡尔积 join 连起来 |
| `AggregationPushDownSolver`（`aggregation_push_down`） | `rule_aggregation_push_down.go` | 先试一次聚合消除；子节点是 join 且 `tidb_opt_agg_push_down = ON` 时把可分解的聚合推到 join 的一侧（eager aggregation）；子节点是投影时把投影合并进聚合；子节点是 UNION ALL 时把部分聚合推到每个分支 |

**谁调用**：只有优化器驱动。四条规则在规则表 `optRuleList`（`optimizer.go:78`）中的位置是第 6、7、9、16 项（`optimizer.go:85`、`:86`、`:88`、`:95`），标志位是 `rule.FlagEliminateAgg`、`FlagSkewDistinctAgg`、`FlagMaxMinEliminate`、`FlagPushDownAgg`（`rule/logical_rules.go:26`、`:27`、`:29`、`:36`）。`logicalOptimize`（`optimizer.go:992`）按标志位依次调用规则的 `Optimize`。设置标志位的地方：

- `buildAggregation`（`logical_plan_builder.go:249-270`）：设置 key-info、聚合下推、max/min 消除、TopN 下推、谓词下推、聚合消除、投影消除；`tidb_opt_skew_distinct_agg` 打开时再设置 skew 改写；
- `buildDistinct`（`logical_plan_builder.go:1960-1962`）：只设置 key-info 和聚合下推；
- IN 子查询改写成 join + 聚合时（`expression_rewriter.go:1280`）：设置聚合消除。

另外，旧的 cascades 优化器调用 `ConvertAggToProj`（`pkg/planner/cascades/transformation_rules.go:2208`），那是例外 B3（只有旧 cascades 用的代码不移植），不影响本块：core 自己也调用这个函数，照常移植。

**sysbench 用到的路径**：`oltp_read_only` 的 `SELECT SUM(k) ... WHERE id BETWEEN ? AND ?` 会设置聚合消除、max/min 消除、聚合下推三个标志，三条规则在这条语句上都不改计划（没有 GROUP BY，消除不了；不是 max/min；子节点是 DataSource），但必须能跑完，不能停在边界上。`SELECT DISTINCT c ... ORDER BY c` 只设置聚合下推，预计会走"聚合合并下面的投影"这一支（`rule_aggregation_push_down.go:560-664`），以 govec 实际记录为准（§7）。

## 2. 依赖

本块调用的东西，按是否已经移植分开。

**已移植**（直接调用）：

- 表达式框架 D08.1：`Schema`（`ColumnsIndices`、`Contains`、`Clone`、`Append`、`Keys` / `UniqueKeys`）、`KeyInfo.String`、`Column`（`Clone`、`Equal`、`EqualColumn`、`GetStaticType`）、`Column2Exprs`、`ExtractColumns`、`ExtractColumnsFromExpressions`、`ComposeDNFCondition`、`MergeSchema`、`NewZero` / `NewOne`；
- 表达式工具 D08.12：`ColumnSubstitute`、`ColumnSubstituteImpl`、`ExprsHasSideEffects`；
- CAST D08.8：`BuildCastFunction`、`WrapWithCastAsInt`；`isnull`、`not`、`or` 的函数类（D08.5）；
- ranger D10.4（95/95）：`DetachCondsForColumn`、`DetachCondAndBuildRangeForIndex`；
- 逻辑算子 D09.14 / D09.11：`DataSource`（含 `AllPossibleAccessPaths`、`HandleCols`、`CommonHandleCols`）、`LogicalSelection`、`LogicalProjection`、`LogicalSort`、`LogicalLimit` 的 `Init`，`NewBaseLogicalPlan`，`DataSource.PruneColumns`（D09.7）；
- 规则的 trace（`util/optimizetrace`、`logicaltrace`）、优化器驱动 D09.13 的规则表；会话变量 D05.2：`AllowAggPushDown`、`EnableSkewDistinctAgg`、`RangeMaxSize`、`AllocPlanColumnID`。

**未移植**（实现时是 `blocked-by` 边界，或者要等它们先合并）：

| 依赖 | 所属块 | 用在哪里 | 处理 |
|---|---|---|---|
| `LogicalAggregation`（结构、`Init`、`GetGroupByCols`、`GetUsedCols`、`BuildKeyInfo`）以及构建器的 `buildAggregation` / `buildDistinct` | D09.4（聚合、窗口的构建与逻辑算子） | 四条规则的输入 | **前置条件**：没有它，计划中不会出现聚合，本块的代码不可达。第 1 部分在 D09.4 合并之后开工 |
| `AggFuncDesc`（字段 `Name`、`Args`、`RetTp`、`Mode`、`HasDistinct`、`OrderByItems`；`Clone`、`NewAggFuncDesc`、`StringWithCtx`、`EvalNullValueInOuterJoin`、`IsAllFirstRow`、聚合模式枚举） | D08.11（聚合函数描述，#333） | 所有规则 | 前置条件，同上 |
| `LogicalAggregation.PruneColumns` | D09.7（列裁剪） | max/min 拆分时裁剪新聚合（`rule_max_min_eliminate.go:170`） | 与 D09.4 一起到位；之前这一支返回 `blocked-by:D09.7` |
| key-info 规则 `BuildKeySolver` 与 `BuildKeyInfoPortal` | D09.13（规则框架、其他规则） | 聚合消除读子节点 schema 的 `Keys`；join 下推后重算 key（`rule_aggregation_push_down.go:525`、`:557`） | 端到端需要它（它现在是 `UnportedRule`，`blocked-by:D09.13`）；govec 回放从 Go 记录的输入计划（带 `Keys`）开始，不依赖它（§7） |
| `IF`、`IFNULL` 的函数类 | D08.9（控制流、杂项函数） | `rewriteCount` 总是建 `if(...)`（`rule_aggregation_elimination.go:235`）；`rewriteBitFunc` 建 `ifnull(...)`（`:245`、`:247`） | `blocked-by:D08.9`，在建函数的位置返回（§4.4） |
| `LogicalJoin`（`Init`、`JoinType`、各类条件、`DefaultValues`、`CartesianJoin`）、`BuildLogicalJoinSchema` | D09.3（join 构建与逻辑 join 算子） | join 下推（第 2 部分）、多个 max/min 用 join 连起来（`rule_max_min_eliminate.go:49-63`） | `blocked-by:D09.3` |
| `ResetNotNullFlag`（`pkg/planner/util/null_misc.go:469`） | D10.10（物理属性与优化器工具） | join 下推后清 NOT NULL 标志 | 没有出边的小函数，第 2 部分可按宪章第四条 5 提前移植到它的文件 |
| `LogicalUnionAll`、`LogicalPartitionUnionAll`、`LogicalCTE` | D09.5（集合运算、CTE） | UNION 下推（第 3 部分）；max/min 跳过 CTE（`rule_max_min_eliminate.go:223`） | `blocked-by:D09.5`；分区表的 `LogicalPartitionUnionAll` 另外要等分区裁剪 D09.12 |
| `BuildFinalModeAggregation`、`AggInfo`（`task.go:1365`、`:1377`） | D10.1（物理算子定义与 task） | UNION 下推时拆部分 / 最终聚合（`rule_aggregation_push_down.go:367`） | 第 3 部分的前置条件 |
| `intset.FastIntSet` | D00.3（通用工具） | skew 改写记录要补 first_row 的列（`rule_aggregation_skew_rewrite.go:102`） | 不移植 intset，照 D08.12 已有的做法用有序集合代替（L2，§4.6） |

## 3. 函数清单与划分

tracker `data/func_blocks.tsv` 中 block 为 D09.9 的函数共 50 个，全部在上面四个文件里。**没有写路径（X）的函数，也没有豁免的函数**：四条规则只改写只读查询的逻辑计划，都在只读范围内。按依赖分成三部分：

| 部分 | 函数数 | 内容 | 前置条件 |
|---|--:|---|---|
| 第 1 部分：单表 | 28 | 聚合消除（除 `CheckCanConvertAggToProj`）、skew 改写、max/min 消除（除用 join 连接的两个函数）、聚合下推的外壳和"合并投影"一支 | D09.4、D08.11 |
| 第 2 部分：join | 17 | 聚合推过 join、旧聚合的二次消除检查、多个 max/min 用 join 连接 | 第 1 部分、D09.3 |
| 第 3 部分：UNION | 5 | 聚合推过 UNION ALL / 分区 UNION | 第 1 部分、D09.5、D10.1 的 `BuildFinalModeAggregation` |

第 1 部分是 sysbench 聚合链（描述 → 规则 → 执行器 → 聚合函数）要的部分；`tidb_opt_agg_push_down` 默认 OFF，第 2 部分中 eager aggregation 那一支在默认设置下不会运行，但多个 max/min 的连接在默认设置下会运行（`select max(a), min(a) from t`）。

### 3.1 第 1 部分（28 个）

| Go 函数 | 文件 | 说明 |
|---|---|---|
| `AggregationEliminator.Optimize`、`.Name` | elimination | 规则入口，后序遍历 |
| `aggregationEliminateChecker.tryToEliminateAggregation` | elimination | GROUP BY 覆盖唯一键 → 投影；含 `GROUP_CONCAT` 时不消除（`:65`） |
| `aggregationEliminateChecker.tryToEliminateDistinct` | elimination | 原地把 `HasDistinct` 改成 false（`:127`） |
| `ConvertAggToProj`、`rewriteExpr`、`rewriteCount`、`rewriteBitFunc`、`wrapCastFunction` | elimination | 聚合函数换成表达式；`rewriteCount` / `rewriteBitFunc` 在建 `if` / `ifnull` 处是 `blocked-by:D08.9` |
| `appendAggregationEliminateTraceStep`、`appendDistinctEliminateTraceStep` | elimination | trace |
| `SkewDistinctAggRewriter.Optimize`、`.Name`、`.rewriteSkewDistinctAgg`、`.isQualifiedAgg`、`appendSkewDistinctAggRewriteTraceStep` | skew | 整个文件；规则只在变量打开时运行 |
| `MaxMinEliminator.Optimize`、`.Name`、`.eliminateMaxMin`、`.eliminateSingleMaxMin`、`.splitAggFuncAndCheckIndices`、`.checkColCanUseIndex`、`.cloneSubPlans`、`appendEliminateSingleMaxMinTrace` | max_min | 单个 max/min 完整移植；多个时拆分、检查索引都移植，最后调用 `composeAggsByInnerJoin` 处是 `blocked-by:D09.3`（第 2 部分解除） |
| `AggregationPushDownSolver.Optimize`、`.Name`、`.aggPushDown`、`appendAggPushDownAcrossProjTraceStep` | push_down | `aggPushDown` 移植消除、合并投影、递归子节点；join 一支（`:495-559`）是 `blocked-by:D09.3`，UNION 两支（`:665-675`）是 `blocked-by:D09.5` |

### 3.2 第 2 部分（17 个）

`CheckCanConvertAggToProj`（elimination）；`AggregationPushDownSolver` 的 `checkValidJoin`、`isDecomposableWithJoin`、`getAggFuncChildIdx`、`collectAggFuncs`、`collectGbyCols`、`splitAggFuncsAndGbyCols`、`addGbyCol`、`decompose`、`tryToPushDownAgg`、`getDefaultValues`、`checkAnyCountAndSum`、`checkAllArgsColumn`、`makeNewAgg`，`appendAggPushDownAcrossJoinTraceStep`（push_down）；`MaxMinEliminator.composeAggsByInnerJoin`、`appendEliminateMultiMinMaxTraceStep`（max_min）。理由：都要用 `LogicalJoin`（D09.3）。`CheckCanConvertAggToProj` 只在 join 一支里调用（`rule_aggregation_push_down.go:529-535` 打开检查之后），并且要判断子节点是不是 join。

### 3.3 第 3 部分（5 个）

`AggregationPushDownSolver` 的 `isDecomposableWithUnion`、`splitPartialAgg`、`pushAggCrossUnion`、`tryAggPushDownForUnion`，`appendAggPushDownAcrossUnionTraceStep`。理由：要用 `LogicalUnionAll`（D09.5）和 `BuildFinalModeAggregation`（D10.1）。

## 4. Go → Rust 的结构映射

### 4.1 文件与规则对象

每个 Go 文件一个 Rust 文件：`crates/planner/src/core/rule_aggregation_elimination.rs`、`rule_aggregation_skew_rewrite.rs`、`rule_max_min_eliminate.rs`、`rule_aggregation_push_down.rs`。四个规则类型实现 `LogicalOptRule`（`fn optimize(&self, ctx: &mut PlanCtx<'_>, p: PlanId, opt: &LogicalOptimizeOp) -> Result<(PlanId, bool), Error>`），替换 `core/optimizer.rs` 规则表中现在的四个 `UnportedRule`（它们现在标的是 `blocked-by:D09.4`，tracker 中是 `AGGREGATION_ELIMINATOR` 等四行）。`Name()` 的字符串照抄（`isLogicalRuleDisabled` 按名字匹配禁用表）。

### 4.2 固定写法（N1 §4，映射规则 M-PLAN / M-SELF / M-IFACE）

- type switch（`p.(*logicalop.LogicalAggregation)` 等）写成 `match` 算子类型加 `downcast`。`LogicalKind` 现在没有 Aggregation、Join、UnionAll、CTE，由各自的块加入；本块只在它们存在之后匹配。
- 规则改子树：Go 的 `p.SetChildren(newChildren...)` 和返回新根，照 D09.14 的写法：递归返回新 `PlanId`，由调用方写回子节点；节点在摘出期间不按 id 回查（N1 §4）。
- **plan id 的分配顺序照 Go**：每个 `XXX{}.Init(...)` 在 Go 的同一位置调用 `ctx.alloc_plan_id()`。例如 `cloneSubPlans` 先建 Selection 再递归克隆 DataSource（`rule_max_min_eliminate.go:117-118`），`splitAggFuncAndCheckIndices` 先建新聚合再克隆子树（`:162-163`）。trace 的文本里有 id（Go 的 trace 测试期望 `Aggregation_4`、`Sort_8`、`Join_12` 这样的编号），顺序错了 trace 就不同。列 id（`AllocPlanColumnID`）同理。
- trace：Go 的 `reason` / `action` 闭包在 `AppendStepToCurrent` 中立即求值（`pkg/planner/util/optimizetrace` 的 `AppendStepToCurrent`），所以打印的是调用那一刻的状态；例如 join 下推的 trace 打印的是 `decompose` 改写之后的参数（Go 测试期望 `count(Column#38)`）。Rust 在同一位置生成文本。max/min 拆分时 `newAgg.PruneColumns` 会往本规则的 trace 里加列裁剪的步骤（D09.7 的代码），照样保留。

### 4.3 共享的 `*AggFuncDesc` 和规则对象上的状态

1. **聚合函数描述的原地修改。** Go 有三处通过 `*AggFuncDesc` 指针原地改聚合节点里的描述：`tryToEliminateDistinct` 改 `HasDistinct`（`rule_aggregation_elimination.go:127`）；聚合合并投影时改 `Args`、`OrderByItems`（`rule_aggregation_push_down.go:652-655`）；join 下推时 `collectAggFuncs` 收集指向旧聚合 `AggFuncs` 的指针，`decompose` 再通过它们把旧聚合的函数改成最终模式、参数换成新列（`:235-249`），之后旧聚合的二次消除读到的是改过的描述。Rust 中描述由聚合节点持有（表示由 D08.11 / D09.4 定，见 §10 第 5 点），规则通过 `&mut LogicalAggregation` 修改；第 2 部分的 `leftAggFuncs` / `rightAggFuncs` 改为旧聚合 `agg_funcs` 的下标列表（L2：Go 的指针别名 → 下标），读写顺序与 Go 相同。
2. **`oldAggEliminationCheck`。** Go 把这个开关放在规则对象上（`rule_aggregation_push_down.go:36-38`，字段定义在 `rule_aggregation_elimination.go:37-48`），join 一支中先置为 true、二次消除后恢复（`:529-535`）。规则对象是全局表 `optRuleList` 中的单例（`optimizer.go:78-104`），所有会话共用。Rust 的规则对象是 `&'static dyn LogicalOptRule`，方法只拿 `&self`，不能改。改为每次 `Optimize` 在栈上新建一个 `AggregationEliminateChecker { old_agg_elimination_check: bool }`，`aggPushDown` 接收 `&mut` 它（L2）。`AggregationEliminator` 也嵌入这个检查器，开关恒为 false，照此写成同一个结构。与 Go 的差异见 §8 第 1 条。Go 的保存 / 恢复不是 `defer`；中间的 `tryToEliminateAggregation` 在 Rust 中可能返回 `blocked`，这时整个优化中止、检查器随之丢弃，不需要恢复。

### 4.4 错误

- **返回值。** Go 中这些规则只有 `Optimize` 返回 `error`，内部函数（`tryToEliminateAggregation`、`eliminateMaxMin`、`rewriteSkewDistinctAgg`、`checkColCanUseIndex` 等）不返回错误。Rust 中它们调用的建函数、CAST、ranger、列裁剪都可能返回 `blocked`，所以内部函数一律返回 `Result`（与 D09.14 中 `PredicatePushDown` 的处理相同），只有 `blocked` 会走这条路。
- **Go 吞掉错误的地方。** 下面几处 Go 遇到错误就当作"不能改写"继续：ranger 出错时跳过这条路径（`rule_max_min_eliminate.go:93-96`）、新聚合列裁剪出错时放弃拆分（`:170-172`）、建 first_row / sum 描述出错时放弃 skew 改写（`rule_aggregation_skew_rewrite.go:128-130`、`:164-166`、`:188-190`）、算外连接默认值出错时当作没有默认值（`rule_aggregation_push_down.go:300-303`）。Rust 在这些位置：错误是 `blocked`（`tidb_base::blocked::blocked_by(&e).is_some()`）就向上返回，其他错误照 Go 处理。已有先例：`core/find_best_task.rs`（选择率出错）、`core/stats.rs`。不这样做的话，一个 Rust 还做不了的分支会被悄悄当成"Go 也放弃了"，计划与 Go 不同而没有任何提示，违反"不静默"的规则。选项见 §10 第 3 点。
- **`NewFunctionInternal`。** Go 的 `NewFunctionInternal` 记日志后返回 nil；Rust 的 `new_function_internal` 返回 `Option`，函数类没移植时也只是 `None`。本块建 `isnull`、`not`、`if`、`ifnull` 的地方（`rule_aggregation_elimination.go:229`、`:235`、`:245`、`:247`，`rule_max_min_eliminate.go:192-193`）改用能返回 `blocked` 的版本：ranger 已有 `new_function_internal_or_blocked`（`crates/planner/src/ranger/ranger.rs`，胶水），实现时把它挪到规则和 ranger 都能用的位置，并且按 `builtin_registry_blocks` 给出函数类所属的块，不写死 D08.9。函数类已移植而建函数仍然出错时，照 Go 记日志并得到 `None`；Go 在 nil 上会继续走并在后面出错，这种情况 Go 自己不会发生（参数个数和类型都由规则保证），Rust 遇到时返回内部错误，不继续。
- **panic。** 这四个文件没有 `panic` 调用。有几处不检查的类型断言：`eqFunc.GetArgs()[0].(*expression.Column)`（`rule_aggregation_push_down.go:165-166`，等值条件的两边总是列）、`p.(*logicalop.LogicalAggregation)`（`rule_max_min_eliminate.go:173`，聚合裁剪返回自己）、`arg.(*expression.Column)`（`rule_aggregation_skew_rewrite.go:132`，见 §9 第 1 条）。Rust 在同样的位置 `expect`，panic 按 M-PANIC 在 task 边界恢复成 1105。
- **`defer`**：四个文件里没有 `defer`，没有要写成 drop guard 的收尾。

### 4.5 并发、内存记账、落盘

都不涉及。四条规则在会话的 task 上同步运行，没有 goroutine、channel、锁（M-GO、M-CH、M-LOCK 不适用）；Go 的规划器不对这些计划节点做内存记账（M-MEM 不适用）；没有落盘，所以没有 `blocked-by:D01.10` 的边界。唯一和并发有关的是 §4.3 第 2 条：Go 在全局单例上写状态，Rust 改为每次调用的局部状态。

### 4.6 其他映射

- `intset.FastIntSet`（skew 改写的 `firstRowCols`）：intset 没有移植，照 D08.12 spec §4 的做法用 `BTreeSet<i64>`（L2）。这里只用 `Insert` / `Remove` / `Has`，不遍历，顺序无关。
- `cloneSubPlans` 的 DataSource 浅拷贝（`rule_max_min_eliminate.go:123-137`）：Go `newDs := *p` 复制所有字段（切片、指针共享），再换掉 `BaseLogicalPlan`（分配新 plan id）、克隆 schema、复制 `Columns`、逐个浅拷贝访问路径。Rust 的 `DataSource` 没有 `Clone`；在规则文件中写一个按字段复制的辅助函数（`// go-part:`，放在 `cloneSubPlans` 下），`Arc` 字段克隆指针，值字段克隆值（M-SHARE 的 `GoCopy`）。Go 的克隆把 `PossibleAccessPaths` 设成**全部**路径、并且与 `AllPossibleAccessPaths` 共享同一批 `*AccessPath`（`:130-136`）；Rust 设 `possible_access_paths = all_possible_access_paths.clone()`，指针共享由 D10.2 的 `PathAliases` 按位置恢复（D10.2 spec §4 第 2 条）。
- M-INT：`childIdx`、`blockOffset`、下标是 Go `int`，写 `isize` / `usize`；没有可能回绕的运算。
- M-MAPORDER：没有按 map 顺序遍历的地方。
- `planChanged`：`AggregationEliminator.Optimize` 和 `SkewDistinctAggRewriter.Optimize` 在递归中用 `:=` 遮蔽了外层的 `planChanged`（`rule_aggregation_elimination.go:265`、`rule_aggregation_skew_rewrite.go:285`），另外两条规则直接返回 false，所以四条规则总是返回 false。照抄。Go 的 `optInteractionRuleList` 是空表（`optimizer.go:110`），这个值不影响任何东西。

## 5. 边界桩

| Rust 位置（实现时） | Go 位置 | 类型 | 何时解除 |
|---|---|---|---|
| `rewrite_count` 建 `if(...)` | `rule_aggregation_elimination.go:235` | 临时 `blocked-by:D08.9` | IF 函数类（D08.9）移植时 |
| `rewrite_bit_func` 建 `ifnull(...)` | `rule_aggregation_elimination.go:245`、`:247` | 临时 `blocked-by:D08.9` | IFNULL 函数类移植时 |
| `agg_push_down` 的 join 一支 | `rule_aggregation_push_down.go:495-559` | 临时 `blocked-by:D09.3`（第 1 部分时不可达：计划中还不会有 join） | 第 2 部分 |
| `agg_push_down` 的 UNION ALL / 分区 UNION 两支 | `rule_aggregation_push_down.go:665-675` | 临时 `blocked-by:D09.5`（同样不可达） | 第 3 部分 |
| 多个 max/min 调用 `compose_aggs_by_inner_join` 处 | `rule_max_min_eliminate.go:262` | 临时 `blocked-by:D09.3`（**可达**：`select max(a), min(a) from t`，`a` 有索引） | 第 2 部分 |
| 新聚合的列裁剪 | `rule_max_min_eliminate.go:170` | 临时 `blocked-by:D09.7`，只在 D09.4 先于聚合的列裁剪合并时存在 | 聚合的列裁剪移植时 |
| 第 2、3 部分的函数 | §3.2、§3.3 | 临时 `blocked-by:D09.9`（本块后续部分），在 tracker 中登记 | 对应部分 |

`eliminateMaxMin` 开头对 CTE 的判断（`rule_max_min_eliminate.go:223`）不写成桩：CTE 算子还不存在，计划中不会出现；D09.5 加入 CTE 算子时必须同时在这里加上判断，写进 D09.5 的完成条件。

**棘轮**（`scripts/check_stub_ratchet.py`）：第 1 部分删除 `optimizer.rs` 中四个规则的 8 行 `glue: blocked-by:D09.4` 注释（每条规则 `Optimize`、`Name` 各一行），在上表的 Go 分支处新增显式边界（预计 5 处，每处多为注释加 `blocked(...)` 两行）。这属于 port-block §3 允许的第 1 种情况（删掉整函数桩，新代码在跟不了的分支上各标一个），PR 正文逐条列出，以脚本实际报告为准。

## 6. 共享修改的审计（N1 §5，映射规则 M-FT / M-SHARE）

两个审计 grep（赋值 `.Index =` / `.RetType =`；`SetFlag` / `DelFlag` 等方法调用）在四个文件中的结果，加上读代码找到的共享：

| Go 位置 | 内容 | 结论 | 处理 |
|---|---|---|---|
| `rule_aggregation_push_down.go:240-243` | `arg := *col; newFieldType := *arg.RetType; DelFlag(NotNull); arg.RetType = &newFieldType` | S：Go 自己先复制了列和类型 | 照写，改的是新副本 |
| `rule_aggregation_push_down.go:354-355` | 克隆的新列 `newCol.RetType = firstRow.RetTp` | S：写的是新建的列，之后与描述共享同一个类型 | `Arc` 共享同一个 `FieldType` |
| `rule_aggregation_push_down.go:229-232`、`rule_aggregation_skew_rewrite.go:153-156`、`:168-171` | 新列的 `RetType` 直接用描述的 `RetTp` | S：新建的列，类型共享但不修改 | `Arc::clone` |
| `rule_aggregation_elimination.go:247` | `Constant{..., RetType: targetTp}`，与聚合的返回类型共享 | S | `Arc::clone` |
| `rule_aggregation_push_down.go:521`、`:523` → `ResetNotNullFlag` | 先复制列和类型再清标志（`pkg/planner/util/null_misc.go:470-476`） | S | 第 2 部分照写 |
| `rule_aggregation_elimination.go:197`、`rule_aggregation_skew_rewrite.go:94`、`:235`、`rule_aggregation_push_down.go:400`、`:478`、`rule_max_min_eliminate.go:125` | `SetSchema(x.Clone())`，Clone 只复制列，`*FieldType` 仍共享 | S：之后没有对这些类型的原地修改 | 默认写时复制 |
| `rule_max_min_eliminate.go:164` | 新聚合的 schema 直接用旧聚合 schema 的第 i 列（同一个 `*Column`） | S：旧聚合随后被丢弃 | 克隆 `Arc` |
| `rule_max_min_eliminate.go:162` | 新聚合与旧聚合共享同一个 `*AggFuncDesc`；随后 `PruneColumns` 可能原地改它的 `OrderByItems`（`operator/logicalop/logical_aggregation.go` 的 `PruneColumns`） | V0：max/min 没有 ORDER BY，且旧聚合要么被丢弃、要么（列裁剪出错时）保留但 Go 的修改无可见内容 | Rust 复制描述，不模拟 |
| `rule_max_min_eliminate.go:123-137` | DataSource 浅拷贝，切片与路径的字段共享 | S（"Go 应该先复制"）：之后的规则对这些字段都是整体重新赋值，不改元素 | 值复制，见 §4.6 |
| §4.3 第 1 条的三处 | 原地改聚合节点里的描述 | 依赖共享可见性（设计如此） | 通过 `&mut` 节点修改，第 2 部分用下标 |

结论：没有需要 `// Go aliasing (M-FT)` 模拟的位置。

## 7. 测试计划

### 7.1 Go 单测

- `logical_plan_trace_test.go` 的 `TestSingleRuleTraceStep` 中本块的 5 个用例（聚合消除 1、聚合下推 2：join 和 UNION、max/min 2）：变成 govec 记录（Go 跑出计划和 trace，Rust 回放），不手工翻译。join、UNION 两个用例属于第 2、3 部分。
- `logical_plans_test.go` 的 `TestAggPrune`（约 12 条，聚合消除 + 列裁剪 + 投影消除）、`TestEagerAggregation`（约 32 条，`tidb_opt_agg_push_down = ON` 的 join / UNION 下推，第 2、3 部分）：输入语句并入 govec 生成器，比较规则跑完之后的计划。
- 依赖存储的（`casetest/physicalplantest` 的 skew 用例、`tests/integrationtest` 中 `planner/core/casetest/physicalplantest/physical_plan.test` 第 405-417 行的 skew 语句、`executor/aggregate.test`）：要等物理聚合（D10.x）和聚合执行器（D12.3 / D12.4），由维护者合并后用 `tools/ab` 做 EXPLAIN 和结果对比，不在本块验收。

### 7.2 govec

新建领域 `aggrules`（tracker `govec/domains/aggrules.go`），写法照 `logicalrules`（D09.14 §5）：Go 用 v8.5.8 的构建器把语句建成逻辑计划，先跑本块之前的规则（列裁剪、key-info、投影消除、谓词下推等，照 Go 的标志位），记录本块规则的**输入计划**（含 schema 的 `Keys` / `UniqueKeys`），再依次跑本块的规则，记录每一步之后的计划和 trace。Rust 按 Go 的 plan id、列 id 重建输入计划，跑同样的规则并比较。这样回放不依赖 key-info 规则是否已移植。

- 计划节点在 `logicalrules` 的基础上增加：Aggregation（`aggs`：名字、参数、模式、`distinct`、返回类型、ORDER BY；`gby`；`prefer_agg_type`、`prefer_agg_to_cop`）、Join（第 2 部分）、UnionAll（第 3 部分）。
- 记录的参数包括 `tidb_opt_agg_push_down`、`tidb_opt_skew_distinct_agg` 的取值。
- 覆盖：
  - sysbench 只读语句中的聚合语句（`SUM(k)` 范围、`DISTINCT c` 范围 + ORDER BY）；
  - 聚合消除：按主键 / 唯一索引 / 复合唯一键分组，`min` / `max` / `sum` / `avg` / `first_row` / `count`（参数可空 / 非空、多参数 DISTINCT）/ `bit_and` / `bit_or` / `bit_xor` / `group_concat`；DISTINCT 参数覆盖 `Keys` 或 `UniqueKeys`；
  - max/min：主键、二级索引、复合索引的前缀和非前缀、无索引列、可空列（加 Selection）、ENUM / SET 列（不改写）、表达式参数、常量参数（只加 Limit）、带 WHERE（含会碰到 ranger 边界的 IN 条件）、多个函数（第 1 部分停在 D09.3 边界）；
  - skew 改写：变量打开后，`count` + `count(distinct)`、`sum(distinct)`、`avg(distinct)`、表达式分组、多个 DISTINCT（不改写）、多参数 DISTINCT（不改写）；
  - 聚合合并投影：投影中有副作用函数、类型的 EvalType 改变（不合并）、聚合函数带 ORDER BY；
  - 随机生成的单表聚合语句（与 `logicalrules` 相同的生成器思路，可带派生表）。
- 第 1 部分中停在边界的记录（`count` / `bit_*` 消除的 D08.9、多个 max/min 的 D09.3）按块计数并钉住（`EXPECTED_BLOCKED`），以免本应通过的路径悄悄变成 `blocked`。
- 每个移植函数都要有覆盖（领域的 `Coverage`）；第 2、3 部分的函数在对应部分之前登记为豁免（"本块后续部分"），对应部分时删除。
- amd64 向量来自 tracker CI 的产物；回放也在 arm64 job 中运行。

### 7.3 单元测试

- 每次调用的检查器（§4.3 第 2 条）：同一个规则对象上交错运行两次优化，开关互不影响。
- `cloneSubPlans`：克隆的 plan id 顺序、`possible_access_paths` 等于全部路径、`PathAliases` 能还原。
- §4.4 的 `blocked` 传递：在 Go 吞错误的每个位置构造一个 `blocked`，检查它被返回而不是被吞掉。

## 8. 与 Go 的已知差异

1. **规则对象上的状态不再跨会话共享**（`rule_aggregation_push_down.go:36-38`、`:529-535`；规则表 `optimizer.go:78-104`）。Go 的 `AggregationPushDownSolver` 是全局单例，join 一支在它上面写 `oldAggEliminationCheck`，同时优化的另一个会话可能读到 true（于是 `rule_aggregation_push_down.go:487` 的消除要多过一道 `CheckCanConvertAggToProj`，子节点是外连接时可能不再消除），或者在本会话的二次消除期间被另一个会话恢复成 false（于是跳过上游 issue `44795` 修复的检查，外连接下的旧聚合可能被错误地消除）。这是 Go 的数据竞争，只在 `tidb_opt_agg_push_down = ON` 且有并发的 join 聚合查询时出现。Rust 每次调用用自己的状态，结果等于 Go 单会话时的结果；govec 由单线程生成，不受影响。怎么登记见 §10 第 2 点。
2. **Go 吞掉的错误中，`blocked` 改为向上返回**（位置见 §4.4）。只影响 Rust 还做不了的分支：Go 会给出计划，Rust 返回 `blocked`、语句回退到 PoC。这是边界，不是行为差异，列在这里是因为它改变了这些函数的控制流。
3. **聚合函数描述的指针别名改为下标**（`rule_aggregation_push_down.go:107-143`、`:224-251`，第 2 部分），没有可见差异。
4. **`cloneSubPlans` 的浅拷贝改为值复制**（`rule_max_min_eliminate.go:123-137`），审计结论见 §6，没有可见差异；克隆的 `PossibleAccessPaths` 是全部路径（照 Go）。
5. **`NewFunctionInternal` 的 nil**（§4.4）：函数类已移植而建函数仍出错时，Go 带着 nil 继续，Rust 返回内部错误。规则建的函数参数都由规则保证，Go 中不会走到这里。

## 9. Go 本身的疑似问题（实现时复现后开 issue）

下面两条是读代码发现的，还没有在 v8.5.8 上复现。实现第 1 / 2 部分时用 govec 生成器构造对应语句：能复现就在本仓库开英文 issue（标签 `upstream-pitfall`），写进本节和代码注释，照 Go 移植；复现不了就从本节删掉。

1. **skew 改写对常量 DISTINCT 参数的断言**：`isQualifiedAgg` 允许参数是常量（`rule_aggregation_skew_rewrite.go:249-255`），而 DISTINCT 函数一支不检查就把参数断言成列（`:132`）。如果构建器保留了 `count(distinct 1)` 这样的参数，变量打开时 Go 会 panic（恢复成 1105）。Rust 在同一位置 panic，同样恢复成 1105；消息文本无法与 Go 运行时的 interface conversion 文本逐字相同，这一点到时一并登记。
2. **join 下推先改旧聚合、后放弃**：`tryToPushDownAgg` 先调用 `makeNewAgg`（其中 `decompose` 已经把旧聚合的函数改成最终模式、参数换成新聚合的列，`rule_aggregation_push_down.go:235-249`、`:274`），然后在外连接的空值侧取不到默认值时返回原来的子节点（`:286-291`），旧聚合的参数就指向了计划中不存在的列。按 `collectAggFuncs` 的限制（空值侧的参数都是列，`:118-125`），`EvalNullValueInOuterJoin` 总能得到常量，这一支看起来走不到，只有 `EvaluateExprWithNull` 出错时可能发生。

## 10. 需要负责人决定的点

1. **分几部分、按什么顺序移植。**
   - 选项 A：按 §3 分三部分。第 1 部分（28 个）在 D09.4、D08.11 合并后开工，随 sysbench 聚合链一起验收；第 2 部分（17 个）等 D09.3；第 3 部分（5 个）等 D09.5 和 D10.1 的 `BuildFinalModeAggregation`。
   - 选项 B：等 join、UNION 都移植后一次做完 50 个。sysbench 聚合链要等到那时。
   - 选项 C：只做第 1 部分，第 2、3 部分不排期。
   - 建议 A。第 1 部分中多个 max/min（默认设置下可达）在 D09.3 之前停在边界、回退到 PoC。
2. **`oldAggEliminationCheck` 的数据竞争（§8 第 1 条）怎么登记。**
   - 选项 A：按映射处理（Go 的规格是单会话时的确定行为，竞争不是"行为"），写进已知差异，开 `upstream-pitfall` issue，不进裁定台账。
   - 选项 B：按宪章第一条 2 的例外处理（Go 的竞争可能导致结果错误），开 issue，并在 `docs/testing/rulings.toml` 登记一条裁定（`approver = "pending-owner"`，`test` 指向 §7.3 的交错测试）。
   - 选项 C：照搬全局共享的开关（规则对象上放原子变量），连竞争一起复刻。
   - 建议 A；如果负责人认为"可能导致外连接下结果错误"足以按例外处理，就选 B。不建议 C。
3. **Go 吞掉错误的位置遇到 `blocked` 怎么办（§4.4）。**
   - 选项 A：`blocked` 向上返回，其他错误照 Go（与 `find_best_task.rs`、`stats.rs` 的先例一致）。
   - 选项 B：完全照 Go 吞掉。Rust 做不了的分支会被当成"Go 也放弃了"，计划与 Go 不同而没有提示。
   - 选项 C：在规则入口先检查整条语句会不会碰到边界，碰到就整体 `blocked`。检查本身要复刻规则的判断，容易漏。
   - 建议 A。
4. **COUNT / BIT_* 的聚合消除要用 IF / IFNULL（D08.9）。**
   - 选项 A：在建函数处返回 `blocked-by:D08.9`，govec 钉住条数，等 D08.9 移植后解除。sysbench 的 `SUM` / `DISTINCT` 用不到这两个函数。
   - 选项 B：把 IF、IFNULL 两个函数类提前排期（D08.9 的一部分），与第 1 部分同时完成。
   - 建议 A；如果希望 `SELECT pk, COUNT(x) ... GROUP BY pk` 这类常见语句尽早走新栈，选 B，作为 D08.9 的单独一项排期（宪章第四条 3 不允许只拉进单个函数）。
5. **聚合函数描述在逻辑聚合中的表示**（与 D08.11 #333、D09.4 的 spec 一起定）。本块需要：聚合节点持有自己的描述并能原地修改（§4.3 第 1 条），`Clone` 是深拷贝，max/min 拆分时新聚合可以复制描述。
   - 选项 A：`LogicalAggregation.agg_funcs: Vec<AggFuncDesc>`，按值持有；Go 的指针别名改成下标。
   - 选项 B：`Vec<Arc<AggFuncDesc>>`，修改经 `Arc::make_mut`（M-EXPR 的写时复制）。
   - 建议 A：Go 中描述只在同一个节点内部或在被丢弃的旧节点之间共享（§6），没有需要共享可见性的地方；按值持有最简单。最终以 D08.11 spec 的决定为准，两份 spec 保持一致。
6. **govec 用新领域还是扩展现有领域。**
   - 选项 A：新建 `aggrules` 领域（§7.2）。
   - 选项 B：扩展 `logicalrules`，放开对聚合的过滤。`logicalrules` 已有的 6,630 步计数会变，D09.14 的回放要随之调整。
   - 建议 A。

## 本文所用 ID

- D00.3：通用工具（`util/*`，按需整文件移植）
- D01.10：落盘容器与磁盘 chunk
- D05.2：SessionVars 与 StatementContext
- D08.1：表达式框架（Expression、Column、Constant、ScalarFunction、Schema）
- D08.5：算术、数学与逻辑运算（含 `isnull`、`not`、`or`）
- D08.8：CAST
- D08.9：控制流、信息、杂项函数（含 IF、IFNULL、IN）
- D08.11：聚合函数描述（`expression/aggregation`）
- D08.12：表达式工具（`expression/util.go` 等）
- D09.3：join 构建与逻辑 join 算子
- D09.4：聚合、窗口、only_full_group_by 的构建与逻辑算子
- D09.5：集合运算、CTE、Expand
- D09.7：列裁剪与投影消除
- D09.9：本块，聚合相关规则（下推、消除、skew、max/min）
- D09.11：TopN / Limit 下推与推导（含 Sort、Limit 算子）
- D09.12：分区裁剪
- D09.13：优化器驱动、规则框架、其他规则（含 key-info 规则）
- D09.14：逻辑算子公共部分与其余逻辑算子
- D10.1：物理算子定义与 task（含 `BuildFinalModeAggregation`）
- D10.2：`FindBestTask` 与访问路径
- D10.4：ranger（区间构造）
- D10.10：物理属性与优化器工具（`planner/util`）
- D12.3：聚合执行器（hash / stream agg）
- D12.4：聚合函数实现（aggfuncs）
- N1：计划表示的设计（arena 与 `PlanId`，§4 是摘出分派，§5 是共享修改的审计）
- M-PLAN、M-SELF、M-IFACE、M-EXPR、M-FT、M-SHARE、M-PANIC、M-GO、M-CH、M-LOCK、M-MEM、M-INT、M-MAPORDER：`docs/porting/mappings.md` 中的映射规则（计划表示、嵌入结构、接口、表达式共享、`FieldType` 共享、长期共享结构、panic、goroutine、channel、锁、内存记账、定宽整数、map 顺序）
- L1 / L2：来源标注的等级（L1 逐行对应；L2 有理由的改写）
- 例外 B3：只有旧 cascades 优化器调用的代码，不移植
- S / V0 / V1：共享修改审计的结论分类（S：新建的或已 Clone 的对象；V0：可能共享，但其他持有者是否看到修改无关紧要；V1：共享且看到修改后计划、结果或 EXPLAIN 会变），见 D09.7（列裁剪试点）报告附录 B
- PoC：原型实现（新栈遇到 `blocked` 时回退的目标）
