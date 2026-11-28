# D05.4 事务上下文（spec）

状态：第 1 部分（只读路径中 VS1 需要的部分）进行中（2026-11-14）。块：D05.4（ROADMAP P2，#49），新建 crate `tidb-sessiontxn`（N0 rank 11，`layers.toml` 去掉 planned）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) VS1-14、§9；[N11](../exceptions/N11-cluster-readts/spec.md) §4.2、§4.4、§4.5 与 D8 / D11（#146）；[ADR 0005](../../docs/adr/0005-schema-sync.md)；[N2](../exceptions/N2-session-context/spec.md)。

## 1. 范围

Go 范围：`pkg/sessiontxn`（`interface.go`、`future.go`、`failpoint.go`、`internal`、`isolation`、`isolation/metrics`、`staleread`），另有 tracker 划给本块的 `pkg/session/txninfo`。共 157 个函数。

### 1.1 第 1 部分（本 PR，VS1-14 的 sessiontxn 部分）

| Go | Rust | 内容 |
|---|---|---|
| `interface.go`、`future.go` | `lib.rs` | `EnterNewTxnType`、`EnterNewTxnRequest`、`StmtErrorAction`、`ErrorAction` / `RetryReady` / `NoIdea`、`ConstantFuture`、`TxnContextProvider` / `TxnManager`（读取部分）、`AdviseOptimizeWithPlanAndThenWarmUp`、`NewTxn`、`NewTxnInStmt` |
| `internal/txn.go` | `internal.rs` | `CommitBeforeEnterNewTxn`、`GetSnapshotWithTS` |
| `isolation/base.go` | `isolation/base.rs` | `OnInitialize`（三种进入方式）、`ActivateTxn`、`prepareTxn` / `prepareTxnWithOracleTS` / `replaceTxnTsFuture`（TSO 预取：规划前 `AdviseWarmup` 发出、`ActivateTxn` 等待）、`GetStmtReadTS`、`GetSnapshotWithStmtReadTS` / `getSnapshotByTS`、`SetOptionsOnTxnActive`（读取相关选项）、`newOracleFuture`（含 `tidb_low_resolution_tso`）、`canReuseTxnWhenExplicitBegin`、各钩子 |
| `isolation/optimistic.go` | `isolation/optimistic.rs` | autocommit 读、点查 MaxTS（`AdviseOptimizeWithPlan`） |
| `isolation/repeatable_read.go` | `isolation/repeatable_read.rs` | 悲观 RR：读用事务 start ts；`notNeedGetLatestTSFromPD` |
| `isolation/readcommitted.go` | `isolation/readcommitted.rs` | 悲观 RC：每条语句一个 ts（预取、`getStmtTS`）、`NeedSetRCCheckTSFlag`、`planSkipGetTsoFromPD` |
| `isolation/serializable.go` | `isolation/serializable.rs` | 构造与错误钩子 |
| `staleread/util.go`、`provider.go` | `staleread/` | `IsStmtStaleness`；staleness provider 的骨架（状态、`GetStmtReadTS` 等取值方法；进入事务是桩） |

converge：D05.4 0/157 → 75/157。

### 1.2 永久豁免：写路径

tidb-rs 是只读节点；显式事务里的写语句和加锁读由会话拒绝（N11 D8，1792，D05.3 / VS1-14）。不移植：RR 的 for-update ts（`getForUpdateTs`、`updateForUpdateTS`）、悲观锁错误处理（`handleAfterPessimisticLockError`）、fair locking、`SetOptionsBeforeCommit`、`SetTxnAssertionLevel`、乐观重试（`onTxnActive` / `isOptimisticTxnRetryable` 设置的 `CouldRetry`）、`temporaryTableKVFilter`、RC 的写检查（`GetSnapshotWithStmtForUpdateTS` 的 RCCheckTS、`IsCheckTSInWriteStmtMode`）。`SetOptionsOnTxnActive` 中只用于提交的选项（commit hook、async commit / 1PC、disk full、`GuaranteeLinearizability`、后台 goroutine 钩子）同理。

### 1.3 第 2 部分

见 [tasks.md](tasks.md)。

## 2. 承载方式

