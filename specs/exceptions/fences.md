# 围栏清单（例外与 X 类）

状态：初版（#262，2026-11-21）。格式见 [模板](../templates/fence.md)。这里只登记**今天从 SQL
能走到**的永久围栏，也就是例外或"不移植"（X）决定让用户看到"不支持"的地方。临时的
`blocked-by:<块>` 桩不在这里（tracker stubs.tsv，数量只降不升：`scripts/check_stub_ratchet.py`）。
行为不同而不是拒绝的差异写在偏差台账 `docs/testing/rulings.toml`。

tidb-rs 现在是只读计算节点：写路径、DDL、ANALYZE、导入、备份等由集群中的 Go TiDB 承担
（ROADMAP "不移植（X）"、宪章第四条 3 的"委托"类）。这些语句的 SQL 入口今天在原型的语句降级
（`crates/sql`）和原型会话（`crates/poc/session`）里报 1235。它们的固定文案登记在
`tools/ab/prototype-not-supported.txt`，所以对照测试中按容差 T1 计为 unsupported。

## F1 写语句与 DDL（委托给 Go TiDB）

- **边界**：所有写入和集群管理语句：`INSERT`、`REPLACE`、`UPDATE`、`DELETE`、`LOAD DATA`、
  `IMPORT INTO`、DDL（`CREATE` / `DROP` / `ALTER` / `TRUNCATE` / `RENAME` 表、库、索引）、
  `ANALYZE`、`ADMIN`、`SPLIT`、`BACKUP` / `RESTORE`、`GRANT` / `CREATE USER`、`LOCK TABLES`、`SAVEPOINT`
  等。也就是解析器给出但原型没有降级的语句（`ast::Stmt::Unported`）。`SELECT`、`SET`、`USE`、
  `SHOW WARNINGS` 等读语句和会话语句放行。
- **理由**：ROADMAP "不移植（X）"：写路径、DDL、ANALYZE、导入、BRIE 由 Go TiDB 承担（D09.15、
  D09.17、D11.3、D12.14 等 X 块）；宪章第四条 3 "委托"：SQL 入口报明确的错误。
- **围栏位置**：`crates/sql/src/ir.rs:326`（`ast::Stmt::Unported` → `unsupported`）。
- **用户看到的确切错误**：`ERROR 1235 (42000): This version of TiDB doesn't yet support
  '<语句节点> statements (read-only prototype)'`，例如 `INSERT` 为
  `'InsertIntoStmt#0 statements (read-only prototype)'`，`ALTER TABLE` 为
  `'AlterTableStmt#0 statements (read-only prototype)'`。消息中带解析器的节点名，这是原型的写法，
  与 Go 没有对应。
- **重开条件**：负责人决定 tidb-rs 承担写路径（结束只读阶段）；或原型会话被移植的会话层
  （D05.3）取代时，按 N11 D8 的决定改用统一的写语句错误码。
- **测试**：`crates/sql/tests/fences.rs::writes_and_ddl_answer_1235`（21 种语句，逐字比对消息）。

## F2 显式事务（只读节点只有 autocommit）

- **边界**：`BEGIN`、`START TRANSACTION`、`COMMIT`、`ROLLBACK`、`SET autocommit = 0`（`OFF`、
  `'false'` 等同）。`SET autocommit = 1` / `DEFAULT` 放行，非布尔值按 MySQL 报 1231。
- **理由**：只读节点不持有事务状态，每条语句在 autocommit 下执行；事务中的写和加锁读属于写路径
  （X）。显式只读事务的语法和错误码等例外 N11 D8 定稿（草案）。
- **围栏位置**：`crates/poc/session/src/lib.rs:350`（`txn_fence`，由 `run` 在 `:483` 调用）、
  `:361`（`autocommit_fence`，由 `set` 在 `:731` 调用），错误由 `:340` 的 `not_in_txn` 构造。
- **用户看到的确切错误**：`ERROR 1235 (42000): This version of TiDB doesn't yet support
  '<语句>: explicit transactions are not supported by the read-only node (every statement runs in
  autocommit mode)'`，`<语句>` 为 `BEGIN/START TRANSACTION`、`COMMIT`、`ROLLBACK` 或 `autocommit=0`。
- **重开条件**：N11 D8 定稿（显式只读事务）；或会话层 D05.3 取代原型会话。
- **测试**：`crates/poc/session/src/lib.rs::explicit_transactions_answer_1235`、
  `::autocommit_off_answers_1235`（模块 `fence_tests`）。

## F3 ROLLBACK TO SAVEPOINT（写路径）

- **边界**：`ROLLBACK TO [SAVEPOINT] <名字>`。`SAVEPOINT <名字>` 本身由 F1 挡住。
- **理由**：savepoint 是事务内写缓冲的检查点，属于写路径（X；D05.2 plan 第 237 行建议 Savepoint 归 X）。
- **围栏位置**：`crates/sql/src/ir.rs:304`。
- **用户看到的确切错误**：`ERROR 1235 (42000): This version of TiDB doesn't yet support 'ROLLBACK TO SAVEPOINT'`。
- **重开条件**：同 F2。
- **测试**：`crates/sql/tests/fences.rs::rollback_to_savepoint_answers_1235`。

## 其余条目（今天从 SQL 走不到围栏，或还没有围栏）

下面的例外"原则同意"或 spec 还在草案阶段。tidb-rs 今天照 Go 移植（新栈）或由原型的通用
"不支持"挡住（原型），**还没有专门的围栏代码**。定稿实现时按模板补五段和测试：

| 例外 / X | 计划中的用户可见行为 | 现状 |
|---|---|---|
| B1 静态分区裁剪 | `tidb_partition_prune_mode = 'static'` 在使用该取值的语句上报错（N8 §3.3） | 新栈照 Go 接受（`ss_tidb_partition_prune_mode`），没有围栏；原型对该 SET 报通用的 `setting system variable '…'` |
| B2 v1 统计信息 | 只读 v2，v1 表按无统计处理 | 不报错，无围栏（行为差异，定稿时进台账） |
| B3 / B4 cascades 优化器 | 不移植；设置 `tidb_enable_cascades_planner` 的处理待 spec | 新栈照 Go 接受该变量；`go-exception: EXC.B3` 标注的函数不移植 |
| B5 代价模型 v1 | `tidb_cost_model_version = 1` 在使用时报错 | 新栈照 Go 接受，没有围栏 |
| D1 TiFlash / MPP | 只读阶段不做，边界桩隔开 | 读 TiFlash 副本的路径由 `blocked-by` 桩和原型的通用"不支持"挡住 |
| N11 D8 / D10 | 显式只读事务的语法与错误码；`tidb_replica_read` 非 leader 值报错 | 草案；现状见 F2 和 `docs/compatibility.md` "TiKV client gaps" |
| X：`SET GLOBAL`（原型会话） | 原型会话不支持 | 临时缺口（D05.3 之前），不是永久围栏：`crates/poc/session/src/lib.rs:728` |
