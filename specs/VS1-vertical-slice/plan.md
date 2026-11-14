# VS1：第一条纵向切片（检查点 C2）计划

状态：规划草案（2026-11-14），待项目负责人确认 §10 的决定项。类型：跨块的排期与集成计划，不是新的功能块，也不是例外。回应 #128 §二.2（纵向切片）和 #54 的检查点 C2。

相关：[ROADMAP](../ROADMAP.md)、[宪章](../../CONSTITUTION.md)（第四条 3 边界桩、第七条 4 端到端不退化）、[方法论](../../docs/porting/methodology.md) §6、[N1](../exceptions/N1-plan-representation/spec.md) 与 [D09.7 试点报告](../D09.7-column-pruning/pilot-report.md)（#149 已批准的修订：摘出分派、物理计划一开始就是 `Arc`、M-FT 方案 (a)）、[N2](../exceptions/N2-session-context/spec.md)、[N3+A1](../exceptions/N3-A1-concurrency-memory/spec.md)、[N4](../exceptions/N4-expr-eval/spec.md)、P2 里程碑 #49。

函数级清单：[functions.tsv](functions.tsv)（2,193 个 Go 函数，每行一个，带工作项编号和行数，§3.3 说明生成方法）。任务列表：[tasks.md](tasks.md)。

## 0. 结论先行

1. **C2 的目标**：点查，以及单表扫描（过滤、投影、limit、order by），走**真 planbuilder → 真执行器 → 真会话**。其余部分用 `blocked-by` 边界桩。验收有三条：每周构建中这些形态 100% 一致；sysbench point_select ≥ 0.9× Go；sysbench read_only ≥ 0.9× Go。
2. **工作量**：需要移植约 **43.9k 行** Go 函数体，分属 30 个块。
   - 其中 **8.1k** 行本来就属于 P2（#49），算广度线，只是 VS1 要求先做这几部分。
   - 其余 **35.7k** 行是 P3/P4 中的块的一部分，算深度线，约占 P3/P4 总量（125k）的 29%。
   - 深度线不是额外工作，只是把 P3/P4 的约三分之一提前、按查询链路纵向排序。除约 0.5 周的胶水和集成外，没有要扔掉的代码。
3. **按块的一部分移植**：VS1 按块的一部分（part）移植，沿用 D06.2 / D06.3 "第 1 部分"的做法。每个部分给出函数清单，块内其余函数登记为 `blocked-by:<块>`。这一点需要负责人确认（🔶 §10-1）。
4. **接入方式**：在 PoC 会话里加一个路由器，由开关 `--new-stack=off|vs1|strict` 控制。
   - 符合 VS1 形态的 SELECT（含预处理语句）交给新的 `tidb-session`；其余语句，以及规划阶段碰到边界桩的语句，回退到 PoC。
   - 回退只会发生在执行器打开之前，所以不会有副作用。
   - `strict` 模式不回退，供每周构建统计"新栈能执行的形态"。
5. **日程**（§6）：
   - 按负责人定的 30% 深度份额，C2 整体要到约 1 月底，赶不上 #54 草案的 12-11。
   - 建议把 C2 拆成两步：
     - **C2a（12-11）**：点查纵向（会话、预处理、点查快速路径、点查执行器、结果编码），sysbench point_select ≥ 0.9×。
     - **C2b**：扫描、过滤、投影、limit、order by，再加 read_only ≥ 0.9×。30% 份额下约 1 月底；如果到 C2 之前把深度份额临时提到 50%，约 12-30。
   - 这两个选项由负责人定（🔶 §10-2）。
6. **最大的三个风险**（§8）：
   - FindBestTask 和 `taskMap` 还没有验证过 N1 的表示（D10.2 开工前补一个小试点）；
   - plan cache 的时机：Go 基线开着会话级 plan cache，tidb-rs 只做实例级（#128 §一.5）；
   - 相对 PoC 的性能回退：PoC 是 Go 的 1.13–1.27 倍，新栈的目标是 ≥ 0.9 倍，切换开关时每周构建会报 REGRESSED。

## 1. 范围

### 1.1 查询形态（C2 要求 100% 一致）

| 形态 | Go 计划中的算子（每周构建的分类） | 例子 |
|---|---|---|
| 点查（主键 / 唯一索引等值，含 `_tidb_rowid`） | `Point_Get` | sysbench `point_selects`：`SELECT c FROM sbtest1 WHERE id=?` |
| 单表全表 / 主键范围扫描 | `TableReader`、`TableFullScan`、`TableRangeScan` | `simple_ranges`：`SELECT c FROM sbtest1 WHERE id BETWEEN ? AND ?` |
| 过滤（下推到 cop 或留在 root） | `Selection` | `WHERE id > ? AND pad LIKE 'a%'` |
| 投影（含表达式） | `Projection` | `SELECT id + 1, concat(c, pad) ...`（函数已移植时） |
| limit（下推） | `Limit` | `... LIMIT 10` |
| order by（含与 limit 合成 TopN） | `Sort`、`TopN` | `order_ranges`：`... ORDER BY c` |
| 恒假条件 / 无表 | `TableDual` | `WHERE 1=0`、`SELECT 1` |

协议上覆盖 COM_QUERY 和 COM_STMT_PREPARE / EXECUTE / CLOSE。sysbench 用 `--db-ps-mode=auto`，走的是预处理语句。

### 1.2 不在 VS1 内（边界桩，回退到 PoC）

- **其他语句和形态**：聚合、DISTINCT、GROUP BY / HAVING、窗口、join、子查询、集合运算、CTE（D09.3–D09.6、D12.3–D12.10）；
- **其他访问路径**：IndexReader / IndexLookUp / IndexMerge / Batch_Point_Get（D10.2 的索引路径、D10.7、D12.1 剩余部分、D12.11）；
- **特殊表**：分区表（D09.12）、memory table 与 infoschema（D06.6、D12.13）、临时表 / 缓存表；
- **其他 SQL 功能**：FOR UPDATE、显式事务内的读、`AS OF TIMESTAMP`（D05.4 剩余部分）、SHOW / EXPLAIN / SET（D12.12）、未移植的内置函数（D08.6 / D08.7 / D08.9 / D08.10）。

sysbench read_only 每个事务有 14 条语句，其中 12 条属于 VS1：10 条 point、simple_ranges、order_ranges。另外 2 条（sum_ranges、distinct_ranges）在 C2 时仍由 PoC 执行。

### 1.3 不变的东西

- 默认模式下，每周构建的总一致率保持 100%（宪章第七条 4）。
- 开关为 `off` 时，行为与现在完全相同。
- PoC 的规划器和执行器在 C2 时**不删除**：它们承担回退和非 VS1 语句，到 C5 再删。

## 2. 已有的基础

