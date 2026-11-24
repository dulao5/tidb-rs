# D13.1 Plan Cache 的预处理语句部分与预处理执行器（spec）

状态：VS1-15（预处理语句的规划）评审中（2026-11-15）。块：D13.1 plan cache（例外 A3）的非缓存部分，crate `tidb-planner`；D12.12 预处理执行器（P5）的 `PrepareExec`，crate `tidb-executor`。相关：[A3 plan cache spec](../exceptions/A3-plan-cache/spec.md)（缓存本身，待确认）；[VS1 plan](../VS1-vertical-slice/plan.md) §3（D13.1、D12.12 两行）、§4.7（预处理语句）、§10-2（plan cache 的时机）；[D05.3 spec](../D05.3-session/spec.md) §6（会话的 `PrepareStmt` / `ExecutePreparedStmt`，#247）；[D13.1 tasks](tasks.md)。

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

例外 A3 的 plan cache spec（[spec](../exceptions/A3-plan-cache/spec.md)，2026-11-23 起草）还没有经负责人确认，所以本项**不缓存任何东西**（plan §3 D13.1 行："缓存本身是 VS1-39，有条件"）。范围不清的地方一律取窄的一边：

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
2. **参数。** Go 的 `Params` 是指向 `?` 节点的指针，EXECUTE 把值写进节点。Rust 的 `?` 节点不存值，值只写进 `SessionVars.PlanCacheParams`，表达式改写（D09.2）和点查的快速计划（D10.8，`getNameValuePairs`）按节点的 `order` 读。`order` 与 Go 一样由 PREPARE 写：解析器把每个 `?` 的 order 都留为 0，`GeneratePlanCacheStmtWithAST`（以及 `ExtractAndSortParamMarkers`）把 extractor 收集的 `?` 按 offset 排序后逐个 `SetOrder(i)`（Go `plan_cache_utils.go` 122–129 行、603–616 行）。Rust 的 AST 遍历只借出只读的节点，extractor 存的是 offset，所以第二遍遍历同一条语句，给每个 `?` 写入它的 offset 在排好序的 offset 中的下标（一个 offset 只有一个 `?`，编号与 Go 相同）；`order` 是原子整数（`ParamMarkerExpr::set_order`，只在 PREPARE 独占语句时写一次）。`PlanCacheStmt.params` 存排好序的 offset，个数即参数个数。
   - 个数核对（#356，Go 没有，见 §5 第 9 条）：解析器数出它生成了多少个 `?`（`ParseOutput::param_markers`），PREPARE 把这个数传给 `generate_plan_cache_stmt_with_ast`；extractor 遍历找到的个数与它不同，就返回边界桩 `blocked-by:D02.2`，不编号、不继续。
   - 修正记录（#352）：原来以为解析器已按位置编号，PREPARE 没有写 order，结果每个 `?` 都读第一个参数。`id BETWEEN ? AND ?`（3, 7）变成范围 [3, 3]，规划成点查，只返回一行；两个及以上 `?` 的预处理语句都受影响（`fallback` / `strict` 模式下新栈执行时结果错误，默认 `off` 模式不走新栈）。回归测试见 §7。
3. **`infoschema.InfoSchema`** 是窄接口 `InfoSchemaLookup`（`TableByName`、`TableByID`、`SchemaByID`、`SchemaMetaVersion`）。真正会话的 `InfoSchemaV2` 实现它：按名 / 按 ID 查表是 `blocked-by:D06.3`（InfoSchema 接口，VS1-17），与 preprocess 相同。govec 用按 JSON 建的 mock。
4. **`OptimizeAstNode`** 在 Go 是包变量（`pkg/planner` 初始化时设置），Rust 是 `get_plan_from_plan_cache` 的参数；生产用 `optimize::optimize_ast_node`，它与 `planner.Optimize` 一样是 `blocked-by:D13.3` 的桩（先要 hint 提取，VS1-24）。
5. **AST 遍历。** Rust AST 还没有 visitor（D02.2），`Walker` 按 Go `Accept` 的顺序调用 `enter` / `leave`（决定先报哪个原因），被 checker、`paramMarkerExtractor`、`planCacheStmtProcessor` 共用。Rust AST 没有的节点（`Unported`）不猜：返回 `blocked-by:D02.2`；`SELECT /*+ ... */` 的 hint 名字没有解析，返回 `blocked-by:D13.3`。

## 3. 各项要求

### 3.1 可缓存性检查（`IsASTCacheable`）

