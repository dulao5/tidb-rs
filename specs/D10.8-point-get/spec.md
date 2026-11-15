# D10.8 点查快速路径（spec）

状态：第 1 部分（VS1-11）评审中（2026-11-15）。块：D10.8（ROADMAP P3），crate `tidb-planner`（N0 rank 13）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（D10.8 一行）、§5（VS1-11）；[D12.1 spec](../D12.1-read-executors/spec.md)（点查执行器，VS1-12，本项替换它的计划承载类型）；[N1](../exceptions/N1-plan-representation/spec.md)（计划表示）；[N13](../exceptions/N13-shared-metadata/spec.md)（共享元数据）。

## 1. 范围

Go 范围：`pkg/planner/core/point_get_plan.go`，tracker 中 88 个函数。

### 1.1 第 1 部分（VS1-11）

函数清单是 `specs/VS1-vertical-slice/functions.tsv` 中 VS1-11 的行：

| 块 | Go | Rust | 函数 |
|---|---|---|---|
| D10.8 | `point_get_plan.go` | `crates/planner/src/core/point_get_plan.rs` | `TryFastPlan`、`tryPointGetPlan`、`checkTblIndexForPointPlan`、`getNameValuePairs` 等名字和值的提取、`PointGetPlan` 的全部方法（52 个） |
| D09.0 | `util.go` | `crates/planner/src/core/util.rs` | `EncodeUniqueIndexKey`、`EncodeUniqueIndexValuesForKey`（按决定并入本项，D12.1 spec §5 第 1 条） |
| D00.3 | `util/logutil/consistency/reporter.go` | `crates/executor/src/consistency.rs` | `Reporter.ReportLookupInconsistent`（点查索引有行无时的错误，完成条件要求测试它） |

converge：
- `scripts/converge.py --block D10.8 --part specs/VS1-vertical-slice/functions.tsv --item VS1-11 crates/planner/src/core/point_get_plan.rs`：46/52 移植 + 6 个边界桩（`ExplainInfo` / `ExplainNormalizedInfo` → D10.9；`getHashOrKeyPartitionColumnName` → D06.4；`isExprHasSubQuery`、`subQueryChecker.Enter` / `Leave` → X）。
- D09.0 `--item VS1-11 crates/planner/src/core/util.rs`：2/2；D00.3 `--item VS1-11 crates/executor/src/consistency.rs`：1/1。

清单外顺带移植（快速路径直接调用，函数很小）：`fixcontrol.GetBool` / `GetBoolWithDefault`（D09.13，`util/fixcontrol.rs`）、`CheckTableLock` / `checkStableResultMode`（D09.13，`core/optimizer.rs`）、`extractLimitCountOffset` / `getUintFromNode`（D09.1，`core/logical_plan_builder.rs`）、`isForUpdateReadSelectLock`（D09.0）、`IsSupportedSelectLockType` 等三个（D09.14）、`SessionVars.PessimisticLockEligible`（X）。它们以 `// go: <path> <Symbol>` 标注在用到的位置或对应文件里，所属块移植时并过去。另外，`IsPointGetWithPKOrUniqueKeyByAutoCommit`（VS1-19 移植的会话部分）补上了 `PointGetPlan` 分支（无二次读、非缓存表才可用 MaxTS），reader / projection 分支仍是 `blocked-by:D10.1`。

### 1.2 块内其余（`blocked-by:D10.8`）

`BatchPointGetPlan` 及其方法、`newBatchPointGetPlan`、`tryWhereIn2BatchPointGet`（VS1 不做批量点查）；点更新 / 点删除（`tryUpdatePointPlan`、`buildPointUpdatePlan` 等，写路径 X）；两个测试钩子（failpoint，不移植）。

## 2. 承载方式

