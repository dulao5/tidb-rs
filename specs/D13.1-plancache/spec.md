# D13.1 Plan Cache 的预处理语句部分与预处理执行器（spec）

状态：VS1-15（预处理语句的规划）评审中（2026-11-15）。块：D13.1 plan cache（例外 A3）的非缓存部分，crate `tidb-planner`；D12.12 预处理执行器（P5）的 `PrepareExec`，crate `tidb-executor`。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（D13.1、D12.12 两行）、§4.7（预处理语句）、§10-2（plan cache 的时机）；[D05.3 spec](../D05.3-session/spec.md) §6（会话的 `PrepareStmt` / `ExecutePreparedStmt`，#247）；[D13.1 tasks](tasks.md)。

## 1. 范围

### 1.1 移植的 Go 函数

函数清单是 `specs/VS1-vertical-slice/functions.tsv` 中 VS1-15 的 46 个函数。

| Go | Rust | 内容 |
|---|---|---|
| `plan_cacheable_checker.go` | `crates/planner/src/core/plan_cacheable_checker.rs` | `IsASTCacheable`、`cacheableChecker.Enter/Leave`、`CacheableWithCtx`、`checkTableCacheable`、`getMaxParamLimit`、`enablePlanCacheForGeneratedCols`、`isPlanCacheable`、`isPhysicalPlanCacheable` |
| `plan_cache_utils.go` | `plan_cache_utils.rs` | `PlanCacheStmt`、`GeneratePlanCacheStmtWithAST`、`paramMarkerExtractor`、`planCacheStmtProcessor`、`ExtractAndSortParamMarkers`、`CollectPlanCacheStmtInfo`、`PlanCacheStmt.DBName/Tbls/SetDBNameAndTbls` |
| `plan_cache.go` | `plan_cache.rs` | `SetParameterValuesIntoSCtx`、`planCachePreprocess`、`GetPlanFromPlanCache`（只有未命中路径）、`generateNewPlan`、`checkPreparedPriv` |
| `executor/prepared.go` | `crates/executor/src/prepared.rs` | `NewPrepareExec`、`PrepareExec.Next` |
| `session.go` | `crates/session/src/session.rs` | `cleanRetryInfo` 中删 plan cache 的部分（#247 只移植了删语句）；`PrepareStmt` 改为调用 `PrepareExec`；`GetSessionPlanCache` |

converge：D13.1 `--item VS1-15` 44 个函数，24 个移植（L1 13、L2 11）+ 20 个边界桩；D12.12 2/2（L2）。

### 1.2 不做：缓存本身（例外 A3）

例外 A3 的 plan cache spec 还没有经负责人确认，所以本项**不缓存任何东西**（plan §3 D13.1 行："缓存本身是 VS1-39，有条件"）。范围不清的地方一律取窄的一边：

| 内容 | 做法 |
|---|---|
| 命中路径：`NewPlanCacheKey`、`parseParamTypes`、`lookupPlanCache`、`adjustCachedPlan`、`instancePlanCacheEnabled` | 边界桩。`lookupPlanCache` 总是未命中；`NewPlanCacheKey` 是替身：返回"库名 + 语句文本"作 key、总是可缓存，只给 `cleanRetryInfo` 的删除用（§4.3） |
| 放入缓存：`generateNewPlan` 中 `NewPlanCacheValue`、`NormalizePlan`、`Put` | 边界桩，什么都不放 |
| 会话的 LRU cache（`GetSessionPlanCache` 懒建的 `NewLRUPlanCache`）、实例级 cache、range 重建（`plan_cache_lru.go`、`plan_cache_instance.go`、`plan_cache_rebuild.go`） | 不移植。`GetSessionPlanCache` 返回一个**不存计划**的替身（`SessionPlanCache`），它只记下 `Delete` 收到的 key |
| 非预处理 plan cache（`tidb_enable_non_prepared_plan_cache`：`NonPreparedPlanCacheableWithCtx`、`plan_cache_param.go`） | 不移植，新栈不用它（边界桩） |
| 预处理去重缓存（`tidb_enable_cache_prepare_stmt`，`SetPrepareStmtDedupCache` / `rebuildFromPrepareCache`） | **归入 A3**：它缓存的是 `PlanCacheStmt` 本身，命中后靠 `ExtractAndSortParamMarkers` / `CollectPlanCacheStmtInfo` 重建，是 plan cache 对象的缓存。新会话**不写**它（`PrepareStmt` 成功后跳过 `SetPrepareStmtDedupCache`），命中分支仍是 `blocked-by:D13.1` 的桩（不写就不会命中） |