- 原因字符串与 Go 逐字节相同（它们就是 `skip prepared plan-cache: …` warning 的内容）。govec `plancache` 268 条（arm64；amd64 来自 tracker CI），回放 238 条逐字节一致，18 条是边界桩（DML 16 条 `D02.2`、hint 2 条 `D13.3`），0 条不一致；Rust 解析器全部能解析。用例：`plan_cacheable_checker_test.go` 的 `TestCacheable`（AST 行改写成 SQL，DML 行保留）、`TestFixControl44823`，加上 `Enter` 每个分支、`checkTableCacheable` 每个分支（分区表 static / dynamic、Fix33031、生成列与 Fix45798、临时表、表不存在、无库名）和遍历顺序。
- `isPlanCacheable` / `isPhysicalPlanCacheable`：Go-vs-Rust 的记录要先有物理算子（D10.1，VS1-27），所以用 Rust 单测覆盖新栈能有的分支（参数 limit / 子查询开关、计划大小、TableDual 带参数、递归）；TableReader / TableScan 的字段是 `blocked-by:D10.1`。

### 3.2 参数

- 二进制协议参数（NULL 位图、unsigned 标志、长数据）到 `Datum` 的解码是 `parseBinaryParams`（D03.1，VS1-16），不在本项；新栈收到二进制参数时仍是 `newstack.rs` 的 `blocked-by:D03.1`。
- 本项负责：`?` 的个数和顺序（按位置）；EXECUTE 时个数不等报 Go 的 `ErrWrongParamCount`（8112）；`SetParameterValuesIntoSCtx` 按顺序写入 `PlanCacheParams`。`EXECUTE ... USING @var`（文本协议，`isGetVarBinaryLiteral`）的 EXECUTE 节点已在 Rust AST 里，但变量要经表达式改写（D09.2），现在是边界桩。

### 3.3 PREPARE 与 EXECUTE 之间 schema 变了

照 `planCachePreprocess` 第 3、4 步：逐表按 ID 重查（不在则按名、表 ID 变了删 `RelateVersion`），比 `Revision`；任一表变了或 schema 版本不同，清掉缓存的点查执行器，以 `InPrepare` 重新 preprocess，失败时报 `ErrSchemaChanged`（8113，`Schema change caused error: …`），成功则换上新的 resolve context 和版本。不复用旧的 `PlanCacheStmt` 内容。元数据锁（`tryLockMDLAndUpdateSchemaIfNecessary`）已照 Go 移植（D05.4 spec §3a，#252）：autocommit 的 EXECUTE 是只读语句，不登记；它的普通错误照 Go 记日志、让该表的缓存失效后继续；其中的边界桩（`Blocked`）不能这样吞掉，所以直接返回。

### 3.4 `max_prepared_stmt_count`

`PrepareExec.Next` 最后调用 `AddPreparedStmt`，在那里检查上限（1461），计数包括已 CLOSE、事务还没结束的语句（#247 的 `DropPreparedStmt` 只登记，事务结束才删）。

### 3.5 列定义

`PrepareExec` 用 `colNames2ResultFields` 生成 `Fields`，返回给会话（`PreparedStmtInfo.fields`）。计划由 `PlanBuilder.Build` 产生，在 D09.1（VS1-21）之前构建就失败，所以现在拿不到列定义。ab 的 sysbench 用例增加选择列表里有 `?` 的语句（`SELECT ?, c FROM sbtest1 WHERE id=?`，整数 / 无符号最大值 / 字符串三种参数），由 #247 的 PREPARE-vs-EXECUTE 元数据检查比较。

**列名取预处理语句的原文（#361）。** 表达式列的列名是选择字段的原文（Go `SelectField.Text()`，见 D09.13 spec §5.1）。Rust AST 只存字节区间，规划时要知道区间所指的文本：

- PREPARE：构建器的 `sql_text` 是参数化 SQL（D09.13 spec §7）。
- EXECUTE 未命中：`generateNewPlan` 把预处理语句交给 `OptimizeAstNode` 时，在节点包装 `NodeW` 上设置 `PlanCacheStmt.stmt_text`（Go 的 `StmtText`，就是 PREPARE 解析的那段文本）。以前没有设置，EXECUTE 的表达式列名是空串，于是和 PREPARE 的列定义不同（`SELECT 1 + 1` 在 PREPARE 时列名是 `1 + 1`，EXECUTE 时是空串）。
- EXECUTE 命中：沿用未命中时放进缓存的列名（`PlanCacheValue.OutputColumns`），所以也是正确的。

