# D09.1 SELECT 构建（spec，第 1 部分：VS1-21）

状态：第 1 部分评审中（2026-11-21）。块：SELECT 构建（D09.1），加上聚合、窗口构建（D09.4）中 ORDER BY / HAVING 必经的 `resolveHavingAndOrderBy`。crate `tidb-planner`。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（D09.1、D09.4 两行）、§5（VS1-21）；[表达式改写 spec](../D09.2-rewriter/spec.md)；[逻辑算子 spec](../D09.14-logical-operators/spec.md)；[N1 计划表示](../exceptions/N1-plan-representation/spec.md)。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/planner/core/`，行号以参照树为准。

## 1. 这一块做什么

预处理之后的 SELECT 语法树，由计划构建器（`PlanBuilder`）翻译成逻辑计划树。第一条纵向切片的查询都是单表查询，构建结果只有这几种算子：

```text
SELECT c FROM sbtest1 WHERE id BETWEEN 1 AND 100 ORDER BY c LIMIT 10

Limit ← Sort ← Projection ← Selection ← DataSource(sbtest1)
```

- `FROM t`：`buildDataSource` 建 DataSource，带上表的全部公开列、句柄列（整数主键、聚簇主键或隐藏的 `_tidb_rowid`）、可能的访问路径（表扫描、每个可用索引）；`FROM` 省略时是一行的 TableDual；`FROM (SELECT ...) s` 递归构建子查询块并把输出列名的表名改成 `s`。
- `WHERE`：`buildSelection` 把条件拆成 AND 项，逐项交给表达式改写（D09.2）；常量恒真的项去掉，恒假时整个子树换成空的 TableDual。
- 选择列表：`unfoldWildStar` 展开 `*` / `t.*`，`buildProjection` 改写每一项并给出输出列名（列引用取列名，别名取别名，表达式取原文去掉 `/*! */` 标记，字面量按 MySQL 的规则取名）。
- `HAVING`（没有聚合时）：列名先在选择列表中找，再到 FROM 的计划中找；建在 Projection 之上的 Selection。
- `ORDER BY`：`resolveHavingAndOrderBy` 先把 ORDER BY 中不在选择列表里的列追加为"辅助列"，然后 `buildSort`；最后多出的辅助列由一个 Projection 去掉。`ORDER BY 1` 这样的位置项由改写器处理。
- `LIMIT`：`buildLimit`；`LIMIT 0` 换成空的 TableDual。

构建同时累积优化标志（`optFlag`，决定后面运行哪些逻辑规则）和权限检查项（`visitInfo`）。

Go 代码：`logical_plan_builder.go`（`buildSelect` 4346 行起，`buildDataSource` 5072 行起，`resolveHavingAndOrderBy` 2876 行起），`planbuilder.go` 的 `Build`、访问路径和句柄列栈。

## 2. 范围

函数清单是 `specs/VS1-vertical-slice/functions.tsv` 中 VS1-21 的 58 个函数（D09.1 43 个，D09.4 6 个，D10.2 9 个：访问路径和句柄列栈，原属 VS1-28，见 §2.2）。

### 2.1 converge（`--part specs/VS1-vertical-slice/functions.tsv --item VS1-21`）

| 块 | 已移植 | 桩 | 合计 |
|---|--:|--:|--:|
| D09.1 SELECT 构建 | 29 | 14 | 43 |
| D09.4 聚合、窗口构建（HAVING / ORDER BY 解析） | 6 | 0 | 6 |
| D10.2 访问路径、句柄列栈 | 9 | 0 | 9 |

missing 0、extra 0。D09.1 中不在 VS1 清单里的 5 个函数（视图 3 个、UPDATE / DELETE 的 `updatableTableListResolver` 2 个）属于本块第 2 部分。

### 2.2 清单之外一起移植的函数

构建 SELECT 时直接调用、又不能做成桩的函数（做成桩的话每条 SELECT 都会停下）。D10.2 的 9 个（`getPossibleAccessPaths`、`filterPathByIsolationRead`、`fillContentForTablePath`、`getPathByIndexName`、`isPrimaryIndex`、`removeIgnoredPaths`、`removeGlobalIndexPaths`、`handleColHelper.pushMap` / `popMap`）在 functions.tsv 中从 VS1-28 移到了 VS1-21，计入 §2.1；`genTiFlashPath`、`checkAutoForceIndexLookUpPushDown` 不在 VS1 清单中：

| 函数 | 所属块 | 为什么 |
|---|---|---|
| `getPossibleAccessPaths`、`filterPathByIsolationRead`、`fillContentForTablePath`、`genTiFlashPath`、`getPathByIndexName`、`isPrimaryIndex`、`removeIgnoredPaths`、`removeGlobalIndexPaths`、`checkAutoForceIndexLookUpPushDown` | 访问路径（D10.2，原属 VS1-28） | `buildDataSource` 要算出 DataSource 的访问路径；它们也可能报错（隔离读引擎没有可用路径）。VS1-28 直接使用这里的翻译 |
| `handleColHelper.pushMap` / `popMap` | D10.2 | 构建器的句柄列栈，DataSource、TableDual、`buildSelect` 都要压栈出栈 |
| `PlanBuilder.detectSelectWindow`、`isForUpdateReadSelectLock` | 计划构建器公共部分（D09.0） | `buildSelect` 调用。后者原来在 `point_get_plan.rs` 有一份私有翻译，移到 `planbuilder.rs`（文件对应），快速计划路径改为调用它 |
| `PlanBuilder.checkOnlyFullGroupBy`、`checkOnlyFullGroupByWithOutGroupClause`、`colResolverForOnlyFullGroupBy.Enter / Leave` | D09.4 | 默认 SQL mode 含 `ONLY_FULL_GROUP_BY`，每条带 FROM 的 SELECT 都经过这一检查。没有聚合时它只遍历选择列表就返回；有聚合时后半段（WHERE / join 的函数依赖）是桩 |
| `SelectField.Match`、`ColumnName.Match`（parser AST 的方法） | parser（D02.2） | Rust AST 没有这两个方法，构建器用到时移植到构建器中 |
| `logical_rules.go` 的规则标志常量 | 优化器驱动（D09.13） | `optFlag` 的各个位 |

### 2.3 与表达式改写评审（#258）有关的修改

SELECT 构建器开始往这些结构里写之前，先处理了表达式改写评审中的意见（同一 PR 的第一个提交）：

1. 聚合、窗口、列三个映射表原来用 AST 节点地址（`usize`）做键，不能保证节点活着。改为 `ByAddr<'ast, T>`：按地址比较和取哈希，同时借用节点，借用检查保证键不会比节点活得久（D09.2 spec §3 第 4 条）。`PlanBuilder` 因此带上 AST 的生命周期 `PlanBuilder<'ast>`。
2. `VisitInfo.err` 原来只存错误消息，改为存整个错误（`Option<Error>`），`CheckPrivilege` 返回给客户端时有错误码和 SQLSTATE。
3. 改写回放中已知的别名差异，原来整条记录豁免，现在只把那一个值（cast 下面 ENUM 列的 coercibility）置空，其余必须与 Go 相同。
4. 函数类 → 块的对照表加了单元测试：表按类名严格有序，注册表中每个没移植的函数类都有块号。

## 3. 承载方式

1. **入口。** `PlanBuilder::build(ctx, stmt)` 返回逻辑计划根节点的 `PlanId`（计划在 `PlanCtx` 的 arena 中，N1）。SELECT 走 `build_select`；`SELECT ... INTO`、集合运算和其他语句是各自块的桩。`PlanBuilder::init(vars, is)` 接收语句的 InfoSchema（Go 的 `b.is`），`build_data_source` 用它的 `table_by_name` 取表，再走已移植的元数据锁函数（`tryLockMDLAndUpdateSchemaIfNecessary`，域的 InfoSchema 由调用方放进 `domain_is`，与预处理器相同）。Go 把节点原文存在节点里（`Text()`），Rust AST 只存字节区间，所以构建器另有 `sql_text`（语句的 SQL 文本）。
2. **选择列表。** Go 在构建过程中改写 AST：`sel.Fields.Fields` 被换成展开 `*` 之后的列表，HAVING / ORDER BY 解析时再往里追加辅助列，最后恢复原样。Rust 的 AST 是借来的，构建器维护自己的列表 `Vec<SelectFieldW>`：每一项借用 AST 中的节点，或拥有构建器新建的列名节点（Go 新分配的 `*ast.ColumnNameExpr`），带上 Go 只在构建器里设置的字段（`Auxiliary`、`AuxiliaryColInAgg`、`AuxiliaryColInOrderBy`、`IsUnfoldFromWildCard`）。映射表的键只来自语句 AST（HAVING / ORDER BY 中的列名、聚合函数），不会是新建的节点。
3. **聚合参数的替换。** 解析 HAVING / ORDER BY 时，Go 把聚合函数里的列名替换成它对应的选择列表达式（`select a+1 as b from t having sum(b) < 0` 聚合的是 `a+1`）。替换后的参数只有 `buildAggregation` 读，而聚合是桩（D09.4），所以不做替换（L2，写在 `Leave` 的注释中）。HAVING / ORDER BY 中有聚合时，`detectSelectAgg` 为真，构建一定停在聚合的桩，替换不会被观察到。
4. **派生表的表名（共享写入）。** `FROM (SELECT ...) s` 时，Go 通过共享的 `*types.FieldName` 原地写 `name.TblName = s`，子查询块里所有持有同一批名字的算子（Selection、Sort、Limit 等）都看到新表名；`resolveFromPlan` 会沿这些算子往下找名字，所以这是可见的。Rust 的输出列名是共享的 `Arc<FieldName>`：改名时生成新的名字，并在派生表子树里把每个算子中指向同一个 `Arc` 的名字都换掉，结果与 Go 相同（`M-FT` 的显式模拟）。
5. **计划 id 的分配顺序。** EXPLAIN 的算子 id 来自分配顺序，Rust 在 Go 调 `Init` 的同一位置分配：Selection、Projection、Sort 在改写各项表达式**之前**分配（`buildSelection` 返回 TableDual 时，Selection 的 id 被跳过，Go 也是如此），DataSource 在算访问路径之后、分配列 id 之前。
6. **共享的访问路径。** Go 的 `PossibleAccessPaths` 和 `AllPossibleAccessPaths` 是同一批 `*AccessPath` 指针的两个切片，之后对 `AllPossibleAccessPaths` 写入的 `FullIdxCols` 在两个列表中都看得到。Rust 按值保存，写完 `all_possible_access_paths` 后把它复制给 `possible_access_paths`（此时两个列表的内容本来就相同）。
7. **query block offset。** Go 的 `sel.QueryBlockOffset` 由 hint 处理器（D13.3）在优化前遍历语法树时设置。hint 处理器还没有移植，Rust 的每个查询块 offset 都是 0，与 Go 的 `BuildLogicalPlanForTest`（不运行 hint 处理器）相同。`len(b.qbOffset) == 1`（是否顶层查询块）用的是栈深度，不受影响。
8. **会话的未提交写入。** `tableHasDirtyContent` 问会话的事务是否写过这张表。规划上下文增加 `has_dirty_content`（与 `client` 一样由优化器驱动在路由语句时设置，VS1-24）；没有设置时是桩（`blocked-by:D09.13`）。
9. **表的列。** Go 用 `tbl.Cols()`（`table.Table` 的公开列）。规划器只有表的元数据，按 `TableInfo.Columns` 的顺序取状态为 public 的列，结果相同。`tbl.Type().IsVirtualTable()`（内存表）按库名判断（`information_schema` 等内存库，`IsMemDB`）。
10. **PREPARE 和 `buildLogicalPlan`。** 构建器需要规划上下文（arena、表达式上下文），这由优化器驱动建立（VS1-24）。PREPARE 的 `GeneratePlanCacheStmtWithAST` 和 `optimize.rs` 的 `buildLogicalPlan` 还没有规划上下文，原来的 `blocked-by:D09.1` 改为 `blocked-by:D09.13`（session 的测试相应更新）。

## 4. 数据源与访问路径

`buildDataSource` 照 Go 的顺序：表名解析（不带库名时用当前库；Go 先试 CTE，CTE 是桩所以总找不到）→ 取表 → 元数据锁 → 内存表、分区表的检查 → 访问路径 → 视图、序列 → 隔离读引擎过滤 → 生成列替换标志 → 建 DataSource：列、输出列名、列 id、句柄列、句柄列栈 → 虚拟生成列 → 聚簇主键的完整列 → 每条访问路径的完整索引列（含 `tidb_shard()` 前缀唯一索引的标记）→ 未提交写入 / 本地临时表 / 缓存表的 UnionScan。

`getPossibleAccessPaths`：表路径（整数句柄或聚簇主键），TiFlash 副本可用时加 TiFlash 路径，再加每个可用的公开索引（不可见索引在 `tidb_opt_use_invisible_indexes` 关闭时跳过，聚簇主键已是表路径）。默认的 `tidb_index_lookup_pushdown_policy`（`hint-only`）下不自动下推索引回表。全部路径都"待定"（多值索引、部分索引）时再加一次表路径。表名上的索引 hint（`USE / FORCE / IGNORE INDEX`，#489 起）照 Go 处理：只看作用于扫描的 hint（`FOR JOIN / ORDER BY / GROUP BY` 跳过）；`USE INDEX ()` 选表路径；每个索引名按 `getPathByIndexName` 找路径（全名，或唯一的前缀），找不到时报 `ErrKeyDoesNotExist`（1176，`Key 'x' doesn't exist in table 't'`）；`IGNORE` 的路径之后去掉；`USE` / `FORCE` 把路径标为 `Forced`（Go 不区分两者），只保留被选的路径；`tidb_isolation_read_engines` 不含 tikv 时，TiKV 索引报 Go 的 `fmt.Errorf` 错误（1105）。Go 的 `available` / `ignored` 存的是指向 `publicPaths` 中同一对象的指针，Rust 存下标，循环结束后再取出路径（hint 设的标志都在），结果相同。注释形式的 hint（`/*+ USE_INDEX(...) */`、`INDEX_LOOKUP_PUSHDOWN` 等，`tableHints`）属于 hint 框架（D13.3），这里还没有，所以 Go 中"注释形式的 hint 只给 warning"的分支走不到。`filterPathByIsolationRead` 按 `tidb_isolation_read_engines` 去掉不允许的存储，一条不剩时报 Go 的错误。

