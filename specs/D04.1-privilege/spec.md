# D04.1 权限检查：Go v8.5.8 SELECT 权限注册审计与 tidb-rs 系统性检查设计（spec）

状态：草案（仅设计规格，对应 `dulao5/tidb-rs#438`，待项目负责人与团队评审；先于 D04.1 完整移植）。

块与依赖模块：主归属 D04.1（权限检查与认证，`layers.toml` 中规划为 `tidb-privilege`，rank 8）；跨模块落地于 `tidb-planner`（rank 13，包含 D09.0 预处理器与计划构建骨架、D09.1 SELECT 构建、D09.2 表达式改写、D09.13 优化器驱动、D10.8 点查快速路径、D13.1 计划缓存；写路径涉及不移植的 D09.15）。

引用与实测口径：
- 源码基线：Go TiDB **v8.5.8**（`8b857efa2`），路径均相对于参照树根目录（不附带任何上游链接）。
- 实测环境：本机通过 `tidb-server --store=unistore`（服务端报告版本 `8.0.11-TiDB-v8.5.8`）对全部 SQL 矩阵进行了真实建表、建视图、建账号与执行复现。
- MySQL 8.0 对照说明：**本机未安装 `mysqld` / MySQL 8.0 服务端**，下文中所有 MySQL 8.0 行为对比均标明“**据 MySQL 8.0 官方文档与源码授权模型推断，未在本机实跑**”。

相关文档：[移植宪章](../../CONSTITUTION.md)（第一条 2 安全例外、第九条 决策分级）、[兼容性差异说明](../../docs/compatibility.md)、[偏差台账规格](../../docs/testing/rulings.md)、[D09.1 SELECT 构建 spec](../D09.1-select-builder/spec.md)、[D09.2 表达式改写 spec](../D09.2-rewriter/spec.md)、[D09.13 优化器驱动 spec](../D09.13-optimizer/spec.md)、[D13.1 Plan Cache 预处理 spec](../D13.1-plancache/spec.md)、[A3 Plan Cache 例外 spec](../exceptions/A3-plan-cache/spec.md)、[N0 crate 分层 spec](../exceptions/N0-crate-layering/spec.md)。

---

## 1. 审计结论总览

Go TiDB v8.5.8 在逻辑计划构建层**没有建立基于“表数据源访问”的统一 `SELECT` 权限登记边界**。`buildDataSource`（`pkg/planner/core/logical_plan_builder.go:5072`）解析并扫描表、视图、分区表、序列与内存表时，**完全不登记任何 `SelectPriv`**；全路径仅依赖表达式改写阶段的“列引用懒收集”和一个仅识别小写 `"count"` 的特例补丁。

这一结构性缺失在 Go v8.5.8 上实测确认了以下 9 类 `SELECT` 权限绕过与信息泄漏：

1. **无列引用的表扫描全部放行**：`SELECT 1 FROM test.t`、`SELECT 'x' FROM test.t LIMIT 1`、`SELECT 1 FROM test.t WHERE 1=0`、`SELECT 1 FROM test.t WHERE 1=1`，以及使用 `EXISTS` / `IN` / 标量子查询 / 派生表 / 无引用列 `JOIN` / `UNION` 的查询，对无任何 `t` 上权限的用户（包括不指定默认库 `db=None` 直连的 `u_none`、仅对 `t` 有 `INSERT/UPDATE` 的 `u_ins`、仅对另一张表 `t2` 有 `SELECT` 的 `u_other`）均直接执行并返回真实行数与行内容（如跨表乘积泄露表行数）。
   - **连带表现（`PREPARE / EXECUTE` 与计划缓存，非独立漏洞）**：在会话级、实例级及非预处理计划缓存下，凡在 `PREPARE` 时实际登记了 `VisitInfo` 的语句（如小写 `count(*)`、`SELECT a`、`SELECT b FROM t_pk WHERE a = ?`），在 `REVOKE` 后命中缓存时都会被 `checkPreparedPriv` 正确拦截（分别报 `1142`/`1143`，且 `@@last_plan_from_cache = 0`）。唯独上述无列引用语句（如 `SELECT 1 FROM test.t`、`SELECT COUNT(*) FROM test.t`）在 `PREPARE` 时登记到的 `VisitInfos` 为空，因此 `REVOKE` 后继续以 `@@last_plan_from_cache = 1` 放行——但这并非计划缓存本身的独立漏洞，关闭缓存或由从未获权的 `u_none` 直接 `PREPARE`/`EXECUTE` 同样放行，根因完全归属于本条（无列引用未登记 `VisitInfo`）。
2. **仅小写的 `count` 补丁全面失效**：`expressionRewriter.Leave`（`expression_rewriter.go:1517`）用大小写敏感的 `v.F == ast.AggFuncCount`（`"count"`）比对保留了原始词法大小写的 `AggregateFuncExpr.F`。结果 `SELECT count(*)` / `count(1)` 报 `1142`，而 `SELECT COUNT(*)`、`Select Count(*)`、`SELECT COUNT(1)` 以及其他常量聚合 `sum(1)` / `SUM(1)` / `max(1)` / `min(1)` / `avg(1)` / `group_concat('x')`、`HAVING COUNT(*) > 0` **全部绕过**。
3. **隐藏列 `_tidb_rowid` 完全无权限校验**：`buildDataSource` 为 `_tidb_rowid` 构造 `FieldName` 时遗漏了 `OrigTblName`（`logical_plan_builder.go:5367-5372`），`precollectColNames` 因 `len(colName.Table.L) == 0` 跳过登记。实测 `u_none(db=None)`、`u_ins`、`u_other`、`u_cola` 均可直接执行 `SELECT _tidb_rowid FROM test.t` 与 `SELECT 1 FROM test.t WHERE _tidb_rowid = 1` 并读出全部行 ID。
4. **CTE 内部常数与引用侧双重绕过**：`WITH c AS (SELECT 1 FROM test.t) SELECT * FROM c` 以及不引用 `c` 的 `WITH c AS (SELECT 1 FROM test.t) SELECT 1` 全数绕过；即便外层写小写 `SELECT count(*) FROM c`，由于 `c` 输出常量列的 `OrigTblName` 为空，`collectPrivsForCount` 同样跳过校验，导致连小写 `count(*)` 也可绕过 CTE 读取 `t`。
5. **视图权限模型双端漏防（`DEFINER` 与 `INVOKER`）**：`BuildDataSourceFromView`（`logical_plan_builder.go:5740-5750`）在用定义者校验并清空内部 `b.visitInfo` 后，**从不登记调用者对视图自身的 `SelectPriv`**。实测 `u_none(db=None)` 可对 `DEFINER` 视图执行 `SELECT 1 / COUNT(*) FROM test.v_definer`；仅有底层表 `t` 权限、对 `INVOKER` 视图毫无权限的 `u_tbl` 可执行 `SELECT 1 / COUNT(*) FROM test.v_invoker`。
6. **`EXPLAIN` 与 `EXPLAIN ANALYZE` 泄漏查询计划与实际行数**：`u_none(db=None)`、`u_ins`、`u_other` 均可对 `test.t` / `test.tp` 执行 `EXPLAIN SELECT 1 ...` 与 `EXPLAIN ANALYZE SELECT 1 ...`，实测 `EXPLAIN ANALYZE` 真实扫表并返回 `actual rows = 3`、`cop_task` 等运行时执行细节。
7. **DML 读侧无需 `SELECT` 权限即可读取他表**：仅有目标表写权限的用户可执行 `INSERT INTO test.t2 SELECT 1 FROM test.t`、`INSERT INTO test.t2 SELECT COUNT(*) FROM test.t`、`REPLACE INTO test.t2 SELECT 1 / COUNT(*) FROM test.t`（将受保护表行数物化写入 `t2`）以及 `UPDATE / DELETE ... WHERE EXISTS (SELECT 1 FROM test.t)`。
8. **表存在性与库权限错误分叉构成探测神谕（Oracle）**：在 `db=None` 环境下，对 `u_none` / `u_other` / `u_cola`，访问不存在的表 `test.nosuch` 时 `tableByName` 统一返回 `1142`（`for table 'nosuch'`），而访问真实存在的 `test.t` 时，`SELECT 1` / `SELECT COUNT(*)` 直接成功（`OK`）、`SELECT a`（或 `u_cola` 查 `b`）报 `1143`（`for column 'a' in table 't'`），仅凭返回码即可准确枚举表是否存在；此外对仅持 `INSERT ON test.*` 的 `u_db_ins`，`tableByName` 使用 `mysql.AllPrivMask` 判定通过并直接暴露 `1146 Table 'test.nosuch' doesn't exist`。
9. **内存表、fastplan 与 `CREATE VIEW` 边界漏洞**：
   - `buildMemTable`（`logical_plan_builder.go:5492`）未登记 `SelectPriv`，`performance_schema` 与 `metrics_schema` 同样存在无列引用绕过；
   - `PointGet IsTableDual` 绕过**仅在表具备二级唯一索引时触发**（如 `test.t_uk(id INT PRIMARY KEY, a INT, b INT, UNIQUE KEY uk_a(a))`，在仅含主键的 `test.t_pk` 上不复现，且无溢出的 `a = 1 AND a = 2` 仍正常报 `1143`）：当类型溢出（`ErrOverflow`）与 `LogicAnd` 组合触发 `isTableDual`（`pairs == nil`，如 `WHERE a = 999999999999999999999 AND a = 1` 或经主键 `WHERE id = 999999999999999999999 AND id = 1`）时：
     - **属于 `tidb-rs` 范围的 SELECT 路径**：仅持 `SELECT(b)` 的 `u_colb` 执行 `SELECT b FROM test.t_uk WHERE a = <overflow> AND a = 1` 返回 `OK(0)`，跳过了对 `WHERE` 列 `a` 的权限校验（零权限 `u_none` 仍因 `b` 报 `1143`）；
     - **不移植的 DML 写路径（`X`，仅作审计记录）**：`tryUpdatePointPlan` / `tryDeletePointPlan` 在验权前直接返回 `PhysicalTableDual`，零权限 `u_none` 执行 `DELETE` / `UPDATE ... WHERE a = <overflow> AND a = 1`（甚至 `UPDATE test.t_uk SET nosuch = 1 WHERE ...`）均返回 `OK`（0 行受影响，同时跳过权限检查与 `SET` 列存在性检查）；虽不读写行数据，但对比 `DELETE FROM test.nosuch WHERE ...` 报 `1142` 仍构成表存在性神谕；
   - `CreateViewStmt`（`planbuilder.go:5597`）复用 `b.Build`，持 `CREATE VIEW ON test.*` 而无 `t` 读权的 `u_cv` 可直接执行 `CREATE VIEW test.v_evil AS SELECT 1 FROM test.t`。

> 注：以上漏洞在默认以 `-u u_none test` 指定默认数据库连接的客户端下部分被入口 `COM_INIT_DB`（`1044 Access denied ... to database 'test'`）掩盖；但客户端只要**不带默认库连接（`db=None`）直接使用 `test.t` 限定名**，或用户对任意一张其它表拥有任意非 `SELECT` 权限（如 `u_ins`、`u_other`、`u_dml_ins`、`u_cola`），所有绕过全部直接暴露。

---

## 2. Go v8.5.8 权限注册与校验全链路代码审计

### 2.1 权限登记模型与核心判定函数

Go TiDB 将需要检验的权限需求收集到 `PlanBuilder.visitInfo`（`[]visitInfo`，定义于 `pkg/planner/core/planbuilder.go:226`），每条记录包含 `(privilege, db, table, column, err, alterWritable, dynamicPrivs, dynamicWithGrant)`。优化主驱动 `pkg/planner/optimize.go:538-543` 在构建完逻辑计划、尚未进入物理优化与执行前，且仅在 `pm := privilege.GetPrivilegeManager(sctx); pm != nil` 时执行一次全量校验：

1. `VisitInfo4PrivCheck`（`pkg/planner/core/optimizer.go:159`）：过滤掉本地临时表（`needCheckTmpTablePriv`：仅当表存在且 `TempTableType == TempTableLocal` 时返回 `false`；表不存在时返回 `true` 以便报 `err` 而不泄密）。
2. `CheckPrivilege`（`pkg/planner/core/optimizer.go:130-154`）：按 `vs` 切片**顺序遍历**，第一条 `pm.RequestVerification(...)` 返回 `false` 时立即返回该条 `v.err`（因此 `vs` 的顺序直接决定对外错误码与消息）。
3. `UserPrivileges.RequestVerification`（`pkg/privilege/privileges/privileges.go:160-204`）与 `MySQLPrivilege.RequestVerification`（`pkg/privilege/privileges/cache.go:1621-1673`）对 `column` 参数的语义存在关键区别：
   - **`column == ""`**（仅表级以上权限）：在 `cache.go:1653` 跳过将 `tableRecord.TablePriv` 提升至 `columnPriv`，仅依次检查全局 `user`、库级 `db`、表级 `tables_priv`；随后虽会调 `MatchColumns(..., "")`，但 `columnsPrivRecord.match`（`cache.go:1427-1434`）对空列名永不匹配。因此 **`column == ""` 会拒绝只有 `SELECT(a)` 列级权限的用户（`u_cola`）**。
   - **`column == "*"`**（表级或**任一列** `SELECT` 权限）：`tableRecord.TablePriv` 可通过，且 `columnsPrivRecord.match`（`cache.go:1433`）显式特判 `col == "*" && (record.ColumnPriv&mysql.SelectPriv > 0)`。配合 `MatchColumns`（`cache.go:1556-1567`）遍历该用户全部列记录、在 `match` 为真时返回的设计，**只要用户拥有全局、库级、表级 `SELECT`，或对该表任意至少一列拥有列级 `SELECT(col)`，检查即通过**。
   - **内存库特判**（`privileges.go:174-189`）：对 `IsMemDB(dbLowerName)`，`information_schema` 直接返回 `true`（除非开启 SEM 且命中 `sem.IsInvisibleTable`）；`metrics_schema` 中的指标表将 `SelectPriv` 扩为 `SelectPriv | ProcessPriv`；`performance_schema` 继续走标准 `MySQLPrivilege.RequestVerification`。

### 2.2 全路径注册点与漏网位置逐项审计