`?` 列在 PREPARE 和 EXECUTE 的类型本来就不同（Go 同样如此，§3.5 开头），但列名两次都是 `?`。

## 4. 会话与路由器

### 4.1 PREPARE / EXECUTE 走到哪里

- `PrepareStmt` → `PrepareExec.Next`：解析（Rust 解析器的错误文本与 Go 不同，语法错误和解析 warning 都是 `blocked-by:D02.1`，交给 PoC）→ 多条语句报 `ErrPrepareMulti`（8115）→ `ResetContextOfStmt` → `GeneratePlanCacheStmtWithAST`。后者现在停在：有表的语句在 preprocess 查表（`blocked-by:D06.3`），无表的 SELECT 在 `PlanBuilder.Build`（`blocked-by:D09.1`）。没有库名时，有表的语句按 Go 报 1046。
- `ExecutePreparedStmt`：按 id 找到 `PlanCacheStmt` 之后，照 Go 包成 EXECUTE 节点（`ast.ExecuteStmt`，解析器现已有这个节点，带 Restore）交给 `ExecuteStmt`。节点上的 `PrepStmt` / `BinaryArgs` 在 Go 是 `interface{}`，Rust 也存为 `Any`（参数是路由器已求值的常量值）。
- 读这个节点的地方都照 Go 改为先取预处理语句（`GetPreparedStmt`）：`ResetContextOfStmt`（按预处理的 SELECT 设置语句上下文，`OriginalSQL` 取预处理的 SQL 文本）、语句标签、`isEndTxnStmt`、`resolvePreparedStmt`、`planner.IsReadOnly` 的 EXECUTE 分支、编译器取 `preparedObj`、preprocess 的 `resolveExecuteStmt`。
- `OptimizeExecStmt` 已移植：`buildExecute` 生成 `Execute` 计划（`PlanBuilder.Build` 还不返回计划，所以直接调这个分支），然后走 `GetPlanFromPlanCache` 的未命中路径（照旧不缓存，A3 例外未确认），最后到语句自己的 `Optimize`（`OptimizeAstNode`，`blocked-by:D13.3`）。文本协议的 `EXECUTE s USING @a` 要先经表达式改写（`b.rewrite`，`blocked-by:D09.2`）。
- **现在停在哪里**：会话的 EXECUTE 先经过 `ResetContextOfStmt`、语句开始、语句标签，停在编译器里规划器的入口（`planner.IsReadOnly` / `Optimize`，`blocked-by:D09.13`），会话恢复检查点后交给 PoC（`real_execute_reaches_the_planner_and_falls_back`、`execute_of_a_prepared_stmt_reaches_the_planner`）。编译器接上 `planner.Optimize` 之后，下一站是 `OptimizeAstNode`（D13.3），再之后是 `buildSelect`（D09.1）。

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
8. 元数据锁（已照 Go 移植，见 D05.4 事务上下文 spec §3a，#252）中的边界桩（没有 domain、会话扩展的 InfoSchema 未移植）返回 `Blocked`，而不是像 Go 那样把错误记日志后吞掉（§3.3）；Go 的普通错误照 Go 记日志、让缓存失效。
9. PREPARE 核对 `?` 的个数（#356，Go 没有这一步）：Go 的 `GeneratePlanCacheStmtWithAST`（`pkg/planner/core/plan_cache_utils.go` 122–129 行，排序后逐个 `SetOrder(i)`）和 `ExtractAndSortParamMarkers`（同文件 603–616 行）直接给 extractor 找到的 `?` 编号，不检查是否漏了。Rust 的 extractor 用的是本项的 `Walker`（§2 第 5 条，代替还没有的 visitor），它进不去而表达式改写却会读到的节点里的 `?` 会保持 order 0、读第一个参数（#352 那样的错误结果）。所以解析器数出它生成的 `?` 个数（`ParseOutput::param_markers`），`generate_plan_cache_stmt_with_ast` 在 extractor 遍历之后、第一次用到个数（`ErrPsManyParam` 检查）之前比较两者，不同就返回 `blocked-by:D02.2`（"a parameter marker the AST walk does not reach"），语句回退。只有 Rust 的 `Walker` 漏掉某个节点时才会触发；触发时行为是回退，不是错误的结果；正常语句两个数相同，路径不变。Go 先报的错误（`SELECT ... INTO` 的 `ErrUnsupportedPs`）仍然先报。`ExtractAndSortParamMarkers` 拿不到解析器的个数，不核对（Rust 里只有测试调用它）。