| 内容 | 状态 | VS1 中的用途 |
|---|---|---|
| P1 全部 24 块（#48 已关闭）：类型、chunk、编码、collation、表达式框架与比较 / 算术 / 逻辑 / CAST、上下文接口（planctx、exprctx、stmtctx、errctx）、SessionVars / StatementContext、系统变量、kv 接口、tablecodec / rowcodec、内存统计 | 已完成 | 规划器、执行器、会话都直接使用 |
| D14.0 执行统计结构 | 已完成（P2 提前） | 执行器的 runtime stats |
| D08.3 下推判定与 `ExprToPB` / `PBToExpr`（converge 50/50，#179、#181、#182） | 已完成 | Selection / TopN / Limit 下推到 cop |
| D06.2 第 1 部分（meta 读取，130/240） | 已完成 | infoschema 加载 |
| D06.3 第 1 部分（infoschema v2 构建与读取，184/270，#176、#177） | 已完成 | 新栈的 InfoSchema；第 2 部分（`InfoCache`、`InfoSchema` trait，T8 / T9）由 VS1-17 补上 |
| D09.7 设计试点（`crates/spike/planner-pilot`，spike，不合入） | 已完成，#149 已批准 N1 修订 | 规划器 crate 的 arena、摘出分派、`Arc` 物理计划写法直接沿用；spike 本身不复用 |
| N1 修订 PR（#149 的后续） | **未合入**（N1 spec 仍是 11-04 的草稿，`mappings.md` 的 M-PLAN 仍写"冻结"） | VS1-00 的前置条件 |
| 规划器 / 执行器 / 会话 / 服务器 crate | `layers.toml` 中 `planned = true`，尚未建立 | VS1-01 建立 |

## 3. 需要的 Go 函数（按 ROADMAP 块）

### 3.1 汇总

"块总量"和"VS1"都是 Go 函数体行数（口径同 ROADMAP §1），来自 tracker 的 `func_blocks.tsv` 和 `go_funcs.tsv`，按 §3.3 的规则选出。"状态"中，"待做"指尚未开工；P2 中的块标出由 #49 负责。

| 块 | 阶段 | 块总量 | VS1 | 占比 | 工作项 | VS1 需要的部分 | 块内其余部分（`blocked-by:<块>`） |
|---|---|--:|--:|--:|---|---|---|
| D09.0 planbuilder 骨架、preprocess | P3 | 3,850 | 1,748 | 45% | VS1-10 | `preprocess.go` 的名字解析与 SELECT 检查、`PlanBuilder.Build/Init`、计划 `Init`、`core_init` | DDL / DML / 视图 / 锁相关的 preprocess 检查、`buildSelectLock` |
| D09.13 优化器驱动 | P3 | 4,838 | 547 + 1,334 | 39% | VS1-10（`planner/optimize.go`）、VS1-24（`core/optimizer.go`） | `Optimize` / `optimize`（含 `TryFastPlan` 入口）、`DoOptimize`、`logicalOptimize`、`physicalOptimize`、`postOptimize` 的各 pass、`InjectExtraProjection`、`ResultReorder`、fixcontrol | `rule_collect_plan_stats`（同步加载统计信息，等 D11.2）、`rule_prune_indexes`（索引数超过阈值时才触发）、函数依赖、fine-grained shuffle、生成列替换 |
| D10.8 点查快速路径 | P3 | 2,100 | 1,121 | 53% | VS1-11 | `TryFastPlan`、`tryPointGetPlan`、`PointGetPlan` 全部方法、名字和值的提取 | `BatchPointGetPlan`、点更新 / 点删除（写路径归 X） |
| D10.1 物理算子与 task | P3 | 6,222 | 322 + 1,830 | 35% | VS1-19（`baseimpl`、`base_physical_plan`、`IsPointGetWithPKOrUniqueKeyByAutoCommit`）、VS1-27 | TableReader / TableScan / Selection / Projection / Limit / TopN / Sort / TableDual 的定义、`Attach2Task`、`CopTask`、`task_base.go`、`expression_codec_fn.go` | join / agg / window / MPP / IndexLookUp / IndexMerge 的 task 处理 |
| D10.10 物理属性与优化器工具 | P3 | 1,896 | 1,387 | 73% | VS1-19 | `property`、`planner/util` 的 path、handle_cols、misc、byitem、column、costusage | `null_misc`（外连接） |
| D06.4 table 抽象 | P2 #49 | 7,095 | 1,132 | 16% | VS1-12（广度） | `TableFromMeta`、`TableCommon` 读取方法、列默认值、`DecodeRawRowData`、`table.go` / `index.go` 接口 | 分区表、写入、缓存表、临时表、mutation checker |
| D12.1 读取类执行器 | P4 | 6,633 | 743 + 631 | 21% | VS1-12（点查）、VS1-34（TableReader） | `PointGetExecutor`（不含加锁）、`NewRowDecoder`、`TableReaderExecutor`（不含分区）、`buildTableReader` 等 | IndexReader、IndexLookUp（`distsql.go`）、BatchPointGet、UnionScan、mem_reader、TABLESAMPLE |
| D12.0 执行器框架 | P4 | 4,537 | 2,012 | 44% | VS1-13 | `ExecStmt.Exec/PointGet/buildExecutor/FinishExecuteStmt`、`recordSet`、`Compiler.Compile`、`executorBuilder.build`、`internal/exec`、detach | 慢日志、语句摘要、悲观锁、外键级联、plan replayer、Top SQL |
| D05.3 会话主流程 | P4 | 6,372 | 2,124 | 33% | VS1-14 | `ExecuteStmt`、`runStmt`、`ExecutePreparedStmt`、`PrepareStmt`、`LazyTxn` 的只读部分、`txnManager`、`tidb.go` 的 `finishStmt` | 认证、bootstrap、内部会话、提交重试、非事务 DML、advisory lock |
| D05.4 事务上下文 | P2 #49 | 2,086 | 682 | 33% | VS1-14（广度） | `isolation/base.go`、可重复读与乐观事务的只读 provider | 悲观 / RC 的加锁路径、staleread |
| D13.1 plan cache（A3） | EXC | 2,369 | 1,070 | 45% | VS1-15 | `PlanCacheStmt` 构建（`GeneratePlanCacheStmtWithAST`）、`GetPlanFromPlanCache` 的未命中路径（`generateNewPlan`）、参数、可缓存性检查 | 缓存本身（`plan_cache_instance` / `lru` / `rebuild`）：VS1-39，有条件 |
| D12.12 预处理语句执行器（P5） | P5 | 8,723 | 87 | 1% | VS1-15 | `NewPrepareExec`、`PrepareExec.Next` | SHOW、EXPLAIN、SET 等 |
| D03.1 连接与协议 | P2 #49 | 6,394 | 1,943 | 30% | VS1-16（广度） | `driver_tidb.go` 的 `ExecuteStmt` / `Prepare` / 结果集、`writeResultSet` / `writeChunks` / `writeColumnInfo`、`internal/column`、`internal/dump`、`conn_stmt` 的执行与参数解析 | 连接循环、握手、认证插件（PoC 继续承担，属于 D03.1 剩余部分和 D04.1） |
| D06.5 schema 同步与 domain | P2 #49 | 5,786 | 820 | 14% | VS1-17（广度） | `loadInfoSchema`、`tryLoadSchemaDiffs`、`fetch*`、`Reload`、schema validator | 统计、权限、绑定、TTL、server ID 等后台任务 |
| D06.3 第 2 部分 | P2 #49 | — | 约 600 | — | VS1-17（广度） | `InfoCache`、`InfoSchema` trait（D06.3 tasks T8 / T9） | — |
| D07.1 store driver | P2 #49 | 2,473 | 591 | 24% | VS1-18（广度） | snapshot 的 `Get` / `BatchGet` / `SetOption`、错误转换、`batch_getter` | 迭代器（T11）、事务写入、helper |
| D09.1 SELECT 构建 | P3 | 2,208 | 1,980 | 90% | VS1-21 | `buildSelect`、`buildDataSource`、`buildSelection`、`buildProjection`、`buildSort`、`buildLimit`、通配符展开、visitInfo | 视图（`BuildDataSourceFromView`） |
| D09.4 聚合 / 窗口构建 | P3 | 3,160 | 313 | 10% | VS1-21 | `resolveHavingAndOrderBy`、`havingWindowAndOrderbyExprResolver`（ORDER BY 必经） | 其余全部 |
| D09.2 表达式改写 | P3 | 2,835 | 1,789 | 63% | VS1-22 | `expressionRewriter` 除子查询外的分支 | 子查询（`blocked-by:D09.6`）、`INSERT ... ON DUPLICATE` |
| D09.14 逻辑算子公共部分 | P3 | 1,597 | 1,352 | 85% | VS1-23 | DataSource、Projection、Selection、TableDual、schema producer、base | 其余逻辑算子 |
| D09.7 / D09.8 / D09.11 规则 | P3 | 2,196 | 1,295 | 59% | VS1-23 | 列裁剪、投影消除、谓词下推与简化、TopN / Limit 下推，只做 VS1 的算子 | join / agg / window / CTE / union 上的同名方法 |
| D13.3 hint | P3 | 2,007 | 837 | 42% | VS1-24 | query block、`ParseStmtHints`、表级 hint 栈 | 各 hint 的具体效果按需（未支持的 hint 报 warning，与 Go 相同） |
| D10.4 ranger | P3 | 3,309 | 3,309 | 100% | VS1-25 | **整块** | — |
| D10.3 物理实现枚举 | P3 | 3,174 | 368 | 12% | VS1-27 | Selection / Projection / Sort / TopN / Limit 的 `exhaustPhysicalPlans` | join / agg / window / MPP |
| D10.2 FindBestTask 与访问路径 | P3 | 5,886 | 3,127 | 53% | VS1-28 | `findBestTask`、DataSource / TableDual、skyline、表路径、`convertToTableScan` / `convertToPointGet`、`getPossibleAccessPaths`、`stats.go` 的路径统计 | 索引路径（`convertToIndexScan` 等，触发时回退）、IndexMerge、index join 路径、CTE、memtable |
| D10.6 基数估算 | P3 | 3,526 | 2,009 | 57% | VS1-29 | selectivity、pseudo、row_size、ndv、`row_count_column` | 索引行数、跨列估算、trace |
| D11.1 统计结构 | P3 | 4,961 | 913 | 18% | VS1-29 | `PseudoTable`、`HistColl` 与表 / 列 / 索引统计的读取部分 | 直方图构建、CMSketch、FMSketch、采样 |
| D10.5 代价模型 v2 | P3 | 1,195 | 641 | 54% | VS1-30 | VS1 算子的 `getPlanCostVer2` 与公共因子 | join / agg / MPP / IndexLookUp |
| D10.9 后处理 | P3 | 4,516 | 663 | 15% | VS1-31 | VS1 算子的 `ResolveIndices`、`ToPB`、access object | EXPLAIN 输出（VS1-38，可选）、plan 编码、clone |
| D07.2 coprocessor 客户端 | P2 #49 | 2,306 | 1,775 | 77% | VS1-33（广度） | `copIterator` 与 worker、任务构建、region 切分 | batch cop、MPP、paging、coprocessor cache（A3） |
| D07.3 distsql | P2 #49 | 1,818 | 1,209 | 67% | VS1-33（广度） | `Select`、`RequestBuilder` 的表路径、`selectResult` | 索引 range、分区、TiFlash |
| D12.2 基础算子 | P4 | 1,823 | 1,006 | 55% | VS1-35 | Selection、Limit、Projection（含并行）、TableDual、`ResetContextOfStmt` | MaxOneRow、Union、SelectLock、SELECT INTO |
| D12.5 排序与 TopN | P4 | 2,763 | 1,154 | 42% | VS1-36 | 内存中的 Sort、TopN、`sortPartition` | 落盘（`blocked-by:D01.10`，触发时报错，不静默）、并行排序 |
| **合计** | | | **43,864** | | | 深度 35.7k；广度（P2）8.1k | |

