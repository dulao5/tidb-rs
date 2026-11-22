# D09.13 优化器驱动与 D13.3 hint 骨架（spec，VS1-24）

这一项移植"优化器驱动"：拿到建好的逻辑计划之后，按标志位依次跑逻辑改写规则，再做物理优化，最后对物理计划做一组收尾处理（postOptimize）。同时移植 hint 处理的骨架、`planner.Optimize` 这一层语句驱动，并让 PREPARE 在同一个规划上下文里调用计划构建器。

Go 源码（v8.5.8）：`pkg/planner/core/optimizer.go`、`pkg/planner/optimize.go`、`pkg/planner/core/rule_inject_extra_projection.go`、`pkg/planner/core/rule_result_reorder.go`、`pkg/planner/core/rule/rule_constant_propagation.go`、`pkg/planner/core/rule/util/build_key_info_misc.go`、`pkg/planner/util/fixcontrol/get.go`、`set.go`，`pkg/util/hint/` 三个文件；另外顺带移植了 `rule_eliminate_projection.go` 的物理部分、`rule_derive_topn_from_window.go` 的规则外壳、`resolve_indices.go` 的 `refine4NeighbourProj`（见 §2）。

## 1. 范围与结论

| 部分 | 状态 |
|---|---|
| `DoOptimize` / `doOptimize` / `adjustOptimizationFlags` / `logicalOptimize`（规则表 `optRuleList`） | 已移植，向量逐条与 Go 一致 |
| `physicalOptimize` | 已移植驱动部分；其中的统计信息推导、`preparePossibleProperties`、`FindBestTask`、`ResolveIndices`、代价计算是边界（§4） |
| `postOptimize` 的全部 pass，含 `InjectExtraProjection`、物理投影消除、选择合并 | 已移植，手工构造的物理计划向量逐条一致 |
| `ResultReorder`（`tidb_enable_stable_result_mode`） | 已移植，向量覆盖 |
| 常量传播规则、`DeriveTopNFromWindow` 规则外壳 | 已移植（VS1 的 SELECT 会设置它们的标志） |
| `planner.Optimize` / `optimize` / `buildLogicalPlan` / `OptimizeExecStmt` | 已移植，边界见 §5 |
| PREPARE（`GeneratePlanCacheStmtWithAST`） | 恢复对 `PlanBuilder.Build` 的调用，规划上下文与 `Optimize` 相同（§7） |
| hint 骨架（D13.3） | 已移植；hint 的内容是解析器的边界（§6） |
| fixcontrol 的取值函数、key-info 的两个辅助函数 | 已移植，向量覆盖 |

converge（`--part specs/VS1-vertical-slice/functions.tsv --item VS1-24`）：D09.13 56/60（另 4 个登记为桩：`CheckPrivilege`、`refineCETrace`、`InjectProjBelowAgg`、`injectProjBelowUnion`），D13.3 38/38，D09.11 2/2，D10.9 1/1。

**最终物理计划的向量要等 FindBestTask（VS1-28）。** 当前一条 SELECT 走完逻辑优化之后，物理优化的第一步（统计信息推导，属于基数估算 VS1-29）就返回 `blocked-by:D10.6`，之后依次是 `preparePossibleProperties` 和 `FindBestTask`（`blocked-by:D10.2`，VS1-28）、`ResolveIndices`（D10.9，VS1-31），代价（`getPlanCost`，D10.5）已随代价模型 v2（VS1-30，#289）合并、直接调用。这几项完成后，`DoOptimize` 的完整输出由 VS1-28 的 EXPLAIN 计划向量验收；本项能做的部分（逻辑优化经过驱动、手工物理计划上的 postOptimize）已经用向量验证。

## 2. 函数清单的增减

