# D09.14 逻辑算子公共部分，以及基本逻辑改写规则（spec，VS1-23 第 1 部分）

状态：第 1 部分评审中（2026-11-17）。块：D09.14（逻辑算子）、D09.7（列裁剪、投影消除）、D09.8（谓词下推与简化）、D09.11（TopN / Limit 下推），只做第一条纵向切片需要的部分。crate `tidb-planner`。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3、§11（M-FT 审计表）；[N1 计划表示](../exceptions/N1-plan-representation/spec.md)；[D09.7 试点报告](../D09.7-column-pruning/pilot-report.md)。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/`。

## 1. 范围

第一条纵向切片的 SELECT 只有这几种逻辑算子：DataSource、Selection、Projection、Sort、Limit、TopN、TableDual。本部分移植它们的公共结构，以及作用在它们上面的五条规则：

| 规则（Go 类型） | Rust | 作用 |
|---|---|---|
| `ColumnPruner` | `core/rule_column_pruning.rs` | 去掉上层用不到的列 |
| `ProjectionEliminator` | `core/rule_eliminate_projection.rs` | 去掉只是原样转发列的投影（逻辑部分） |
| `PPDSolver` | `core/rule_predicate_push_down.rs` | 把过滤条件尽量推到 DataSource |
| `PredicateSimplification` | `core/rule_predicate_simplification.rs` | `a != c AND a IN (...)` 合并，`P AND (P1 OR P2)` 去掉不可能的分支 |
| `PushDownTopNOptimizer` | `core/rule_topn_push_down.rs` | 把 Limit / ORDER BY + LIMIT 往下推，Limit 变成 TopN |

算子在 `logicalop/`，每个 Go 文件一个 Rust 文件：`base_logical_plan.rs`、`logical_schema_producer.rs`、`logical_plans_misc.rs`、`logical_datasource.rs`、`logical_selection.rs`、`logical_projection.rs`、`logical_limit.rs`、`logical_sort.rs`、`logical_top_n.rs`、`logical_table_dual.rs`、`logical_table_scan.rs`（全部是例外，见 §2）、`logical_join.rs`（只有两个辅助函数，见 §2）。

不在本部分：聚合（LogicalAggregation）。VS1 的函数清单里没有它：聚合的构建是 VS1-21 的边界（`blocked-by:D09.4`），聚合函数描述符（D08.11）也没有移植。所以 sysbench 的 SUM / DISTINCT 查询目前走旧的 PoC 路径。

### 1.1 converge（`--part specs/VS1-vertical-slice/functions.tsv --item VS1-23`）

| 块 | 已移植 | 桩 | 例外 | 合计 |
|---|--:|--:|--:|--:|
| D09.14 逻辑算子 | 59 | 20 | 12 | 91 |
| D09.7 列裁剪、投影消除 | 18 | 3 | 0 | 21 |
| D09.8 谓词下推与简化 | 34 | 0 | 0 | 34 |
| D09.11 TopN / Limit 下推 | 28 | 1 | 2 | 31 |

177 个函数都有交代（missing 0、extra 0）。L1 152 个、L2 6 个。

## 2. 函数清单的增减

VS1 的清单是按函数名挑的（plan §3.3，精度约 ±20%）。逐个确认后：

**增加**（其他块的函数，规则直接调用，一起移植并计入各自的块）：

| 函数 | 所属块 | 为什么 |
|---|---|---|
| `Conds2TableDual`、`AppendAddSelectionTraceStep`（`logical_join.go`） | D09.3 join | `AddSelection` 和 Selection 的谓词下推调用它们：条件恒假时换成 TableDual |
| `DeleteTrueExprs`（`constraint/exprs.go`） | D09.8 | 谓词下推删掉恒真条件；它在 D09.8 但不在 VS1 清单里 |
| `ResolveExprAndReplace` 等 3 个（`rule/util/misc.go`） | D09.13 | 投影消除和各算子的 `ReplaceExprColumns` 用它替换列 |
| `logicaltrace` 整个文件（`util/optimizetrace/logicaltrace`） | D09.13 | 规则的 trace 步骤。叶子工具文件，按宪章第四条 4 整文件移植 |
| `ExplainByItems`、`ExplainPartitionBy`（`planner/util/explain_misc.go`） | D10.10 | 算子的 `ExplainInfo`。叶子工具文件，整文件移植 |

**例外**（不移植）：

- `Hash64` / `Equals`（6 个）：cascades 的 `base.HashEquals`，与已有的 `ByItems.Hash64` 一样按例外 B 处理。
- `DataSource.Convert2Gathers` / `buildTableGather` / `buildIndexGather`、`LogicalTableScan` 的 3 个方法：只有旧的 cascades 优化器调用（`planner/cascades/transformation_rules.go`），例外 B3。

**边界桩**（tracker `data/stubs.tsv` +18，每个都有说明）：

| 桩 | 阻塞块 | 原因 |
|---|---|---|
| 各算子的 `HashCode`（Selection、Projection、Limit、TableDual） | D14.2 | 要用 `plancodec.TypeStringToPhysicalID`（`util/plancodec`，未移植）。这几个规则不用 `HashCode` |
| `ExtractFD`、`FDs`、`SetFDs`（Base、DataSource、Selection、Projection） | D09.14 | 函数依赖集 `pkg/planner/funcdep` 未移植。ONLY_FULL_GROUP_BY 检查和基于 FD 的规则才用 |
| `GetTask`、`StoreTask`、`GetLogicalTS4TaskMap` | D10.2 | `FindBestTask` 的 memo（`taskMap`），按 N1 §3.2 是以 `PlanId` 为键的旁表，随物理优化来 |
| `DataSource.BuildKeyInfo`、`LogicalSelection.BuildKeyInfo` | D09.13 | 要用 `rule/util` 的 `CheckIndexCanBeKey` / `CheckMaxOneRowCond`，它们属于 key-info 规则（VS1-24）。Limit、TopN、TableDual、Projection 的 `BuildKeyInfo` 已移植 |
| `DataSource.IsSingleScan`、`CheckPartialIndexes`、`CheckPartialIndexByFilters` | D10.2 | 访问路径的处理 |
| `LogicalProjection.TryToGetChildProp`、`tryTransformSortItemPtrs` | D10.2 | 物理属性的转换，计划枚举时用 |
| `LogicalProjection.getGroupNDVs` | D10.6 | 只有 `DeriveStats`（统计推导，VS1-29 的边界）调用 |
| `canProjectionBeEliminatedStrict`、`doPhysicalProjectionElimination`、`eliminatePhysicalProjection` | D10.1 | 投影消除的物理部分，在 `postOptimize`（VS1-24）中运行，要用物理算子 |
| `AppendFunctionPruneTraceStep` | D08.11 | 聚合函数描述符未移植 |

`ExplainInfo` 中需要表达式 EXPLAIN 文本的（Selection、Projection，以及 Sort / TopN 里不是列的排序项）返回 `blocked-by:D10.9`：`ScalarFunction` / `Constant` 的 `ExplainInfo` 属于 D10.9，还没有移植。

## 3. 承载方式

| Go | Rust | 说明 |
|---|---|---|
| `BaseLogicalPlan` 嵌入 `baseimpl.Plan` | `BaseLogicalPlan { plan: baseimpl::Plan, max_one_row, children }` | **解决 VS1 tasks 中的待定问题**：逻辑侧也嵌入同一个 `baseimpl::Plan`，query block offset 统一为 `isize`（M-INT）。原来的 `id` / `tp` / `qb_offset` 字段删除 |
| 接口方法 + `p.self` | trait 默认方法分派到 `base_logical_plan.rs` 中的移植函数（泛型 `P: LogicalPlan + ?Sized`），算子覆盖 Go 覆盖的方法 | 与物理侧（`BasePhysicalPlan`）一样。改树的方法带 `this: PlanId`（Go 的 `p.self`），返回新的根 |
| `child.PruneColumns(...)` 等 | `base::prune_columns(ctx, child, ...)` 等分派函数（`logical_dispatch!`，N1 §4 摘出分派） | 6 个：`predicate_push_down`、`prune_columns`、`push_down_top_n`、`derive_top_n`、`predicate_simplification`、`convert_outer_to_inner_join` |
| `topN.AttachChild(p)`，`p` 常常是调用者自己 | `LogicalTopN::attach_child_to(ctx, lt, p, p_node)`，`p_node` 是已经摘出的节点；子节点在 arena 中时用 `attach_child` | 摘出分派的约束：节点摘出期间不能再按 id 查它（N1 §4） |
| `PredicatePushDown` 不返回错误 | 返回 `Result`，只有边界桩（store client 缺失、IN 函数类未移植）会出错 | |
| `util.GetPushDownCtx(p.SCtx())` | `PlanCtx::push_down_ctx()`；`PlanCtx` 增加 `client: Option<Arc<dyn Client>>` | 会话把 store client 交给规划上下文之前（VS1-24 / VS1-37），下推检查返回 `blocked-by:D09.13` |
| `ExplainInfo() string`、`HashCode() []byte` | 返回 `Result` | 边界见 §2 |
| `[]*util.ByItems`（Sort / TopN） | `Vec<ByItems>`，按值 | Go 经共享的 `*ByItems` 原地写，见 M-FT 审计 |
| `LogicalSchemaProducer.OutputNames()` 读时初始化 | `OnceCell`，与 `Schema()` 相同 | |

## 4. 与 Go 的已知差异

1. **IN 内置函数（D08.9）**：IN 的函数类没有移植，tidb-rs 不能新建或折叠 `in(...)`。影响三处：谓词简化重建 `a != c AND a IN (...)` 中的 IN 列表（tidb-rs 返回错误）、常量传播把 `a = c AND a IN (...)` 折叠成常量（tidb-rs 保留原条件）、投影的列替换在 IN 上重建函数（tidb-rs 保留条件不下推）。govec 中这 16 步按 D08.9 边界计数并钉住。路由上，含 IN 的查询要在 IN 移植前回退到 PoC（VS1-21 / VS1-24 处理）。
2. **谓词简化中列的比较**：Go 用 `*Column` 指针相等（`iCol == jCol`）和 cascades 的 `Column.Equals`。Rust 的 `Expr::Column` 按值持有列，没有指针身份，改为比较 `UniqueID`（L2）。Go 的表达式改写把 schema 中同一个 `*Column` 放进每一处引用，所以两者在改写后的计划上一致；只有某个前面的改写复制过列时才可能不同。govec 未见差异。
3. **`LogicalSelection.DeriveTopN`**：只在子节点是窗口算子时才有动作，窗口算子（D09.4）还不能出现在树中，这一分支不可达（L2）。
4. **`Conds2TableDual` 的 schema**：Go 让 dual 和原来的子节点共享同一个 `*Schema`，Rust 复制一份。共享的 schema 在两者同时在树中时不会被写（M-FT 审计第 5 条）。

**Go 的潜在问题**（#275，标签 `upstream-pitfall`）：`LogicalSelection.PredicatePushDown` 在条件没有全部推下去时直接返回 `p`，没有把子节点换成下推返回的 `child`（`retConditions, child = p.Children()[0].PredicatePushDown(...)`，之后只在条件全部推下时用 `child`）。目前所有算子的下推在这种情况下都返回自己，所以没有可见影响；如果以后某个子算子在保留条件的同时换了根，Selection 会继续指向旧的子节点。Rust 照搬。

## 5. 验证

- **govec**（tracker `govec/domains/logicalrules.go`，1,692 条记录；回放 `crates/planner/tests/govec_logicalrules.rs`）：Go 用 v8.5.8 自己的规划器把查询建成逻辑计划，依次运行规则，记录每一步之后的计划（算子、schema、表达式和它们的类型）和 trace 步骤。Rust 按 Go 的 plan id、列 id 在 arena 中重建输入计划，运行同样的规则并比较。结果：**6,630 步计划与 Go 相同，trace 6,630 步相同**，16 步是 IN 边界（§4 第 1 条），0 处不同。查询包括 sysbench 只读语句中不含聚合的那些（点查 `SELECT c FROM sbtest1 WHERE id=?`、范围、ORDER BY 范围），手工挑选的形状，以及 1,500 条随机单表查询（可带派生表）。
- 单元测试：原有的 N1 固定写法测试（`idiom_tests.rs`）改用新的 `BaseLogicalPlan` 构造。

## 6. 第 2 部分（未做）

- §2 中的边界桩随各自的块解除；其中 `BuildKeyInfo` 的两个桩在 key-info 规则（VS1-24）移植 `rule/util/build_key_info_misc.go` 时一起解除。
- 聚合和其他算子上的同名规则方法（join、window、CTE、union 等）不在 VS1。

## 本文所用 ID

- VS1-21：SELECT 语句的逻辑计划构建（`buildSelect` 等）
- VS1-23：本任务，逻辑算子公共部分与基础逻辑规则
- VS1-24：优化器驱动与后优化（`DoOptimize`、`postOptimize`）、key-info 规则
- VS1-29：pseudo 基数估算与统计结构读取
- VS1-37：新栈的集成与默认值评审
- D08.9：控制流、信息、杂项函数（含 IN）
- D08.11：聚合函数描述
- D09.3：join 构建与逻辑 join 算子
- D09.4：聚合、窗口的构建与逻辑算子
- D09.7：列裁剪与投影消除
- D09.8：谓词下推、谓词简化、约束
- D09.11：TopN / Limit 下推与推导
- D09.13：优化器驱动、规则框架、其他规则、函数依赖
- D09.14：逻辑算子公共部分与其余逻辑算子
- D10.1：物理算子定义与 task
- D10.2：`FindBestTask` 与访问路径
- D10.6：基数估算
- D10.9：后处理（EXPLAIN 输出、plan 编码等）
- D10.10：物理属性与优化器工具（`planner/util`）
- D14.2：慢日志、语句摘要、plan 编码表
- N1：计划表示的设计（arena 与 `PlanId`，§4 是摘出分派）
- M-FT：`FieldType` 共享与原地修改的映射规则
- M-INT：Go 定宽整数的映射规则
- L1 / L2：来源标注的等级（L1 逐行对应；L2 有理由的改写）
- 例外 B / B3：不移植的函数类别（cascades 专用接口；只有旧 cascades 优化器调用）
- PoC：原型实现（新栈路由回退的目标）