### 3.2 主要的边界桩

每个桩都在 tracker 的 `stubs.tsv` 中登记为临时类 `blocked-by:<块>`。规划阶段的桩返回一个带块号的类型化错误 `Blocked(block)`，路由器收到后回退到 PoC（§4.3）。执行阶段不允许出现这种错误；一旦出现，就按宪章第五条报 1235。

| 位置 | 触发条件 | 桩 |
|---|---|---|
| 表达式改写的子查询分支 | 有子查询 | `blocked-by:D09.6` |
| `buildSelect` 中的聚合、窗口、DISTINCT、join、集合运算、CTE | 对应语法 | `blocked-by:D09.3` / `D09.4` / `D09.5` |
| `getPossibleAccessPaths` 之后选中的索引路径 | skyline 之后留下了索引路径 | `blocked-by:D10.2`（VS1 之后的部分） |
| 分区表、memory table、临时表、缓存表 | 表类型 | `blocked-by:D09.12` / `D06.6` / `D06.4` |
| 函数注册表中没有的内置函数 | 用到未移植的函数族 | `blocked-by:D08.6` / `D08.7` / `D08.9` / `D08.10` |
| 统计信息 | 新栈的统计句柄只给出 pseudo 表（Go 在没有加载统计信息时也是这样） | `blocked-by:D11.2`，见 §8 风险 7 |
| 权限检查（`CheckPrivilege`） | 每条语句 | `blocked-by:D04.1`，沿用 PoC 的单用户认证模型，与 PoC 现状相同；`docs/compatibility.md` 中写明 |
| SQL binding | 集群里有 global binding | `blocked-by:D13.2`：新栈启动时读 `mysql.bind_info`，非空就整体关闭新栈，并报警告 |
| 慢日志、语句摘要、Top SQL | 每条语句 | `blocked-by:D14.2`，不记录（ROADMAP §3 已有的决定） |
| Sort / TopN 落盘 | 超过内存配额 | `blocked-by:D01.10`，报内存超限错误 |
| resource group | 每条语句 | 走默认组，`blocked-by:D15.2` |

### 3.3 函数清单的生成方法和精度

- `functions.tsv` 由 tracker 数据生成，每行是 (工作项, 块, Go 文件, 函数, 行数)。
- 选取规则：按 Go 调用链从入口开始，逐个文件挑出 VS1 算子和语句需要的函数。入口包括 `clientConn.handleQuery`、`handleStmtExecute`、`session.ExecuteStmt`、`ExecutePreparedStmt`、`planner.Optimize`、`executorBuilder.build`。挑选用的是函数名的规则，不是机器求出的可达闭包。按函数级引用，Go 是一个强连通分量（ROADMAP §3.1），求可达闭包会把几乎所有块都拉进来。
- 所以这份清单是**规划精度，约 ±20%**。每个工作项开工时，在自己的 spec 中逐个确认函数，多出或缺少的函数在 spec 中写明。

## 4. 集成路径

### 4.1 一条语句在新栈上怎么走（与 Go 对应）