1. **计划不存会话（N1 §7 第 2 条，`M-CTX`）。** Go 的 `PointGetPlan` 内嵌 `baseimpl.Plan`，带 `ctx`。Rust 的计划节点不存上下文：读上下文的方法改为由调用方传入要读的值——`OperatorInfo(normalized, enable_redact_log)`、`PrunePartitions(use_cache)`。`TryFastPlan` 拿 `&mut PlanCtx`（会话变量 + 表达式上下文）和 `NodeW`。
2. **内嵌（`M-SELF`）。** `PhysicalPlan` trait 要求一个 `BasePhysicalPlan`，计划内嵌它，其中的 `plan` 就是 Go 的 `baseimpl.Plan`；Go 的 `PointGetPlan` 自己实现了 `PhysicalPlan` 的每个方法，Rust 逐一覆盖 trait 的默认实现。`Cost` / `SetCost` / `Attach2Task` / `ToPB` 不在 trait 中（`base.Task` 是 D10.1 的），是固有方法，返回 Go 的值（`Attach2Task`、`ToPB` 为 nil）。Go 的未导出字段在 Rust 中是 `pub`：执行器（D12.1）和它的测试要构造计划。
3. **共享元数据（N13）。** `TblInfo` / `IndexInfo` 共享 info schema 的 `Arc`（Go `plan-cache-clone:"shallow"`）。Go 的 `handleFieldType` 和 `ColsFieldType` 指向列的 `FieldType`；Rust 的 `ColumnInfo.field_type` 是值，所以这里在建计划时复制一份放进 `Arc`，之后只读。schema 的 `RetType` 与 Go 一样是列类型的副本（`colInfoToColumn` 的 `Clone()`）。
4. **结果。** `TryFastPlan` 返回 `Result<Option<PhysicalPlanRef>, Error>`：`Ok(None)` 是 Go 的 nil，`Ok(Some)` 是点查；Go 的其他结果是边界桩（§4）。
5. **执行器的计划（D12.1 spec §2 第 4 条）。** `glue::PointGetPlan` 删除，builder 和执行器改用 `tidb_planner::core::point_get_plan::PointGetPlan`（字段名不变；计划 ID 改从 `PhysicalPlan::id()` 读）。Go 的 `buildPointGet` 在计划上直接 `PrunePartitions`（改写缓存计划的 `PartitionIdx`）；Rust 的计划是共享的 `Arc`（N1 §3.4），有分区的表在副本上裁剪，执行器读副本。
6. **唯一索引 key。** `EncodeUniqueIndexKey` / `EncodeUniqueIndexValuesForKey` 在 Go 中取会话，读 `StmtCtx`、`SQLMode`、`ConnectionID`（`table.CastValue`）。执行器在 `Next` 中只有语句上下文（N2），所以这三者是参数（`M-CTX`）；执行器在 `Init` 时从会话取 SQL mode 和连接 ID。D06.4 的 `CastValue` 拆出 `cast_value_with_stmt_ctx`，原函数调用它。Go 原地转换 `idxVals`（执行器的切片就是计划的），Rust 原地转换执行器自己的副本。
7. **不一致错误。** `consistency.Reporter.ReportLookupInconsistent` 移植到 `crates/executor/src/consistency.rs`（目前只有执行器用，D00.3 整体移植时搬走）：错误 `ErrLookupInconsistent`（8133）与日志字段相同；Go 在存储是 `helper.Storage`（有 MVCC 调试接口的 TiKV）时把行和索引的 MVCC 历史写进日志，tidb-rs 的存储没有这个接口，相当于 Go 的 `!ok` 分支。

## 3. 拒绝条件逐条对照（评审要求）

Go 的每个"不出点查"的条件，Rust 都按同一顺序移植；govec `fast` 记录逐条覆盖（§6），下表列出位置。