## 5. 边界桩

每一处都放在 Go 走那条分支的位置，返回 `blocked-by:<块>`（错误 1235，路由器回退到原型）。tracker `data/stubs.tsv` 中每一行都有说明。

| 位置 | 情况 | 阻塞块 |
|---|---|---|
| `buildSelect` | 查询块的表 hint（`/*+ ... */`） | D13.3 |
| `buildSelect` | `WITH`（CTE） | D09.5 |
| `buildSelect` | `GROUP BY`、窗口函数和 `WINDOW`、聚合、`DISTINCT` | D09.4 |
| `buildSelect` | `FOR UPDATE` / `FOR SHARE`（LogicalLock） | D09.14 |
| `buildSelect` / `resolveHavingAndOrderBy` | 选择列表、HAVING、ORDER BY 中的子查询（外层聚合、相关列） | D09.6 |
| `checkOnlyFullGroupByWithOutGroupClause` | 聚合和普通列混用时的函数依赖检查 | D09.4 |
| `buildResultSetNode` / `Build` | 集合运算（UNION 等） | D09.5 |
| `Build` | `SELECT ... INTO` | D09.16 |
| `buildJoin` | 两个表源的 join | D09.3 |
| `buildProjectionField` | NATURAL / USING join 冗余一侧的列 | D09.3 |
| `buildProjection` / `buildProjectionField` | `tidb_enable_new_only_full_group_by_check`（函数依赖） | D09.14 |
| `buildDataSource` | 内存表（`information_schema` 等） | D10.11 |
| `buildDataSource` | 分区表（D06.4 第 1 部分只标记分区表，读它的 record prefix 会触发 `debug_assert`，#217 评审） | D09.12 |
| `buildDataSource` | 视图、虚拟生成列、聚簇主键含非公开列 | D09.1（第 2 部分） |
| `buildDataSource` | 事务写过的表、本地临时表、缓存表（LogicalUnionScan） | D09.14 |
| `buildDataSource` | `TABLESAMPLE` | D02.2 |
| `buildDataSource` | 构建器没有 InfoSchema | D05.4 |
| `getPossibleAccessPaths` | `HYPO_INDEX` hint | D13.3 |
| `getPossibleAccessPaths` | EXPLAIN 的假设索引 / 假设 TiFlash 副本；加锁读或 READ COMMITTED 下读域中最新的索引状态（`GetLatestIndexInfo`） | D10.2 |
| `checkAutoForceIndexLookUpPushDown` | `tidb_index_lookup_pushdown_policy` 为 `force` / `affinity-force` | D10.2 |
| `tableHasDirtyContent` | 规划上下文没有会话的未提交写入检查 | D09.13 |
| `itemTransformer.Enter` | `ORDER BY ?`（Rust 的位置节点没有参数标记形式，与改写器一致） | D02.2 |
| `collectGenerateColumn`（`buildProjection` 有 ORDER BY 时） | 被索引覆盖的虚拟生成列（数据源已先停在虚拟生成列） | D09.13 |
| 桩函数 | `CheckParamTypeInt64orUint64`（参数标记的值，D13.1）；`addAliasName`、`getTableOffset`、`collectTableName`（CREATE VIEW、UPDATE / DELETE，D09.15）；`addExtraPhysTblIDColumn4DS`（D09.12）；`findColumnNameByUniqueID`（D09.6）；`extractSingeValueColNamesFromWhere`、`allColFromAggExprNode`、`allColFromExprNode`、`colNameResolver`、`compareItems`、`getAllByItems`、`restoreByItemText`、`checkOrderByInDistinct`（D09.4） | 见左 |