```
PoC 连接循环（tidb-mysql-protocol：握手、包读写，C2 时保留）
  └ poc-session Session::query / prepare / execute
      └ Router（glue，§4.3）：开关 + AST 预检 → VS1 候选？
          ├ 否 → PoC：tidb_sql::parse_and_lower → plan_select → poc-executor（现状）
          └ 是 → tidb-session（VS1-14）
                 COM_QUERY:   session.ExecuteStmt                       (session.go:2019)
                 COM_STMT_*:  session.PrepareStmt / ExecutePreparedStmt (session.go:2483 / :2654)
                   ├ PrepareTxnCtx / PrepareTSFuture（TSO 预取，sessiontxn，VS1-14）
                   ├ Compiler.Compile                                   (executor/compiler.go:45，VS1-13)
                   │   └ planner.Optimize                               (planner/optimize.go:138，VS1-10)
                   │       ├ TryFastPlan → PointGetPlan                 (optimize.go:250，VS1-11)
                   │       └ buildLogicalPlan → DoOptimize              (VS1-21…31)
                   │           logicalOptimize → physicalOptimize(FindBestTask) → postOptimize → ResolveIndices
                   │     预处理语句：GetPlanFromPlanCache → generateNewPlan (plan_cache.go:194 / :292，VS1-15)
                   ├ runStmt → ExecStmt.Exec / ExecStmt.PointGet        (adapter.go:501 / :340，VS1-13)
                   │   └ executorBuilder.build → PointGet / TableReader / Selection / Projection / Limit / Sort / TopN
                   │       ├ 点查：kv.Snapshot.Get（D07.1，VS1-18）
                   │       └ 扫描：distsql.Select → copIterator（D07.3 / D07.2，VS1-33）
                   └ recordSet → driver_tidb 结果集 → writeResultSet / writeChunks → dumpTextRow / dumpBinaryRow（D03.1，VS1-16）
                       → 字节交回 PoC 的包写出
```

- **InfoSchema**：新栈只认 `tidb-infoschema` 的 v2（D06.3）。VS1-17 移植 D06.5 的 `loadInfoSchema` / `tryLoadSchemaDiffs`，由 PoC domain 的同步循环驱动：同一个 schema version、同一批 SchemaDiff，两边各自应用。
- **语句开始时校验版本**：如果新 InfoSchema 的版本还没追上 PoC 的版本，这条语句走 PoC，不等待，也不会读到不一致的 schema。D06.5 完成后由 domain 统一驱动，PoC 的 `tidb-meta` 退出。
- **会话状态**：唯一的真实来源是新会话中的 `SessionVars`（D05.2，已移植）。
  - `USE`、`SET NAMES`、`SET` 系统变量仍由 PoC 解析（这些语句属于 D12.12，P5），解析后同步写入新会话。字段只有 PoC 已经支持的那几个：current_db、charset / collation、results charset、PoC 接受的 sysvar。
  - `SHOW WARNINGS` 由上一条语句的执行方回答，路由器记住"上一条语句由谁执行"。

### 4.2 被替换的 PoC 部件（只针对 VS1 形态）

| PoC 部件 | 行数 | 替换为 | 何时删除 |
|---|--:|---|---|
| `crates/sql` 的 `ir.rs`（AST → IR 降级）、`plan.rs`、`ranges.rs`、`types.rs` | 2.7k | D09.0 / D09.1 / D09.2、D10.x、D10.4 | C5（还要承担回退和非 VS1 语句） |
| `poc-executor` 的 `lib.rs`、`compare.rs`、`convert.rs` | 0.5k | D12.0 / D12.1 / D12.2 / D12.5 | C5 |
| `poc-session` 的 `plan` / `run` 与预处理语句表 | 约 0.6k | D05.3（VS1-14）、D13.1 的 `PlanCacheStmt`（VS1-15） | C5 |
| `tidb-mysql-protocol` 的 `value.rs` 行编码 | 0.8k | D03.1 的 `internal/dump` 与 `column`（只用于新栈的结果） | D03.1 整块完成时 |
| `tidb-distsql`（legacy）的 `dag.rs`、`decode.rs`、`ranges.rs` | 0.8k | D10.9 `ToPB`、D07.3、D10.4 | VS1-33 合入后不再被新栈使用；PoC 回退仍用到 C5 |
| `tidb-meta`（legacy）的 `infoschema.rs` | 0.5k | `tidb-infoschema` v2（D06.3） | D06.5 完成 |
| `poc-kv` 的点查 | — | D07.1 snapshot | D07.1 完成 |

**C2 时保留**：
- 连接循环与握手（D03.1 剩余部分）、认证（D04.1）；
- USE / SET / SHOW WARNINGS / BEGIN 的处理（D12.12）；
- 聚合和 join 等非 VS1 语句；
- domain 同步循环（直到 D06.5 完成）；
- status 端口。

### 4.3 路由规则与开关

**开关**：`poc-server` 的启动参数 `--new-stack=off|vs1|strict`，环境变量 `TIDB_RS_NEW_STACK` 可以覆盖。不新增 SQL 可见的系统变量，避免自创 sysvar。

| 模式 | 行为 | 用途 |
|---|---|---|
| `off` | 全部走 PoC，与现在相同 | 默认值；C2a 门槛通过并经负责人确认后改成 `vs1`（🔶 §10-4） |
| `vs1` | VS1 候选走新栈，碰到 `Blocked` 时回退 PoC | 线上 / sysbench / 默认的 ab |
| `strict` | VS1 候选走新栈，碰到 `Blocked` 报 1235，不回退；非候选仍走 PoC | 每周构建统计新栈覆盖面和一致率 |

**判定顺序**（每条语句一次，开销要求小于 1 µs）：

1. **语句类型**：只有 `SelectStmt`（COM_QUERY，或预处理语句的 SELECT）是候选。
2. **AST 预检**（glue，只用于提前分流，不承担语义）。下面任何一项不满足，就直接走 PoC，不进新规划器：
   - FROM 只有一个 `TableName`，或者没有 FROM；
   - 没有 join、子查询、派生表、CTE、集合运算；
   - 没有聚合函数、GROUP BY、HAVING、DISTINCT、窗口；
   - 没有 FOR UPDATE / 锁子句、INTO、`AS OF`；
   - 函数名都在新栈的函数注册表里。
3. **规划**：新栈规划。只要碰到任何 `blocked-by` 桩（比如选中了索引路径、表是分区表），就返回 `Blocked(block)`。这时还没有打开执行器，没有副作用，路由器把这条语句交给 PoC 重新执行（`vs1` 模式），或者报错（`strict` 模式）。
4. **预处理语句**：PREPARE 时两边都做（PoC prepare + 新栈 `PrepareStmt`）。
   - 新栈返回 `Blocked`：这个 stmt id 标记为只走 PoC，之后每次 EXECUTE 都不再尝试新栈。
   - 新栈成功：列元数据用新栈的（与 Go 一致）。
   - EXECUTE 时规划碰到桩（例如参数让计划变成了索引路径），只回退这一次。

**度量**：指标 `tidbrs_route_total{engine="new|poc",reason="<block>|ast|kind"}`。每周报告据此给出"新栈执行的语句数 / 按 `blocked-by` 块归类的回退数"，回应 #128 §二.1 的"新栈上能跑的查询形态"指标。