| 条件 | Go 位置 | 结果 |
|---|---|---|
| `tidb_enable_stable_result_mode`（非 DML）、fix control 52592 为 ON | `TryFastPlan` 开头 | nil |
| `SELECT ... INTO` | `TryFastPlan` | nil |
| `WHERE (a, b) IN (...)` / `a IN (...)` 且无 ORDER BY / GROUP BY / LIMIT / HAVING / DISTINCT / 窗口、单表 | `tryWhereIn2BatchPointGet` | BatchPointGet（Rust：`blocked-by:D10.8`） |
| HAVING、ORDER BY；LIMIT 的 count 为 0、offset 大于 0 或不是非负整数 | `tryPointGetPlan` | nil |
| 不是单表（JOIN、多表、子查询作数据源） | `getSingleTableNameAndAlias` | nil |
| 表有生成列、有非 public 的列 | `tryPointGetPlan` | nil |
| 选择列表不是列名 / `*` / `tidb_row_checksum()`，或表名限定不符 | `buildSchemaFromFields` | nil |
| WHERE 不是 `AND` 连接的 `列 = 常量`；常量为 NULL（`a = NULL` 不是点查）；视图；列名限定不符 | `getNameValuePairs` | nil |
| 字符串列配数值常量、BIT 列配字符串常量 | `checkCanConvertInPointGet` | nil |
| 常量转成列类型时出错（截断除外）或转换前后不等（按列的 collation 比较） | `getNameValuePairs` | nil |
| 转换溢出 | `getNameValuePairs` | TableDual（Rust：`blocked-by:D10.1`） |
| 分区表上的 `_tidb_rowid` | `getNameValuePairs` | nil |
| 条件里有 handle 列但条件不止一个，或 handle 被 hint 排除 | `tryPointGetPlan` | nil |
| 索引非唯一、非 public、不可见（且未开 `tidb_optimizer_use_invisible_indexes`）、MV 索引、带条件的部分索引、被 hint 排除 | `checkTblIndexForPointPlan` | 跳过该索引 |
| 全局索引所在表正在增删分区（`AddingDefinitions` / `NewPartitionIDs` / `DroppingDefinitions`） | `checkTblIndexForPointPlan` | 跳过该索引 |
| 条件列与索引列不完全一致、索引有前缀列 | `getIndexValues` | 跳过该索引 |
| FOR UPDATE 或 READ COMMITTED 且有连接 ID：用最新 schema 再查一次索引 | `checkTblIndexForPointPlan` | Rust：`blocked-by:D06.5`（domain） |
| 输出名与 unfold 长度不一致 | `getVisitInfo` | nil |
| 权限检查失败、表锁检查失败 | `checkFastPlanPrivilege` | nil（tidb-rs 没有权限管理器，D04.1；表锁检查只在 `enable-table-lock` 打开时做，那时是 `blocked-by:X`） |
| 内存库（information_schema 等） | `TryFastPlan` | nil |
| `sql_select_limit` 已设置 | `TryFastPlan` 的 defer | nil + warning |

不在 `TryFastPlan` 中的条件：
- **`tidb_isolation_read_engines` 不含 TiKV 时不走快速路径**：这个检查在 `planner.Optimize`（`optimize.go:244`），不在 `TryFastPlan`。Rust 的 `Optimize` 目前是 VS1-10 的桩（`blocked-by:D13.3`），VS1-24 移植它时连同这个检查一起移植；`optimize.rs` 的模块注释已写明。
- **分区表**：`TryFastPlan` 本身不拒绝分区表（Go 生成点查，`PartitionIdx` 为空），裁剪在执行器建立时（`PrunePartitions`）。Rust 中按值裁剪是 `blocked-by:D06.4`（分区表对象是 D06.4 第 2 部分），全局索引和 `_tidb_rowid` + 显式单分区两个分支已移植。
- **hint**：`USE / IGNORE / FORCE INDEX (...)` 已移植并与 Go 一致；注释形式的 `/*+ USE_INDEX(...) */` 在 Rust AST 中还是未移植的节点（`ast::TableOptimizerHint`），有这种 hint 的语句是 `blocked-by:D02.2`。

## 4. 边界桩