| # | Go 位置（文件与行号） | 机制与覆盖范围 | 缺失 / 缺陷原因 | 安全后果 |
|---|---|---|---|---|
| 1 | `logical_plan_builder.go:5072-5457` `buildDataSource` | 构建基表、分区表、序列、`LogicalUnionScan` 等数据源 | **全程未向 `b.visitInfo` 追加任何 `SelectPriv`**；遇到序列提前返回 `TableDual`（`:5222`）同样不登记 | 只要查询不在表达式阶段引用实列，整个数据源读取完全脱离权限校验 |
| 2 | `logical_plan_builder.go:5109-5114, 5492-5604` `buildMemTable` | 处理 `information_schema`、`performance_schema`、`metrics_schema` | 从 `buildDataSource` 提前分流返回且**未登记 `SelectPriv`** | `SELECT 1` / `SELECT COUNT(*)` 读 `performance_schema.*` 或 `metrics_schema.*` 绕过表级 `SELECT` / `PROCESS` 检验 |
| 3 | `expression_rewriter.go:2577, 2855-2876, 2817-2853, 303-307` `toColumn` / `precollectColNames` / `appendColNamesToVisitInfo` | 仅在表达式改写访问 `*ast.ColumnName` 时收集 `(DBName, OrigTblName, OrigColName)` | (a) 不引用列（常数投影、`EXISTS(SELECT 1 ...)`、`JOIN ... ON 1=1` 等）不触发 `toColumn`；(b) `precollectColNames` 对派生表因 `Schema.L == ""` 跳过、对 CTE 因 `b.nameMapCTE` 命中跳过（`:2869-2873`），假定内层自行校验，一旦内层也无列引用即全链失守 | 全类无列引用 `SELECT` / 子查询 / Join / 派生表 / `UNION` / DML 读侧均可绕过 |
| 4 | `logical_plan_builder.go:4163-4202, 1794-1801` `unfoldWildStar` / `buildProjection` | 将 `*` / `t.*` 展开为全部非隐藏列并置 `IsUnfoldFromWildCard = true`，`buildProjection` 据此将 `checkColPriv` 设为 `reportTableErrOption` | 对显式 `SELECT *` 或 `TABLE t`（parser 展开为 `*`）有效，但无 `*` 即退化为零校验 | 与 `SELECT 1` / `SELECT COUNT(*)` 行为剧烈割裂 |
| 5 | `logical_plan_builder.go:5367-5372` 非主键表 `_tidb_rowid` 列 | 为无整数/聚簇主键表构造隐藏行句柄列 | **`FieldName` 仅填 `TblName` 与 `ColName`/`OrigColName`，未填 `OrigTblName`**；`precollectColNames` 检查 `len(colName.Table.L) > 0` 失败直接忽略 | 任意账号可直接读取非主键表 `_tidb_rowid` 全表行号并按 `_tidb_rowid = 1` 过滤探测数据 |
| 6 | `expression_rewriter.go:1516-1519, 2510-2540` `Leave` + `collectPrivsForCount` | 遇到 `AggregateFuncExpr` 时若 `v.F == ast.AggFuncCount`（`"count"`），按 `er.names` 收集表级 `column = "*"` | (a) parser 保留 SQL 原始大小写，仅小写 `count` 命中，`COUNT(*)` / `Count(*)` / `COUNT(1)` 及 `SUM`/`MAX`/`MIN`/`AVG`/`GROUP_CONCAT` 全部失效；(b) CTE 仅输出常数（`WITH c AS (SELECT 1 FROM t)`) 时 `OrigTblName == ""`，`collectPrivsForCount` 连小写 `count(*)` 也跳过；(c) 视图需依赖 `infoschema.TableIsView` 转用 `TblName` | 大写/混合大小写 `COUNT` 及所有非 `count` 聚合、常数 CTE 内 `count(*)` 均绕过校验 |
| 7 | `logical_plan_builder.go:1281-1298` `coalesceCommonColumns` | `NATURAL JOIN` / `USING` 合并同名列时手动调用 `appendColNamesToVisitInfo` | 仅覆盖存在合并列的 `NATURAL` / `USING` join；`CROSS JOIN`、逗号连接、`LEFT/INNER JOIN ON 1=1` 无对应检查 | 无权限表参与 Join 可放大/过滤有权限表结果或暴露行数 |
| 8 | `logical_plan_builder.go:5629-5762` `BuildDataSourceFromView` | 递归构建视图定义 `SelectStmt`；若 `SecurityDefiner` 则用 `view.Definer` 校验内部 `b.visitInfo` 并清空 `b.visitInfo = b.visitInfo[:0]`（`:5740-5750`） | (a) **未向调用者附加视图自身的 `SelectPriv`**；(b) `EXPLAIN` 下仅在 `viewDepth > 0` 时校验嵌套视图 `SelectPriv`（`:5734-5739`），顶层视图仅追加 `ShowViewPriv`（`:5753-5755`）；(c) `INVOKER` 视图仅依赖外层列引用校验视图自身，不引用视图列时（`SELECT 1 / COUNT(*) FROM v_invoker`）仅校验底层表 | `u_none(db=None)` 可直接读 `SELECT 1 / COUNT(*) FROM v_definer`；有底层表权限却无视图权限的 `u_tbl` 可读 `SELECT 1 / COUNT(*) FROM v_invoker` |
| 9 | `point_get_plan.go:971-1056, 1439-1581, 1736-1800, 1872-1978, 2140-2170, 2320-2350` `TryFastPlan` / `getVisitInfo` / `checkFastPlanPrivilege` / `tryUpdatePointPlan` / `tryDeletePointPlan` | `tryWhereIn2BatchPointGet` 与 `tryPointGetPlan` 仅在输出为列/`*`/`tidb_row_checksum()` 时收集输出列及 `colsInWhereClause`（`SELECT 1` 被 `buildSchemaFromFields` 拒绝并回退常规路径） | (a) `checkFastPlanPrivilege` 失败只令 `TryFastPlan` 返回 `nil` 回退常规构建路径（遇到 `_tidb_rowid` 等常规路径漏洞仍会放行）；(b) **触发前提：表必须含二级唯一索引（如 `t_uk(id PK, a, b, UNIQUE KEY(a))`；在仅含主键的 `t_pk` 上不复现，且无溢出的 `a = 1 AND a = 2` 仍报 `1143`）**——当 `ErrOverflow` 在 `LogicAnd` 内触发 `isTableDual`（`pairs == nil`，如 `WHERE a = 999999999999999999999 AND a = 1` 或经主键 `id = <overflow> AND id = 1`）时，`getNameValuePairs:1882-1884` 返回 `pairs == nil, isTableDual == true`，`tryPointGetPlan:1505` 与 `checkTblIndexForPointPlan:1578` 均错传 `tblName.Schema.O` 且跳过条件列检查；(c) 属 `X` 写路径的 `tryUpdatePointPlan:2141` / `tryDeletePointPlan:2321` 在 `IsTableDual` 时于 `checkFastPlanPrivilege`、`UpdatePriv`/`DeletePriv` 及 `SET` 列检查之前直接返回 `PhysicalTableDual` | **属 `tidb-rs` 范围（SELECT）**：含二级唯一索引表上，`u_colb`（仅 `SELECT(b)`）查 `SELECT b ... WHERE a = <overflow> AND a = 1` 跳过 `a` 检查返回 `OK(0)`；**属 `X` 写路径**：`u_none` 执行 `DELETE` / `UPDATE ... SET nosuch = 1 WHERE ...` 均 `OK`（0 行），且对比 `nosuch` 报 `1142` 构成表存在性神谕 |
| 10 | `planbuilder.go:5859-5932, 5781` `buildExplain` / `buildExplainFor` / `buildTrace` / `buildSelectInto` & `parser.y:9478-9499` `TABLE` | `buildExplain`、`buildSelectInto`（再加 `FilePriv`）与 `executor/trace.go:115, 144, 191` 均调 `OptimizeAstNode`；`buildExplainFor` 对跨用户检查 `SuperPriv`；`TABLE t` 由 parser 构造 `SelectStmtKindTable` + `*` | `buildExplain` 与 `buildTrace` 不附加表级权限，全靠内部 `OptimizeAstNode`；`ANALYZE` 与非 `ANALYZE` 共用同一无表检漏洞 | 未授权用户可执行 `EXPLAIN` / `TRACE` 查看表扫描结构，甚至用 `EXPLAIN ANALYZE` 触发真实扫表读出 `actual rows` |
| 11 | `planbuilder.go:4227, 4718, 4898` `buildInsert` / `buildLoadData` / `buildImportInto`、`logical_plan_builder.go:6189, 6600-6665` `buildUpdate` / `buildDelete` | 写语句中的 `SELECT` 子查询 / `IMPORT INTO ... FROM SELECT` / `WHERE` / `SET` 均复用 `buildSelect` 与 `buildResultSetNode`（`buildLoadData` 无读侧） | (a) 无列引用的读侧（`INSERT/REPLACE/IMPORT ... SELECT 1`、`UPDATE/DELETE ... WHERE EXISTS(SELECT 1 ...)`）因 `buildDataSource` 不登记而漏检；(b) `buildDelete:6663` 在 `Where == nil && Order == nil` 时调 `b.popVisitInfo()`，而 `buildUpdate:6212` 同调 `buildResultSetNode -> buildDataSource` 却**无任何弹出逻辑**——后续修补 `buildDataSource` 时，必须同时保护单表无读列 `DELETE`-only 与 `UPDATE`-only 目标表不被误加 `SelectPriv` | 持单一写表权限即可窃取/统计其他任意无权限表的数据；若修补不当则会破坏合法 `DELETE`-only / `UPDATE`-only 写入 |
| 12 | `plan_cache_utils.go:213, 230`、`plan_cache.go:270, 339-348`、`pkg/executor/compiler.go:102, 133` | `PREPARE` 保存 `VisitInfos: destBuilder.GetVisitInfo()` 并验权；`EXECUTE` 命中缓存时调用 `adjustCachedPlan -> checkPreparedPriv` 重验 `stmt.VisitInfos` | **非独立漏洞，属第 1 项无列引用 `VisitInfos` 为空的连带表现**：只要 `VisitInfos` 非空（如小写 `count(*)`、`SELECT a`、`SELECT b FROM t_pk WHERE a = ?`），缓存命中时 `checkPreparedPriv` 均正确重验并拦截（`1142`/`1143`，`from_cache = 0`）；仅当 `PREPARE` 登记到空切片时重验空切片直接通过（不开缓存或 `u_none` 直接 `PREPARE` 亦同）；另 `DEFINER` 视图内层 `VisitInfo` 在准备后被清空，撤回定义者底层表权不会体现在 `stmt.VisitInfos` 中 | 修复第 1 项并确保 `PlanCacheStmt` 持久化 `visit_infos` 后，已缓存的 `SELECT 1` / `COUNT(*)` 在 `REVOKE` 后将同步被拦截（无需将计划缓存计为额外 Go 漏洞） |
| 13 | `preprocess.go:525-556` `tableByName` | 预处理器查表返回 `ErrTableNotExists` 时检查 `RequestVerification(..., sName.L, tn.Name.O, "", mysql.AllPrivMask)`，不通过则改报 `ErrTableaccessDenied (1142)` | (a) 使用 `AllPrivMask` 过宽（仅 `INSERT` 权限的 `u_db_ins` 在执行 `SELECT` 时也通过此检查，导致直出 `1146`）；(b) `column = ""` 导致只有 `SELECT(a)` 的 `u_cola` 访问不存在表报 `1142`；(c) 存在的表先过 `tableByName`、后在表达式阶段报 `1143` 或放行 | **稳定表存在性神谕**：`test.nosuch` 报 `1142`（`u_db_ins` 报 `1146`），而存在表 `test.t` 报 `1143` 或直接 `OK` |
| 14 | `planbuilder.go:3664-3768` `buildShow` & `executor/infoschema_reader.go:861, 945, 990, 1102, 1290` & `executor/show.go:607, 678, 718-750` | `SHOW TABLES`、`SHOW TABLE STATUS`、`I_S.TABLES`、`SHOW CREATE TABLE/SEQUENCE` 用 `AllPrivMask`（`column = ""`）；`I_S.COLUMNS` 遍历 `AllColumnPrivs`（仍传 `column = ""`）；`I_S.PARTITIONS`/`CHECK_CONSTRAINTS` 与 `SHOW CREATE VIEW`/`SHOW STATS_*` 用 `SelectPriv`（`column = ""`）；`SHOW COLUMNS` 则支持回退逐列校验 | `I_S.TABLES` 与 `I_S.COLUMNS` 仅认表级以上权限（`column = ""`），而 `SHOW COLUMNS` 对列逐一核对列级权限 | 同一个 `u_cola` 用户在 `I_S.TABLES` / `I_S.COLUMNS` 看不到 `test.t`（0 行），但 `SHOW COLUMNS FROM test.t` 能看到列 `a` |
| 15 | `planbuilder.go:5575-5623, 3345, 5123` `buildDDL` (`CreateViewStmt`) / `buildAnalyze` & `preprocess.go:990-995` | `CreateViewStmt` 调 `b.Build`（`:5597`）校验视图定义；`buildAnalyze` / `buildUnlockStats` 调 `requireInsertAndSelectPriv` 要求 `InsertPriv + SelectPriv`（`column = ""`）；`buildTableDual`（`logical_plan_builder.go:4716`）本身不读真实表 | `CreateViewStmt` 经 `b.Build` 同样受 `buildDataSource` 缺失影响；其余已核对路径（`CREATE TABLE ... AS SELECT` 在 `preprocess.go:990-995` 报未实现 `1105`、`HANDLER` 与裸 `CHECKSUM TABLE` 在 parser 报语法不支持 `1064`、`ADMIN CHECKSUM TABLE` 在 `planbuilder.go:1750` 要求 `SuperPriv`）均无 `SELECT` 泄漏 | 仅持 `CREATE VIEW ON test.*` 的 `u_cv` 可无读权创建 `CREATE VIEW test.v_evil AS SELECT 1 FROM test.t`（已实测 `OK`） |

---

## 3. Go TiDB v8.5.8 实测 SQL 矩阵与 MySQL 8.0 对照

### 3.1 实测环境与数据集