- **会话、存储、规划器是 trait**（`sctx.rs`，glue）：Go 的 sessiontxn 持有 `sessionctx.Context`（会话，D05.3，rank 15）、调用 `kv.Storage.GetOracle()`（D07.1）、对规划器的值做类型断言（`planner/core`，rank 13）。它们都在本 crate 之上，所以写成 `SessionCtx`、`LazyTxn`、`Oracle`、`PlanInfo`（`plan_kind()` 枚举 + `IsPointGetWithPKOrUniqueKeyByAutoCommit`）、`StmtInfo`（`IsReadOnly`），由上层实现。每个方法对应一次 Go 调用。
- **会话作为参数传入**，不保存在 provider 中（N2：会话拥有 txnManager，txnManager 拥有 provider）。
- **provider 是一个结构体**：Go 用嵌入 `baseTxnContextProvider` 并设置函数字段的方式构造各隔离级别的 provider；这里是 `BaseTxnContextProvider` + `Kind` 枚举（各隔离级别的状态），函数字段改为 `match`（L2，同 M-SELF 的思路）。Go 的类型名保留为类型别名。
- **等待 TSO 的方法是 async**（N3）：`ActivateTxn`、`GetStmtReadTS`、`GetSnapshotWithStmtReadTS`、`OnInitialize`。TSO future 是 `BoxFuture`；RC 的语句 future 是枚举（`UseStartTs` 对应 Go 的 `funcFuture(p.getTxnStartTS)`、`Constant`、`Oracle`）。
- **`TransactionContext`** 加 `txn_scope`、`info_schema`（`TxnInfoSchema`：`Arc<dyn Any>`，因为 InfoSchema 的类型在更高的 crate）。

## 3. 读时间戳与 N11

- **TSO 预取**：照 Go，`AdviseWarmup`（规划前）发出 TSO 请求，`ActivateTxn`（首次需要读 ts）等待；点查在 `AdviseOptimizeWithPlan` 中改用 MaxTS，不取 TSO。
- **`ReadTsSource`**（N11 §4.4）：provider 在语句取得读 ts 之后给出它的来源，以及本语句发出的 TSO 请求数（`stmt_tso_requests`，从 `OnStmtStart` 起计）。schema lease 检查（N11 §4.2、ADR 0005、#145）在语句准入时进行，只接受本语句在准入之后得到的"当前"，不接受事务的 start ts。各来源的准入规则（由会话 / domain 实现，VS1-14、VS1-17）：

  | 来源 | 用的 schema | lease 检查用的"当前" | TSO 请求 |
  |---|---|---|---|
  | `Tso` | 同步的 | 本语句自己的新 TSO | 1 |
  | `LowResolution` | 同步的 | 低精度 ts 加它的最大年龄（保守上界） | 0（低精度 oracle） |
  | `MaxTs` | 同步的 | validator 的本地时钟上界：从同步的 TSO 请求发出时起的单调时钟，在 etcd lease 之内 | 0 |
  | `TxnStart` | 同步的（事务的） | 同 `MaxTs`，不用 start ts | 0 |
  | `Stale` | 该 ts 上的 InfoSchema（不持有 MDL） | 不检查（不用同步的 schema） | 0 |
  | `Snapshot` | 该 ts 上的 InfoSchema（`tidb_snapshot`） | 不检查 | 0 |

  govec `sessiontxn` 的 `read_ts`（随机语句序列，记录每次读的来源与 TSO 请求数）是它的对照（#219 评审）。
- **显式只读事务**（N11 D8，已确认）：`EnterNewTxnWithBeginStmt` 照 Go 进入（RR / optimistic 用一个 start ts，RC 每条语句一个 ts）。`READ ONLY` 的接受与写语句 1792 由会话负责（D05.3 / VS1-14）。
- **`tidb_rc_read_check_ts`**（N11 D11，**暂定**，待负责人在 #146 决定）：Go 在事务内的只读语句上设置 `RCCheckTS`，复用上一个 ts 并让 TiKV 检查写冲突。client-rust 支持之前，该语句返回 TiDB 的 1235（`ErrNotSupportedYet`）。它是真正的错误，**不是** `blocked` 边界桩：路由器不能回退到 PoC（PoC 会只复用 ts 而不检查）。路由器测试 `rc_read_check_ts_is_not_a_fallback`（`crates/poc/session`）固定这一点。

## 3a. 元数据锁（MDL）与读路径（照 Go v8.5.8；Go 本身的缺口见 #252，标签 `go-gap`）