### 4.4 AST 与 `Session: Send`

- **AST**：parser AST 中只有 `WithClause` 用 `Rc`（`ast.rs` L1000、L1014、L1067）。`PlanCacheStmt` 要在会话里保存 AST，而 N2 要求 `Session: Send`，所以 VS1-04 把它改成 `Arc`。这是 parser 的一处小改动，属于 D02 的维护，不改语法表。
- **preprocess 的结果**：v8.5.8 已经把 preprocess 的结果放在 `resolve.Context` 旁表里，不写回 AST，Rust 照此移植，AST 保持不可变。

## 5. 工作项与阶段

**口径**：
- **周**：项目一周的全部产能。按 #54 的估计（P1 约 53.5k 行用了约 3.5 周），1 周 ≈ 15k 行 Go 函数体（含测试、govec、评审）。
- **系数**：规划器中按算子方法组织的规则 ×1.2（试点估计 +10–20%）；遍历树的自由函数（FindBestTask、优化器驱动）×1.3–1.5；其余 ×1.0–1.1。
- **轨道**：`depth` 计入 30% 的深度份额；`breadth` 是 #49 中的 P2 块，计入广度份额，只是顺序由 VS1 决定。
- **验收**：每一项都要求 converge（所含函数 100% 有来源标注，或登记为 `blocked-by`）、Lucky 评审，以及每周构建默认模式不退化。表中只列各项自己的验收。

### Phase 0：前置（可并行，C2a 之前）

| ID | 内容 | 块 | Go 行 | 轨道 | 周 | 依赖 | 验收 |
|---|---|---|--:|---|--:|---|---|
| VS1-00 | N1 修订 PR 合入（摘出分派、`Arc` 物理计划、M-FT、M-SELF），同步改 `mappings.md` 的 M-PLAN | N1 | — | depth | 0.03 | #149 | 负责人确认 |
| VS1-01 | 建立 `tidb-planner`、`tidb-executor`、`tidb-session` 三个 crate（去掉 `planned`）；arena / `PlanCtx` / `M-SELF` 宏 / `make_mut_plan`；执行器 trait（N3 async `Next`）；plan 模板写进固定写法 | N0 / N1 / N3 | — | depth | 0.10 | VS1-00 | `check_layers.py` 通过；试点中的固定写法测试移过来后通过 |
| VS1-02 | 路由器与开关（§4.3）、`Blocked` 错误类型、会话状态同步、`tidbrs_route_total` 指标 | glue | — | depth | 0.08 | VS1-01 | 开关为 `off` 时 ab 与 sysbench 不变；单元测试覆盖判定顺序 |
| VS1-03 | tracker：块的部分（part）覆盖率、`stubs.tsv` 的 `blocked-by` 登记；ab / 每周构建：`strict` 模式加一轮，报告按 engine 分列 | 工具 | — | depth | 0.05 | — | 每周报告出现"新栈"一节 |
| VS1-04 | parser AST 的 `Rc<WithClause>` → `Arc`；`Session: Send` 静态断言 | D02 | — | depth | 0.02 | — | parser 全部测试通过 |

### Phase 1：点查纵向（C2a）

本阶段的项除标出的以外可以并行；VS1-20 依赖本阶段的所有其他项。

| ID | 内容 | 块 | Go 行 | 轨道 | 周 | 依赖 | 验收 |
|---|---|---|--:|---|--:|---|---|
| VS1-10 | planbuilder 骨架、preprocess（SELECT 部分）、`planner.Optimize` / `optimize`（非快速路径在 Phase 2 之前返回 `Blocked`） | D09.0、D09.13（optimize.go） | 2,295 | depth | 0.17 | VS1-01 | govec：`preprocess_test.go` 的 SQL → 错误码 / 文本（mock InfoSchema） |
| VS1-11 | 点查快速路径：`TryFastPlan`、`PointGetPlan` | D10.8 | 1,121 | depth | 0.07 | VS1-10、VS1-19 | govec 计划向量（§7.1）：点查语句集合 → PointGetPlan 的 handle / 索引值 / schema / 输出名 |
| VS1-19 | 规划器基础：`property`、`planner/util`、`baseimpl`、`base_physical_plan`、`IsPointGetWithPKOrUniqueKeyByAutoCommit` | D10.10、D10.1（部分） | 1,709 | depth | 0.11 | VS1-01 | 移植 `property` / `util` 的 Go 单测；govec（handle_cols、path） |
| VS1-12 | 点查执行器（不含加锁）、`NewRowDecoder`；table 抽象的读取部分 | D12.1（部分）、D06.4（部分，breadth） | 1,875 | depth 0.05 + breadth 0.08 | 0.13 | VS1-13、VS1-18 | govec：`DecodeRowValToChunk` / 默认值（含 NULL / 零值 / 新增列）；ab 的 `Point_Get` |
| VS1-13 | 执行器框架：`ExecStmt`、`recordSet`、`Compiler.Compile`、`executorBuilder.build`、`internal/exec` | D12.0 | 2,012 | depth | 0.15 | VS1-01 | 移植 `internal/exec` 的单测；`ExecStmt.PointGet` 的 MaxTS 短路径与 PoC 现有判定一致 |
| VS1-14 | 会话：`ExecuteStmt` / `runStmt` / `PrepareStmt` / `ExecutePreparedStmt`、`LazyTxn` 只读部分、txnManager；sessiontxn 的只读 provider | D05.3（部分）、D05.4（部分，breadth） | 2,806 | depth 0.16 + breadth 0.05 | 0.21 | VS1-13 | N2 §5 的编译期断言；SQL 级：autocommit 读、TSO 预取、warning 计数与 Go 一致 |
| VS1-15 | 预处理语句：`PlanCacheStmt`、`GetPlanFromPlanCache` 的未命中路径、参数、可缓存性检查；`PrepareExec` | D13.1（部分）、D12.12（部分） | 1,157 | depth | 0.08 | VS1-14 | govec：可缓存性检查（`plan_cacheable_checker_test.go` 的表）；ab 的预处理语句部分 |
| VS1-16 | 结果编码与预处理语句协议：`driver_tidb` 结果集、`writeResultSet` / `writeChunks`、`internal/column` / `dump`、`conn_stmt` 执行与二进制参数（含 D03.1 tasks P1-1） | D03.1（部分） | 1,943 | breadth | 0.13 | VS1-14 | govec：`dumpTextRow` / `dumpBinaryRow` 的类型矩阵；`tools/protocol-smoke` 通过 |
| VS1-17 | InfoSchema 接入：D06.3 第 2 部分（`InfoCache`、`InfoSchema` trait）；D06.5 的 `loadInfoSchema` / `tryLoadSchemaDiffs` / validator，由 PoC domain 驱动 | D06.3、D06.5（部分） | 约 1,420 | breadth | 0.10 | — | 现有 `schema_sync_process` 测试在新 InfoSchema 上通过；DDL 后版本追上 |
| VS1-18 | store：snapshot `Get` / `BatchGet` / `SetOption`、错误转换（不需要 T11 的迭代器） | D07.1（部分） | 591 | breadth | 0.04 | — | 故障场景（`kill_tikv`、`transfer_leader`）点查结果与 PoC 相同 |
| VS1-20 | C2a 集成：点查走新栈、sysbench point_select 调优（分阶段计时、火焰图） | glue | — | depth | 0.08 | 以上全部 | **C2a 门槛**（§7） |