- 从 VS1-31 移来 `refine4NeighbourProj`（`InjectProjBelowSort` 调用它）。
- 新增 `ConstantPropagationSolver` 的 3 个函数（D09.13）、`DeriveTopNFromWindow.Optimize` / `Name`（D09.11）：构建器对带 WHERE 的查询设置这两个规则的标志，驱动必须能跑。
- `rule_eliminate_projection.go` 的物理部分（`eliminatePhysicalProjection`、`doPhysicalProjectionElimination`、`canProjectionBeEliminatedStrict`）原属 VS1-23，那时是 `blocked-by:D10.1` 的桩；物理算子已合并（#285），postOptimize 第一步就要用，所以在这里移植。
- `fixcontrol.ParseToMap` 原来以 `go-dep` 形式写在会话变量里，改为正式的 `// go:` 标注（只有一个翻译）。
- 不移植、继续登记为边界的：其他块的逻辑规则（见 §3）、两个统计信息加载点的实际加载、`CheckPrivilege`、各种 trace。

## 3. 逻辑优化：规则表

`optRuleList` 按 Go 的顺序排成 26 项，第 i 个标志位对应第 i 条规则。规则分三类：

1. **已移植的规则**：列裁剪（两次）、`ResultReorder`、投影消除、常量传播、谓词下推、谓词化简、`DeriveTopNFromWindow`、TopN 下推。
2. **其他块的规则**（`UnportedRule`）：生成列替换、key-info（D09.13）、去相关 / 半连接 / 相关化（D09.6）、各种聚合规则（D09.4）、外连接相关（D09.3）、join 重排（D09.10）、sequence（D09.14）、Expand（D09.4）。标志位被设置时返回 `blocked-by:<块>`，路由器回退。VS1 的 SELECT 不会设置这些标志（向量中没有一条走到）。
3. **两类特殊处理**，需要评审确认：
   - **统计信息加载点**（`CollectPredicateColumnsPoint`、`SyncWaitStatsLoadPoint`）：Go 对每条用户语句都会设置这两个标志，它们收集统计信息的使用情况、同步加载统计信息。`CollectPredicateColumnsPoint` 还会**裁剪索引路径**（`rule_prune_indexes.go`）：索引数超过 `tidb_opt_index_prune_threshold`（默认 20）时按阈值裁剪；**在阈值以下也会裁剪**——只要至少有一个索引得分，就去掉"零分"索引（既不覆盖查询列、也不含 WHERE / ORDER BY 列的索引，`rule_prune_indexes.go:86`）。所以表有两个及以上二级索引时，Go 在逻辑优化中就会去掉候选路径，新栈不去。sbtest（只有 `k_1`）不受影响，而且物理优化目前停在 D10.6，所以本项不改代码；`PruneIndexesByWhereAndOrder`（纯计算）由 VS1-28 移植并配 govec，使两个以上二级索引的表的候选路径与 Go 一致（负责人决定：移植，不作边界）。新栈的统计信息只有 pseudo 表（VS1 plan §3.2 统计信息一行），没有东西可加载，所以这两条规则**直接返回原计划**，不是 blocked。它们在 tracker 中登记为 `blocked-by:D11.2` 的桩，注明行为。差异：Go 会把谓词列写入统计信息使用记录（供自动 analyze），新栈不写。（更新：FindBestTask 第 1 部分（VS1-28）已移植 `CollectPredicateColumnsPoint` 中的感兴趣列收集和索引裁剪，见 `specs/D10.2-find-best-task/spec.md` §4 第 7 条；使用记录和加载部分不变。）
   - **分区裁剪**（`PartitionProcessor`）：`UseDynamicPruneMode` 为 false 时 Go 会跑。构建器遇到分区表已经返回 `blocked-by:D09.12`，Go 对非分区表的数据源原样返回；所以这里遍历计划，遇到分区表才返回 `blocked-by:D09.12`，否则原样返回。
- `isLogicalRuleDisabled` 读全局的禁用规则表（Go 由 domain 从 `mysql.opt_rule_blacklist` 加载）。新栈还不读这张表，表为空。差异：集群里配置了禁用规则时，新栈仍会跑这些规则；建议与 SQL binding 一样，由新栈启动时检查（列为开放问题）。
- `optInteractionRuleList` 在 Go 中是空表，"再跑一次"的列表永远为空，照此移植。
- trace（`EnableOptimizeTrace` / `EnableOptimizerCETrace` / 调试 trace）只由 `TRACE PLAN`、`PLAN REPLAYER` 打开，Rust AST 没有这些语句；万一被设置，返回 `blocked-by:D02.2`，不静默跳过。