## 6. 边界桩（登记在 tracker `data/stubs.tsv`）

| 位置 | 块 | 原因 |
|---|---|---|
| `lookupPlanCache`、`adjustCachedPlan`、`instancePlanCacheEnabled`、`NewPlanCacheKey`、`parseParamTypes`、`generateNewPlan` 的 Put、`LRUPlanCache.Delete` 替身、去重缓存命中 | D13.1 | 缓存本身，例外 A3 |
| `NonPreparedPlanCacheableWithCtx`、`extractTableNames`、`getColType`、`plan_cache_param.go` 11 个函数、`Cacheable`（测试辅助） | D13.1 | 非预处理 plan cache，新栈不用 |
| `buildExecute` 的 `USING` 变量 | D09.2 | 表达式改写 `b.rewrite` |
| `planner.IsReadOnly` 里的 `ast.IsReadOnly` | D02.2 | 要遍历整条语句（找用户变量赋值），Rust AST 没有 visitor |
| `OptimizeAstNode` | D13.3 | `planner.Optimize`（VS1-24） |
| `isPhysicalPlanCacheable` 的 TableReader / TableScan | D10.1 | 物理算子（VS1-27） |
| `IsASTCacheable` 的 hint、`Unported` 节点 | D13.3、D02.2 | hint 解析、Rust AST |
| `PrepareExec` 的解析错误 / warning | D02.1 | 错误文本 |
| `NormalizeDigest`、`GetStmtLabel` | D02.2、D12.0 | 见 §5 |
| `GeneratePlanCacheStmtWithAST` 的 `?` 个数核对（Go 没有，§5 第 9 条） | D02.2 | `Walker` 代替 visitor，漏掉的 `?` 会编错号；只在遍历与解析器的个数不同时触发 |

## 7. 验收

- govec `plancache`：见 §3.1。
- 单测：planner `plan_cache_tests`（PREPARE 停在 plan builder 之前发出的 warning、参数个数 8112 与顺序、PREPARE 后 `?` 按位置编号为 0..n（`prepare_numbers_the_markers_by_position`，含 `LIMIT ?, ?` 遍历顺序与位置不同的情形）、`?` 个数核对（`prepare_stops_when_the_walk_misses_a_marker`：传入的解析器个数比遍历多一个或少一个时 PREPARE 返回 `blocked-by:D02.2`、不发 warning；`SELECT ... INTO` 仍先报 `ErrUnsupportedPs`；多个 `?` 的正常语句照常 PREPARE、编号 0..n，`LIMIT ?, ?` 通过核对、停在 plan builder 后面的 `D13.1` 边界，#356；解析器计数见 parser `counts_the_parameter_markers_it_builds`）、schema 变化后重新 preprocess 与 8113、未命中路径的 `UseCache` / `InPreparedPlanBuilding` / `isPlanCacheable` warning、`isPlanCacheable` 的开关与大小）；session（`cleanRetryInfo` 的坑、1461 计入已关闭语句、真正的 `PrepareStmt` 停在 D09.1 / D06.3 / 8115；`prepared_statement_markers_read_their_own_parameters`：表里有 1..10 行，`BETWEEN ? AND ?`、`>= ? AND <= ?`、`c = ? AND id BETWEEN ? AND ?`、`id = ? AND id = ?` 各执行两次（第一次未命中，第二次换参数、可缓存的命中），每次要么返回与 Go 相同的行，要么在固定的边界桩回退、没有执行器打开，#352）；表达式列名在 PREPARE、EXECUTE 未命中、EXECUTE 命中三处都和 Go 相同（会话测试 `expression_columns_are_named_by_their_text`，#361）；路由器（§4.2）。
- **预处理点查（`SELECT c FROM sbtest1 WHERE id=?`）在 `strict` 模式下由新栈执行：本项做不到**。这条路径还被以下各项挡住：preprocess 查表（D06.3，VS1-17）、`PlanBuilder.Build`（D09.1，VS1-21）、`planner.Optimize`（D13.3，VS1-24；点查的 `TryFastPlan` 由它调用）、结果集与二进制参数的编码（D03.1，VS1-16）。本项完成后，PREPARE 在新会话中走到 preprocess 查表，`strict` 模式下 EXECUTE 报 `1235 blocked-by:D06.3`。