### Phase 2：规划器主体（C2b 的前半）

本阶段的项除依赖列标出的以外都可以并行。VS1-25（ranger）和 VS1-29（基数估算）没有规划器结构上的依赖，可以最先开始。

| ID | 内容 | 块 | Go 行 | 轨道 | 周 | 依赖 | 验收 |
|---|---|---|--:|---|--:|---|---|
| VS1-26 | **`taskMap` 小试点**（spike）：FindBestTask 的记忆化、按时间戳回滚、`Arc` 子计划共享 | N1 §3.2 | — | depth | 0.05 | VS1-01 | 试点报告；不通过时先修订 N1，再开工 VS1-28 |
| VS1-21 | SELECT 构建：`buildSelect` / `buildDataSource` / `buildSelection` / `buildProjection` / `buildSort` / `buildLimit`，加上 `resolveHavingAndOrderBy` | D09.1、D09.4（部分） | 2,293 | depth | 0.18 | VS1-10、VS1-22、VS1-23 | govec 计划向量：逻辑计划树文本（`ToString`）与 Go 相同 |
| VS1-22 | 表达式改写（子查询之外） | D09.2（部分） | 1,789 | depth | 0.14 | VS1-10 | govec：`WHERE` / 投影表达式 → `Expression` 的字符串形式与类型 |
| VS1-23 | 逻辑算子公共部分，以及列裁剪、投影消除、谓词下推与简化、TopN / Limit 下推（只做 VS1 的算子；FieldType / Schema 别名审计按试点附录 A / B） | D09.14、D09.7、D09.8、D09.11（部分） | 2,647 | depth | 0.21 | VS1-01 | govec 计划向量：每条规则执行后的逻辑计划；M-FT 审计表写进 plan |
| VS1-24 | 优化器驱动（`DoOptimize`、`postOptimize` 的各 pass、`InjectExtraProjection`、`ResultReorder`）与 hint 骨架 | D09.13、D13.3（部分） | 2,171 | depth | 0.19 | VS1-21 | govec 计划向量：最终物理计划 |
| VS1-25 | ranger 整块 | D10.4 | 3,309 | depth | 0.22 | — | govec：`ranger_test.go` 的表（TestTableRange 等，用 Go 的输出作期望值） |
| VS1-27 | 物理算子与 task（VS1 算子）、Selection / Projection / Sort / TopN / Limit 的 `exhaustPhysicalPlans` | D10.1、D10.3（部分） | 2,198 | depth | 0.18 | VS1-19 | 随 VS1-28 的计划向量验收 |
| VS1-28 | FindBestTask：DataSource / TableDual、skyline、表路径、`getPossibleAccessPaths`、路径统计 | D10.2（部分） | 3,127 | depth | 0.31 | VS1-26、VS1-27、VS1-29、VS1-30 | govec 计划向量：VS1 形态的 EXPLAIN（去掉 estRows / cost 列）与 Go 相同 |
| VS1-29 | 基数估算（pseudo 路径）与统计结构的读取部分 | D10.6、D11.1（部分） | 2,922 | depth | 0.21 | — | govec：pseudo selectivity / 行数（`selectivity_test.go` 中的 pseudo 用例） |
| VS1-30 | 代价模型 v2（VS1 算子） | D10.5（部分） | 641 | depth | 0.04 | VS1-19 | govec：代价公式（`plan_cost_ver2_test.go`） |
| VS1-31 | `ResolveIndices`、`ToPB`、access object（VS1 算子） | D10.9（部分） | 663 | depth | 0.04 | VS1-27 | `ToPB` 产出的 DAG 与 Go 逐字节相同（govec，用 tipb 编码比较） |

### Phase 3：扫描执行与集成（C2b）

| ID | 内容 | 块 | Go 行 | 轨道 | 周 | 依赖 | 验收 |
|---|---|---|--:|---|--:|---|---|
| VS1-33 | coprocessor 客户端与 distsql（表路径）。如果 P2 赶不上，先用 glue 适配器把 legacy `tidb-distsql` 包在 D07.3 的 `SelectResult` 接口后面 | D07.2、D07.3（部分） | 2,984 | breadth | 0.20 | — | 故障场景全部 GREEN；`wide_rows` 大响应通过 |
| VS1-34 | TableReader 执行器 | D12.1（部分） | 631 | depth | 0.04 | VS1-13、VS1-31、VS1-33 | ab：`TableReader` / `TableFullScan` / `TableRangeScan` |
| VS1-35 | Selection / Limit / Projection（含并行投影）/ TableDual 执行器 | D12.2（部分） | 1,006 | depth | 0.07 | VS1-13 | 移植不依赖存储的 Go 单测（mock 数据源）；ab：`Selection` / `Projection` / `Limit` / `TableDual` |
| VS1-36 | Sort / TopN（内存中） | D12.5（部分） | 1,154 | depth | 0.08 | VS1-13 | 移植 `sortexec` 中不落盘的单测；ab：`Sort` / `TopN` |
| VS1-37 | C2b 集成：扫描类语句走新栈、read_only 调优、`vs1` 默认值评审 | glue | — | depth | 0.10 | 以上全部 | **C2b 门槛**（§7） |
| VS1-38 | （可选）EXPLAIN（brief / row）只支持 VS1 算子，供 ab 做计划级对照 | D10.9、D09.16、D12.12（部分） | 约 450 | depth | 0.05 | VS1-28 | ab 在 `strict` 模式下比较 EXPLAIN 的算子树 |
| VS1-39 | （有条件）VS1 形态的实例级 plan cache：A3 plan cache spec、`plan_cache_instance`、PointGet / TableScan 的 range 重建（路径复制） | D13.1（部分） | 约 650 | depth | 0.15 | VS1-15、VS1-28 | 只有在 VS1-37 测得 read_only < 0.9× 时才做，或者负责人要求切换默认值之前必须有它（🔶 §10-3） |

### 关键路径

```
VS1-00 → VS1-01 → VS1-10 → VS1-21 → VS1-24 ┐
                  VS1-26 → VS1-28 ←────────┤← VS1-27 / VS1-29 / VS1-30
                                            └→ VS1-31 → VS1-34 → VS1-37
                  VS1-13 → VS1-14 → VS1-15 / VS1-16 → VS1-20（C2a）
```

最长链是 VS1-01 → VS1-10 → VS1-22 → VS1-21 → VS1-24 → VS1-28 → VS1-31 → VS1-34 → VS1-37。并行度足够时，日历时间约为总量的一半。

## 6. 估算与日程

| 部分 | 深度（周） | 广度（周） |
|---|--:|--:|
| Phase 0 | 0.28 | — |
| Phase 1（C2a） | 0.87 | 0.40 |
| Phase 2 | 1.77 | — |
| Phase 3（不含可选 / 有条件项） | 0.29 | 0.20 |
| **合计** | **3.21** | **0.60** |
| 可选 / 有条件（VS1-38、VS1-39） | 0.20 | — |

深度线的日历时间，从 11-16 起算：