## 4. 物理优化与 postOptimize

### 4.1 physicalOptimize

驱动部分照 Go 移植：统计信息推导 → `preparePossibleProperties` → 根属性 `RootTaskType` / `ExpectedCnt = MaxFloat64` → `TaskMapBakTS = 0` → `FindBestTask` → `nth_plan` 越界警告 → 无效 task 的内部错误（含分离式 TiFlash 的提示）→ `ResolveIndices` → 代价。前五个调用的被调方都是边界（§1 最后一段）。`nth_plan` 计数在 hint 解析之前恒为 Go 的零值处理（`ForceNthPlan == 0` 时当作 -1）。

### 4.2 postOptimize 的各个 pass

按 Go 的顺序：物理投影消除、`InjectExtraProjection`、合并连续的 Selection、消除 UnionScan / Lock、Union 下的投影标记、并行 Apply、fine-grained shuffle、probe parents、`count(*)` 改写、chunk 复用检查、TiFlash 延迟物化、runtime filter。

只有 VS1 算子（Projection、Selection、Sort、TopN、NominalSort、Limit、TableDual、TableReader / TableScan、PointGet）能出现在计划里。Go 的 pass 中针对其他算子（HashAgg / StreamAgg、UnionAll、Apply、各种 Join、Lock、UnionScan、IndexReader 等、ExchangeSender）的分支，因为没有计划能包含这些算子而不可达；代码中在对应位置写明"该算子未移植"，函数标 L2。下面几处单独说明：

- **runtime filter**：新建的 `SessionVars` 中 `runtimeFilterMode` 是 0 而不是 `RFOff`（Go 也是这样），所以生成器会"开着"。Go 的生成器只在 TiFlash 的 HashJoin 处产生 filter；没有 HashJoin 时它的遍历不做任何事，所以这里直接返回，不报 blocked。
- **TiFlash 延迟物化**：Go 的 `predicatePushDownToTableScan` 只在"TiFlash 表扫描上的 Selection"处改写（`DEF.MPP`，返回 blocked），其他情况照抄 Go 的遍历。注意 Go 丢弃了它的返回值，这里也一样。
- **chunk 复用检查**：完整移植（含 120 GB 内存门槛、可信统计信息的平均行宽）。会话的 chunk 分配器不在回放上下文中，这部分由单元测试覆盖。
- **probe parents**：Go 中 probe parents 只加在 Apply 和 IndexJoin 的内侧子树下，这些算子未移植，所以每个节点得到的都是空列表，节点已经是空列表时不写（避免复制共享的节点）。VS1 plan 中关于"不形成 `Arc` 环的表示"的决定：probe parent 只需要父节点的类型、外侧子节点的 plan id 和统计信息（`getActualProbeCntFromProbeParents`、`getEstimatedProbeCntFromProbeParents` 只读这些），所以 Apply / IndexJoin 移植时把 `probe_parents` 改为保存这几项的记录，而不是父节点的 `Arc`。在此之前字段保留 Go 的类型。

### 4.3 InjectExtraProjection

`InjectProjBelowSort` 和 `TurnNominalSortIntoProj` 完整移植；`InjectProjBelowAgg`、`injectProjBelowUnion` 需要未移植的算子，登记为桩。

Go 把同一个 `*Column` 同时放进底层投影的表达式和 schema（子节点的列那一段），`refine4NeighbourProj` 改表达式里列的 `Index` 时，schema 里的列也跟着变。Rust 中这是两份拷贝，所以在 `refine4NeighbourProj` 之后把 schema 前面那段列的 `Index` 同步一次（`sync_aliased_schema_columns`）。向量中有 8 条记录依赖这一点（子投影输出重复列时）。

### 4.4 计划树的写法（N1 §3.4 与 taskMap spike 的结论）