与 Go 的可见差异只有一条：Go 第二次 EXECUTE 同一语句会命中缓存（`@@last_plan_from_cache` = 1），新栈每次都未命中、重新优化。warning 与 Go 相同：未命中路径照样计算 `UseCache`、调用 `isPlanCacheable`，`skip prepared plan-cache: <原因>` 照 Go 发出。

## 2. 承载方式

1. **AST 归 `PlanCacheStmt` 所有。** Rust 的 preprocess 要 `&mut Stmt`，resolve context 以节点地址为 key，所以语句放在 `Box<Stmt>` 里由 `PlanCacheStmt` 独占；`PlanCacheStmt` 放在 `Arc<parking_lot::Mutex<_>>`（`PlanCacheStmtRef`）里，会话的语句表和执行它的语句共享。执行器原来的 `glue::PlanCacheStmt`（只有缓存的点查执行器）换成 planner 的类型，点查执行器以 `Any` 存在 `PointGet.Executor` 里（planner 在执行器下层）。
2. **参数。** Go 的 `Params` 是指向 `?` 节点的指针，EXECUTE 把值写进节点。Rust 的 AST 不可变，`?` 节点已带 `order`（解析器按位置编号，等于 Go 排序后的 `SetOrder(i)`），值只写进 `SessionVars.PlanCacheParams`，表达式改写（D09.2）按 order 读。`PlanCacheStmt.params` 存排好序的 offset，个数即参数个数。
3. **`infoschema.InfoSchema`** 是窄接口 `InfoSchemaLookup`（`TableByName`、`TableByID`、`SchemaByID`、`SchemaMetaVersion`）。真正会话的 `InfoSchemaV2` 实现它：按名 / 按 ID 查表是 `blocked-by:D06.3`（InfoSchema 接口，VS1-17），与 preprocess 相同。govec 用按 JSON 建的 mock。
4. **`OptimizeAstNode`** 在 Go 是包变量（`pkg/planner` 初始化时设置），Rust 是 `get_plan_from_plan_cache` 的参数；生产用 `optimize::optimize_ast_node`，它与 `planner.Optimize` 一样是 `blocked-by:D13.3` 的桩（先要 hint 提取，VS1-24）。
5. **AST 遍历。** Rust AST 还没有 visitor（D02.2），`Walker` 按 Go `Accept` 的顺序调用 `enter` / `leave`（决定先报哪个原因），被 checker、`paramMarkerExtractor`、`planCacheStmtProcessor` 共用。Rust AST 没有的节点（`Unported`）不猜：返回 `blocked-by:D02.2`；`SELECT /*+ ... */` 的 hint 名字没有解析，返回 `blocked-by:D13.3`。

## 3. 各项要求

### 3.1 可缓存性检查（`IsASTCacheable`）

- 原因字符串与 Go 逐字节相同（它们就是 `skip prepared plan-cache: …` warning 的内容）。govec `plancache` 268 条（arm64；amd64 来自 tracker CI），回放 238 条逐字节一致，18 条是边界桩（DML 16 条 `D02.2`、hint 2 条 `D13.3`），0 条不一致；Rust 解析器全部能解析。用例：`plan_cacheable_checker_test.go` 的 `TestCacheable`（AST 行改写成 SQL，DML 行保留）、`TestFixControl44823`，加上 `Enter` 每个分支、`checkTableCacheable` 每个分支（分区表 static / dynamic、Fix33031、生成列与 Fix45798、临时表、表不存在、无库名）和遍历顺序。
- `isPlanCacheable` / `isPhysicalPlanCacheable`：Go-vs-Rust 的记录要先有物理算子（D10.1，VS1-27），所以用 Rust 单测覆盖新栈能有的分支（参数 limit / 子查询开关、计划大小、TableDual 带参数、递归）；TableReader / TableScan 的字段是 `blocked-by:D10.1`。

### 3.2 参数

- 二进制协议参数（NULL 位图、unsigned 标志、长数据）到 `Datum` 的解码是 `parseBinaryParams`（D03.1，VS1-16），不在本项；新栈收到二进制参数时仍是 `newstack.rs` 的 `blocked-by:D03.1`。
- 本项负责：`?` 的个数和顺序（按位置）；EXECUTE 时个数不等报 Go 的 `ErrWrongParamCount`（8112）；`SetParameterValuesIntoSCtx` 按顺序写入 `PlanCacheParams`。`EXECUTE ... USING @var`（文本协议，`isGetVarBinaryLiteral`）不在 Rust AST 里。

### 3.3 PREPARE 与 EXECUTE 之间 schema 变了