两处比 Go 停得更早，路由器会回退到原型：

- READ COMMITTED 下（以及加锁读），连接号大于 0 的会话里，`getPossibleAccessPaths` 要从域中读最新的索引状态（`GetLatestIndexInfo`），这是桩，所以 READ COMMITTED 下对任何有二级索引的表的 SELECT 都会回退。
- 不在 EXPLAIN 中的 `HYPO_INDEX` hint：Go 只给一个 warning 然后继续，Rust 停在 `blocked-by:D13.3`（hint 的语句级状态还没有移植）。

用户变量赋值（`@a := ...`）在 `preprocessUserVarTypes` 中交给改写器，停在改写器的桩（D08.9）；系统变量停在改写器第 2 部分（D09.2）。

## 6. 测试

- **govec 领域 `selectbuild`**（tracker `govec/domains/selectbuild.go`，1,743 条；回放 `crates/planner/tests/govec_selectbuild.rs`）。表来自 7 条 CREATE TABLE：sysbench 表（整数主键）、无主键表（隐藏句柄）、非聚簇字符串主键、聚簇复合主键（含前缀索引）、不可见索引和 ENUM 列、分区表、虚拟生成列表。Go 照 `BuildLogicalPlanForTest` 预处理并构建（SQL mode 有 / 无 `ONLY_FULL_GROUP_BY`，`tidb_remove_orderby_in_subquery` 开 / 关），记录整棵计划（算子、id、schema、每个算子的输出列名、表达式和类型、DataSource 的表、句柄列、聚簇主键列、两组访问路径及其完整索引列）、计划 id 和列 id 计数器、`optFlag`、权限检查项（含错误码）和 warning，或者错误。Rust 用同一份表的元数据建 InfoSchema，解析、预处理、构建后比较。
  - 结果：**1,472 个计划、174 个错误与 Go 相同**，0 处不同；96 条停在边界桩，按块钉在测试中（`EXPECTED_BLOCKED`）：D08.9 53（IN、`ANY_VALUE` 等函数族未移植）、D09.4 15、D09.3 6、D09.12 4（分区表）、D09.5 4、D13.3 4、D04.1 2（预处理器的缺表检查）、D09.1 2（虚拟生成列）、D09.14 2、D09.2 2、D09.6 2。
  - #489 移植表名上的索引 hint 之后：`selectbuild`、`findbest`、`optdriver` 三个领域中原来停在 `blocked-by:D13.3` 的 2 条（`SELECT a FROM t2 USE INDEX (ia)`，两种 SQL mode）现在与 Go 比较且相同，三个测试的 `EXPECTED_BLOCKED` 去掉 D13.3；`plancache` 领域的 D13.3 是注释形式的优化器 hint，不受影响。会话层测试 `index_hints_pick_access_paths_and_unknown_index_is_1176`（`crates/session/src/tests.rs`）在 sbtest1 上检查 `USE / FORCE / IGNORE INDEX` 选出的读取算子、返回的行，以及不存在的索引名给客户端的 1176 / 42000 / 完整消息。
  - 查询：sysbench 只读语句中不含聚合的那些，手工挑选的形状（通配符的各种写法和错误、字面量和表达式的列名、`/*! */` 注释、恒真 / 恒假条件、字符串条件、没有聚合的 HAVING、ORDER BY 辅助列 / 别名 / 位置 / 歧义、派生表的别名和重名列、`LIMIT 0` 和极大的 LIMIT、子查询块中 ORDER BY 的保留、各种边界），以及 1,500 条随机单表查询（可带派生表、别名、HAVING、ORDER BY 位置）。
  - tracker 中领域声明 D09.1、D09.4、D10.2、D09.0、D09.2 五个块；未覆盖的函数在 `selectbuild_exempt.go` 中写明理由（本部分的桩、D09.4 / D10.2 不在本部分的函数）。原来豁免的 `PlanBuilder.Build`、`detectSelectWindow`、`isForUpdateReadSelectLock`（预处理领域）和 `positionToScalarFunc`（改写领域）现在由本领域覆盖，豁免删除。