- `rewrite_table_plan` 出错时（目前只有 TiFlash 延迟物化的边界会出错），reader 的链已交给 `f` 而 `table_plans` 已清空，reader 处于写了一半的状态。出错后整条语句回退到 PoC、这棵计划被丢弃，所以不影响结果；以后若有出错后继续使用计划的调用方，需要先恢复原链。
- 计划是 `Arc` 树。Go 原地改节点的地方用 `make_mut_plan` / `map_children`：语句独占树时原地改，共享时只复制被改的那个节点。
- `PhysicalTableReader` 的 `table_plans`（扁平列表）与 `table_plan`（链）持有相同的节点，所以 reader 下面的节点总有两个持有者，直接写会复制节点并让列表指向旧节点（taskMap spike #287 的发现）。所有写 reader 下层计划的 pass（投影消除、`InjectExtraProjection`、选择合并、延迟物化、probe parents）都通过 `rewrite_table_plan`：先清空 `table_plans`，在链上原地写，写完再从链重新生成列表。按 N1 §3.9（#288）只有链可写、列表只读；"列表不再存储、需要时从链推导"的形式涉及 `PhysicalTableReader` 的结构，留给 VS1-31。
- FindBestTask 的 memo 必须只活在 `physicalOptimize` 内、返回前释放，否则 postOptimize 会把每个节点都看成共享而复制（spike 结论 3，由 VS1-28 实现；`physicalop` 模块文档中已有 postOptimize 入口的 `debug_assert` 要求）。候选计划要 `into_iter()` 移出，不要克隆 `Arc`（spike 结论 2）。

## 5. 语句驱动：planner.Optimize

`Optimize` 的顺序照 Go：调试 trace → 只读模式检查 → EXECUTE 走 `OptimizeExecStmt` → 提取并解析语句 hint → `SET_VAR` hint → 严格模式下非只读语句去掉 TiFlash 引擎（结束后恢复）→ `TryFastPlan`（隔离读引擎含 TiKV 时）→ 事务预热 → SQL binding → `TryAddExtraLimit` → 非预处理 plan cache → `optimize` → 成功后的延迟处理（资源组 hint、`SET_VAR`）。

与 Go 的差异与边界：

- panic 的恢复：Go 在 `Optimize` 里 `recover`；新栈由路由器统一恢复成 1105（port-block 规则）。
- `ast.IsReadOnly`（解析器包）原是 `blocked-by:D02.2` 的桩，严格模式默认打开，使每条语句都停在这里；现在按 Go 移植 SELECT（含 `readOnlyChecker` 对用户变量赋值的检查）、集合运算、SHOW 三个分支。为此计划缓存检查器的遍历器 `CNode::Variable` 带上了变量节点。
- 多语句的 `PointPlanKey`：新栈不路由多语句查询，会话不保存这个值。
- SQL binding：新栈只在集群没有 binding 时运行（VS1 plan §3.2），`MatchSQLBinding` 永远不匹配，binding 循环和基线演进不会执行。
- 非预处理 plan cache（默认关闭）：打开时返回 `blocked-by:D13.1`（plan cache 例外 A3）。
- alternative logical plan（`tidb_opt_enable_alternative_logical_plans`，默认关闭）：需要语句上下文的构建状态保存 / 恢复（`StmtCtx.SaveLogicalPlanBuildState`，D09.13 继承项 P1-1），打开时返回 `blocked-by:D09.13`；关闭时只有一轮，这一轮的计划就是最优计划。
- cascades planner：返回 `blocked-by:D09.17`。
- 权限检查：新栈没有权限管理器（VS1 plan §3.2），Go 在没有管理器时跳过 `VisitInfo4PrivCheck` / `CheckPrivilege`；`CheckTableLock` 照旧，`CheckTableMode` 对导入 / 恢复模式的表返回 `blocked-by:X`（`ErrProtectedTableMode` 属于块 X）。
- 事务预热（`AdviseTxnWarmup`）通过规划上下文中的回调；`None` 等同于 Go mock 上下文的空实现。会话在 PREPARE / EXECUTE 之前已经自己发起预热。
- 资源组 hint：hint 不能解析，`HasResourceGroup` 恒为 false；分支照抄，真被设置时返回 `blocked-by:D15.2`。
- `PlanCacheStmt` 不保存 `VisitInfos`（只用于有权限管理器时的检查），所以 EXECUTE 的 `StmtCtx.Tables` 为空。
- 执行器的 `Compile` 还没有接入 `planner.Optimize`（它的预处理和优化仍是 `blocked-by:D09.0` / `D09.13` 的桩，属于把新栈接上执行器的任务）。本项不改 `Compile`，所以通过会话执行的 SELECT / EXECUTE 仍停在那里。