- **测试对象构造**：
  - 基础表：`test.t(a INT, b INT)` 写入 `(1,1),(2,2),(3,3)`；`test.t_pk(a INT PRIMARY KEY, b INT)`（无二级唯一索引）写入 `(1,10),(2,20),(3,30)`；`test.t_uk(id INT PRIMARY KEY, a INT, b INT, UNIQUE KEY uk_a(a))`（含二级唯一索引）写入 `(1,1,10),(2,2,20),(3,3,30)`；`test.t2(x INT)` 写入 `(10),(20)`；`test.t_nat(a INT, c INT)` 写入 `(1,100),(2,200)`。
  - 分区表：`test.tp(a INT, b INT) PARTITION BY RANGE (a) (p0 < 10, p1 MAXVALUE)` 写入 `(1,1),(20,2)`。
  - 视图：`test.v_definer`（`SQL SECURITY DEFINER`，定义 `SELECT a, b FROM test.t`）、`test.v_invoker`（`SQL SECURITY INVOKER`，定义 `SELECT a, b FROM test.t`）、`test.v_def_on_inv`（`DEFINER` 查 `v_invoker`）、`test.v_inv_on_def`（`INVOKER` 查 `v_definer`）。
- **测试账号（均为 `'...'@'%'`）**：
  - `u_none`：无任何授权。矩阵中区分 **`u_none(db=None)`**（连接不设默认库，直写 `test.t`）与 **`u_none(db=test)`**（客户端指定默认库 `test`，在 `COM_INIT_DB` 即报 `1044`）。
  - `u_ins`：`GRANT INSERT, UPDATE ON test.t`（有非读权限，无 `SELECT`）。
  - `u_other`：`GRANT SELECT ON test.t2`（有别表 `SELECT`，无 `t` 权限）。
  - `u_cola`：`GRANT SELECT(a) ON test.t`（仅有单列 `a` 的 `SELECT`）。
  - `u_tbl`：`GRANT SELECT ON test.t`（整表对照，注意其**无** `t2`、`tp`、`v_definer`、`v_invoker` 权限）。
  - 补充专用账号：`u_colb`（`SELECT(b) ON t_pk, t_uk`）、`u_pk_cola`（`SELECT(a) ON t_pk`）、`u_pk_tbl`（`SELECT ON t_pk`）、`u_tp_cola`（`SELECT(a) ON tp`）、`u_vdef_sel`（`SELECT ON v_definer`）、`u_dml_ins`（`INSERT, DELETE ON t2`）、`u_dml_upd`（`UPDATE(x), SELECT(x) ON t2`）、`u_dml_upd_no_sel`（`UPDATE(x) ON t2`）、`u_dml_del`（`DELETE ON t2`）、`u_db_ins`（`INSERT ON test.*`）、`u_rev1`/`u_rev2`/`u_rev_inst`/`u_rev_nonprep`（初始 `SELECT ON test.t, test.t_pk` 随后 `REVOKE`）。

### 3.2 全量 SQL 矩阵对照表

表中 `OK(n)` 表示执行成功并返回 `n` 行；`1142` 表示 `SELECT command denied ... for table '...'`；`1143` 表示 `SELECT command denied ... for column '...' in table '...'`；`1356` 表示 `View '...' references invalid...`；`1345` 表示 `EXPLAIN/SHOW can not be issued...`；`1146` 表示 `Table '...' doesn't exist`。**所有 Go 结果均为本地实测输出；所有 MySQL 8.0 列均为据官方文档与源码规则推断，未在本机实跑。**
**连接默认库图例说明**：若客户端连接时指定默认库 `test`（`u_none(db=test)`），在入口 `COM_INIT_DB` 阶段即因 `1044 Access denied ... to database 'test'` 被拒；该报错仅为连接层默认库准入，**切勿误判为权限修复**——只要不指定默认库连接（`db=None`）改写限定名 `test.t`，或账号在任意其他表/对象上持有任意权限（如 `u_ins`、`u_other`、`u_cola`），下表全部绕过均可稳定复现。