| 深度份额 | C2a（Phase 0 + 1，1.15 周产能） | C2 整体（3.21 周产能） |
|---|---|---|
| 30%（负责人当前的设定） | 约 3.8 周 → **约 12-11** | 约 10.7 周 → 约 2 月初 |
| 50%（到 C2 为止临时调高） | 约 2.3 周 → 约 12-02 | 约 6.4 周 → **约 12-30** |
| 30%，按 PR 时间算的速度（约 25k 行/周） | 约 2.3 周 | 约 6.4 周 → 约 12-30 |

说明：

- **速度的不确定性**：#54 草案说"P1 约 3 周完成 17/24"，按它算 15k 行/周；但 P1 的大部分 PR 合入集中在 11-07 至 11-13，按 PR 时间算接近 25k 行/周。本计划用保守的 15k。每周构建的"本周用量"有了数据之后再校正。
- **调高深度份额不影响 C3**：P2 未完成的部分约 30k 行，其中 8.1k 已经算在 VS1 的广度项里。剩下约 22k 行，按 50% 份额（7.5k 行/周）约 3 周，仍然赶得上 C3（12-25）的估计。
- **建议**：C2a 定在 12-11（30% 份额就可以达到）。C2b 有两个选项，由负责人定（🔶 §10-2）：
  - 到 12-11 为止把深度份额临时调到 50%，C2b 目标约 12-30；
  - 保持 30%，C2b 目标约 2 月初。

## 7. 验收

### 7.1 函数级与计划级（每个工作项）

- **纯函数**用 govec 向量回放：ranger、pseudo 基数估算、代价公式、`dumpTextRow` / `dumpBinaryRow`、行解码与默认值、可缓存性检查、preprocess 的错误。
- **规划器**用"计划向量"：
  - 生成：Go 侧用 v8.5.8 在 unistore 上建固定的表（sbtest 形状，加 4 张带不同主键 / 唯一键 / 类型组合的表），对一组 VS1 形态的 SQL 输出逻辑计划（`ToString`）和 `EXPLAIN FORMAT='brief'`。
  - 比较：Rust 用同一份 TableInfo JSON 和 pseudo 统计信息回放，去掉 estRows 和 cost 两列后逐行比较。
  - 语句集合取自 `planner/core/casetest` 的 testdata 中的单表语句，加上 sysbench 的语句。
  - 生成器放在 tracker 的 govec 下（新领域 `plan`）。

### 7.2 结果级（每周构建）

| 检查 | C2a | C2b |
|---|---|---|
| ab 默认模式（`vs1`）的总一致率 | 100%（不退化） | 100% |
| ab `strict` 模式：`Point_Get` 分类 | 100% 一致，并且新栈执行了其中所有不含 `blocked-by` 的语句 | 同左 |
| ab `strict` 模式：`TableReader` / `TableFullScan` / `TableRangeScan` / `Selection` / `Projection` / `Limit` / `Sort` / `TopN` / `TableDual` | — | 100% 一致；回退数按 `blocked-by` 归类，只能是 §1.2 列出的原因 |
| 故障场景（3 TiKV）、锁测试 | 新栈下 GREEN | 新栈下 GREEN |
| sysbench point_select（1 / 8 线程，`vs1` 模式） | ≥ 0.9× Go | ≥ 0.9× Go |
| sysbench read_only（1 / 8 线程，`vs1` 模式，其中 sum / distinct 由 PoC 执行） | — | ≥ 0.9× Go |
| 只含 VS1 语句的 sysbench（自定义 lua：point_selects + simple_ranges + order_ranges） | — | 记录，作为新栈本身的性能（不设门槛，用于定位） |

## 8. 风险与缓解

| # | 风险 | 影响 | 缓解 |
|---|---|---|---|
| 1 | **N1 的 `taskMap` 没有验证过**：FindBestTask 用 `taskMap` 记忆化，并按时间戳回滚（`taskMapBak`）。D09.7 试点没有覆盖这部分（试点报告 §6 第 6 条）。 | 这是 VS1-28（3.1k 行，最贵的一项）的形态问题。如果中途发现 `Arc` 共享或回滚的写法行不通，要返工 VS1-27 / VS1-28 | VS1-26 在 D10.2 开工前补一个小试点（0.05 周），覆盖三点：同一子计划挂在多个父候选下、回滚、落选候选丢弃后 `Arc::get_mut` 能拿到独占。不通过时先修订 N1 再开工。VS1-26 不依赖其他项，可以在 Phase 1 期间就做 |
| 2 | **plan cache 的时机**（#128 §一.5）：Go 默认开着会话级 prepared plan cache（`DefTiDBEnablePrepPlanCache = true`，`tidb_vars.go:1556`），sysbench 用预处理语句，所以 Go 基线有缓存。tidb-rs 只做实例级（负责人决定），VS1 没有缓存。 | 点查走 `TryFastPlan`，开销小；range / order 语句每次 EXECUTE 都要完整优化一遍，read_only 可能低于 0.9×。负责人要求 D13.1 在 PoC 规划器被替换之前完成 | (a) VS1-15 先做 `PlanCacheStmt` 和未命中路径，预处理语句的结构与 Go 相同，以后加缓存不用改会话。(b) VS1-39 是有条件项：测得 read_only < 0.9×，或者负责人要求"切换默认值之前必须有实例级 plan cache"时才做，并且先写 A3 的 plan cache spec。spec 要处理三件事：试点报告 §5 的风险 1–4，以及 N4 的 `BuiltinFuncCache` 在多个会话共享计划时的锁争用（试点报告 §5 风险 3）。(c) 开关默认值是否切换，以这一条为前提（🔶 §10-3） |
| 3 | **相对 PoC 的性能回退**：PoC 是 Go 的 1.13–1.27 倍（每周构建 W46），新栈的目标是 ≥ 0.9 倍。 | 默认值切到 `vs1` 时，每周构建的结论会变成 REGRESSED；宪章第七条 4 要求端到端不退化 | 不退化的检查对象是正确性。性能基线在负责人确认后，随默认值切换重新设定（🔶 §10-4）。默认值在连续两次每周构建达标后才切换。每个 Phase 用 `poc-session` 的分阶段计时（`timing.rs`）比较 parse / plan / execute，再按需优化：TSO 预取、点查 MaxTS 短路径、chunk 复用、arena 一次性释放。不加 Go 没有的捷径 |
| 4 | 速度与范围：C2 按 30% 份额赶不上 12-11 | 检查点失信 | §6 拆成 C2a / C2b；每周构建报告新栈的覆盖面，逐周校正 |
| 5 | 两份 schema 状态不一致：PoC 的 `tidb-meta` 和新的 v2 InfoSchema 同时存在 | 新栈读到旧 schema，结果错误 | 同一个 diff 流驱动两边；每条语句检查版本，新 InfoSchema 落后时走 PoC（§4.1）；故障场景加一个"扫描期间 DDL"的情况 |
| 6 | 会话状态分叉：current_db、charset、sysvar、warning、预处理语句 id | 结果或报错不同 | 唯一的真实来源是新会话的 `SessionVars`；PoC 处理完 SET / USE 后写入它；SHOW WARNINGS 按上一条语句的执行方回答；ab 的 integrationtest 子集在 `vs1` 模式下跑 |
| 7 | 只有 pseudo 统计信息（D11.2 在 P3 后段）：Go 集群里有真实统计信息时，计划可能不同 | 有索引竞争时访问路径不同；EXPLAIN 的 estRows 不同 | VS1 形态的主路径（handle 点查、handle 范围、Sort / TopN / Limit）不依赖统计信息；计划向量去掉 estRows；选中索引路径时本来就回退 PoC；`docs/compatibility.md` 中写明 |
| 8 | 按块的一部分移植与宪章第四条 2 的关系 | 覆盖率口径混乱，以及"拉进单个函数"的嫌疑 | 沿用 D06.2 / D06.3 第 1 部分的先例：每个部分有函数清单（`functions.tsv`）、converge 按部分统计、其余函数登记 `blocked-by`。tracker 加部分标签（VS1-03）。需要负责人确认（🔶 §10-1） |
| 9 | 桩泄漏到执行阶段：规划时没有拦住，执行时才发现 | 执行了一半才报错 | `Blocked` 只允许在规划和 `executorBuilder.build` 期间返回（这时还没有 `Open`）；之后出现就按 1235 报错，并计入 `bug`；`strict` 模式的 ab 会暴露这种情况 |
| 10 | 与 P2 的人手和合并冲突：D03.1 / D05.4 / D06.4 / D06.5 / D07.x 在两个计划里都出现 | 重复劳动，或者顺序冲突 | §9：这些项由 P2 的负责人按"第 1 部分 = VS1 需要的部分"先做，VS1 不另派人 |