## 7. PREPARE 使用同一个规划上下文

- 规划上下文由会话给出：执行器的 `SessionContext::get_plan_ctx()`（Go 的 `GetPlanCtx`）返回 `SessionPlanCtx`（表达式上下文、存储客户端、事务的脏表检查、预热回调），`planner.Optimize` 和 PREPARE 都用它的 `plan_ctx(vars)` 生成 `PlanCtx`。
- `GeneratePlanCacheStmtWithAST` 改为接收 `PlanCtx`，恢复对 `PlanBuilder.Build` 的调用，返回计划的 schema 和输出列名（`BuiltPlanInfo`；逻辑计划本身在语句的 arena 中，随 PREPARE 结束释放）。构建器的 `sql_text` 设为参数化 SQL，`?` 这种列名才能从原文取出。
- 会话的表达式上下文现在是会话的 sessionexpr（D05.3 继承项 P1-1，[D05.3 spec](../D05.3-session/spec.md) §8）：plan cache tracker、参数值、计划列 id 都是会话自己的，PREPARE 中的 `?` 按 tracker 判断是否带参数标记，EXECUTE 构建 `?` 时读到 plan cache 写进会话的参数值。（原来是静态替身，只共用语句的 tracker。）
- 脏表检查：Go 在事务的 membuffer 中查表的 key。没有事务、流水线事务或只读事务的 membuffer 为空，答案对所有表都是 false；写过数据的事务不在新栈上（写入走 PoC），Rust 事务也还没有 membuffer，这时脏表检查为"未知"，构建器返回边界错误。
- 存储客户端：会话跑在替身存储（`UnwiredStore`）上时没有客户端，规划上下文的 `client` 为 `None`，谓词下推的检查返回边界错误。
- 结果：路由器测试中真实会话 PREPARE sysbench 点查已经在新栈上成功（两边都准备了语句，列定义来自新栈）。`Compile` 已接入 `planner.Optimize`（D05.3 spec §9）：EXECUTE 走到优化器，点查停在 fast plan 中点查条件的参数（D13.1），其他查询停在物理优化的边界（统计信息推导 D10.6 / FindBestTask D10.2），都在执行之前回退到 PoC。会话测试的 `sbtest1` 表补上了 `dbid`（InfoSchema 加载器会设置，元数据锁信息需要它）。
- EXPLAIN 在新栈有 EXPLAIN 之前不路由到新栈：路由器测试 `explain_is_never_routed_to_the_new_stack` 固定这一点。

## 6. hint 骨架（D13.3）

### 6.移植了什么

Go 的 `pkg/util/hint` 中优化器驱动要调用的部分，放在 `crates/planner/src/hint/`（`StmtHints` 本身仍在 `tidb_sessionctx::hint`，因为语句上下文内嵌它，这里只是重新导出）：

- `hint.go`：`ParseStmtHints`、`ParsePlanHints`、`PlanHints` 及其 20 个 `IfPreferXxx` / `IfNoIndexLookUpPushDown` 查询、`MatchTableName`、`matchTiKVOrTiFlash`，以及它们用到的类型 `PlanHints`、`HintedTable`（含 `Match`）、`HintedIndex`、`IndexJoinHints`。
- `hint_processor.go`：`ExtractTableHintsFromStmtNode`、`extractHintWarns`。
- `hint_query_block.go`：`NewQBHintHandler` 和 `QBHintHandler` 的全部方法（`Enter` / `Leave`、`NewBuildState`、`MaxSelectStmtOffset`、`checkQueryBlockHints`、`handleViewHints`、`HandleUnusedViewHints`、`getBlockOffset`、`SetWarns`、`GetHintOffset`、`checkTableQBName`、`isHint4View`、`GetCurrentStmtHints`、`MarkViewQBNameUsed`），以及每次构建的状态 `QBHintBuildState`。
- 计划构建器：`PlanBuilder.Init` 接收 hint 处理器并创建 `QBHintBuildState`（与 Go 相同）；`GetHintState`、`HandleUnusedViewHints` 已移植；`buildSelect` 的查询块编号改用 hint 处理器给出的编号，表级 hint 经过 `GetCurrentStmtHints` 和 `ParsePlanHints`（即 Go 的 `pushTableHints`）。