| 类别与 SQL 语句 | Go v8.5.8 实测结果（`u_none(db=None)` / `u_ins` / `u_other` / `u_cola` / `u_tbl`） | 泄漏内容 / 异常现象 | Go 根因代码位置 | MySQL 8.0（**文档/源码推断，未在本机实跑**） |
|---|---|---|---|---|
| **常量投影** | | | | |
| `SELECT 1 FROM test.t` | 全 5 账号均为 **`OK(3)`**（`u_none(db=test)` 报 `1044`） | 向零权限/他表用户泄露表精确行数及存在性 | `buildDataSource` 无登记；`toColumn` 未触发 | 无 `t` 上任何 `SELECT` 权限者（`u_none`/`u_ins`/`u_other`）报 **`1142`**；`u_cola` 放行与否见 §5.2 与 §6.2（采用 `column = "*"`） |
| `SELECT 'x' FROM test.t LIMIT 1` | 全 5 账号均为 **`OK(1)`** | 确认表非空及存在性 | 同上 | 同上（无权者 **`1142`**） |
| `SELECT 1 FROM test.t WHERE 1=0` | 全 5 账号均为 **`OK(0)`** | 确认表存在且恒假条件通过校验 | 同上 | 同上（无权者 **`1142`**） |
| `SELECT 1 FROM test.t WHERE 1=1` | 全 5 账号均为 **`OK(3)`** | 泄露表精确行数 | 同上 | 同上（无权者 **`1142`**） |
| **列引用与通配符** | | | | |
| `SELECT a FROM test.t` | `u_none`/`u_ins`/`u_other`: **`1143(a)`**；`u_cola`/`u_tbl`: `OK(3)` | 暴露表存在性（对比缺表报 `1142`） | `toColumn -> precollectColNames` | `u_none`/`u_ins`/`u_other` 报 **`1142`**（无表及任意列权限）；`u_cola`/`u_tbl` 放行 |
| `SELECT 1, a FROM test.t` | `u_none(db=None)`: **`1143(a)`** | 同上 | 同上 | 无权者报 `1142` |
| `SELECT b FROM test.t` | `u_none`..`u_cola`: **`1143(b)`**；`u_tbl`: `OK(3)` | 正常拦列；零权账号仍暴露表存在 | 同上 | `u_cola` 报 **`1143(b)`**；无权限者报 `1142` |
| `SELECT 1 FROM test.t WHERE b = 1` | `u_none`..`u_cola`: **`1143(b)`**；`u_tbl`: `OK(1)` | 正常拦列 | `buildSelection` 触发 `toColumn` | `u_cola` 报 `1143(b)`；无权限者报 `1142` |
| `SELECT * FROM test.t` / `SELECT *, 1 FROM test.t` | `u_none`..`u_cola`: **`1142(t)`**；`u_tbl`: `OK(3)` | 靠 `unfoldWildStar` 设 `reportTableErrOption` 拦截 | `logical_plan_builder.go:1795` | 零权者 `1142`；部分列权限 `u_cola` 报 `1142`/`1143` 拒访问 |
| **隐藏列 `_tidb_rowid`** | | | | |
| `SELECT _tidb_rowid FROM test.t` | 全 5 账号均为 **`OK(3)`**（输出 `1,2,3`） | **直接读取受保护表物理行 ID 序列** | `buildDataSource:5367` 未设 `OrigTblName` | MySQL 无 `_tidb_rowid` 隐藏列 |
| `SELECT 1 FROM test.t WHERE _tidb_rowid = 1` | 全 5 账号均为 **`OK(1)`** | 无权按行号过滤存在性 | 同上（FastPlan 未走或回退后被 `precollectColNames` 丢弃） | MySQL 无 `_tidb_rowid` 隐藏列 |
| **点查快速路径** | | | | |
| `test.t_pk`（仅主键、**无二级唯一索引**）：`SELECT 1/b ... WHERE a = 1` / `a = 1 AND a = 2` / `a = 999999999999999999999 AND a = 1`；`DELETE`/`UPDATE test.t_pk SET nosuch = 1 WHERE a = <overflow> AND a = 1` | `SELECT 1/b` 与 `DELETE`：`u_none` 与 `u_colb` 均报 **`1143(a)`**；`UPDATE SET nosuch = 1`：报 **`1054 Unknown column 'nosuch'`**（**`t_pk` 上无绕过**） | `SELECT 1` 被 `buildSchemaFromFields` 拒绝并走常规路径；无二级唯一索引的 `t_pk` 即使带溢出也不触发绕过 | `point_get_plan.go:1565, 1774`（常规路径或正常点查拦住） | 全部正常校验列与写权限（无权者 `1142` / `1143`） |
| `test.t_uk`（**触发前提：含二级唯一索引 `UNIQUE KEY uk_a(a)`**）：<br>• **属 `tidb-rs` 范围（SELECT）**：`SELECT b FROM test.t_uk WHERE a = 999999999999999999999 AND a = 1`<br>• **属 `X` 写路径（不移植）**：`DELETE FROM test.t_uk WHERE a = <overflow> AND a = 1`（及经 PK `id = <overflow> AND id = 1`）、`UPDATE test.t_uk SET nosuch = 1 WHERE ...` | • SELECT 路径：`u_none` 报 `1143(b)`，**`u_colb`（仅 `SELECT(b)`）得 `OK(0)`**；无溢出 `a = 1 AND a = 2` 仍报 **`1143`**<br>• DML 写路径（`X`）：`u_none` 与 `u_colb` 均得 **`OK`（0 行受影响）**，连 `SET nosuch = 1` 亦得 **`OK`**（对比 `DELETE FROM test.nosuch WHERE ...` 报 `1142`） | `ErrOverflow` + `LogicAnd` 触发 `IsTableDual (pairs == nil)`：SELECT 路径跳过 `WHERE` 列 `a` 校验；`X` 写路径在 `checkFastPlanPrivilege` 及 `SET` 列存在性检查之前直接返回 `TableDual`（虽未读写行数据，但构成表存在性神谕） | `point_get_plan.go:1505, 1578, 1882, 1962, 2141, 2321` | `a` 受检拒止（无权者 `1142`，仅持 `SELECT(b)` 者不得绕过 `WHERE a` 检验）；`DELETE`/`UPDATE` 须检验写权、条件列权与 `SET` 列存在性 |
| **聚合与大小写分裂** | | | | |
| `SELECT count(*) FROM test.t` / `count(1)` / `HAVING count(*) > 0` | `u_none(db=None)`/`u_ins`/`u_other`: **`1142(t)`**；**`u_cola`/`u_tbl`: `OK`** | 仅小写 `count` 生效；`u_cola` 通过 `"*"` 放行 | `expression_rewriter.go:1517` (`v.F == "count"`) + `cache.go:1433` | 零权者 **`1142`**；`u_cola` 放行或拒止取决于是否认列权限可计数 |
| `SELECT COUNT(*) FROM test.t` / `Count(*)` / `COUNT(1)` / `HAVING COUNT(*) > 0` | 全 5 账号均为 **`OK`**（返回行数 `3` / `1`） | **大写/首字母大写直接绕过，精准读出表行数** | `expression_rewriter.go:1517` 区分大小写 | 与小写 `count(*)` **严格等价**，绝不因大小写不同出现权限分叉（`1142`） |
| `SELECT sum(1)` / `SUM(1)` / `max(1)` / `MAX(1)` / `min(1)` / `avg(1)` / `group_concat('x') FROM test.t` | 全 5 账号均为 **`OK`**（输出 `3` / `1` / `1.0000` / `'x,x,x'`） | 除小写 `count` 外**一切无列引用聚合全部绕过**，直接泄漏行数或串拼接行数 | `Leave` 仅特判 `ast.AggFuncCount` | 与 `COUNT(*)` 同等检查表读取 `SELECT` 权限（无权者 **`1142`**） |
| `SELECT count(a)` / `COUNT(a)` / `count(b)` / `COUNT(b)` / `COUNT(*) WHERE b=1` | 缺列权者报 **`1143`**；有权者 `OK` | 依赖实参或 `WHERE` 中的列引触发 `toColumn` | `precollectColNames` | 需具备对应列（或整表）`SELECT` |
| **子查询与派生表** | | | | |
| `SELECT EXISTS(SELECT 1 FROM test.t)` | 全 5 账号均为 **`OK(1)`** | 零权限探测任意表是否非空 | 内层子查询 `buildDataSource` 无登记 | 无权者 **`1142`** |
| `SELECT 1 IN (SELECT 1 FROM test.t)` | 全 5 账号均为 **`OK(1)`** | 同上 | 同上 | 无权者 **`1142`** |
| `SELECT (SELECT 1 FROM test.t LIMIT 1)` | 全 5 账号均为 **`OK(1)`** | 探测表非空 | 同上 | 无权者 **`1142`** |
| `SELECT 1 FROM (SELECT 1 FROM test.t) sub` | 全 5 账号均为 **`OK(3)`** | 派生表未泄 `Schema.L`，内层无校验 | `expression_rewriter.go:2867` + `buildDataSource` | 无权者 **`1142`** |
| `SELECT x FROM test.t2 WHERE EXISTS(SELECT 1 FROM test.t)` / `WHERE 1 IN (...)` | `u_other`: **`OK(2)`**；其余 4 人: `1143(x in t2)` | 持他表权限 `u_other` 可结合子查询探测 `test.t` 空/非空 | 外层仅查 `t2.x`，内层子查询零校验 | `u_other` 报 **`1142(t)`** |
| **Join 与集合运算** | | | | |
| `SELECT t2.x FROM test.t2, test.t` / `LEFT JOIN test.t ON 1=1` / `JOIN test.t ON 1=1` | `u_other`: **`OK(6)`**；其余 4 人: `1143(x in t2)` | `u_other` 借笛卡尔积将 `t2` 翻倍为 6 行，**直接窃取 `t` 的精确行数** | `buildJoin` 与 `buildDataSource` 对未引列的 `t` 零登记 | `u_other` 报 **`1142(t)`** |
| `SELECT COUNT(*) FROM test.t2 JOIN test.t ON 1=1` | 全 5 账号均为 **`OK(1)`**（返回 `6`） | 无任何两表权限仍可得乘积行数 `6` | `COUNT` 大写跳过 | 无权者 **`1142`** |
| `SELECT count(*) FROM test.t2 JOIN test.t ON 1=1` | `u_none(db=None)`: `1142(t2)`；`u_other`: **`1142(t)`** | 小写 `count` 通过 `er.names` 对两表均生成 `"*"` | `collectPrivsForCount` | 拒止 `1142` |
| `SELECT t.a FROM test.t NATURAL JOIN test.t_nat` | `u_none`..`u_ins`/`u_other`: `1143(a in t)`；`u_cola`/`u_tbl`: `1143(a in t_nat)` | `coalesceCommonColumns` 补检了自然连接公共列 | `logical_plan_builder.go:1281` | 拒止无权表（`1142`） |
| `SELECT 1 FROM test.t UNION SELECT 1 FROM test.t2`（及顺序互换） | `u_ins`/`u_other`/`u_cola`/`u_tbl`/`u_pk_*`: **`OK(1)`** | 集合运算任意分支无列引用即绕过 | 各分支均无 `SelectPriv` | 任一分支无权即报 **`1142`** |
| **`TABLE` 语句与分区表** | | | | |
| `TABLE test.t` | `u_none(db=None)`..`u_cola`: **`1142(t)`**；`u_tbl`: `OK(3)` | 未绕过，但仅因其语法树按 `*` 展开 | `parser.y:9478` 构造 `WildCard` | `u_none`..`u_cola` 报 **`1142`**；`u_tbl` `OK` |
| `SELECT 1 FROM test.tp` / `PARTITION (p0)` / `SELECT COUNT(*) FROM test.tp` | `u_none(db=None)`/`u_ins`/`u_other`/`u_cola`/`u_tbl`: **`OK(2/1)`** | 分区表及其单分区全表扫描绕过 | `buildDataSource` 分区路径无登记 | 无 `tp` 权限者 **`1142`** |
| `SELECT count(*) FROM test.tp` / `PARTITION (p0)` | `u_none`..`u_tbl`: **`1142(tp)`**；`u_tp_cola`: `OK` | 仅小写 `count(*)` 被拦 | `collectPrivsForCount` | 无权者 **`1142`** |
| **视图（DEFINER / INVOKER / 嵌套）** | | | | |
| `SELECT 1 FROM test.v_definer` | 全 5 账号 + `u_vdef_sel`: **`OK(3)`** | **`u_none` 无需视图权限即读取 `DEFINER` 视图行数** | `BuildDataSourceFromView:5749` 清空内部 `visitInfo` 且不加视图自身权限 | 未授权用户报 **`1142(v_definer)`**；`u_vdef_sel` `OK` |
| `SELECT COUNT(*) FROM test.v_definer` | 全 5 账号 + `u_vdef_sel`: **`OK(1)`（`3`）** | 同上 + 大写 `COUNT` 双重逃逸 | 同上 | 同上（无权者 **`1142`**） |
| `SELECT count(*) FROM test.v_definer` / `SELECT a FROM test.v_definer` | `u_none`..`u_tbl`: `1142` / `1143(a in v_definer)`；`u_vdef_sel`: `OK` | 仅外层表达式恰好触发检查 | `TableIsView` 改写表名 | 无权者 `1142`；`u_vdef_sel` `OK` |
| `SELECT 1 / COUNT(*) FROM test.v_invoker` | `u_none`..`u_cola`: `1356`；**`u_tbl`: `OK(3)`** | **持底层 `t` 权限但毫无 `v_invoker` 权限的 `u_tbl` 直接绕过视图校验** | `BuildDataSourceFromView` 合并底层 `t` 权限，不加 `v_invoker` 自身权限 | `u_tbl` 因无视图自身权报 **`1142(v_invoker)`** |
| `SELECT count(*) / a FROM test.v_invoker` | `u_none`..`u_cola`: `1356`；`u_tbl`: `1142(v_invoker)` / `1143(a)` | 外层表达式拦住了 `u_tbl` | 外层表达式检 `v_invoker` | 无视图权者先报 **`1142(v_invoker)`**；有权缺底层表权报 **`1356`** |
| `SELECT 1 FROM test.v_def_on_inv` / `SELECT 1 FROM test.v_inv_on_def` / `count(*) FROM v_def_on_inv` | `v_def_on_inv`: **`u_none(db=None)` `OK(3)`**；`v_inv_on_def`: `u_none`..`u_tbl` `1356`，**`u_vdef_sel`（无 `v_inv_on_def` 权）`OK(3)`**；`count(*) FROM v_def_on_inv`: 连 `u_vdef_sel` 均 `1142` | 嵌套 `DEFINER` 外壳直接清空内部校验并放行 `u_none`；嵌套 `INVOKER` 外壳仅因未加自身权限被拥有内层 `v_definer` 权的 `u_vdef_sel` 绕过 | 同上 | 调用者必须同时持有目标视图的 `SELECT` 权限（`1142`）及（若 `INVOKER`）内层权限（`1356`） |
| `SELECT a / b / count(*) FROM test.v_definer`（`u_cola_vdef` 持 `SELECT(a) ON v_definer`） | `a`: `OK(3)`、`b`: `1143(b)`、`count(*)`: `OK`（`SELECT 1` 亦 `OK`） | 列级 `SELECT(a) ON v_definer` 在视图上可通过 `"*"` 与具体列匹配 | `cache.go:1433` | 查 `a`/`count(*)` 按 `SELECT(a)` 规则放行或计数，查 `b` 报 `1143` |
| **CTE（通用表表达式）** | | | | |
| `WITH c AS (SELECT 1 FROM test.t) SELECT * FROM c` | 全 5 账号均为 **`OK(3)`** | 零权限读全表行数 | `buildWith` 内部无列引用，外部 `*` 对 `c`（`Schema=""`）不触发库表检 | 无 `t` 权限者 **`1142(t)`** |
| `WITH c AS (SELECT a FROM test.t) SELECT 1 FROM c` | `u_none(db=None)`/`u_ins`/`u_other`: **`1143(a)`**；`u_cola`/`u_tbl`: `OK(3)` | CTE 内部引用实列 `a` 时拦住；但零权者报 `1143` 仍暴露表存在 | `precollectColNames` 在 CTE 定义内部触发 | 无 `t` 权限者报 **`1142(t)`**；`u_cola`/`u_tbl` `OK` |
| `WITH c AS (SELECT 1 FROM test.t) SELECT 1`（未使用 CTE） | 全 5 账号均为 **`OK(1)`** | 即使未消费 CTE，定义本身仍构建但未验权 | 同上 | 定义引用的表若未访问/构建是否检验视优化阶段定，原则上 CTE 引表须具 `SELECT`（`1142`） |
| **`WITH c AS (SELECT 1 FROM test.t) SELECT count(*) FROM c`** | 全 5 账号均为 **`OK(1)`（`3`）** | **连小写 `count(*)` 也被绕过** | `c` 输出常量列的 `OrigTblName == ""`，`collectPrivsForCount:2525` 直接跳过 | 无 `t` 权限者 **`1142(t)`** |
| `WITH RECURSIVE c AS (SELECT 1 AS n FROM test.t UNION ALL ...) SELECT * FROM c` | `u_ins`..`u_tbl`: **`OK(6)`** | 递归 CTE 锚点读 `t` 绕过 | 同上 | 无 `t` 权限者 **`1142(t)`** |
| **`EXPLAIN` 与 `EXPLAIN ANALYZE`** | | | | |
| `EXPLAIN SELECT 1 FROM test.t` / `tp` | `u_none(db=None)`/`u_ins`/`u_other`/`u_cola`/`u_tbl`: **`OK`** | 泄露表存储拓扑、估计行数、分区与访问路径 | `buildExplain -> OptimizeAstNode` 无登记 | 需拥有被解释查询全部表的 `SELECT` 权限，否则 **`1142`** |
| `EXPLAIN SELECT a FROM test.t` | `u_none(db=None)`/`u_ins`/`u_other`: **`1143(a)`**；`u_cola`/`u_tbl`: `OK` | 内部优化触发 `toColumn`；零权者仍暴露表存在性 | 同上 | 无 `t` 权限者报 **`1142(t)`**；`u_cola`/`u_tbl` `OK` |
| `EXPLAIN ANALYZE SELECT 1 FROM test.t` | `u_none(db=None)`/`u_ins`/`u_other`/`u_cola`/`u_tbl`: **`OK`**（输出 `actual rows = 3`） | **真实触发 Coprocessor 扫描并回传实测行数与统计详情** | 同上 | MySQL `EXPLAIN ANALYZE` 同样要求完整权限，否则 **`1142`** |
| `EXPLAIN SELECT COUNT(*) FROM test.t` | 全 5 账号均为 **`OK`**（而小写 `count(*)` 报 `1142`） | 仅小写被拦 | `collectPrivsForCount` 大小写分裂 | 均须 **`1142`** |
| `EXPLAIN SELECT 1 FROM test.v_definer` | `u_ins`..`u_tbl` 及 `u_vdef_sel`: **`1345`** | `BuildDataSourceFromView` 追加了 `ShowViewPriv` | `logical_plan_builder.go:5754` | 无 `SHOW VIEW` 与底层权报 **`1345`** |
| **DML 读侧（写账号执行）** | | | | |
| `INSERT / REPLACE INTO test.t2 SELECT 1 FROM test.t` & `SELECT COUNT(*) FROM test.t` | `u_dml_ins`: **`OK`**（真实将 `3` 行或数值 `3` 插入 `t2`；`count(*)` 报 `1142`） | **利用无权限表内容或行数写入自己有写权限的表实现持久外泄** | `buildInsert` 的 `SELECT` 分支无表读登记 | 对 `t` 无 `SELECT` 报 **`1142(t)`** |
| `UPDATE test.t2 SET x = ... WHERE EXISTS (SELECT 1 FROM test.t)` | `u_dml_upd` / `u_dml_upd_no_sel`: **`OK`** | 利用 `UPDATE` 条件过滤探测 `test.t` 是否为空 | 子查询 `buildDataSource` 无登记 | 对 `t` 无 `SELECT` 报 **`1142(t)`** |
| `DELETE FROM test.t2 WHERE EXISTS (SELECT 1 FROM test.t)` | `u_dml_del`: **`OK`** | 利用 `DELETE` 影响行数探测 `test.t` 是否为空 | 同上 | 对 `t` 无 `SELECT` 报 **`1142(t)`** |
| `DELETE FROM test.t` / `UPDATE test.t SET a = 1`（目标表纯写基准，无读列） | `u_del`: `DELETE FROM test.t` `OK`、`WHERE a=1` `1143`；`u_upd`: `UPDATE test.t SET a=1` `OK`、`SET a=a+1` `1143` | 正常合法基线：纯目标表写不得因补加表读 `SelectPriv` 而回退失败 | `logical_plan_builder.go:6212, 6663` | 仅需目标表 `DELETE` / `UPDATE(a)` 权限（`OK`）；读列则须对应列 `SELECT`（`1143`） |
| `CREATE VIEW test.v_evil AS SELECT 1 FROM test.t`（`u_cv` 仅持 `CREATE VIEW ON test.*`） | `SELECT 1 FROM t`: **`OK`**；`SELECT a FROM t`: `1143(a)` | 持 `CREATE VIEW` 但无 `t` 读权即可无阻建视图 | `planbuilder.go:5597` + `buildDataSource` | 对源表 `t` 无 `SELECT` 权限须拒止建视图（`1142`） |
| **`PREPARE / EXECUTE` 与 Plan Cache（属第 1 项连带表现，非独立漏洞）** | | | | |
| `u_none(db=None)` 执行 `PREPARE s FROM 'SELECT 1 FROM test.t'; EXECUTE s;` | `PREPARE` `OK`，`EXECUTE` **`OK(3)`**（关闭缓存亦同样 `OK(3)`） | 从未获权的用户即可准备并执行，纯因无列引用 `VisitInfos` 为空 | `buildDataSource` 未登记（`VisitInfos == []`） | 需具备对应权限，否则准备或执行拒绝（`1142`） |
| `PREPARE -> REVOKE -> EXECUTE`（分别在会话 cache ON、cache OFF、**实例 cache `SET GLOBAL = ON`** 下实测） | 撤权后凡登记了 `VisitInfo` 的语句均被正确拦截且 `from_cache=0`：小写 `count(*)` 报 **`1142`**、`SELECT a` 报 **`1143`**、`SELECT b FROM t_pk WHERE a = ?` 报 **`1143`**；仅空 `VisitInfos` 的 `SELECT 1` / `SELECT COUNT(*)` 仍 `OK(3/1)`（开缓存 `from_cache=1`，**关缓存同样 `OK`**） | **非计划缓存独立漏洞**：Go 缓存命中重验本身工作正常；上述放行只是第 1 项无列引用未登记 `VisitInfos` 的连带后果 | `plan_cache.go:270, 339-348` 重验空切片 | MySQL 权限随执行/重准备生效；撤权后不能继续越权读取（注：具体 `PREPARE` 后撤权语义随 MySQL 版本重准备策略而异） |
| 非预处理 Plan Cache（`tidb_enable_non_prepared_plan_cache = ON`）`REVOKE` | 带 `WHERE a = 1` 的查询撤权后被拦（`1143`，`from_cache=0`）；`SELECT 1 FROM test.t` 无 `VisitInfos` 照常放行（关缓存亦同） | 同上（第 1 项连带表现） | 同上 | 同上 |
| **缺表探测神谕与元数据** | | | | |
| `SELECT 1 / a / count(*) / COUNT(*) / * FROM test.nosuch` | `u_none(db=None)`/`u_ins`/`u_other`/`u_cola`/`u_tbl`: 全部 **`1142(nosuch)`** | 与真实表 `test.t` 的 `OK` / `1143` 形成明确区分，可逐表爆破库表名 | `preprocess.go:548` 用 `AllPrivMask` 拦住 | MySQL 8.0 在 Stage 2 检查表权限：无 `SELECT` 权限者对存在与不存在表**均先返回 `1142`**，避免泄露存在性 |
| `SELECT 1 FROM test.nosuch`（`u_db_ins` 持 `INSERT ON test.*`） | `u_db_ins`: **`1146 Table 'test.nosuch' doesn't exist`**（而 `test.t` 返回 `OK(3)`） | `INSERT ON test.*` 允许通过 `AllPrivMask` 校验直接获知任意表不存在 | `preprocess.go:548` `AllPrivMask` 过宽 | 同上：对读语句无 `SELECT` 时不应泄露 `1146` |
| `INFORMATION_SCHEMA.TABLES`（`ANALYZE TABLE test.t` 后查 `TABLE_NAME, TABLE_ROWS`）& `I_S.COLUMNS` vs `SHOW COLUMNS FROM test.t` | `u_none(db=None)` / `u_other` / `u_cola`：`I_S.TABLES` 返回 **`0` 行**（不可见）；**`u_ins`（持 `INSERT, UPDATE ON test.t`）：`I_S.TABLES` 返回 `t / TABLE_ROWS = 3`**；`u_cola`：`I_S.COLUMNS` 为 `0` 行，但 `SHOW COLUMNS` 返回列 `a` | (1) `u_ins` 经 `I_S.TABLES` 本就可见表存在性与近似行数（见 §6.1 D1 分层评估）；(2) `u_none`/`u_other` 对 `t` 完全不可见；(3) `u_cola` 在 `I_S` 表级过滤（`column=""`）与 `SHOW COLUMNS` 逐列过滤口径不一致 | `infoschema_reader.go:861, 1102` vs `show.go:718-750` | MySQL 中持有任一列权限用户在 `I_S.TABLES` / `I_S.COLUMNS` 可见对应授权表/列（文档推断） |