## 9. 与 P2 里程碑（#49）的关系和冲突

| #49 中的项 | VS1 需要的部分 | 冲突与处理 |
|---|---|---|
| D06.2 meta 读取 | 第 1 部分已够用（InfoSchema 加载） | 无。第 2 部分（autoid 等）VS1 不需要 |
| D07.1 store driver。#49 写着"开工前决定 Iterator 的 T11" | 只需要 snapshot 的 `Get` / `BatchGet` / `SetOption`（VS1-18） | VS1 **不受 T11 阻塞**。建议 D07.1 分两部分：第 1 部分点查读取（VS1-18），第 2 部分迭代器与其余（等 T11） |
| D08.3 下推判定 | 已完成 | #49 的勾选框还没勾（实际 converge 50/50，#179 / #181 / #182），建议更新 |
| D14.0 | 已完成 | 无 |
| D06.4 table 抽象 | 读取部分 1.1k（VS1-12） | 建议作为 D06.4 第 1 部分先做 |
| D07.2 coprocessor | 1.8k（VS1-33） | 如果 Phase 3 开始时还没有合入，VS1 用 glue 适配器包 legacy `tidb-distsql`，C2b 不等它。client-go 对照表的 P0 已修复，后续的 Backoffer / region cache 项与 VS1 无关 |
| D06.3 InfoSchema | 第 1 部分已完成；第 2 部分 T8 / T9（VS1-17） | #49 的勾选框没有反映第 1 部分已合入（#176 / #177）。第 2 部分是 VS1 的前置，建议排在 P2 的前面 |
| D07.3 distsql | 1.2k（VS1-33）。#49 写着"零拷贝解码与 N5a 的验收见 #35" | 无冲突；#35 的性能验收可以借用 VS1 的 read_only 测量 |
| D05.4 事务上下文 | 0.7k（VS1-14） | 作为 D05.4 第 1 部分，由 VS1-14 的执行者一并完成，或者由 P2 先做；两者不要同时开工 |
| D06.5 schema 同步与 domain | 0.8k（VS1-17） | 在 D06.5 完成之前，新 InfoSchema 由 PoC 的同步循环驱动（§4.1）。D06.5 整块完成时去掉这段 glue |
| D03.1 连接与协议 | 1.9k（VS1-16） | ROADMAP §3 原定"P4 之前协议层接 PoC 会话"，D03.1 → D05.3 有 87 处引用。VS1 先移植 D05.3 的一部分，正好让 D03.1 的结果写出可以接真会话。建议 D03.1 第 1 部分 = VS1-16，连接循环和握手留在第 2 部分 |

另外两处文档要随 VS1 改：

- **方法论 §6**："P3 之前不能把移植代码接到它们里面"和空窗期的表格，改为引用本计划。VS1 不是"Go 形状的最小规划器"，它移植的是真实的 Go 函数，其余用桩，符合原来不写自创规划器的约束。
- **ROADMAP §5 的检查点**：按 #54 的定稿更新。

## 10. 需要负责人决定（🔶）

1. **按块的一部分移植**：VS1 的工作项都是"块 X 的 VS1 部分"，块内其余函数登记为 `blocked-by:X`，块留到后续完成（D06.2 / D06.3 已有先例）。是否作为宪章第四条 2 的正式解释写进方法论？
2. **C2 的日程**：
   - (a) 拆成 C2a（12-11，点查纵向）和 C2b；
   - (b) C2b 前把深度份额临时调到 50%（约 12-30），或者保持 30%（约 2 月初）。
3. **实例级 plan cache 与切换默认值的关系**（#128 §一.5）：
   - 默认值切到 `vs1` 之前，是否必须先有 VS1 形态的实例级 plan cache（VS1-39，约 0.15 周，加 A3 spec）？
   - 还是只要 read_only 达到 ≥ 0.9× 就可以切换？
4. **开关默认值切换与性能基线**：C2a / C2b 达标后，默认值改为 `vs1`，每周构建的性能基线随之改为新栈。同意后，切换那一周的 REGRESSED 视为预期。

## 附录 A：每个块的 VS1 函数数

| 工作项 | 块 | 函数数 | Go 行 |
|---|---|--:|--:|
| VS1-10 | D09.0、D09.13 | 107 | 2,295 |
| VS1-11 | D10.8 | 52 | 1,121 |
| VS1-12 | D06.4、D12.1 | 134 | 1,875 |
| VS1-13 | D12.0 | 123 | 2,012 |
| VS1-14 | D05.3、D05.4 | 170 | 2,806 |
| VS1-15 | D13.1、D12.12 | 46 | 1,157 |
| VS1-16 | D03.1 | 96 | 1,943 |
| VS1-17 | D06.5（另加 D06.3 第 2 部分约 600 行，未计入清单） | 36 | 820 |
| VS1-18 | D07.1 | 40 | 591 |
| VS1-19 | D10.1、D10.10 | 189 | 1,709 |
| VS1-21 | D09.1、D09.4 | 49 | 2,293 |
| VS1-22 | D09.2 | 53 | 1,789 |
| VS1-23 | D09.7、D09.8、D09.11、D09.14 | 177 | 2,647 |
| VS1-24 | D09.13、D13.3 | 95 | 2,171 |
| VS1-25 | D10.4 | 95 | 3,309 |
| VS1-27 | D10.1、D10.3 | 132 | 2,198 |
| VS1-28 | D10.2 | 98 | 3,127 |
| VS1-29 | D10.6、D11.1 | 140 | 2,922 |
| VS1-30 | D10.5 | 32 | 641 |
| VS1-31 | D10.9 | 36 | 663 |
| VS1-33 | D07.2、D07.3 | 165 | 2,984 |
| VS1-34 | D12.1 | 26 | 631 |
| VS1-35 | D12.2 | 37 | 1,006 |
| VS1-36 | D12.5 | 65 | 1,154 |
| **合计** | | **2,193** | **43,864** |