### 6.边界：hint 的内容（blocked-by:D02.2）

Rust 解析器不解析 hint 语法（Go 用单独的 `hintparser.y`），`ast::TableOptimizerHint` 只是一个不透明的占位节点：能知道"语句里有 hint"，但读不到 hint 的名字和参数。因此：

- hint 列表为空时，各函数的结果与 Go 完全相同。注意 `ParseStmtHints` 在没有 hint 时返回零值的 `StmtHints`，其中 `ForceNthPlan` 是 0 而不是 -1（`DoOptimize` 再把 0 当作 -1 处理）；`ParsePlanHints` 返回各字段为空的 `PlanHints`，子查询 hint 标志为 0，不产生警告。
- hint 列表不为空时，在 Go 开始读 hint 内容的位置返回 `blocked-by:D02.2`（解析器的 hint 语法），由路由器回退到 PoC。不会把非空列表当成空列表处理。
- `buildSelect` 中原来的 `blocked-by:D13.3`（"表级 hint"）因此变成 `blocked-by:D02.2`：`selectbuild` 回放中两条 `/*+ USE_INDEX(...) */` 记录的桩从 D13.3 改记为 D02.2；`USE INDEX (...)` 这种写法的索引 hint 解析器能解析，仍然停在 D13.3（索引 hint 的处理）。
- `extractHintWarns` 要比较的解析器错误值（`parser.ErrParse`、`ErrWarnOptimizerHint*` 等）Rust 解析器没有定义，解析器错误类的注册表里也没有这些错误码，所以按 pingcap errors 对规范化错误的 `Equal` 规则，比较错误的标识（RFC 码 `parser:<错误码>`）。

### 6.查询块编号的表示

Go 在 `QBHintHandler.Enter` 中把编号写进 AST 节点（`SelectStmt.QueryBlockOffset`）。Rust AST 没有这个字段，所以处理器按 `SelectStmt` 节点的地址保存编号，计划构建器用 `QBHintHandler::query_block_offset(sel)` 读取；没有被处理器访问过的 SELECT 读到 0，与 Go 中未设置的字段相同（Go 的 `BuildLogicalPlanForTest` 不遍历处理器，所以 `selectbuild` 回放中的编号仍是 0）。处理器每条语句新建一次、随语句释放，地址在它存活期间不会被复用。

遍历复用计划缓存检查器已有的 AST 遍历器（按 Go 的 `Accept` 顺序）：外层 SELECT 为 1，然后依次是 FROM 中的派生表、WHERE 中的子查询；集合运算从左到右编号。遇到 Rust AST 没有的节点时停止遍历并返回 `blocked-by:D02.2`，因为其后的 SELECT 编号无法确定。

### 6.已知差异

- Go 的处理器保存一个警告处理器（语句上下文）；这里会产生警告的方法把它作为参数传入，`None` 等同于 Go 的 nil。
- UPDATE、DELETE、INSERT、EXPLAIN、CREATE BINDING 不在 Rust AST 中，`Enter` 和 `ExtractTableHintsFromStmtNode` 中这些分支没有对应代码（文档注释中写明）。
- `SessionVars.PlannerSelectBlockAsName`（由计划生成 hint 时用到的查询块名，属于 SQL binding）不在 Rust 的会话变量中：`PlanBuilder.Init` 保存的旧值总是空，Go 为每个查询块准备的新列表也不保存（`Init` 标 L2）。
- `pushTableHints` 不保存 hint 栈：没有 hint 时压入的 `PlanHints` 为空，`popTableHints` 不会产生未匹配 hint 的警告，`TableHints()` 始终是空 hint（构建器原有的 `NoTableHints` 不变）。Go 忽略 `ParsePlanHints` 的错误（只有 hint 还原失败时才有），这里非空列表的边界错误照常返回，不丢弃。
- Rust 解析器把 `SelectStmtOpts` 的 hint 和 `TableHints` 分开保存（Go 的解析器把前者复制到后者），所以两个字段都读。