---

## 4. `tidb-rs` 现状与向量基线评估

截至当前分支（`nora/issue-438`，Rust 代码未改动），`tidb-rs` 已移植的部分完全镜像了 Go v8.5.8 的架构缺口，且部分边界尚以桩存在：

1. **权限常量（`crates/base/src/mysql/privs_gen.rs`）**：已生成 `SelectPriv = PrivilegeType(4)`（第 22 行）和 `AllPrivMask = PrivilegeType(8589934591)`（第 90 行）。
2. **语法层（`crates/parser/grammar/tidb_typed.y`）**：`AggregateFuncExpr.f` 保留词法原始大小写；`TABLE tbl` 已展开为 `SelectStmtKind::Table` 并附带 `WildCardField`（第 3198 行），与 Go `parser.y:9478` 同构。
3. **计划构建器（`crates/planner/src/core/planbuilder.rs`, `logical_plan_builder.rs`）**：
   - `VisitInfo`（`planbuilder.rs:47-65`）与 `PlanBuilder.visit_info`（`planbuilder.rs:192`）及 `append_visit_info` / `pop_visit_info`（`logical_plan_builder.rs:181-197, 1943`）已存在；
   - `build_data_source`（`logical_plan_builder.rs:2381-2665`）**同样未登记 `SelectPriv`**，其中 `_tidb_rowid`（第 2556–2562 行）也遗漏 `orig_tbl_name`；目前内存表（`blocked-by:D10.11`）、分区表（`blocked-by:D09.12`）、视图（`blocked-by:D09.1` 第 2 部分）、`CTE`/`UNION`（`blocked-by:D09.5`）、`Join`（`blocked-by:D09.3`）、聚合/窗口（`blocked-by:D09.4`）均处于边界桩，序列在第 2458 行提前返回 `TableDual`。
4. **表达式改写器（`crates/planner/src/core/expression_rewriter.rs`）**：
   - 在本分支基线快照（`2440a7e`，合入 `#431` 之前），第 1085 行 `leave` 对聚合 `count` 曾使用大小写无关匹配 `v.f.eq_ignore_ascii_case(fnames::AggFuncCount)`，调用 `collect_privs_for_count`（第 2044–2083 行，使用 `BTreeMap` 按有序生成 `"*"` 项）；
   - **重要补充（`origin/main` `#431` 状态）**：`tidb-rs#431`（合入于 `63adf55`）按项目负责人在 `#431` 上“暂保留 Go v8.5.8 行为、待 `#438` 落地再统一设计”的决定，已将该处显式改回**大小写敏感**的 `v.f == fnames::AggFuncCount`（位于 `#431` 的第 1137 行，`collect_privs_for_count` 位于第 2096–2135 行）；`append_col_names_to_visit_info`（第 368–411 行）目前对 `user/host` 留空串，视图分支尚未接入。
   - **无论是 `2440a7e` 还是 `#431` 均不足以解决问题**：所有无 `COUNT` 的常量投影、`SUM(1)`/`MAX(1)` 等其余聚合、子查询、CTE、Join、`_tidb_rowid`、视图自身权限与 `EXPLAIN` 依然全部漏检。
5. **点查、优化器驱动、预处理器与计划缓存**：
   - `crates/planner/src/core/point_get_plan.rs`：`get_visit_info`（第 698–755 行）仅收集 `output_names` 与 `cols_in_where_clause`；`check_fast_plan_privilege`（第 1242–1255 行）当前只检 `check_table_lock`，不检 `SelectPriv`。
   - `crates/planner/src/core/optimizer.rs` / `crates/planner/src/optimize.rs`：`check_privilege`（`core/optimizer.rs:312-315`）为 `blocked-by:D04.1` 桩；`visit_info4_priv_check`（`core/optimizer.rs:324-339`）已移植（跳过 `TempTableLocal`）；`build_and_optimize_logical_plan_round`（`optimize.rs:487-498`）因暂无 D04.1 权限管理器而未调用 `visit_info4_priv_check` 与 `check_privilege`；`optimize_exec_stmt` 中的 `prep_stmt_visit_infos`（`optimize.rs:602`）固定返回空 `Vec`。
   - `crates/planner/src/core/plan_cache_utils.rs` / `plan_cache.rs`：`PlanCacheStmt` 尚未保存 `visit_infos`；`check_prepared_priv`（`plan_cache.rs:418-421`）目前直接返回 `Ok(())`；`new_plan_cache_key`（`plan_cache_utils.rs:1015`）暂对 `user_name` / `host_name` 传空串。
   - `crates/planner/src/core/preprocess.rs`：`table_by_name`（第 736–762 行）在表不存在时于第 756 行返回 `blocked-by:D04.1`。
6. **`govec` 向量基线盘点（本地 `arm64` 向量文件实查）**：
   - 本分支基线（`2440a7e`）`crates/planner/tests/vectors/v8.5.8/arm64/selectbuild.jsonl.gz` 共 **1,743** 条记录（含 1 条 `tables` 与 **1,742** 条 `build`），其中含 `"vi"` 字段 **1,539** 条：`1142`-only **143** 条、`1143`-only **1,072** 条、同时含 `1142` 与 `1143` **284** 条、**`vi` 为空 `[]` 共 40 条**（含 `SELECT 1`、`SELECT 1 FROM dual`、常量 derived `(SELECT 1 AS a, 2 AS b)`、常量 CTE `WITH c AS (SELECT 1)` 等 **12 条非基表查询**——经逐条核对无任何读取基表的 derived table——以及 **28 条读取真实基表 `t2`/`sbtest1`/`t3`/`tc`/`ti` 却零 `VisitInfo` 的查询**，例如 `SELECT 1 FROM sbtest1`、`SELECT @x := 1 FROM t2`、`SELECT @@sql_mode FROM t2`、`SELECT 's1' FROM tc LIMIT 8 OFFSET 0`）。`star_vi = 0`（2 条大写 `SELECT COUNT(*) FROM t2` 因聚合停在 `unsupported`）。
   - 合入 `#431` 后的 `origin/main`（`63adf55`）已将 `selectbuild.jsonl.gz` 扩充至 **2,449** 条记录（1 条 `tables` + **2,448** 条 `build`），其中含 `"vi"` 字段 **2,139** 条：`1142`-only **145** 条、`1143`-only **1,658** 条、同时含 `1142` 与 `1143` **284** 条、`star_vi = 2`（2 条小写 `SELECT count(*) FROM t2`）、**`vi` 为空 `[]` 共 52 条**（含 **20 条非基表查询**以及 **32 条读取真实基表的零 `VisitInfo` 查询**，即上述 28 条再加 4 条大写 `SELECT COUNT(*) FROM t2/t3`）。
   - `crates/planner/tests/vectors/v8.5.8/arm64/rewrite.jsonl.gz`（`2440a7e` 与 `#431` 同为 **2,392** 条），含 `"vi"` **2,327** 条（1,770 非空、557 空），`star_vi = 0`（其 2 条 `COUNT(*)` 停在 `D09.4` 桩）。由于 `rewriteAstExprWithPlanCtx` 构造的是假 `TableDual`，不走 `build_data_source`。

---

## 5. `tidb-rs` 系统性权限检查设计

### 5.1 设计原则：数据源 / 表访问边界的单点无遗漏登记

按照宪章第一条 2（安全例外）与第五条（不支持就报错），`tidb-rs` 必须摆脱“靠表达式阶段碰巧遇到列名”的脆弱做法，将 `SELECT` 表访问权限的注册下沉并固定到**所有表数据源构建的唯一入口**，实现“**查询形状无关、优化器剪枝免疫**”的系统性保护：

```text
任意查询（SELECT / SUBQUERY / JOIN / CTE / VIEW / EXPLAIN / DML 读侧 / PointGet）
   │
   ├─► 1. 预处理器 resolve::table_by_name（按语句所需权限防护缺表枚举）
   │
   ├─► 2. 计划构建 DataSource / View / FastPlan 边界（单点注册表读 SelectPriv）
   │        ├─ 基表 / 分区表 / 序列：build_data_source
   │        ├─ 内存表：build_mem_table（排除 information_schema 普通表，见 §5.2）
   │        ├─ 视图：BuildDataSourceFromView（外层视图权 + DEFINER/INVOKER 隔离）
   │        └─ 点查快速路径：point_get_plan::get_visit_info（SelectStmt）
   │
   ├─► 3. 表达式改写阶段：仍对实际引用的具体列与通配符列 `*` 注册精确列权（1143 / 1142）
   │
   └─► 4. 优化入口 optimize() 与计划缓存命中 adjust_cached_plan()：
            均执行 visit_info4_priv_check + check_privilege（拒绝前绝不优化执行或泄露 EXPLAIN ANALYZE）
```

**关键时序保证**：`build_data_source` 位于逻辑优化器（列裁剪 `D09.7`、投影消除、聚合规则、`WHERE 1=0` / `LIMIT 0` 折叠为 `TableDual`）**之前**执行。因此，哪怕整张表最终被常量折叠或列裁剪完全消去，或者输出中未选中任何实际列，表读 `VisitInfo` 均已不可撤销地进入 `PlanBuilder.visit_info` 并接受 `CheckPrivilege` 拦截。

### 5.2 注册规格与各 SQL 结构的精确组合

#### （1）表读 `VisitInfo` 编码与列级 `SELECT(col)` 的协同

在 `logical_plan_builder.rs` 中定义统一辅助函数（如 `append_table_select_visit_info`），按 `(db, table)` 登记：

- `privilege`: `SelectPriv.0`（`tidb_base::mysql::SelectPriv`）
- `db`: 已解析的库名小写（`db_name.l`）
- `table`: 基表/视图/内存表真实物理名小写（`table_info.name.l`，**绝不**用表别名 `as_name`）
- `column`: **`"*"`**（维护者已定，见 §6.2 第 1 项——与 Go 现有校验路径 `collectPrivsForCount` + `columnsPrivRecord.match` + `TestSelectColumnPrivilegeInSubqueryAndCTE` 口径一致）：
  - 若用户拥有全局 `SELECT`、库级 `SELECT ON db.*`、整表 `SELECT ON db.t`，或**至少一列的列级权限 `SELECT(col) ON db.t`**，表读准入可通过；
  - 凡未获任何 `t` 读权限（或只有 `INSERT`/`UPDATE` 等非 `SELECT` 权限）的用户，执行任何形式的不引用列查询（`SELECT 1`、`COUNT(*)`、`SUM(1)`、`EXISTS(...)`、`JOIN ... ON 1=1`）均直接被拒，报错 **`1142 (42000): SELECT command denied to user ... for table 't'`**；
  - 若查询同时引用了某具体列 `b`，表达式改写仍会登记 `(db, t, "b")` 并携带 `1143` 错误，使持有 `SELECT(a)` 的用户 `u_cola` 查 `b` 时仍精确保留 `1143`（`for column 'b'`），查 `SELECT *` 时保留 `1142`。
  - 注：实现 D04.1 的 `MatchColumns` 时必须严格沿用 Go `cache.go:1427-1434, 1556-1567` 的行为——将 `col == "*" && (record.ColumnPriv & SelectPriv > 0)` 写在逐条记录匹配条件内部（而不是先按表返回第一条 `UPDATE(a)` 记录后再判断 `SelectPriv`），避免当用户同时拥有 `UPDATE(a)` 和 `SELECT(b)` 时误判。
- **`_tidb_rowid` 处理**：在 `build_data_source` 构建隐式 `_tidb_rowid` 的 `FieldName` 时（`logical_plan_builder.rs:2556-2562`），若按 Go 原样保持 `orig_tbl_name` 为空，零权限用户虽已被表读基线 `1142` 拦住，但仅有 `SELECT(a)` 的 `u_cola` 仍可读出 `_tidb_rowid`。若决定一并封堵（见 §6.1 D5），只需为 `_tidb_rowid` 补充 `orig_tbl_name = table_info.name.clone()`（或在 `precollect_col_names` 中对其要求整表 `column = ""` 或 `_tidb_rowid` 列权），即可确保 `u_cola` 无法越权读取隐含物理行号（同时确认此改动只改变权检，不影响 `OutputNames` 展示列名或列解析，因为 `ColName` 与 `TblName` 本就不变，需同步检查派生表/CTE 对 `OrigTblName` 的行为）。

#### （2）数据源各类分支（基表、分区表、序列、内存表）

1. **基表与分区表**：在 `build_data_source` 完成表名解析与 `table_by_name`、确认非视图非内存表非 CTE 后、甚至可以紧随表类型分发各自登记，确保分支路径（无论静态/动态分区裁剪、`PARTITION(p0)` 显式分区指定、本地临时表、缓存表、`TABLESAMPLE`）统一生成 `VisitInfo`；本地临时表由后续 `VisitInfo4PrivCheck` 自动免检，与 Go 语义一致。
2. **序列（`table_info.is_sequence()`）**：在 `build_data_source` 提前转换为 `build_table_dual` 之前（`logical_plan_builder.rs:2452-2459`），同样登记表读 `SelectPriv`，防止 `SELECT 1 FROM seq` 泄露序列存在性。
3. **内存表（`build_mem_table`，`logical_plan_builder.rs:2415`）**：当 `D10.11` 落地移植 `build_mem_table` 时，同样在该函数内统一登记 `(db_name, table_info.name, SelectPriv)`（`column = "*"`）。配合 D04.1 `UserPrivileges::request_verification` 复刻 `privileges.go:174-189` 规则——`information_schema` 允许通过（开启 SEM 时不可见表除外）、`metrics_schema` 要求 `SelectPriv | ProcessPriv`、`performance_schema` 走标准表/列授权检验——即可系统性堵住 `SELECT 1 FROM performance_schema.*` 漏洞，同时不影响 `INFORMATION_SCHEMA` 的公开元数据查询。