负责人决定（#146 第一部分，2026-11-15）：新栈的元数据锁、lease、MaxTS 三项都**照 Go**。Go 的做法在只读节点上留下的缺口记录在 #252（`go-gap`），由负责人与内核团队讨论；代码注释同样引用 #252。

**先说几个词**
- **元数据锁（MDL）**：不是一把会让语句等待的锁，而是每个会话自己的一张登记表"本会话正在用表 t 的第 v 版结构"（Go `SessionVars.GetRelatedTableForMDL()`，表 ID → schema 版本）。DDL 每推进一步，都等所有节点上已经没有会话登记更旧的版本。它只让 DDL 等，从不让 SELECT 等。
- **登记 / 清除**：`tryLockMDLAndUpdateSchemaIfNecessary`（预处理器查到表之后调用，`planner/core/preprocess.rs`）写登记；事务结束时 `TxnCtx.Cleanup` 清掉整张表。
- **上报**：Go 的 domain 每 50 ms 对本节点所有会话调用 `RemoveLockDDLJobs`，只有没有会话还在用旧版本时，才向 etcd 报告"本节点已到新版本"。

**谁登记（照 Go）**

| 语句 | 登记吗 | Go 依据 |
|---|---|---|
| `PREPARE` | **登记**，并改用 domain 最新的表定义；`PrepareStmt` 结束时清除（`rollbackOnError` → `RollbackTxn`），失败时同样清除 | 新建的语句上下文 `IsReadOnly` 为 false，不满足"autocommit 只读就跳过"（`preprocess.go:2101`、`session.go:2547`） |
| `EXECUTE`、autocommit 的普通 SELECT | **不登记** | autocommit、不在事务中、`IsReadOnly` 为 true，直接返回 |
| DML、显式事务里的语句 | 登记，直到事务结束 | 只读节点上这些语句目前由路由器交给 PoC；会话侧的结构已按 Go 实现，不加新的路由 |

跳过的条件按 Go 的顺序：`TxnCtx.EnableMDL` 关（事务开始时取自 `tidb_enable_metadata_lock`）、InfoSchema 版本为 0、`tidb_snapshot`、stale read、本地临时表（不登记也不加锁）、全局临时表（Go 的 `skipLock`，查 `MdlTables`）、autocommit 且只读。

**写入顺序**：先写 0，再取 domain 最新的 InfoSchema，再写它的版本（Go 的注释：否则后台检查可能在 domain 版本变化之前看到本会话"没有登记"而误报）。Go 的 `defer` 三件事照搬：表不是 public → `ErrTableNotExists`（用换过的表）；成功时（包括跳过的分支）记入语句的 `RelatedTableIDs`；出错时按已登记的表 ID 删除登记。

**表定义变了（`Revision` 不同，且不是悲观 RC 事务）**：domain 的表里新变成 public、而事务的表里没有的索引，在一份**复制**的表定义里标成 `WriteReorganization`（不用于读；共享的定义不改，N13）；public 列的 ID 变了 → `ErrInfoSchemaChanged`（8028，文案照 Go：标准文案加 ". [try again later]"）。

**登记存放在哪里**：每个会话自己的事务上下文（`TransactionContext.related_table_for_mdl`，`Arc<parking_lot::Mutex<HashMap>>`）。读路径上没有全局锁；这张表会被上报跨连接读取（Go `sync.Map`），所以用 `parking_lot` 锁（M-LOCK 规则）。