照 `planCachePreprocess` 第 3、4 步：逐表按 ID 重查（不在则按名、表 ID 变了删 `RelateVersion`），比 `Revision`；任一表变了或 schema 版本不同，清掉缓存的点查执行器，以 `InPrepare` 重新 preprocess，失败时报 `ErrSchemaChanged`（8113，`Schema change caused error: …`），成功则换上新的 resolve context 和版本。不复用旧的 `PlanCacheStmt` 内容。元数据锁（`tryLockMDLAndUpdateSchemaIfNecessary`）是 `blocked-by:D05.4` 的桩：Go 对它的错误只记日志、继续，桩的 `Blocked` 不能这样吞掉，所以直接返回。

### 3.4 `max_prepared_stmt_count`

`PrepareExec.Next` 最后调用 `AddPreparedStmt`，在那里检查上限（1461），计数包括已 CLOSE、事务还没结束的语句（#247 的 `DropPreparedStmt` 只登记，事务结束才删）。

### 3.5 列定义

`PrepareExec` 用 `colNames2ResultFields` 生成 `Fields`，返回给会话（`PreparedStmtInfo.fields`）。计划由 `PlanBuilder.Build` 产生，在 D09.1（VS1-21）之前构建就失败，所以现在拿不到列定义。ab 的 sysbench 用例增加选择列表里有 `?` 的语句（`SELECT ?, c FROM sbtest1 WHERE id=?`，整数 / 无符号最大值 / 字符串三种参数），由 #247 的 PREPARE-vs-EXECUTE 元数据检查比较。

## 4. 会话与路由器

### 4.1 PREPARE / EXECUTE 走到哪里

- `PrepareStmt` → `PrepareExec.Next`：解析（Rust 解析器的错误文本与 Go 不同，语法错误和解析 warning 都是 `blocked-by:D02.1`，交给 PoC）→ 多条语句报 `ErrPrepareMulti`（8115）→ `ResetContextOfStmt` → `GeneratePlanCacheStmtWithAST`。后者现在停在：有表的语句在 preprocess 查表（`blocked-by:D06.3`），无表的 SELECT 在 `PlanBuilder.Build`（`blocked-by:D09.1`）。没有库名时，有表的语句按 Go 报 1046。
- `ExecutePreparedStmt`：按 id 找到 `PlanCacheStmt` 之后，Go 包成 `ast.ExecuteStmt` 交给 `ExecuteStmt`；Rust AST 没有 `ExecuteStmt`，这里是 `blocked-by:D02.2`。`OptimizeExecStmt` 同样改为 `blocked-by:D02.2`（它到达的 `GetPlanFromPlanCache` 已移植）。

### 4.2 关闭的语句在事务结束时才删除（Lucky，#247 的完成条件）

Go：`COM_STMT_CLOSE`（`DropPreparedStmt`）只登记，语句在事务结束时由 `cleanRetryInfo` 删除，之前仍可执行。路由器原来立刻丢掉归属、PoC 立刻关闭。现在：

- `Router::close`：归属为 both / new 时把 CLOSE 交给新会话（只登记）；PoC 的副本**不关**；语句记入 `closed`，归属保留。路由器不认识的 id 仍立即交给 PoC（它报 Go 的错误）。
- 每个命令（`COM_QUERY`、`COM_STMT_EXECUTE`、`COM_STMT_PREPARE`）结束时，如果这次有语句执行过（任一方）、两边都不在事务中，事务就结束了：关闭 PoC 的副本、让新会话执行 `cleanRetryInfo`（`clean_closed_prepared_stmts`；若语句由新栈执行，新会话已自行做过，再做一次无影响）、忘掉归属。解析失败的 `COM_QUERY` 不算语句结束（Go 不会走到 `finishStmt`）。PREPARE 无论成败都算：Go 的 `PrepareStmt` 在预处理执行器之后总是调用 `rollbackOnError`，事务外即 `RollbackTxn` → `cleanRetryInfo`（解析在执行器内部，所以解析失败也算）。
- 已知的提前一步：EXECUTE 一个不存在的 id 时，Go 在任何事务操作之前就返回（不清理），路由器却把 PoC 的每个结果都记为语句结束，所以这种情况下已关闭的语句会比 Go 早一个命令被删除。只影响"关闭之后、事务结束之前执行一个不存在的语句"这一种顺序，记为已知差异。
- 测试：`autocommit_close_then_prepare`（autocommit 下 `CLOSE 1; PREPARE; EXECUTE 1` 报 1243，与 Go 相同）；`closed_stmt_lives_until_txn_end`（`BEGIN; PREPARE; CLOSE; EXECUTE` 在事务中成功，COMMIT 之后两边都没有了，EXECUTE 报 1243）；`autocommit_close_then_poc_statement`（autocommit 下 CLOSE 之后由 PoC 执行一条语句，结束时 PoC 关闭副本、新会话删掉语句，不再计入上限）；`close_by_owner` 改为下一条语句结束时才关闭。
- `--new-stack=off`（没有路由器）时 PoC 仍立即关闭：那是 PoC 本身的行为，不在本项。