#### （3）视图（规划用于 `D09.1` 第 2 部分 `BuildDataSourceFromView`）

Go 的漏洞源于两处：清除了 `DEFINER` 内部权限且**漏记调用者对视图自身的 `SelectPriv`**，以及 `INVOKER` 内部表错未统一封装为视图安全语义。设计如下：

1. **调用者对视图自身的 `SelectPriv`**：只要构建视图 `DataSource`，必须为**当前安全上下文（外层调用者，或外层包壳视图的 `DEFINER`）**登记针对目标视图本身 `(db_name, view_name)` 的表读 `SelectPriv`（`column = "*"`；经实测，对视图仅有列级 `SELECT(a) ON v_definer` 的账号 `u_cola_vdef` 亦可正常通过 `"*"` 准入并受限于列 `a`）。若当前处于外层视图内部 `len(saved_views) > 0`，则按外层规则封装为外层视图的 `1356 ErrViewInvalid`；若为顶层查询，失败时直接报 `1142` `for table '<view_name>'`。
2. **检查顺序与防探测（消除 `DEFINER` 同步先报错陷阱）**：在 Go 中，`DEFINER` 验权（`logical_plan_builder.go:5743-5747`）与内层构建报错封装（`:5728`）是在 `BuildDataSourceFromView` **内部同步立即返回 `1356`** 的，而 `b.visitInfo` 直到 `optimize()` 才由 `CheckPrivilege` 校验——若仅仅将调用者视图权加入 `b.visitInfo`，无权者访问已失效或定义者被撤权的 `DEFINER` 视图仍会提前收到 `1356`！因此正确实现需满足：
   - 顶层入口（`view_depth == 0`）**先**在 `pm != nil` 时即时校验调用者对目标视图的 `SelectPriv`（不通过即刻返回 `1142`），同时仍将其写入 `b.visit_info`（供 `PlanCacheStmt` 撤权重验与 `govec` 记录），再切到隔离切片展开内部 `SelectStmt`；
   - `INVOKER` 视图同时保证外层视图 `SelectPriv (1142)` 在 `b.visit_info` 中位于内层表 `VisitInfo (1356)` 之前。
   这样，完全无权访问视图的用户一律首先收获 `1142 (for table 'v')`，不能借 `1356` 探测视图定义内部是否失效或引了哪些表！
3. **`SECURITY DEFINER` vs `SECURITY INVOKER` 内部检查**：
   - `DEFINER`：保留与 Go 一致的逻辑——用 `view.definer` 及默认角色通过 `request_verification_with_user` 验证内部收集到的每条 `visit_info`（包括内层 `build_data_source` 生成的表读项），失败一律报 `1356 ErrViewInvalid`，验证通过后清空内部 `visit_info`，仅保留前述调用者对视图自身的 `SelectPriv`；
   - `INVOKER`：内部 `build_data_source` 若检测到 `len(b.saved_views) > 0`，其生成的 `SelectPriv` `err` 同样设为外层视图的 `1356 ErrViewInvalid`（与 Go `appendColNamesToVisitInfo` 在 `len(views) > 0` 时的 `ErrViewInvalid` 对齐），再合并回外层 `b.visit_info`。如此既补齐视图自身授权，又彻底防止 `u_tbl` 越权读 `v_invoker` 及向 `u_none` 泄漏 `v_invoker` 底层基表名 `'t'`。
4. **`EXPLAIN` 视图**：继续保留 `ShowViewPriv`（`1345 ErrViewNoExplain`）检查，同时因为视图自身的 `SelectPriv` 已被无条件登记，无需再像 Go 那样只在 `viewDepth > 0` 时打补丁判断 `SelectPriv`。

#### （4）CTE、子查询、派生表与 Join

- **CTE（`D09.5`）**：`build_with` 对 `WITH` 子句中声明的**每一个** CTE 均会构建其逻辑计划，因此无论主查询是否消费该 CTE（`WITH c AS (SELECT 1 FROM t) SELECT 1`），`c` 定义内凡读到基表或视图即进入 `build_data_source` 并登记 `SelectPriv`。对后续从 CTE 读取列或执行 `SELECT count(*) FROM c`，`precollect_col_names` 跳过 CTE 虚拟名不会导致漏洞（已由 CTE 定义侧拦截）。若未来优化器对“完全未引用 CTE”考虑跳过构建，必须保持先构建/先鉴权或由安全策略明确裁定。
- **递归 CTE**：种子分支与递归分支中读取的每一张基表都在各自 `build_data_source` 时登记，递归迭代引用的 CTE 自身不重复引入基表外权限。
- **子查询与派生表（`D09.6` / `D09.1`）**：无论是 `EXISTS`、`IN (SELECT ...)`、标量子查询还是 `FROM (SELECT ...) AS s`，全部递归进入 `build_select -> build_table_refs -> build_data_source`，`b.visit_info` 全局累积（注意 `BuildDataSourceFromView` 对 `DEFINER` 视图需用保存/恢复切片隔离，而普通子查询和派生表直接共享外层 `b.visit_info` 即可）。
- **Join（`D09.3`）**：无论是逗号连表、`INNER JOIN`、`LEFT/RIGHT OUTER JOIN` 还是无列引用条件 `ON 1=1`，每个实表叶节点都独立走过 `build_data_source`，必然全部纳入权限校验。

#### （5）点查快速路径（`D10.8` `point_get_plan`）

- **属于 `tidb-rs` 范围的 SELECT 路径**：对 `SelectStmt` 进入 `try_where_in2_batch_point_get` 与 `try_point_get_plan` 时，除收集 `output_names` 与 `cols_in_where_clause` 外，`get_visit_info`（或由 `SelectStmt` 调用方追加）**必须附带表读 `SelectPriv`（`column = "*"`）**，并修正 `is_table_dual` 分支使用解析后的小写 `db_name`（而非可能为空的 `tbl_name_schema.o`，见 `point_get_plan.rs:981, 1084` / Go `:1505, :1578`），同时在含二级唯一索引的表因 `ErrOverflow` + `LogicAnd` 触发 `is_table_dual`（`pairs == None`）时仍保留 `WHERE` 条件列校验，防止 `u_colb`（仅 `SELECT(b)`）执行 `SELECT b FROM t_uk WHERE a = <overflow> AND a = 1` 跳过对列 `a` 的检查（或 `tidb_row_checksum()` 跳过校验）。
- **不移植的 DML 写路径（`X`，仅作架构参考）**：`try_update_point_plan` 与 `try_delete_point_plan` 不应盲目套用“输出列读权”（保留 `outputNames = nil` 语义以免要求无关列的 `SELECT`），但**严禁**在含二级唯一索引表的 `pointGet.IsTableDual` 分支于 `check_fast_plan_privilege`、`UpdatePriv` / `DeletePriv` 与 `SET` 列存在性检查之前提前返回 `PhysicalTableDual`（避免 `u_none` 执行 `DELETE` 或 `UPDATE ... SET nosuch = 1` 得 `OK`）；`check_fast_plan_privilege` 在 `D04.1` 落地后应真实校验 `SelectPriv`（若失败，返回 `None` 回退到常规优化器也将得到完全一致的拒绝结果，无绕过可能）。

#### （6）`EXPLAIN` / `EXPLAIN ANALYZE` 与 `TABLE tbl`

- `TABLE test.t` 在 `tidb-rs` 解析器已转为带通配符的 `SelectStmt`（`crates/parser/grammar/tidb_typed.y:3198`），自然经过 `build_select -> build_data_source` 与 `unfold_wild_star` 校验。
- `EXPLAIN` / `EXPLAIN ANALYZE`（后续 `D09.16` 移植时）内部通过 `OptimizeAstNode` 构建目标查询计划并调用 `CheckPrivilege`，一旦 `build_data_source` 登记了表读权限，`EXPLAIN` 与 `EXPLAIN ANALYZE` 将在生成计划与真正执行扫描**之前**报错拒绝，不会泄露表统计估算或真实执行行数。

#### （7）DML 写路径目标表与读侧分离（供架构完整性与将来 `D09.15` 参考）

`tidb-rs` 当前只读阶段不移植 `D09.15`（DML 计划构建归 `X`），但**在 `build_data_source` 统一登记 `SelectPriv` 的设计下，绝不能破坏合法的 `DELETE`-only 与 `UPDATE`-only 写操作**（已于 Go v8.5.8 实测：`u_del` 仅持 `DELETE ON test.t` 可执行 `DELETE FROM test.t` / `LIMIT 1` / `WHERE 1=1`，`u_upd` 仅持 `UPDATE ON test.t` 可执行 `UPDATE test.t SET a = 1` / `LIMIT 1` / `WHERE 1=1`）。Spec 必须明确以下分离约束：

1. **读侧来源与条件子查询**：`INSERT / REPLACE ... SELECT`、`IMPORT INTO ... FROM SELECT` 以及 `UPDATE / DELETE` 的 `WHERE` / `ORDER BY` / `SET` 右值子查询或关联表都会正常进入 `build_select` 或 `build_result_set_node -> build_data_source`，全部自动被单点机制覆盖；`INSERT / REPLACE INTO t VALUES (...)` 不经过 `build_data_source`（仅检查目标表 `InsertPriv` / `DeletePriv`）。
2. **同时覆盖 `buildUpdate` 与 `buildDelete` 的目标表 `SelectPriv` 精确豁免（废弃 `popVisitInfo()`）**：
   - Go 仅在 `buildDelete`（`logical_plan_builder.go:6663`）于 `ds.Where == nil && ds.Order == nil` 时调一次 `b.popVisitInfo()`，**在 `buildUpdate`（`logical_plan_builder.go:6212`）中则完全没有弹出逻辑**——原因是 Go 的 `buildDataSource` 原本就不加 `SelectPriv`。若在 `buildDataSource` 加了表读 `SelectPriv` 而仍沿用 Go 的写法，不仅盲目 `popVisitInfo()` 会在无条件多表删除 `DELETE t1 FROM t1, t2` 时弹出错误表，更会**直接导致 `u_upd` 执行无条件 `UPDATE t SET a = 1` 时因缺 `SelectPriv` 而误报 `1142`**！
   - **正确做法**：彻底禁用 `pop_visit_info()`，改用显式目标表作用域标记（仅在解析单表 `DELETE` / `UPDATE` 目标表自身的 `build_data_source` 调用期间关闭该目标表 `SelectPriv` 登记，在进入 `WITH` CTE、多表 Join、`WHERE`、`ORDER BY`、`SET` 右值表达式及子查询前立即恢复）。如此：`DELETE FROM t`（无列读）仅需 `DELETE`、`UPDATE t SET a = 1`（无列读）仅需 `UPDATE(a)`；一旦 `WHERE` / `ORDER BY` / `SET` 右值引用列则由 `expression_rewriter` 精确报 `1143`，引用其他表则由子查询/Join 侧 `build_data_source` 精确报 `1142`。

#### （8）`PREPARE / EXECUTE` 与计划缓存（`D13.1` + 例外 `A3`）

结合 `specs/exceptions/A3-plan-cache/spec.md` 与 `specs/D13.1-plancache/spec.md`：

1. `PlanCacheStmt`（`crates/planner/src/core/plan_cache_utils.rs:152`，构造于第 517 行）新增字段 `pub visit_infos: Vec<VisitInfo>`，在 `generate_plan_cache_stmt_with_ast` 调用 `dest_builder.build(...)` 后完整保存 `dest_builder.get_visit_info().to_vec()`。
2. `generate_plan_cache_stmt_with_ast` 末尾以及 `plan_cache.rs:adjust_cached_plan`（第 318 行）**每次都必须执行** `check_prepared_priv`：调用 `visit_info4_priv_check` + `check_privilege`，且**传入当前执行连接此刻的 `ActiveRoles` 与实时权限缓存快照（`PrivilegeManager`）**，禁止缓存权限判定布尔结果。
3. 由于 `build_data_source` 与 `point_get_plan` 现在对所有读表语句稳定生成非空的表读 `VisitInfo`，`SELECT 1 FROM t`、`SELECT COUNT(*) FROM t` 的 `PlanCacheStmt.visit_infos` 不再为空；在 `REVOKE` / `SET ROLE` 后重新 `EXECUTE` 任何已缓存的实例级计划（`tidb-rs` 按 `A3` 仅保留实例级缓存；第 4 阶段接入的非预处理计划缓存亦共用同一实例级缓存与 `check_prepared_priv` 重验路径），或触发 `crates/executor/src/compiler.rs:215` 的 `is_safe_to_reuse_point_get_executor` 短路（位于 `optimize_exec_stmt -> adjust_cached_plan` 之后）时，都会实时拦截并报 `1142` / `1143`。
4. **两条必须同步闭合的缓存边界**：
   - `new_plan_cache_key`（`plan_cache_utils.rs:1015`）目前将 `user_name` / `host_name` 留空，接入 `D04.1` 多用户身份时必须按 `A3` 决定 9 写入真实账号名与主机名，防止不同用户间共享缓存计划对象；
   - 对 `DEFINER` 视图，由于 `BuildDataSourceFromView` 验证定义者后会清空内层 `visit_info`，`PlanCacheStmt.visit_infos` 只含调用者对视图自身的 `SelectPriv`；将来 `D09.1` 第 2 部分允许对视图使用 `PREPARE` 时，需同步保存 `(view.definer, inner_visit_infos)` 于 `check_prepared_priv` 中重验（或将含视图的 `PlanCacheStmt` 标为不可缓存），避免撤回定义者底层表权限后已缓存 `EXECUTE` 仍继续生效。
5. 同步修改 `crates/planner/src/optimize.rs:602` 的 `prep_stmt_visit_infos`，直接返回 `prep_stmt.visit_infos`，修复 `StmtCtx.tables` 对 `EXECUTE` 为空的已知占位问题。

#### （9）`D04.1` 边界接口与分层协同（`N0`）