- **单元测试**（`logical_plan_builder.rs`）：`ByAddr` 按节点身份做键；`/*! */` 标记的去除；没有 SELECT 用到的 `appendDynamicVisitInfo`、`appendMultiColumns2VisitInfo`、`popVisitInfo`；句柄列栈。

## 7. 与 Go 的其他差异

1. **`TblInfo2UnionScan`。** Go 在 `buildDataSource` 末尾记录 `StmtCtx.TblInfo2UnionScan[tableInfo] = dirty`。走到这里时 `dirty` 一定为假（为真时已停在 UnionScan 的桩），读这张表的 UnionScan 执行器也没有移植，Rust 的语句上下文没有这个字段，不记录。
2. **`DataSource` 中与 hint、统计有关的字段**（`AstIndexHints`、`IndexHints`、`IndexMergeHints`、`PreferPartitions`、`SampleInfo`、`IS`）不在 Rust 的 DataSource 中：`AstIndexHints` 只有分区处理（D09.12，分区表停在桩）读；其余几个来自注释形式的 hint（D13.3），在 Go 中也是空的；`IS` 等读取它的阶段（D10.2）移植时再加。
3. **`setPreferredStoreType`**：没有表 hint 时，Go 的两次查找都找不到，什么也不做；Rust 照此写成空操作（L2）。
4. **向量索引**：`getPossibleAccessPaths` 对向量索引读 `tblInfo.TiFlashReplica.Available`，副本信息为空时 Go 会空指针退出；Rust 当作不可用（跳过）。向量索引要求表有 TiFlash 副本，正常情况下走不到。
5. **`buildProjectionFieldNameFromColumns`**：Go 判断的是一个刚声明的零值变量（`tblName.L == ""` 总为真），表名总取 `name.TblName`；照抄。