### 4.3 `cleanRetryInfo` 的 Go 坑（D13.1 tasks，照搬）

Go v8.5.8 只用第一条被关闭语句的 `PlanCacheStmt` 计算 key，之后每次循环还用它，所以一个事务里关闭多条语句时，每次删除的都是第一条的计划。照搬；`tidb_ignore_prepared_cache_close_stmt` 打开时不删计划，但语句照删。测试 `clean_retry_info_deletes_the_first_closed_stmt_plan_each_time`：关闭两条语句，替身 cache 收到的 `Delete` 是两次第一条的 key。可能的上游 bug，记录在此，不提交上游。

## 5. 与 Go 的差异（均在代码中标注）

1. 不缓存（§1.2）：EXECUTE 从不命中，`@@last_plan_from_cache` 恒为 0。
2. `parser.NormalizeDigest` 未移植（D02.2）：`NormalizedSQL` / `SQLDigest` 为空，只影响 Top SQL、语句摘要和 cache key，新栈都没有。
3. 语句标签 `stmtctx.GetStmtLabel` 只有 SELECT（其余 `blocked-by:D12.0`）。
4. `checkPreparedPriv`：没有权限管理器（D04.1，与 Go 未注册权限管理器时相同），`CheckTableLock` 属写路径。
5. `PlanBuilder.Build` 还不返回计划：`GeneratePlanCacheStmtWithAST` 在 Build 之后返回 `blocked-by:D09.1`，而不是带着空的列定义继续。
6. `PrepareExec`：解析错误 / warning 交给 PoC（`D02.1`）；语句文本用整个 SQL（只允许一条语句）；Top SQL 未移植（D14.3）。
7. `planCacheStmtProcessor` 不收集表列表（只给 cache key 的统计信息用，A3）。
8. 元数据锁的桩返回 `Blocked` 而不是像 Go 那样吞掉错误（§3.3）。

## 6. 边界桩（登记在 tracker `data/stubs.tsv`）

| 位置 | 块 | 原因 |
|---|---|---|
| `lookupPlanCache`、`adjustCachedPlan`、`instancePlanCacheEnabled`、`NewPlanCacheKey`、`parseParamTypes`、`generateNewPlan` 的 Put、`LRUPlanCache.Delete` 替身、去重缓存命中 | D13.1 | 缓存本身，例外 A3 |
| `NonPreparedPlanCacheableWithCtx`、`extractTableNames`、`getColType`、`plan_cache_param.go` 11 个函数、`Cacheable`（测试辅助） | D13.1 | 非预处理 plan cache，新栈不用 |
| `GetPreparedStmt`、`OptimizeExecStmt`、`ExecutePreparedStmt` 之后 | D02.2 | Rust AST 没有 `ExecuteStmt` |
| `OptimizeAstNode` | D13.3 | `planner.Optimize`（VS1-24） |
| `isPhysicalPlanCacheable` 的 TableReader / TableScan | D10.1 | 物理算子（VS1-27） |
| `IsASTCacheable` 的 hint、`Unported` 节点 | D13.3、D02.2 | hint 解析、Rust AST |
| `PrepareExec` 的解析错误 / warning | D02.1 | 错误文本 |
| `NormalizeDigest`、`GetStmtLabel` | D02.2、D12.0 | 见 §5 |

## 7. 验收

- govec `plancache`：见 §3.1。
- 单测：planner `plan_cache_tests`（PREPARE 停在 plan builder 之前发出的 warning、参数个数 8112 与顺序、schema 变化后重新 preprocess 与 8113、未命中路径的 `UseCache` / `InPreparedPlanBuilding` / `isPlanCacheable` warning、`isPlanCacheable` 的开关与大小）；session（`cleanRetryInfo` 的坑、1461 计入已关闭语句、真正的 `PrepareStmt` 停在 D09.1 / D06.3 / 8115）；路由器（§4.2）。
- **预处理点查（`SELECT c FROM sbtest1 WHERE id=?`）在 `strict` 模式下由新栈执行：本项做不到**。这条路径还被以下各项挡住：preprocess 查表（D06.3，VS1-17）、`PlanBuilder.Build`（D09.1，VS1-21）、`planner.Optimize`（D13.3，VS1-24；点查的 `TryFastPlan` 由它调用）、`EXECUTE` 节点（D02.2）、结果集与二进制参数的编码（D03.1，VS1-16）。本项完成后，PREPARE 在新会话中走到 preprocess 查表，`strict` 模式下 EXECUTE 报 `1235 blocked-by:D06.3`。