| 位置 | 情况 | 处理 |
|---|---|---|
| `TryFastPlan` | 条件不可能成立时的 `PhysicalTableDual` | `blocked-by:D10.1`（物理算子，VS1-27）；`sql_select_limit` 已设置时与 Go 一样不出计划 |
| `TryFastPlan` | `tidb_enable_optimize_trace`：`SetFastPlan(p.BuildPlanTrace())` 要 `ExplainInfo` | `blocked-by:D10.9` |
| `TryFastPlan` | UPDATE / DELETE（`tryUpdatePointPlan` / `tryDeletePointPlan`） | `blocked-by:X`（写路径，永久） |
| `tryWhereIn2BatchPointGet` | 通过前置检查后建 `BatchPointGetPlan` | `blocked-by:D10.8`（VS1 不做） |
| `getNameValuePairs`、`getUintFromNode` | 参数标记（`?`）的值 | `blocked-by:D13.1`（Rust AST 的参数标记不带值，EXECUTE 绑定的值归 VS1-15） |
| `indexIsAvailableByHints` | 注释形式的 hint、AST 未建模的 index hint | `blocked-by:D02.2` |
| `checkTblIndexForPointPlan` | `GetLatestIndexInfo` | `blocked-by:D06.5` |
| `getLockWaitTime` | `PessimisticLockEligible` 在 `pessimistic-auto-commit` 打开时要读 `BulkDMLEnabled` | `blocked-by:X`（D05.2 把这个字段划给写路径） |
| `CheckTableLock` | `enable-table-lock` 打开时的表锁检查 | `blocked-by:X` |
| `ExplainInfo` / `ExplainNormalizedInfo` | `AccessObject()` | `blocked-by:D10.9`（trait 返回 `String`，桩 panic 带 blocked 错误文本） |
| `ResolveIndices` | schema 有虚拟列（`resolveIndicesForVirtualColumn`） | `blocked-by:D10.9`；快速路径拒绝有生成列的表，到不了 |
| `LoadTableStats` | `loadTableStats` | `blocked-by:D10.2` |
| `PrunePartitions` | 按值裁剪 | `blocked-by:D06.4` |
| `GetAvgRowSize`（`plan_cost_ver1.go`） | `tidb_replica_read=closest-adaptive` 的行大小 | `blocked-by:D10.5` |
| `getHashOrKeyPartitionColumnName` | 分区表达式（只有 BatchPointGet 用） | `blocked-by:D06.4` |
| `isExprHasSubQuery`、`subQueryChecker` | 只有点更新 / 点删除用 | `blocked-by:X` |

## 5. 与 Go 的其他差异

1. **用户名。** `TryFastPlan` 用 `SessionVars.User` 的用户名和主机拼权限错误；Rust 的会话变量没有 `User`（D05.2），权限错误只在有权限管理器时才返回（tidb-rs 没有，D04.1），所以传空用户。
2. **`StatsInfo()` 的惰性默认值。** Go 在没有统计信息时设成 `{RowCount: 1}`；`newPointGetPlan` 总会设置，Rust 的读取是 `&self`，不做惰性设置。
3. **`MemoryUsage`。** `emptyPointGetPlanSize` 是 Go 64 位上的 `unsafe.Sizeof(PointGetPlan{})`（528，govec 校准）。`FieldType.MemoryUsage` 计 `cap(Elems)`：Go 的元数据由 `encoding/json` 解码，切片按它的增长规则分配（3 个元素容量为 4），Rust 的 `Vec` 克隆后容量等于长度（D02.3 已记录的差异）。govec 回放按这个容量差调整后比较，其余逐字节相同。
4. **`ExplainInfo`** 是桩（§4），所以 `BuildPlanTrace` 和 EXPLAIN 文本要等 D10.9。

## 6. 唯一索引点查与 schema（评审要求）