**与 Go 的差异（边界桩，代码中已标注）**
1. **上报未接到 etcd**（`tidb_session::session::report_metadata_locks`，`blocked-by:D06.5`，即"domain 的 MDL 检查循环尚未移植"）：`RemoveLockDDLJobs` 已照 Go 移植（`remove_lock_ddl_jobs`），但没有调用者。本节点向集群的上报仍由原型路径（`crates/poc/domain`，根据它自己的准入登记）完成，它看不到新栈会话的登记。后果：**DDL 不会等新栈上正在执行的 `PREPARE`**（比 Go 宽松；PREPARE 中只用一个 InfoSchema 版本，路由器又比较它与 PoC 的版本）。在上报接上之前，路由器**不得**把 DML 或显式事务交给新栈（VS1 tasks 的完成条件），因为它们的登记跨语句存在。
2. `domain.GetDomain(sctx)`：预处理器没有会话，调用方用 `with_domain_info_schema` 传入会话的 domain InfoSchema；没有时，需要加锁的语句停在 `blocked-by:D06.5`。
3. `SessionExtendedInfoSchema.UpdateTableInfo`（让事务后续的查找看到更新的表）未移植（D06.3 第 2 部分，"会话扩展的 InfoSchema"）：domain 的表与事务 InfoSchema 的表不同时，语句停在 `blocked-by:D06.3`，由 PoC 回答。因此新栈**永远不会**用比事务 InfoSchema 更新的表定义规划；路由器在 PREPARE 前已要求新栈与 PoC 的 schema 版本相同，所以"两边都持有"的预处理语句的列检查（`check_columns`）不会因为这里换表而不一致（Lucky 评审第 4 点）。全局临时表同样停在 `blocked-by:D06.3`。
4. `InitMDLVariable`（启动时从 meta 读 `MetadataLock` 键）：新栈启动时假设为 Go 的默认值"开"（`init_mdl_variable`，`blocked-by:D06.5`）；`SET GLOBAL tidb_enable_metadata_lock` 由 PoC 执行。
5. `table.TableFromMeta` / autoid 分配器：规划只用表的元数据，复制的表定义就是元数据。

**lease**：照 Go，新栈的读语句**不检查** schema lease（Go 只在写事务提交时检查，`session.go:510-513`；#252）。§3 表格中"lease 检查用的当前"一列描述的是 PoC 的准入，新栈不做。

**MaxTS**：照 Go，autocommit 点查若不需要二次读取，读在 MaxTS（`IsPointGetWithPKOrUniqueKeyByAutoCommit`，规划器已有移植，会话经 `AdviseOptimizeWithPlanAndThenWarmUp` 接入），0 次 TSO 请求。注意 Go 的名字容易误解：**整数主键**和**聚簇表的主键**用 MaxTS；**普通唯一索引**要回表（二次读取），Go 不用 MaxTS，取 TSO（1 次请求）。

**现在停在哪里**（真实新会话 + sbtest InfoSchema + 模拟存储）：`PREPARE s FROM 'SELECT c FROM sbtest1 WHERE id=?'` 通过元数据锁与表的解析（`resolveCtx.AddTableName`），停在**规划器**的 `buildSelect`（`blocked-by:D09.1`，计划构建器）。路由器因此把该语句交给 PoC，`EXECUTE` 也由 PoC 执行；新会话的 `ExecutePreparedStmt` 已照 Go 把语句包成 EXECUTE 节点交给 `ExecuteStmt`，停在编译器里规划器的入口（`blocked-by:D09.13`）。所以用模拟存储还不能返回行。

**测试**：`crates/session/src/tests.rs`（PREPARE 登记并在结束时清除，失败也清除；autocommit 读不登记、显式事务登记并按 Go 上报；整数主键 / 聚簇主键点查 MaxTS、0 次 TSO，普通唯一索引 1 次 TSO；PREPARE 停在 D09.1），`crates/planner/src/core/plan_cache_tests.rs`（跳过条件、写 0 再写版本、出错删除、非 public、表定义变化）。govec：`preprocess` 原先停在元数据锁桩的 9 条记录现在与 Go 输出一致（92 条一致，原 83）；元数据锁本身的行为依赖会话内部状态与 domain，Go 的输出（登记表）不在 SQL 结果中，没有可直接对照的 Go 输出，用上面的场景测试代替。

## 4. 与 Go 的差异（均已在代码中标注）

1. `GetTxnInfoSchema` 不返回 `SnapshotInfoschema`（该会话字段随 D05.2 / D06.5；`tidb_snapshot` 语句由 VS1 路由到 PoC），也不包 `SessionExtendedInfoSchema`（D06.3 第 2 部分 T9）。
2. `TransactionContext` 不记录 `CreateTime`（指标与慢日志，D14.x）；`History` 只用于写重试，恒为 nil。
3. `RetryInfo.Retrying`（写重试）恒为 false。
4. 临时表：`OnLocalTemporaryTableCreated` 与快照 interceptor 是 `blocked-by:D06.4` 的桩（`tidb_base::blocked`）。
5. RR provider 不保存 for-update 状态（写路径）；它的 `GetStmtForUpdateTS` 返回 1792（N11 D8，事务内的写与加锁读）。