可能的上游问题：暂无需要登记的。

## 8. 第 2 部分（未做）

- 视图（`BuildDataSourceFromView`、`buildProjUponView`、`checkRecursiveView`）、虚拟生成列（列的生成表达式要从表定义解析）、UPDATE / DELETE 用的 `updatableTableListResolver`。
- §5 中其他块的桩随各自的块解除；`getPossibleAccessPaths` 中注释形式 hint 和 `HYPO_INDEX` 的分支随 hint 框架（D13.3）解除，`GetLatestIndexInfo` 和 index lookup 下推的检查随访问路径（VS1-28）解除。

## 本文所用 ID

- VS1-21：本任务，SELECT 构建
- VS1-24：优化器驱动与后优化，会建立规划上下文
- VS1-28：`FindBestTask` 与访问路径
- D02.2：parser 的 AST
- D04.1：权限管理
- D05.4：事务上下文与 InfoSchema
- D06.4：table 层（第 1 部分只标记分区表）
- D08.9：控制流、信息、杂项函数（含 IN、`ANY_VALUE`、用户变量）
- D09.0：计划构建器的公共部分（预处理、`Build` 的骨架）
- D09.1：SELECT 构建（本块）
- D09.2：表达式改写
- D09.3：join 构建
- D09.4：聚合、窗口构建
- D09.5：CTE 与集合运算
- D09.6：子查询与去关联
- D09.12：分区裁剪
- D09.13：优化器驱动、规则框架
- D09.14：逻辑算子
- D09.15：写路径的计划构建（INSERT / UPDATE / DELETE、CREATE VIEW）
- D09.16：其他语句的计划（`SELECT ... INTO` 等）
- D10.2：`FindBestTask` 与访问路径
- D10.11：内存表
- D13.1：计划缓存
- D13.3：hint
- N1：计划表示（arena 与 `PlanId`）
- M-FT：`FieldType` 等共享对象原地修改的映射规则
- L1 / L2：来源标注的等级