- **NULL 不是点查。** `getNameValuePairs` 遇到 NULL 常量返回 nil（`a = NULL`、多列索引中有一列为 NULL）；`IS NULL` 不是等值条件。govec `fast` 有这几种语句。
- **new collation。** 字符串常量先设成列的 collation（`getNameValuePairs`），比较用列的 collator；key 由 `codec.EncodeKey` 编码，字符串按 collation 的 sort key 编码。`uidx` 记录约四分之三开 new collation、四分之一关，列有 `utf8mb4_bin` / `general_ci` / `unicode_ci` / `0900_ai_ci` / `binary`，key 逐字节比较。
- **restored data。** 开 new collation 时，非 binary collation 的字符串列在索引值中带原值（restored data，在 handle 之后）；点查只从索引值解 handle（`decode_handle_in_index_value`），`uidx` 的索引值按 Go 写入时的格式带上 restored data（486 条），执行器必须跳过它读出正确的 handle。
- **全局索引。** 分区 ID 从索引值中解出（`SplitIndexValue`），行在该分区读；语句显式选了分区而该分区不在其中时不出行（`matchPartitionNames`）。`uidx` 有 328 条全局索引记录。
- **ErrNotExist。** 值转成列类型时溢出、截断、过长，或枚举名不存在，值不可能在索引中：`ErrNotExist`，执行器不读任何 key 直接返回空。
- **不一致。** 索引有、行没有：`ReportLookupInconsistent` 的错误文本与 Go 相同（每条非聚簇主键的 `uidx` 记录都回放一次）。
- **schema 与 UniqueID。** `TryFastPlan` 把 `PlanID`、`PlanColumnID` 清零；计划 ID 由 `newPointGetPlan` 分配（为 1）；schema 各列的 `UniqueID` 是列的 offset（`colInfoToColumn`），行 checksum 列是 `ExtraRowChecksumID`，`PlanColumnID` 不变（为 0）。govec 逐条比较计划 ID、每列的 ID / UniqueID / Index / 类型和最终的 `PlanColumnID`，D10.6 的估算依赖这些值。

## 7. 测试

- **govec 领域 `fastplan`**（tracker `govec/domains/fastplan.go`，5370 条）。
  - `fast`（2199 条，`crates/planner/tests/govec_fastplan.rs`）：语句经 Go parser、preprocess（mock info schema）后交给 `TryFastPlan`，比较是否出计划及计划的全部字段与 warning。Rust 的 preprocess 还不能解析基表（D06.3 第 2 部分），回放时自己把 FROM 中唯一的表名解析到记录的表并补上库名（Go 的 preprocess 也这样做）。Go 解析或 preprocess 失败的 218 条跳过。结果：1811 条与 Go 相同（其中 477 条出点查），170 条停在边界桩，桩的条数按块和 Go 的结果固定在测试中：D02.2（注释 hint）30、D06.5 26、D10.1（TableDual）62、D10.8（BatchPointGet）14、D10.9（trace）38。
  - `uidx`（3171 条，`crates/executor/tests/govec_fastplan_uidx.rs`）：先直接比较两个编码函数的 key、错误、转换后的值和 warning；Go 能建出索引值和行的 2596 条再经完整的点查执行器回放：执行器读的 key 必须依次是 Go 的索引 key 和行 key，返回的行与 Go 解码的相同；去掉行后必须报 Go 的不一致错误。
  - CI：arm64 job 回放 arm64 向量；`govec_fastplan_uidx` 也在 `TZ=America/Sao_Paulo` 下运行。
- **单测**（`core/point_get_plan/tests.rs`）：`OperatorInfo`（无符号 handle、加锁、redact 的 ON / MARKER）、`PrunePartitions` 各分支、`isInExplicitPartitions`、`Clone` 的错误、各桩、点更新 / 点删除的 handle 列（`buildHandleCols`）、`getPointGetValue`、UPDATE / DELETE 停在 X。执行器单测（`point_get/tests.rs`）新增唯一索引点查：读到的 key、行、无索引项、不一致错误。

## 8. 可能的上游问题

无。