## 5. 测试

`crates/sessiontxn/tests/providers.rs`：模拟会话、存储、TSO，按 Go `isolation/*_test.go` 的场景覆盖 autocommit 读与 TSO 预取、点查 MaxTS、RR 显式事务一个 ts、RC 每条语句一个 ts（BEGIN 后与 lazy 事务两种）、`tidb_snapshot`、`autocommit=0` 与低精度 TSO、D11 的拒绝、staleness 骨架。Go 的这些测试依赖 testkit / store，不能直接移植；SQL 级对照随 VS1-14。

## 6. 显式只读事务与 `autocommit=0`（原型路径，#547）

状态：本节随 #547 实现（2026-11-28）。

**问题**：pymysql 的默认设置和 MySQL Connector/Python（纯 Python 实现）在连接时发 `SET AUTOCOMMIT=0`，tidb-rs 原来回 1235（围栏 F2），默认设置下连接失败；Connector/Python 的 C 扩展忽略这个错误，会话实际仍是 autocommit，驱动却报告 `autocommit=False`，两边静默不一致。`BEGIN` 也是 1235。只读节点应当支持只读的事务。

### 6.1 Go v8.5.8 的行为（照搬）

| 项 | Go 的做法 | Go 依据 |
|---|---|---|
| `autocommit=0` 的事务何时开始 | 每条语句开始时 `PrepareTxnCtx` 进入一个"延迟"事务（`EnterNewTxnBeforeStmt`，不取 ts）；第一条**需要读 ts** 的语句激活它时才取 start ts，并在激活时置 `InTxn`。`SELECT 1`、`SET`、`USE` 不取 ts，不开始事务（语句结束时 pending 的事务被丢弃） | `pkg/session/session.go:4190-4212`；`pkg/sessiontxn/isolation/base.go:303-305`；`pkg/session/tidb.go:270-280` |
| 延迟事务的模式 | `tidb_txn_mode = pessimistic` 时悲观，否则乐观；autocommit 的语句总是乐观（`pessimistic-auto-commit` 配置默认关） | `session.go:4196-4202` |
| `BEGIN` / `START TRANSACTION [WITH CONSISTENT SNAPSHOT]` | 先提交当前事务（只读事务的提交什么也不做），再进入新事务并**在 BEGIN 时取 start ts**（`activeNow`），置 `InTxn`。`BEGIN PESSIMISTIC` / `OPTIMISTIC` 覆盖 `tidb_txn_mode`。`WITH CONSISTENT SNAPSHOT` 与 `BEGIN` 相同；`WITH CAUSAL CONSISTENCY ONLY` 只影响提交 | `pkg/executor/simple.go:618-643`；`base.go:76-139`；`canReuseTxnWhenExplicitBegin`（`base.go`）；`pkg/session/txnmanager.go:155-173` |
| 读 ts（provider 的选择） | 乐观：所有语句读 start ts；悲观按 `IsolationLevelForNewTxn`：`READ-COMMITTED` 每条语句一个新 ts（延迟事务中激活它的那条语句读 start ts 本身，BEGIN 之后的第一条语句就取新 ts），`SERIALIZABLE` 和其余级别读 start ts | `txnmanager.go` `newProviderWithRequest`；`isolation/optimistic.go:46`、`repeatable_read.go:66`、`serializable.go:50`；`readcommitted.go:103`、`:142-158` |
| MaxTS | 事务内（`InTxn` 或 autocommit 关）不用 MaxTS | `IsPointGetWithPKOrUniqueKeyByAutoCommit` / `IsAutoCommitTxn`（`pkg/planner/core/common_plans.go:1450-1453`） |
| `COMMIT` / `ROLLBACK` | `SetInTxn(false)`，语句结束时 `autoCommitAfterStmt` 提交；只读事务的提交直接返回。不在事务中时什么也不做、回 OK。`COMMIT AND CHAIN` / `RELEASE` 与 `COMMIT` 相同（执行器不读 `CompletionType`） | `simple.go:778-815`；`tidb.go:310-334`；`session.go:500-513` |
| `SET autocommit = 1` | 从关变为开时隐式提交当前事务（钩子置 `InTxn = false`，语句结束时提交）。`SET autocommit = 0` 不开始事务 | `pkg/sessionctx/variable/sysvar.go:1848-1855`；`tidb.go:325-334` |
| 事务内语句出错 | 事务继续（只在 `!InTxn` 时回滚） | `tidb.go:310-322` |
| 状态标志 | `InTxn` 即 OK / EOF 包的 `SERVER_STATUS_IN_TRANS` | `pkg/sessionctx/variable/session.go:2628-2640` |
| 指标 | 提交时 `tidb_session_transaction_duration_seconds{txn_mode, type="commit"}`（从事务上下文创建时起），`ROLLBACK` 时 `{txn_mode, type="rollback"}`；都只在事务已激活（`txn.Valid()`）时记录。结束事务的语句（`SET autocommit = 1` 后面的赋值）失败时回滚，记 `type="abort"`。命令开始时的空闲时间按状态里的 `SERVER_STATUS_IN_TRANS` 记入 `tidb_server_conn_idle_duration_seconds{in_txn}` | `session.go:781-791`、`:839-841`、`:4369-4400`；`simple.go:801-815`；`tidb.go:327-330`；`pkg/server/conn.go:1319-1323` |