按 `layers.toml`，`tidb-privilege`（`D04.1`）处于 rank 8，而 `tidb-planner` 处于 rank 13，依赖方向合法（`tidb-planner -> tidb-privilege`）。在 `D04.1` 尚未实现前，规划器内部继续在 `optimize.rs:490` 与 `plan_cache.rs:418` 保持基于 `Option<&dyn PrivilegeManager>` 的桩边界（无管理器时仅构建与记录 `VisitInfo`，不中断未鉴权开发流水线），一旦 `D04.1` 落地即无痛接通；`preprocess.rs:table_by_name` 同样在有 `PrivilegeManager` 时立即启用权限预检，阻止未授权用户识别表是否存在。

---

### 5.3 关键设计权衡与影响分析

#### 1. 错误码排序：`SELECT a FROM test.t` 无权限用户报 `1142` 还是 `1143`

因为 `CheckPrivilege` 按切片先后顺序返回第一个失败项，在 `build_data_source` 插入 `column = "*"`（错误为 `1142` `for table 't'`）与列引用表达式插入 `column = "a"`（错误为 `1143` `for column 'a' in table 't'`）的先后顺序直接决定对全无权限用户（`u_none`、`u_ins`、`u_other`）暴露的错误码：

| 选项 | 实现方式 | `u_none`/`u_ins`/`u_other` 查 `SELECT a FROM t` | `u_none` 查 `SELECT 1 FROM t` | `u_cola` 查 `SELECT b FROM t` | 缺表探测 `SELECT a FROM t` vs `nosuch` | 对已有 Go 行为 / `govec` 的冲击 |
|---|---|---|---|---|---|---|
| **方案 A（即时入队，表读检查在先）** | 直接在 `build_data_source` 末尾/前置追加 `("*", 1142)`，`build_selection` / `build_projection` 的列条目 `("a", 1143)` 自然在其后 | **`1142`**（拒绝访问表 `t`） | **`1142`** | **`1143`**（通过了 `"*"`，在 `"b"` 失败） | **完全关闭神谕**：`SELECT a FROM t` 与 `SELECT a FROM nosuch` 均返回 **`1142`** | 对零权限用户访问实列，错误码从 Go 的 `1143` 变为 **`1142`**；`selectbuild` `vi` 切片首元素变为表读项（最符合 MySQL 8.0 逻辑与直观语义） |
| **方案 B（后置/按表优先排布列检查）** | 表读 `"*"` 暂存，在 `build_select` 结束或由 `CheckPrivilege` 同表先检实列再检 `"*"`（仅在没有任何具体列校验时触发 `"*"` 的 `1142`） | **`1143`**（与 Go v8.5.8 保持一致） | **`1142`** | **`1143`** | **残余神谕**：无权限者查 `SELECT a FROM t` 得 `1143`，查 `SELECT a FROM nosuch` 得 `1142`，仍可枚举表名 | 保留了 `1,072` 条仅含 `1143` 查询的首个报错码，但逻辑绕弯、无法根除表名探测且依赖复杂的块级缓冲 |

**分析结论**：从安全本质与 MySQL 规范一致度看，一个连表级及任意列读权限都没有的用户执行 `SELECT a FROM t`，根本问题在于“无权访问表 `t`”而不是“唯独缺列 `a` 的权限”，且方案 B 仍会通过 `1143` vs `1142` 泄露表存在性。方案 A 实现最简、表存在性完全不可探测、错误语义也最合理；需要负责人按宪章第九条 1(a) 就该用户可见错误码变更做出裁定（详见 §6.1 D3）。

#### 2. `collectPrivsForCount()` 补丁的去留（`expression_rewriter.rs`，`2440a7e` `:1085, :2044` / `#431` `:1137, :2096`）

`expression_rewriter.rs` 中的 `collect_privs_for_count` 本质上是 Go 为掩盖 `buildDataSource` 缺失所打的局部补丁。一旦 `build_data_source`（及将来的 `BuildDataSourceFromView` / `build_mem_table`）统一登记了表读 `SelectPriv`：
- 对所有经过规划器完整构建的 SQL，`collect_privs_for_count` 产生的 `"*"` 记录与数据源登记完全重复（每多写一个 `COUNT(*)` 还会重复 push 一次冗余条目，浪费后续 `RequestVerification` 开销）；
- 该函数因无 `InfoSchema` 且直接读 `self.names`，还会产生 `rewriteAstExprWithPlanCtx` 视图名退化与 CTE 空 `OrigTblName` 等长期不一致。
- 实查 `rewrite.jsonl.gz`（共 2,392 条），`star_vi = 0`，移除它也不会造成现有 `govec` 向量回归失败（但在移交修改 `rewrite_ast_expr_with_plan_ctx` 单元测试时需同步确认）。因此推荐在数据源单点注册生效时一并删除 `collect_privs_for_count`（属于不改变 SQL 最终行为的内部清理，记入 §6.2 维护者已定）。

#### 3. 与 Go TiDB v8.5.8 混合集群兼容性

`tidb-rs` 早期定位为与 Go TiDB v8.5.8 节点在同一集群并存的只读节点（`CLAUDE.md` 开头）：
- **协议、元数据、存储与计算下推零变更**：`VisitInfo` 收集与 `CheckPrivilege` 完全发生在 SQL 节点本地内存中的规划校验阶段，不改动 `mysql.*` 授权系统表格式、不改动 DDL 语义、不改动 TiKV 编解码与 Coprocessor/TiFlash protobuf、不改动 MySQL 协议握手与结果包格式。混合集群可零迁移直接灰度上线或回滚。
- **混合路由窗口下不同方案（D1 A / B / C）的具体影响**：
  - **D1 选 A（常开修复）**：在 Go 节点与 `tidb-rs` 节点共挂于同一负载均衡器期间，同一欠权账号执行上述绕过 SQL（如 `u_other` 执行 `SELECT 1 FROM test.t` 或 `SELECT COUNT(*) FROM test.t`），请求落到未修复的 Go v8.5.8 节点会成功、落到 `tidb-rs` 节点会报 `1142`——同一账号+同一 SQL 会按负载均衡落点出现时而成功时而失败的路由漂移，直到业务补齐缺失的 `SELECT` 权限或下线未修复的 Go 节点。合规应用不受影响，但若线上存在隐含依赖“无权查 `SELECT 1` / `COUNT(*)` 成功”的监控/巡检脚本，混部期会间歇报错，需在 `docs/compatibility.md` 中明确预警。
  - **D1 选 B（照 Go 原样保留漏洞）**：混合集群两侧表现一致、无负载均衡漂移，但 `tidb-rs` 节点同样长期开放所有越权读漏洞。
  - **D1 选 C（修复但置于兼容性开关之后，默认值由负责人选定）**：例如提供系统变量开关控制数据源表读拦截（严格模式 vs Go 兼容模式）。若混部期默认兼容或允许运维按集群打开兼容开关，可在灰度与权限梳理期间消除负载均衡两侧分裂，待排查清理遗留欠权脚本后再统一切换为严格模式；若默认严格，则新集群开箱安全，同时为存在历史脚本的混部集群保留临时应急退路。代价是需维护双模式分支、开关文档与测试矩阵，并约定后续移除开关的生命周期。

#### 4. `govec` 向量与偏差台账 `docs/testing/rulings.toml` 影响

- **`govec` 向量冲击**：
  - `selectbuild`：在本分支基线（`2440a7e`：1,743 条，其中 1,539 条带 `vi`）上，涉及基表扫描的 **1,527 条**记录（`1,499` 非空 + `28` 原为空 `vi` 的基表查询）会在 `vi` 中新增对应基表的 `("*", 1142)` 条目，而其余 12 条 `SELECT 1` / `FROM dual` / 常数 CTE / 常数派生表仍保持 `vi: []`。在合入 `#431` 后的 `origin/main`（`63adf55`：2,449 条，其中 2,139 条带 `vi`）上，共 **2,119 条**记录（`2,087` 非空 + `32` 原为空 `vi` 的基表查询，其中含 4 条大写 `COUNT(*)`；另有 2 条小写 `count(*)` 原 `star_vi` 合并入单点条目）会体现基表 `("*", 1142)`，其余 20 条非基表查询仍保持 `vi: []`。在未通过上游补丁方式重新生成对照向量前，`crates/planner/tests/govec_selectbuild.rs` 不能直接用 `&got == out` 全等断言，而应采用类似 `govec_rewrite.rs` 的 `KNOWN_*` 结构化归一化辅助（按已定 spec 精确滤除 Go 缺失的基表 `"*"` 项后比对其余计划、标志与 `visitInfo`），或按 `docs/compatibility.md` `go-fix` 流程生成补丁版参照向量。
  - `rewrite`（2,392 条）：由于使用假 `TableDual` 且 `star_vi = 0`，不受影响。
- **`docs/testing/rulings.toml` 登记计划**：
  - 按宪章第一条 2 与 `docs/testing/rulings.md` §1，任何面向 SQL 用户可见的行为差异（`SELECT 1` / `COUNT(*)` / `_tidb_rowid` 拒止、`EXPLAIN` 拒止、视图校验加严、缺表探测收敛、`SELECT a` 错误码如从 `1143` 变为 `1142`）在 `D04.1` 实际接线生效时，均须在 `rulings.toml` 中按顺序新建裁定（本分支快照 `2440a7e` 停在 `R26`，但 `origin/main` `#431` 已占用了 `R27`，因此后续基于 `main` 顺延编号为 **`R28+`**）：
    - `class = "divergence"`（宪章第一条 2 安全例外），`plane = "error"`；
    - 初期若 `ab` 端到端对比尚无专门多用户权限 workload，则设 `layer = "none"` 并通过 `test` 字段绑定 Rust 集成测试名；接入权限集成测试或 workload 后切换为对应 `layer` 与匹配规则；
    - 写入本 spec 评审结论、复现对照与关联的 `upstream-pitfall` issue，并在 `docs/compatibility.md` 同步列明混合集群注意事项。

---

## 6. 决策点（严格按宪章第九条划分）

### 6.1 需要负责人决定（宪章第九条 1）

以下 **4 个问题**（D1、D3、D4、D5；原 D2 属跟随 Go 已有检查路径的既有规则，已按维护者决议移入 §6.2 第 1 项，保留原编号以便评审追踪）涉及**长期保留、用户可见的与 Go v8.5.8 不同的权限行为与错误码**，按宪章第九条 1(a) 及第九条 4，全部提请项目负责人拍板：

#### D1：是否全面修复 Go v8.5.8 遗漏的 `SELECT` 表级读权限校验，以及如何处理混合集群过渡窗口（宪章第一条 2 安全例外）

- **背景与分层泄露影响（基于本地 Go v8.5.8 实测）**：在 Go TiDB v8.5.8 中，当一条查询不引用表的真实列（如 `SELECT 1 FROM t`、大写 `SELECT COUNT(*) FROM t`、`SUM(1)`、`EXISTS(SELECT 1 FROM t)`、`t2 JOIN t ON 1=1`、常数 CTE、`EXPLAIN / EXPLAIN ANALYZE SELECT 1`、写权限用户跑 `INSERT INTO t2 SELECT 1 FROM t`、视图自身、隐藏列 `_tidb_rowid`，以及前述无列引用语句在 `PREPARE` 后因 `VisitInfos` 为空在 `REVOKE` 后继续放行）时，权限检查会漏防。按调用者既有权限分层，**实际新增泄露的内容并不相同**：
  - **对该表无任何权限的用户（`u_none(db=None)`、`u_other`）**：实测 `information_schema.TABLES` 返回 **0 行**（表完全不可见）。Go 漏洞向他们泄露了**表存在性、精确实时行数、`_tidb_rowid` 物理行号**，且具备他表写权时可通过 `INSERT ... SELECT` 将行数持久化写入他表。
  - **对该表已持有非 `SELECT` 表级权限的用户（如 `u_ins`，持 `INSERT, UPDATE ON test.t`）**：实测在 `ANALYZE TABLE test.t` 后，其查询 `information_schema.TABLES` **已经可见 `test.t` 并读出估算行数 `TABLE_ROWS = 3`**。因此对这类用户，本修复新增阻断的是**无需 `ANALYZE` 即可读取的精确实时行数**、**`_tidb_rowid` 物理行号**以及 `EXPLAIN ANALYZE` 真实执行详情，而非从零暴露表存在与近似行数。（注：持单列 `SELECT(a)` 的 `u_cola` 虽在 `I_S.TABLES` 不可见，但其本就有权读列 `a` 的全部行。）
- **选项与混合集群影响**：
  - **选项 A（系统性修复，常开，遵循宪章第一条 2 安全例外）** —— 在数据源、视图、内存表、快速点查入口统一校验 `SELECT` 权限；未授权用户执行上述所有语句均一律拒绝报错，并在 `docs/compatibility.md`、`rulings.toml` 与 `upstream-pitfall` issue 中登记差异。
    - *混合集群影响*：在 Go v8.5.8 与 `tidb-rs` 共挂同一负载均衡器期间，同一欠权账号执行上述绕过 SQL 时，路由到 Go 节点成功、路由到 `tidb-rs` 节点报 `1142`（结果随后端落点变化，直到补齐授权或下线未修复 Go 节点）。
  - **选项 B（照 Go v8.5.8 原样移植，bug-for-bug 一致）** —— 继续仅在列引用与小写 `count` 时校验。
    - *混合集群影响*：Go 与 `tidb-rs` 节点行为完全一致、无路由漂移，但 `tidb-rs` 长期保留上述越权读漏洞。
  - **选项 C（修复但置于兼容性开关之后，默认值由负责人指定：默认严格 / 混部期默认兼容 Go）** —— 同样实现完整的表读 `SelectPriv` 检查，但通过一个系统变量开关控制是否对无列引用数据源强制生效。
    - *混合集群影响*：若选**“默认严格 + 可显式临时关闭兼容”**，新集群默认安全，混部集群如遇历史欠权脚本报错可由运维一键切至 Go 兼容模式平滑过渡；若选**“混部期默认兼容 Go + 显式开启严格”**，混部接入零抖动，待审计清理后再统一切换严格模式。代价是需在 `D04.1` 维护双模式分支与测试，并约定开关的退场计划。
- **建议与理由**：强烈不建议选 B（对 `u_none` / `u_other` 构成明确的跨表/跨库零权限探测、精确行数与 `_tidb_rowid` 泄露及物化拷贝，符合宪章第一条 2 安全例外）。**在 A 与 C 之间**：若当前部署预期不需兼容依赖 Go 漏洞的遗留欠权脚本，**推荐选 A**（实现与运维最干净，无双模式复杂度）；若规划了与 Go v8.5.8 长期混部且需要平滑灰度窗口避免负载均衡两侧行为分裂，**推荐选 C（默认严格）**，既保证默认安全，又给混部窗口留出受控开关。