## 8. 验证

- govec 新领域 `optdriver`（tracker `govec/domains/optdriver.go`）2,470 条：
  - `optimize` 1,742 条（与 `selectbuild` 相同的 SQL，加上 `tidb_enable_stable_result_mode` 和动态裁剪模式两个开关）：1,638 条逻辑优化后的计划、标志位、plan / 列 id 计数、警告与 Go 相同；104 条停在边界并按块固定（`EXPECTED_BLOCKED`，均为构建器已有的边界，如 IN 函数族 D08.9 61 条、聚合 D09.4 15 条）。其中 2 条分区表在 Go 的 mock 上下文中 panic（分区裁剪需要分区表达式缓存），Rust 在构建器处停在 D09.12，按边界计数。
  - `post` 412 条（12 条手写、400 条随机的 VS1 物理算子链）：postOptimize 的结果、plan id 和列 id 计数逐条与 Go 相同。
  - `fix` 188 条、`max_one_row` 63 条、`index_key` 64 条，全部相同。
  - amd64 向量取自 tracker CI 产物，arm64 本地生成。
- 单元测试：PREPARE 恢复构建、EXECUTE 的计划缓存未命中路径走到物理优化边界（D10.6）、hint 骨架 15 个测试、路由器与会话测试按新的行为更新。
- `cargo clippy -D warnings`、`check_layers.py`、`check_model_mutability.py`、`check_one_translation.py`（无新增重复）通过。

## 9. 开放问题（请评审确认）

1. 统计信息加载点作为"不改计划的空操作"而不是 blocked（§3）；其中的索引裁剪由 VS1-28 移植。用户可见的后果写进 `docs/compatibility.md`：新栈上执行的查询不记录谓词列。
2. 禁用规则表（`mysql.opt_rule_blacklist`）：是否像 SQL binding 一样，表非空时整体关闭新栈（§3）。
3. EXECUTE 的参数值需要 sessionexpr（D05.3 P1-1），列为把 `Compile` 接入优化器的前置条件（§7）。**已解决**：sessionexpr 已移植，`Compile` 已接入（D05.3 spec §8、§9）。

## 本文所用 ID

- VS1-24：本项，优化器驱动与 hint 骨架
- VS1-10 / VS1-21 / VS1-23 / VS1-28 / VS1-29 / VS1-30 / VS1-31：第一个纵向切片中的计划构建骨架 / SELECT 构建 / 逻辑规则 / FindBestTask / 基数估算 / 代价模型 / ResolveIndices 与 ToPB
- D09.13：优化器驱动；D13.3：hint；D13.1：plan cache；D13.2：SQL binding
- D02.2：解析器的 AST；D04.1：权限；D05.3：会话（P1-1：sessionexpr）
- D09.3 / D09.4 / D09.5 / D09.6 / D09.10 / D09.11 / D09.12 / D09.14 / D09.17：join / 聚合 / 集合运算与 CTE / 子查询 / join 重排 / TopN 下推 / 分区 / 逻辑算子 / cascades
- D10.1 / D10.2 / D10.5 / D10.6 / D10.9：物理算子 / FindBestTask / 代价 / 基数估算 / ResolveIndices 与 EXPLAIN
- D11.2：统计信息句柄与加载；D15.2：资源管控；DEF.MPP：MPP 与 TiFlash；X：不在移植范围内的功能
- N1：计划的表示（§3.4 `Arc` 树，§3.9 reader 的链与列表）；A3：plan cache 例外
- M-CTX / M-EXPR：映射规则（上下文按调用传入 / 表达式的共享与写入）