### 6.2 已由负责人决定的部分（N11 D8，#146，2026-11-14）

- `START TRANSACTION READ ONLY`：**始终接受**（Go 在 `tidb_enable_noop_functions` 关时报 1235）。tidb-rs 的事务都是只读的，所以 `READ ONLY` 与 `BEGIN` 行为相同。
- 事务内（已在事务中，或 `autocommit=0`）的写语句（`INSERT`、`REPLACE`、`UPDATE`、`DELETE`、`LOAD DATA`）和加锁读 `SELECT ... FOR UPDATE`：**1792** `Cannot execute statement in a READ ONLY transaction.`（SQLSTATE 25006，Go 定义了但没有使用）。事务照常继续。
- 不变的部分：DDL 等其余 F1 语句（委托给 Go TiDB）仍是 F1 的 1235；`FOR SHARE` / `LOCK IN SHARE MODE` 在 Go 里是 noop 函数、不加锁，原型照旧报 1235；`START TRANSACTION READ ONLY AS OF TIMESTAMP`（stale read 事务，本块第 2 部分）与原型的其他 stale read 一样报 1235 `AS OF TIMESTAMP (stale reads)`。

### 6.3 路由（维护者已定，宪章第九条 2；#547 上请 Maya 确认）

**显式事务和 `autocommit=0` 的语句由原型执行**，新栈暂不接手：

1. §3a 与 VS1-14m 的完成条件：新栈会话的元数据锁登记上报给集群（D06.5 的 MDL 检查循环，桩 `report_metadata_locks`）之前，路由器不得把显式事务交给新栈；
2. `BEGIN` / `COMMIT` 的执行器（`SimpleExec`，D12.12）还没有移植；
3. 多数 SELECT 在新栈上仍会回退到原型（VS1-34 起逐步减少）。事务中的回退必须读同一个快照，只有一个引擎持有事务才能保证。

所以路由器第 0 步的两个会话条件"explicit transaction"（`PocEngine::in_txn`，现在在原型持有事务时为真）和"autocommit=0"保持不变；原型持有事务状态。上面三条都满足后再把事务交给新栈（那时新栈的 `ExecuteStmt` 已按 Go 实现 `validateStatementInTxn` 与 1792）。

### 6.4 原型中的实现（`crates/poc/session/src/txn.rs`，glue）