#### D3：完全无权限用户查询具体列（如 `SELECT a FROM test.t`）时，返回“表拒止 `1142`”还是保持 Go 的“列拒止 `1143`”

- **背景**：当用户对表 `t` **既无整表 `SELECT` 也没有任何一列的 `SELECT`**，却执行 `SELECT a FROM test.t` 时：
  - Go v8.5.8 实测报 **`1143`（`SELECT command denied ... for column 'a' in table 't'`）**；而查询不存在的表 `SELECT a FROM test.nosuch` 时实测报 **`1142`（`for table 'nosuch'`）**，由此造成**单靠错误码即可枚举某个表是否真实存在**的漏洞（按 MySQL 8.0 文档与授权模型推断应先报 `1142`，**未在本机 MySQL 8.0 实测验证**）。
- **选项**：
  - **选项 A（推荐）：先检表/视图读权限，统一报 `1142`（拒绝访问表/视图）** —— 数据源阶段登记的表级检查优先生效（视图场景下也在展开或 `DEFINER` 校验 `1356` 之前先检调用者视图 `SelectPriv`）。零权限用户查 `SELECT a FROM t`、`SELECT 1 FROM t`、`SELECT a FROM nosuch`（及无权查任意视图）全部统一报 `1142`，只在“已经通过表读准入（如拥有 `SELECT(a)`）但访问无权列 `b`”时才报 `1143`（据 MySQL 8.0 文档推断一致，**未在本机实测验证**，也更符合管理员直觉）。
  - **选项 B：列/内层检查优先（维持零权限用户查实列时仍报 `1143`、查 `INVOKER` 视图仍报 `1356`）** —— 将表读检查放到列检查与视图内层之后执行，只有完全不引用任何列的查询（如 `SELECT 1 FROM t`）才触发 `1142`。
- **建议与理由**：推荐选 **A**。选 B 虽能在“带列引用”的报错号上少一处与 Go 的表面差异，却故意保留了 `1143` vs `1142` 的表存在性探测神谕，且增加了检查顺序维护复杂度；选 A 一步到位封死探测缝隙，语义更一致。

#### D4：是否同步修复预处理缺表检查过于宽松的问题（防止持有非 `SELECT` 权限的用户通过 `1146` 探测表是否存在）

- **背景**：当用户在 `SELECT` 语句中引用一个**不存在的表**（如 `SELECT 1 FROM test.nosuch`）时，Go 的预处理器检查的是“用户对该表是否拥有**任意**权限（`AllPrivMask`）”而非“是否拥有 `SELECT` 权限”。实测表明，若用户只有 `GRANT INSERT ON test.*`（`u_db_ins`，没有任何 `SELECT` 权限），Go 对不存在的表会放行通过权限关卡并直接报错 **`1146 Table 'test.nosuch' doesn't exist`**，从而可借助 `SELECT` 枚举全库是否存在某张表（按 MySQL 8.0 文档推断，无 `SELECT` 权限者应对读语句返回 `1142` 而非 `1146`，**未在本机 MySQL 8.0 实测验证**）。
- **选项**：
  - **选项 A（推荐）：按当前语句实际需要的操作权限做缺表拦截** —— 对 `SELECT`（及 `EXPLAIN SELECT` 等只读语句）检查用户是否具备该表的读权限（与 §6.2 第 1 项已定的 `SelectPriv` + `"*"` 一致），不具备则返回 `1142` 而非 `1146`；DML 写语句按需检查对应写权限。
  - **选项 B：保持 Go 现状（`AllPrivMask`，`column = ""`）** —— 维持 `u_db_ins` 在 `SELECT` 下仍看到 `1146`，并且仅拥有列权限 `u_cola` 查不存在表得 `1142`。
- **建议与理由**：推荐选 **A**。既然通过 D1、D3 着力消除表存在性神谕，不应在 `tableByName` 中留下一个显式的 `INSERT -> 1146` 侧门。

#### D5：仅有部分列权用户（如 `SELECT(a)`）是否可读取无整数/聚簇主键表的隐藏列 `_tidb_rowid`

- **背景**：对于无整数主键且非聚簇主键（`!PKIsHandle && !IsCommonHandle`）的表，TiDB 有一个内部隐藏行号列 `_tidb_rowid`。Go v8.5.8 因遗漏设置它的归属表名，导致不仅零权限用户可直接读取 `_tidb_rowid`，即使将来通过 D1 拦截了零权限用户，在 §6.2 第 1 项已定采用 `column = "*"`（有任意列 `SELECT(a)` 即可通过表读准入）的前提下，只有 `SELECT(a)` 的用户依然可以执行 `SELECT _tidb_rowid FROM test.t` 与 `WHERE _tidb_rowid = 1`。
- **选项**：
  - **选项 A（推荐）：非整表 `SELECT` 权限用户禁止访问 `_tidb_rowid`** —— 让 `_tidb_rowid` 也受列/表级权限约束（如补全归属表名触发 `1143`，或要求具备整表 `SELECT` 方可读物理行 ID）。
  - **选项 B：仅拦截零权限用户，持有 `SELECT(a)` 的用户允许继续读取 `_tidb_rowid`** —— 保留 Go 对 `_tidb_rowid` 跳过列检查的逻辑。
- **建议与理由**：推荐选 **A**。`_tidb_rowid` 是物理存储自增句柄而非列 `a` 的数据，未授权用户不应借单列授权获取全表真实行 ID 分布或做跨列隐蔽关联；实现改动极小且明确安全边界。

---

### 6.2 维护者已定（宪章第九条 2）

以下事项属遵循 Go 已有校验口径的既定规则、内部实现、数据结构表示、测试方法与边界面向未移植块的工程组织，不单独提请负责人裁定（随整体 spec 评审一并接受审视，负责人若反对可覆盖）：

1. **列级 `SELECT(col)` 用户的表读准入规则（原 D2，采用 `column = "*"`）**：在 D1 启用表读 `SelectPriv` 检查时，统一使用 `column = "*"`（要求用户拥有全局/库级/表级 `SELECT`，或对该表**至少一列**的列级 `SELECT(col)` 权限；查具体无权列 `b` 或 `SELECT *` 仍由列/通配符校验拦截）。这与 Go 在所有已实施表读校验处的既有规则完全一致——`collectPrivsForCount` + `columnsPrivRecord.match`（`cache.go:1433`）以及 Go 自带的集成测试 `TestSelectColumnPrivilegeInSubqueryAndCTE`（允许仅持 `SELECT(a)` 的用户执行 `SELECT count(*) FROM t1`）均采用同一语义，属于 D1 落地的直接推论，不另开与 Go 分叉的新规则。
2. **规划器单点辅助函数布局**：在 `crates/planner/src/core/logical_plan_builder.rs` 抽取统一辅助（例如 `append_table_select_visit_info`），在 `build_data_source` 解析 `table_info` 之后即刻登记（覆盖基表、分区表、序列分支），并在后续 `D10.11` `build_mem_table` 及 `D09.1` 第 2 部分 `BuildDataSourceFromView`（视图自身与内层分支）复用；同步为 `point_get_plan::get_visit_info` 的 `SelectStmt` 路径补齐同一表读条目。
3. **移除多余且语义不一致的 `collect_privs_for_count` 补丁**：在单点数据源级登记生效后，删除 `expression_rewriter.rs` 中的 `collect_privs_for_count` 调用及实现（`2440a7e` 第 1085–1087、2044–2083 行；`#431` 第 1137–1139、2096–2135 行），避免重复 `VisitInfo` 与无 `InfoSchema` 视图名退化问题（如单独 `rewrite_ast_expr_with_plan_ctx` 接口单测需要保持独立表达式改写测试，可与已确认 `rewrite.jsonl.gz` `star_vi = 0` 基线比对确认无影响）。
4. **视图内部 `INVOKER` 报错统一归口**：`BuildDataSourceFromView`（后续 `D09.1` 第 2 部分）在 `len(saved_views) > 0` 时，让内层 `build_data_source` 生成的表读条目同样使用外层视图的 `ErrViewInvalid (1356)`，避免将底层基表名泄漏给有视图权却无底层权限的调用者；`DEFINER` 视图严格使用 `b.visit_info` 快照隔离后经 `request_verification_with_user` 检验再丢弃（顶层对无视图权限的调用者按 D3 处理）。
5. **`PlanCacheStmt.visit_infos` 与实时重验接线**：在 `D13.1` 补回 `PlanCacheStmt.visit_infos` 保存与 `crates/planner/src/optimize.rs:602` 读取；`adjust_cached_plan -> check_prepared_priv` 每次命中都按当前会话 `ActiveRoles` 与最新 `PrivilegeManager` 重新执行 `visit_info4_priv_check + check_privilege`，并确保校验早于 `is_safe_to_reuse_point_get_executor`。
6. **非盲弹的 `DELETE` / `UPDATE` 目标表豁免机制（供将来 `D09.15` 使用）**：不沿用脆弱的 `b.pop_visit_info()`，而以目标表上下文作用域标记精准豁免单表无读列 `DELETE`（如 `DELETE FROM t`）与单表无读列 `UPDATE`（如 `UPDATE t SET a = 1`）目标表自身的 `SelectPriv`，一旦进入 `WITH` CTE、多表 Join、`WHERE`、`ORDER BY`、`SET` 右值子查询即恢复正常表读登记，绝不破坏合法纯写权限或放行读侧探测。
7. **在 `D04.1` 落地前维护既有边界桩**：`crates/planner/src/optimize.rs:490`、`plan_cache.rs:418`、`point_get_plan.rs:1242` 与 `preprocess.rs:756` 在 `tidb-privilege` 尚为规划中 crate（`blocked-by:D04.1`）期间保持边界接口契约不变；规划侧可先落地单点 `VisitInfo` 生成（`govec_selectbuild.rs` 通过明确的 spec 归一化表记录与 Go v8.5.8 差异），待 `D04.1` `PrivilegeManager` 合并后统一激活执行时拒止与 `rulings.toml` 台账 `R28+`。

---

## 7. 后续实施分解与验收清单

spec 经负责人确认后，按块边界分批落地（不在本 spec-only PR 中改 Rust 代码）：

1. **Planning 层 `VisitInfo` 完善（隶属 `D09.1` / `D09.2` / `D10.8` 增量 PR）**：
   - `build_data_source` 增加单点表读登记，修复 `_tidb_rowid` 归属（若 D5 选 A），补全 `point_get_plan::get_visit_info` 对 `SelectStmt` 的表读条目及 `is_table_dual` 的 `db_name` 传参，移除 `collect_privs_for_count`（按 §6.2 第 3 条），并按 §5.3 更新 `govec_selectbuild` 测试比对。
2. **计划缓存上下文补全（隶属 `D13.1` / `A3` 增量 PR）**：
   - `PlanCacheStmt` 新增 `visit_infos` 字段并更新 `optimize_exec_stmt` 的 `StmtCtx.tables` 收集，为 `D04.1` 准备非空预处理校验载荷；接入用户身份时同步补全 `new_plan_cache_key` 的 `(user_name, host_name)`。
3. **权限管理器与执行校验接线（隶属 `D04.1` 主实现 PR）**：
   - 落地 `tidb-privilege`（`UserPrivileges`、`MySQLPrivilege`、基于 `catalog`/系统表的授权缓存），确保 `MatchColumns` 支持 `"*"` 遍历谓词语义及内存库规则（`information_schema` / `performance_schema` / `metrics_schema`）；
   - 接通 `crates/planner/src/optimize.rs` (`CheckPrivilege`)、`preprocess.rs` (`table_by_name` 防探测)、`plan_cache.rs` (`check_prepared_priv` 每次命中实时重验) 与 `point_get_plan.rs` (`check_fast_plan_privilege`)；
   - 随未来 `D09.1` 第 2 部分（视图 `BuildDataSourceFromView`）、`D10.11`（内存表 `build_mem_table`）、`D09.16`（`EXPLAIN`）逐步补全对应分支。
4. **验证与合规动作清单**：
   - 针对 Go v8.5.8 全部漏洞矩阵建立 SQL 级端到端回归测试（涵盖 `u_none(db=None)`、大写/混写聚合及非 `count` 聚合、`EXISTS`/`IN`/Join/CTE/视图嵌套/`CREATE VIEW ... AS SELECT 1`/`EXPLAIN ANALYZE`/`_tidb_rowid`/PointGet 溢出 `IsTableDual`/`PREPARE -> REVOKE/SET ROLE -> EXECUTE` 在实例缓存 ON/OFF 与 PointGet 复用下的即时撤权拦截、`test.nosuch` 缺表无神谕一致返回码，以及 `DELETE`-only / `UPDATE`-only 无读列纯写的正反回归）；
   - 按宪章第一条 2 与 `CLAUDE.md` 开立英文 `upstream-pitfall` issue 汇总并复现 Go v8.5.8 缺陷，并在 `docs/testing/rulings.toml` 登记经负责人批准的 `divergence` 裁定条目（`R28+`，顺延于 `#431` 的 `R27` 之后）以及更新 `docs/compatibility.md`。

---

## 8. 本文所用 ID

| ID | 含义 |
|---|---|
| `D04.1` | 权限检查与认证（ROADMAP P5，`tidb-privilege`） |
| `D09.0` | 计划构建器骨架与预处理器（`planbuilder`、`preprocess`） |
| `D09.1` | SELECT 逻辑计划构建（数据源、投影、选择、视图） |
| `D09.2` | 表达式改写（`expression_rewriter`） |
| `D09.3` / `D09.4` / `D09.5` / `D09.6` | Join / 聚合与窗口 / 集合运算与 CTE / 子查询与去关联 |
| `D09.12` / `D09.13` / `D09.15` / `D09.16` | 分区裁剪 / 优化器驱动 / DML 计划构建（`X` 不移植） / SHOW 与 EXPLAIN 构建 |
| `D10.8` / `D10.11` | 点查快速路径（`point_get_plan`） / 内存表谓词提取（`buildMemTable`） |
| `D13.1` / `A3` | Plan Cache 实现块 / Plan Cache 例外设计（统一实例级缓存） |
| `N0` / `X` | crate 依赖分层规范 / 不移植模块集合（DDL、写路径等） |