- 事务状态：start ts、读 ts 的规则（start ts / 每条语句新 ts，按 6.1 的 provider 选择）、悲观与否（指标标签）、开始时间、start ts 上可见的 InfoSchema（读过一次后缓存）。
- 事务内的读：**用读 ts 上可见的 schema 和该 ts 的快照**（ADR 0003 的路径：先用最新缓存的 schema 规划，同时校验它就是该 ts 可见的版本，否则按该 ts 的 schema 重新规划），不用同步模式的 schema，不登记元数据锁。读 start ts 的语句复用缓存的 InfoSchema，不再读 schema 版本键。
- 延迟事务：激活它的语句取 TSO，成功后事务才算开始；该语句失败时不保留事务（见 6.5 第 4 条）。
- `@@autocommit` 读会话变量（原来固定为 1）；`status_flags` 带上 `SERVER_STATUS_IN_TRANS`；`txn_timestamp` 返回原型事务的 start ts。
- 指标按 6.1 记录；事务内的语句不再记成 autocommit 事务。`tidb_server_conn_idle_duration_seconds` 原来总记 `in_txn="0"`，现在照 Go 按状态标志分开（`crates/mysql-protocol` 的连接循环）。
- 1792 作为 MySQL 错误直接发给客户端（相当于 Go 的 `mysql.NewErr(ErrCantExecuteInReadOnlyTransaction)`，`writeError` 原样发出）。Go 没有任何错误类注册 1792（v8.5.8 的 `ErrClassToMySQLCodes` 里没有），带这个码的 TiDB 错误经 `terror.ToSQLError` 会变成 1105，所以不用新栈的 `err_read_only_txn`（`ClassSession.NewStd`，调试构建里还会触发登记表的断言）；新栈接手事务时同样要按 MySQL 错误发出。
- 事务内的写语句检查在解析之后、降级之前（`tidb_sql::ir::writes_in_txn`）：`--new-stack=off` 在原型的 `query_inner`，VS1 路由器在走原型的路径上（`PocEngine::txn_write_error`；事务中的语句按第 0 步都走原型）。

### 6.5 与 Go 的差异（代码中已标注）

1. **元数据锁**：原型的事务不登记元数据锁，Go 上的 DDL 不等 tidb-rs 的事务结束（Go 会等，§3a）。读的结果：读 start ts 的事务用 start ts 上的 schema，不受影响；RC 事务每条语句用自己 ts 上的 schema，事务进行中提交的 DDL 对后续语句可见（Go 中这个 DDL 会等到事务结束）。临时差异，跟踪 issue #549；新栈接手事务时照 Go 登记。
2. **minstartts**：不上报（N11 §4.1，与 autocommit 语句相同）。事务长于 `tidb_gc_life_time` 时可能读到 GC 错误；Go 会挡住 GC。跟踪 issue #549。
3. **同步模式**：事务内读 schema 版本键（RC 的每条语句、RR 的第一条），meta region 不可用时这些语句会等待；autocommit 语句在同步模式下不读（#136）。
4. 延迟事务中激活事务的语句在取得 start ts 之后失败（例如读 TiKV 出错）时，Go 保留事务（`base.go:303-305`），原型不保留，下一条语句重新开始事务。
5. 每个事务的语句数直方图（`tidb_session_transaction_statement_num`）不记录（autocommit 语句也不记录）。
6. 不变：`SET TRANSACTION ISOLATION LEVEL`（one-shot）、`tidb_snapshot`、`tidb_read_staleness`、`tx_read_ts`、`tidb_txn_mode` 的会话 `SET` 在原型里仍是 1235，所以只有 `BEGIN PESSIMISTIC/OPTIMISTIC`、集群的 `tidb_txn_mode` 全局值和会话的 `transaction_isolation` 决定读 ts 的规则。

### 6.6 需要负责人决定

无新的点：`READ ONLY` 与 1792 已由 D8 决定；6.5 都是有跟踪 issue 的临时差异或与原型 autocommit 语句相同的已知差异。

### 6.7 测试

- `crates/poc/session/src/txn.rs`：按 6.1 的场景测试事务状态（BEGIN 与延迟事务、乐观 / RR / RC / SERIALIZABLE 的读 ts、`BEGIN PESSIMISTIC/OPTIMISTIC`、结束与指标标签）。
- `crates/sql`：`BEGIN` 的选项、AS OF、事务内写语句与加锁读的判定。
- `crates/poc/session/src/lib.rs`、`constant.rs`：`SET autocommit` 接受 Go 接受的值、`@@autocommit` 读会话值、驱动连接工作负载（`tools/ab/workloads/connect/`，新增 `pymysql.sql`：pymysql 默认设置连接时发的语句和一个只读事务）。
- `crates/metrics`：`commit` / `rollback` / `abort` 与 `txn_mode` 标签、空闲时间的 `in_txn` 标签与 Go 相同。
- `crates/poc/session/tests/cluster.rs`（需要集群，`TIDB_RS_PD` / `TIDB_RS_TIDB_PORT`）：RR 事务中两条 SELECT 之间 Go 插入的行不出现、RC 出现、`autocommit=0` 同样，事务内写 1792，`SERVER_STATUS_IN_TRANS`。
