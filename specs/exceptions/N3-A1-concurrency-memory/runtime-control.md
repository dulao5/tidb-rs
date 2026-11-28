# #542 运行时控制设计规格：取消与 KILL、内存配额与看门狗、控制面运行时隔离

- **状态**：设计定稿（项目负责人 `zhigang-du` 已于 2026-11-28 确认全部决策，见 §7.1）
- **关联 Issue**：`#542`（新执行栈 Cancellation & KILL、Memory Quota & Watchdog、Control-Plane Isolation）、`#474 R-5`（控制面隔离）、`#464`（Expensive Query & OOM 日志接线）
- **Go 基线**：Go TiDB v8.5.8（commit `8b857efa2`）。注：`crates/memory/src/sqlkiller.rs:14-16` 已提前携带了 v8.5.8 之后的并发复位修复（上游 `#70776`，commit `05c7dd7b96`），而 Go v8.5.8 源码 `pkg/util/sqlkiller/sqlkiller.go:169-175` 仍为 `LoadUint32` + `StoreUint32`
- **关联基础 Specs**：
  - `N0`：[specs/exceptions/N0-crate-layering/spec.md](../N0-crate-layering/spec.md) 与 `layers.toml`
  - `N2`：[specs/exceptions/N2-session-context/spec.md](../N2-session-context/spec.md)
  - `N3+A1`：[specs/exceptions/N3-A1-concurrency-memory/spec.md](./spec.md) 与 [runtime-skeleton.md](./runtime-skeleton.md)
  - `D00.3`：[specs/D00.3-logging/spec.md](../../D00.3-logging/spec.md)（§4.4、§9）
  - `D03.1`：[specs/D03.1-server/spec.md](../../D03.1-server/spec.md)
  - `D15.1`：[specs/D15.1-memory/spec.md](../../D15.1-memory/spec.md)
  - `D15.3`：[specs/D15.3-cgroup-memory/spec.md](../../D15.3-cgroup-memory/spec.md)
  - 移植宪章：[CONSTITUTION.md](../../../CONSTITUTION.md)

---

## 1. Go TiDB v8.5.8 精确行为对照（源码 + 实测基线）

本节依据 Go TiDB v8.5.8 源码及真实 Go TiDB v8.5.8（unistore）实测结果，按三大支柱逐条锁定必须对齐的外部行为与内部契约。

### 1.1 支柱一：语句取消、`KILL`、`max_execution_time` 与客户端断连

| 维度 | Go v8.5.8 源码位置与逻辑 | 真实 v8.5.8（unistore）实测行为 |
|---|---|---|
| **语法解析** | `pkg/parser/parser.y:15340-15381`，`pkg/parser/ast/misc.go:1008-1058`：支持 `KILL [TIDB] [CONNECTION \| QUERY] NUM` 与 `KILL [TIDB] BuiltinFunction`（运行期仅允许 `CONNECTION_ID()`）。 | `KILL QUERY <cid>`、`KILL <cid>`、`KILL TIDB QUERY <cid>`、`KILL TIDB QUERY CONNECTION_ID()` 均正确解析。 |
| **权限检查** | `pkg/planner/core/planbuilder.go:3927-3945`：在计划构建阶段检查：<br>1) 目标为自身（或通过 `CONNECTION_ID()`）一律放行；<br>2) 目标存在且 `pi.User != loginUser.Username` 时要求 `CONNECTION_ADMIN` 动态权限（`SUPER` 满足，启用 SEM 时追加 `RESTRICTED_CONNECTION_ADMIN`），否则报 `plannererrors.ErrSpecificAccessDenied.GenWithStackByArgs("SUPER or CONNECTION_ADMIN")`；<br>3) 目标在 `GlobalAutoAnalyzeProcessList` 时同样要求 `CONNECTION_ADMIN`（只读节点无 auto-analyze 且未启用 SEM，不伪造空桩）。 | 非 `SUPER`/`CONNECTION_ADMIN` 用户 `KILL` 他人连接返回 **`1227 (42000): Access denied; you need (at least one of) the SUPER or CONNECTION_ADMIN privilege(s) for this operation`**（不是 MySQL 原生 `1095`）。普通用户 `KILL TIDB QUERY <自身 ID>` 允许执行，并立即杀死正在运行的该 `KILL` 语句自身，返回 `1317 (70100): Query execution was interrupted`。 |
| **`server_id`、32/64 位 ID 编码与集群路由** | `pkg/domain/domain.go:1380, 1469-1489, 3123-3398`，`pkg/util/globalconn/globalconn.go:28-134, 248-317`，`pkg/executor/simple.go:2665-2777`：<br>• 配置默认 `enable-global-kill = true`、`enable-32bits-connection-id = true`、`compatible-kill-query = false`。<br>• **单机（`etcdClient == nil`）固定 `serverIDForStandalone = 1`**（`domain.go:1380, 1485`），**首条连接 ID 为 `(1 << 21) \| (1 << 1) = 2097154`**（而非 `2`！）；**集群模式**向 etcd `/tidb/server_id/<id>` 租用 `[1, 2047]`（`serverIDTTL = 12h`、`5m` keepalive、失联 PD 超 `6h` 触发 `KillAllConnections()`，停机 `releaseServerID()`），写入 `ServerInfo.ServerID`（`MaxServerID32 = 2047` 冲突/占满 90% 时升级 64 位 `[2048, MaxServerID64]`）。<br>• bit0=0 为 32 位 `[serverID(11b)\|localID(20b)\|0]`；bit0=1 为 64 位 `[0(1b)\|serverID(22b)\|localID(40b)\|1]`（20 位 localID 用尽升级 64 位）。若 `server_id = 0` 升级 64 位，低 32 位下高 32 位全 0 会被 `ParseConnID`（`:115`）判为 `isTruncated = true` 而永远杀不掉。<br>• `!enable-global-kill` 时用 `SimpleAllocator`，不带 `TIDB`/`compatible-kill-query` 则写 `Invalid operation...` warning（`simple.go:2681-2685`）；表达式非 `CONNECTION_ID()` 直接报错同文案（`:2672`）；`ParseConnID` 报错写 `"Parse ConnectionID failed: ..."` warning（`:2702-2704`）；`isTruncated` 打 WARN 日志并写 1105 warning（`:2706-2714`）。<br>• `gcid.ServerID != sm.ServerID()` 时调 `killRemoteConn`（`:2728-2777`），若 **`gcid.ServerID == 0` 直接报错 `"Unexpected ZERO ServerID. Please file a bug to the TiDB Team"`**（`:2729`）；否则向目标 `status_port` 发 `tipb.ExecType_TypeKill` DAG 请求。本机不存在 ID 静默 OK（`server.go:929-933`）。 | • `enable-global-kill = true` 时，不带 `TIDB` 的 `KILL QUERY <cid>` 正常生效、无 warning。<br>• `KILL` 不存在的合法格式 ID **静默成功（OK），无错误也无 warning**（不返回 MySQL `1094`）。<br>• 传入被截断的 32 位奇数 ID（如 `99999999`）返回 **`Warning 1105`**: `Kill failed: Received a 32bits truncated ConnectionID, expect 64bits. Please execute 'KILL [CONNECTION \| QUERY] ConnectionID' to send a Kill without truncating ConnectionID.`。 |
| **`KILL QUERY` vs `KILL CONNECTION`** | `pkg/server/server.go:922-974`：<br>1) 打 `[INFO] [server.go:923] [kill] [conn=...] [query=...] [maxExecutionTime=...] [runawayExceed=...]`，递增 `tidb_server_event_total{type="kill"}`（`metrics/server.go:45, 162-168`）。<br>2) 若 `!query`：置 `connStatusWaitShutdown`，并对 `bufReadConn` 设置 `SetWriteDeadline(time.Now())` 与 `SetReadDeadline(time.Now())`（直接中止进行中的网络读写并断开连接）。<br>3) `killQuery`：① `SQLKiller.SendKillSignal(reason)`（`CAS 0 -> reason`，打 `[WARN] [sqlkiller.go:66] ["kill initiated"] ["connection ID"=...] [reason=...]`)；② `conn.cancelDispatch()`；③ `SQLKiller.FinishResultSet()` 停止结果集生产（**不对 socket 设 write deadline，绝不撕裂已发送的包**）。 | • `KILL QUERY <cid>` 中断重查询：客户端收到 **`1317 (70100): Query execution was interrupted`**，连接保持存活（后续 `SELECT 1` 成功）；日志按序输出 `[INFO] [server.go:923] [kill]` → `[WARN] [sqlkiller.go:66] ["kill initiated"]` → `[WARN] [sqlkiller.go:171] ["kill finished"]` → `[INFO] [conn.go:1222] ["command dispatched failed"]`。<br>• `KILL [TIDB] <cid>`（`KILL CONNECTION`）：客户端收到连接断开（pymysql `2013 Lost connection to MySQL server during query`），后续发包失败。 |
| **执行检查点、Panic 日志与 Reset** | • `pkg/executor/internal/exec/executor.go:448, 462`（`exec.Next` 前后调 `HandleSQLKillerSignal()`），`pkg/util/sqlkiller/sqlkiller.go:118-158`：`QueryInterrupted(1)->1317`、`MaxExecTimeExceeded(2)->3024`、`QueryMemoryExceeded(3)->8175`、`ServerMemoryExceeded(4)->8176`（同时 `:153-156` 打 `"global memory controller, NeedKill signal is received successfully"` WARN）、`RunawayQueryExceeded(5)->8253`。<br>• Go 通过 `panic(err)` 传播，并在 `Compiler.Compile`（`compiler.go:62` 打 `Error "compile SQL panic"`）、`ExecStmt.Exec`（`adapter.go:539` 打 `Warn "execute sql panic"`）和 `clientConn.writeResultSet`（`conn.go:2364` 打 `Error "write query result panic"`）recover 转错误（注：`conn.go:2106, 2345` 返回值 `retryable` 的真实含义是“首轮 `rs.Next()` 前尚无副作用、存储层可重试”）。<br>• **`SQLKiller.Reset()` 调用点**：`conn.go:1384`（`dispatch` 入口）、`select.go:1006`（`ResetContextOfStmt`）、`tracker.go:370`（`Detach`）、`builtin_miscellaneous_vec.go:375`（无表/非 DML `SLEEP`）与 `conn.go:1314`（`dispatch` 退出 `defer`）。（Go v8.5.8 为 `LoadUint32`+`StoreUint32`；Rust `crates/memory/src/sqlkiller.rs` 已带 `#70776` CAS swap 修复）。 | 算子内部与结果集写出期间均能立即响应，错误通过 MySQL ERR 包返回客户端，`dispatch` 结束后信号清零不影响下一条语句；无表 `SELECT SLEEP(5)` 被杀后重置 `SQLKiller` 并返回标量 `1`。 |
| **`max_execution_time` 超时** | `pkg/executor/adapter.go:622-631`：仅对 `StmtCtx.InSelectStmt == true` 将 `GetMaxExecutionTime()`（含 `MAX_EXECUTION_TIME(ms)` hint 覆盖）写入 `ProcessInfo.MaxExecutionTime`（`COM_STMT_EXECUTE` 在 `ResetContextOfStmt` `pkg/executor/select.go:1049-1054` / Rust `crates/executor/src/select.rs:650-684` 解包内部 `PreparedAst.Stmt` 后设置 `InSelectStmt`）；`pkg/executor/select.go:317-337`（`checkMaxExecutionTimeExceeded`）；`pkg/util/expensivequery/expensivequery.go:96-100`：100ms 巡检发现 `costTime > MaxExecutionTime*ms` 后**无级别门控地**打 `[WARN] [expensivequery.go:97] ["execution timeout, kill it"]`（fields: `costTime`、`maxExecutionTime`、`processInfo`）并调 `sm.Kill(info.ID, true, true, false)`。 | 默认 `max_execution_time = 0`。设 `200` ms 后：<br>• 重查询返回 **`3024 (HY000): Query execution was interrupted, maximum statement execution time exceeded`**。<br>• `SELECT SLEEP(5)` 返回单行结果 `1`（不向客户端报错），后台仍输出 `[WARN] [expensivequery.go:97] ["execution timeout, kill it"]` → `[INFO] [server.go:923] [kill] [conn=...] [query=true] [maxExecutionTime=true] [runawayExceed=false]` → `"kill initiated"`（`[executor:3024]...`）→ `"kill finished"`。 |
| **客户端断连检测与预处理语句** | `pkg/server/conn.go:2055-2104, 2170-2173` & `conn_stmt.go:316-332`：`handleStmt` / `executePreparedStmtAndWriteResult` 对非事务 autocommit DML（含解包 `ExecuteStmt`）在执行前、其余语句在写回 `ResultSet` 阶段挂载 `IsConnectionAlive`；`pkg/server/internal/util/buffered_read_conn.go:56-88` 设 `30 * time.Microsecond`（`30µs`，第 63 行）read deadline 并 `Peek(1)`，遇 `io.EOF` 或非超时 `net.Error` 返回 `0`；`SQLKiller.HandleSignal` 以 `1s` 节流触发 `QueryInterrupted`。 | 客户端突然 TCP 关闭（`SHUT_RDWR`）：Go 在约 **1s** 内检出并触发 `QueryInterrupted`，产出 `"kill initiated"`（`1317`）→ `"kill finished"` → `"command dispatched failed"`，退出连接。 |

### 1.2 支柱二：内存配额、`OOMAction`、`servermemorylimit` 与 `expensive_query`

| 维度 | Go v8.5.8 源码位置与逻辑 | 真实 v8.5.8（unistore）实测行为 |
|---|---|---|
| **系统变量默认与作用域** | `pkg/sessionctx/variable/sysvar.go:519, 525, 1176, 1200, 1228, 1459, 1718, 2757`：<br>• `tidb_mem_oom_action = 'CANCEL'`（`ScopeGlobal`；注意 `pkg/config/config.go:919` `OOMAction: "cancel"` 仅用于老配置升级 bootstrap，运行时默认始终是大写 `'CANCEL'`）<br>• `tidb_mem_quota_query = 1073741824`（1 GiB，`ScopeGlobal \| ScopeSession`）<br>• `tidb_server_memory_limit = '80%'`（`ScopeGlobal`）<br>• `tidb_server_memory_limit_gc_trigger = 0.7`（`ScopeGlobal`）<br>• `tidb_server_memory_limit_sess_min_size = 134217728`（128 MiB，`ScopeGlobal`）<br>• `tidb_expensive_query_time_threshold = 60`（`ScopeInstance`，不存 `mysql.global_variables`）<br>• `tidb_expensive_txn_time_threshold = 600`（`ScopeInstance`，不存 `mysql.global_variables`） | `SET SESSION tidb_mem_oom_action = 'LOG'` 报错 **`1229`**（全局作用域变量）；默认值与源码完全一致。 |
| **`ResetContextOfStmt`、`MemTracker` 树、游标与全路径解绑** | `pkg/executor/select.go:997-1048`，`pkg/executor/adapter.go:589-591, 865, 1734`，`pkg/server/conn.go:2140-2156`，`pkg/server/conn_stmt.go:395-624`：<br>1) 每次语句开始：`vars.MemTracker`（`IsRootTrackerOfSess=true`）`Detach`、`UnbindActions`、`SetBytesLimit(vars.MemQuotaQuery)`、`ResetMaxConsumed()`，绑定 `Killer = &vars.SQLKiller`、`SessionID = ConnectionID`、`SQLKiller.Reset()`（`:1006`）。<br>2) `sc.InitMemTracker(LabelForSQLText, -1)` 并 `AttachTo(vars.MemTracker)`；若有 `MEMORY_QUOTA` hint（`adapter.go:589`），覆写 `vars.MemTracker.SetBytesLimit(...)`。<br>3) 根据 `variable.OOMAction` 安装 `PanicOnExceed` 或 `LogOnExceed`，强制注入 `SetLogHook(dom.ExpensiveQueryHandle().LogOnQueryExceedMemQuota)`。<br>4) 语句结束或任何早退错误（`handleNoDelay`、`compile`、`open_executor` 失败，`conn.go:2150-2155`）显式调用 `sc.DetachMemDiskTracker()`，在 `Tracker.Detach()`（`tracker.go:362-375`）摘除子结点、清空会话根 `actionOnExceed` 并调用 `parent.Killer.Reset()`。<br>5) **游标/预处理长生命周期**：`COM_STMT_EXECUTE` 开启游标后，`rowContainer` / 游标 `MemTracker` 跨越后续 `COM_STMT_FETCH` 命令继续挂在 `vars.MemTracker` 上（`conn_stmt.go:398-401`），仅在游标走完、`FETCH` 出错、`COM_STMT_RESET`（`:611`）或 `COM_STMT_CLOSE`（`:581`）时解绑关闭。 | • 无落盘算子场景下，设 `tidb_mem_quota_query = 100000` + `tidb_mem_oom_action = 'CANCEL'`：返回 **`8175 (HY000): Your query has been cancelled due to exceeding the allowed memory limit for a single SQL query. Please try narrowing your query scope or increase the tidb_mem_quota_query limit and try again.[conn=<id>]`**。<br>• 设 `tidb_mem_oom_action = 'LOG'`：查询正常返回结果，通过 `LogOnExceed -> LogOnQueryExceedMemQuota` 输出 `["memory exceeds quota"]` WARN（`D00.3` §4.4 黄金样本 2）。 |
| **Action 优先级与落盘链** | `pkg/util/memory/action.go:104-112`：`DefRateLimitPriority(4) > DefCursorFetchSpillPriority(3) > DefSpillPriority(2) > DefLogPriority(1) > DefPanicPriority(0)`；`softScale = 0.8`（`tracker.go:117`）。 | 高优先级行动先触发并 `SetFinished()`，耗尽后自动降级至 `LogOnExceed(1)` / `PanicOnExceed(0)`。 |
| **`servermemorylimit` 实例看门狗** | `pkg/util/servermemorylimit/servermemorylimit.go:35-264`（100ms tick），配合 `pkg/domain/domain.go:2807, 2833`（每 `ReadMemInterval = 300ms` 调 `ForceReadMemStats()`）：<br>1) 读 `ReadMemStats().HeapInuse`，维护 `MemoryMaxUsed`。<br>2) 若 `HeapInuse > bt`：读 `MemUsageTop1Tracker`（`bytesConsumed >= 128 MiB` 才有资格更新 Top1，`tracker.go:489-502`）：<br>   - 若 **`Top1 == nil` 或 `memUsage < limitSessMinSize`**（`:151-193`）：每 5s 打 `"global memory controller tries to kill the top1 memory consumer, but no one larger than tidb_server_memory_limit_sess_min_size is found"` WARN；<br>   - 否则查 `sm.GetProcessInfo(sessionID)` 发送 `ServerMemoryExceeded`（**`8176`**），更新指标与 50 槽 `GlobalMemoryOpsHistoryManager`（`SQL_TEXT` 截断 256 字符）。<br>3) 若杀后仍在（`info.Time == s.sqlStartTime`）：每 5s 打 `"global memory controller failed to kill the top-consumer in %ds"`；**在 `time.Since(s.lastLogTime) > 5*time.Second` 分支内部**检查达到 **60s**（`servermemorylimit.go:106-122`）时打强制告警并**仅直接调 `s.sessionTracker.Killer.FinishResultSet()`**（不额外调 `sm.Kill`）；之后执行 `Succ:`（Go 原文 `:128-130` 先 `s.reset()` 再 `CompareAndSwap(s.sessionTracker, nil)`，属 `upstream-pitfall`，因 `:155-157` 与 `tracker.go:495` 兜底，影响可缓解）。 | 实例级强杀返回 **`8176 (HY000)`**（`ErrMemoryExceedForInstance`），记录进 `INFORMATION_SCHEMA.MEMORY_USAGE_OPS_HISTORY`。 |
| **`expensive_query` 100ms 巡检与 OOM 日志** | `pkg/util/expensivequery/expensivequery.go:50-154`：<br>1) 遍历 `sm.ShowProcessList()`，事务 `>= 600s`（15s metric / 10m WARN `expensive_txn`），查询 `>= 60s` 且距上次 `> 60s` 且 `log.GetLevel() <= Warn` 打 `expensive_query` WARN；<br>2) `MaxExecutionTime > 0 && costTime > MaxExecutionTime*ms`：无级别门控地打 `"execution timeout, kill it"`（3 字段：`costTime`、`maxExecutionTime`、`processInfo`）并调 `sm.Kill(info.ID, true, true, false)`；<br>3) `LogOnQueryExceedMemQuota(connID)`（`:126-145`）：先判 `log.GetLevel() <= Warn`；`sm == nil` 打 `[INFO] "expensive_query during bootstrap phase" ["conn"=connID]`；`sm != nil` 但 `GetProcessInfo` 未找到则**静默返回**；否则打 `["memory exceeds quota"]` WARN。 | 与上述实测一致（60s 慢查、`3024`、`SLEEP(5)` 特例、`memory exceeds quota` 黄金日志）。 |

### 1.3 支柱三：控制面与后台任务

- **Go 调度特点**：HTTP `/status` & gRPC、`Domain` 同步、看门狗、`StoreLiveness` 全部由 M:N 抢占式 Go scheduler 调度。
- **Rust 协作式调度的结构性风险**：在 Tokio 协作式调度（尤其是 `NoSteal` 或有限 `ExecPool` worker）下，若控制面任务及其所使用的共享 `KvClient`（含内部 `TimestampOracle::run_tso`、`BatchConn::run` 与 tonic HTTP/2 I/O 任务）停留在 `ExecPool` 上，重查询突发会饿死 etcd DDL/`server_id` lease 续约、健康检查与 OOM/超时强杀。

---

## 2. `tidb-rs` 现状盘点（已移植 vs 缺失）

### 2.1 分层底座与现有能力对照表

| 模块 / 能力 | 现状与位置（已核验行号） | 缺失 / 缺口 |
|---|---|---|
| **`SqlKiller`** | `crates/memory/src/sqlkiller.rs:1-411` 已完整移植信号枚举 `0..=5`、`Err*` 映射（含 `8253`）、`#70776` `Reset` swap 修复（commit `05c7dd7b96`）、`FinishResultSet`、`is_connection_alive`、`handle_signal`（L254-288，L278-283 已打 `8176` `NeedKill` WARN）、`"connection ID"` 字段，并 **L175 预留 `set_on_kill`**。 | 1) 上层尚未绑定与可复位唤醒机制同步的 `KillCancel`；<br>2) 连接层未安装 `is_connection_alive`、`set_finish_func`、`in_write_result_set`；<br>3) `8176` `NeedKill` 在高频率检查点需做节流评估；<br>4) `KillExec` 未移植。 |
| **`Tracker` & `ActionOnExceed`** | `crates/memory/src/tracker.rs:1-1141`、`action.rs:1-313`、`alloc.rs:1-247` 已移植父子树（`parent.children` 持 `Arc<Tracker>` 强引用 L398，需显式 `detach()` L406-426；注意 **L416-420 `detach()` 会调 `killer.reset()`**！）、`TrackedAlloc`、`MemUsageTop1Tracker`、`ServerMemoryLimit`（L62 初值 **`0`**）、`ServerMemoryLimitSessMinSize`、`ActionOnExceed` 优先级 `4..=0`、`LogOnExceed`/`PanicOnExceed` `set_log_hook`。 | 1) `SessionVars`/`SessionShared` 无会话根 `MemTracker`/`DiskTracker`；<br>2) `crates/executor/src/select.rs:654-657` 未挂到会话根 tracker，未装 `ActionOnExceed` 与 `LogHook`；<br>3) `crates/session/src/session.rs:853` `get_dist_sql_ctx` 临时传 `sc.mem_tracker`；<br>4) `crates/executor/src/adapter.rs:682-684, 916-917` 错误假设 drop 自动解绑；缺乏不清除 `killer` 的 `detach` 变体供 Router 回退使用。 |
| **Heap Stats (`memstats.rs`)** | `crates/memory/src/memstats.rs:1-83` 已定义 `ReadMemInterval = 300ms`（L26），并用 `tikv_jemalloc_ctl` 读取 `allocated`/`active`/`mapped`/`resident`（L72-82）。 | `GlobalMstats.ts` 标有 `#[allow(dead_code)]`（L44），`read_mem_stats()`（L53-57）首次加载后永不刷新，且无人周期调 `force_read_mem_stats()`；实例限额必须看 **`stats.resident`（`MemStats.sys`）**。 |
| **`ExpensiveQuery` 日志** | `crates/session/src/expensive_query.rs:1-461` 已移植 `ProcessInfoForLog`、`gen_log_fields`、`log_expensive_query`（L183）、`log_on_query_exceed_mem_quota`（L205）与黄金样本单测。 | 1) `log_on_query_exceed_mem_quota`（L214-217）把 `sm != None` 但找不到 `ProcessInfo` 与 `sm == None`（bootstrap）混为一谈；<br>2) 无生产调用、无 100ms巡检、无 `ServerMemoryLimitHandle`。 |
| **`ExecCtx` & `exec::next`** | `crates/executor/src/exec.rs:19-28` 仅有 `stmt` 与 `pool`；`crates/executor/src/internal/exec/executor.rs:410-430, 635, 641` 有 `ExecutorKillerHandler`；`crates/executor/src/select.rs:121-151` 已移植 `check_max_execution_time_exceeded`。 | `ExecCtx` 缺少 `killer`/`cancel`/`cpu`/`store_windows` 与 `CheckpointHook`；`exec::next` 无时间片 `checkpoint`；新旧栈均无实时 `ProcessInfo`（`session.rs:810` 仍为 stub，`SHOW PROCESSLIST` SQL 也尚未实现）。 |
| **Parser / IR / SimpleExec / Router** | `crates/parser/grammar/tidb_typed.y:5212-5216` 将 `KillStmt` 降级 `Unported`（`ir.rs:329-334` 报 `1235`）；`simple.rs:1-56` 仅支持 `ADMIN FLUSH ... PLAN_CACHE`。 | 1) 需提升 `ast::Stmt::Kill`、`PlanBuilder` `1227` 鉴权、`SimpleExec::execute_kill_stmt` 及 `--new-stack=off/fallback/strict` 全模式适配；<br>2) **`crates/poc/session/src/lib.rs:1121-1159` 先 `self.router.take()` 再 `.await`**，在 `conn.rs:1082` `catch_unwind_future` 或停机 drop future 时会永久丢失 `self.router`！ |
| **连接层、ID 与 `server_id`** | `crates/mysql-protocol/src/server.rs:82, 136` 使用 `next_connection_id: AtomicU64::new(1)` 顺序递增；`conn.rs:1082` 用 `catch_unwind_future` 保护 `dispatch`；`crates/poc/domain/src/serverinfo.rs:37, 74` 硬编码 `server_id: 0`、`disable_rpc: true`；`crates/poc/server/src/rpc.rs:292-314` 不支持 `ExecType::TypeKill`。 | 1) 递增奇数 `1, 3...` 与 `ParseConnID` 冲突（误判截断 `1105`）；<br>2) **`server_id = 0` 共用在多节点/LB/TiProxy 混部下会导致 `KILL` 误杀另一节点上的无关会话，且 Go `killRemoteConn` 会报 `Unexpected ZERO ServerID`**（见 §4 与 §7.1 决策 7）；<br>3) `PacketIo`（`packet.rs:75-81`）单向结构缺断连探测；<br>4) `crates/metrics/src/lib.rs` 尚未注册 `tidb_server_event_total`。 |
| **Sysvars & Config** | `sysvar_data.rs:191, 193, 359-460` 与 `hint.rs:22-40` 已对齐相关 sysvar；`sysvar_data.rs:191, 193` 标明 `tidb_expensive_*_time_threshold` 为 `ScopeInstance`。 | `SysVarSync::rebuild`（`crates/poc/domain/src/sysvar_loader.rs:161`）传 `None` 跳过全部 `SetGlobal` hook，导致 `ServerMemoryLimit` 永久为 **`0`** 且其他全球原子量不同步（注：`defaults_gen.rs:301` 小写 `"cancel"` 与 Go `config.go:919` 一致，不改默认值）。 |

### 2.2 全部 `checkpoint(&())` 空占位与手动 `consume` / 算子接入现状清单

在新执行栈与 `tidb-store` 中共有 **13 处 `checkpoint(&())` 空占位** 与 **2 处局部 `KillerHook`**（另含 legacy 2 处、测试 1 处）：

| 文件与行号 | 当前写法 | 缺陷分析 |
|---|---|---|
| `crates/store/src/distsql/select_result.rs:380` | `let Ok(()) = tidb_runtime::checkpoint(&()).await;` | `sortedSelectResults.next` k-way merge 循环内无 kill 检查；`new_sorted_select_results`（`:170-209`）未传 `sql_killer` / `cancel`。 |
| `crates/store/src/distsql/select_result.rs:637, 645, 650` | `let Ok(()) = tidb_runtime::checkpoint(&()).await;` | `fetch_resp_with_intermediate_results` 空响应自旋与返回路径未集成统一 hook。 |
| `crates/store/src/distsql/select_result.rs:726` | `let Ok(()) = tidb_runtime::checkpoint(&()).await;` | `read_from_default` 行解码循环内无 kill 检查。 |
| `crates/store/src/distsql/select_result.rs:1219` | `let Ok(()) = tidb_runtime::checkpoint(&()).await;` | `selectResultIter.next` 切换响应循环内无 kill 检查。 |
| `crates/executor/src/distsql.rs:2781, 2860, 3185, 3207` | `let _ = stride.checkpoint(&()).await;` 紧接下一行 `handle_sql_killer_signal()?;` | 拆成两次检查，未使用统一 `ExecCtx` `CheckpointHook`。 |
| `crates/executor/src/sortexec/sort.rs:1299` | `let Ok(()) = stride.checkpoint(&()).await;` | `ResultGenerator::generate_result_from_memory` 内存归并输出循环无 Kill 检查。 |
| `crates/executor/src/sortexec/parallel_sort_worker.rs:194, 323` | `:194` 先查 `sql_killer` 后 `checkpoint(&())`；`:323` 调 `tidb_runtime::slice::checkpoint(&()).await` | `ParallelSortWorker`（`:60`）未统一持 `ExecCtx` hook，`:323` 无 kill 检查。 |
| `crates/executor/src/aggregate/agg_hash_final_worker.rs:146, 195` | `stride.checkpoint(&kill_hook).await?;`（局部 `KillerHook` L82） | 使用局部 hook 而非统一 `ExecCtx`，且与 `:161` `mem_tracker.checkpoint()` 分离。 |
| Legacy / 测试：`crates/poc/executor/src/lib.rs:166`、`crates/distsql/src/lib.rs:140`、`crates/executor/src/testutil.rs:238` | `checkpoint(&()).await` | Legacy / 测试点位（`poc-executor` 内部加入可取消检查；testutil 可接 mock `ExecCtx`）。 |

算子内存与路由现状（Part 1–2 重要前提）：
1. **手写 `consume` 与缺失 `Drop` 归零**：`ProjectionExec`（`projection.rs:249..704`，`Drop` `:514-527` 仅 abort 任务）与 `SortExec`（`sort.rs:768-780, 897-899, 1137-1138`，仅在 `close()` 清零，`Drop for ParallelTasks` `:185` 无内存清零）均缺 `Drop` 内存归零。
2. **`PhysicalHashAgg` 尚未接入新栈路由**：`builder.rs:186-279, 675-683` 无 `PhysicalHashAgg` 分发，`precheck.rs:33-34` 将 `GROUP BY`/`HAVING` 退回 PoC；`PhysicalSort`（`builder.rs:237`）与 `PhysicalTopN`（`:247`）已放行（`precheck.rs:59-68`）。另当前二进制默认 **`--new-stack=off`**，所有内存配额新能力仅对命中新执行栈的语句生效。

### 2.3 未隔离的控制面任务（含隐式 `KvClient` 内部任务）清单

| 组件 | 源码与 spawn 行号 | 当前调度位置与风险 |
|---|---|---|
| **共享 `KvClient` 内部循环** | `crates/poc/server/src/main.rs:437-442`（在 `ExecPool` 连接 `kv`），触发 `client-rust` `TimestampOracle::new`（`pd/timestamp.rs:81` `run_tso`）、`BatchConn::start`（`store/batch.rs:154` `send_loop`）与 tonic HTTP/2 I/O | 控制面服务若共用业务 `kv`，底层 TSO/TiKV/HTTP2 仍跑在 `ExecPool` 上；必须在 `ControlRuntime` 连接专用 `ctrl_kv`。 |
| `stmt-timing` | `crates/poc/server/src/main.rs:520` | `pool.spawn("stmt-timing", ...)` |
| `/status` + gRPC 服务 | `crates/poc/server/src/main.rs:540` + `crates/poc/server/src/status.rs:111` | `pool.spawn("status-server", ...)` + `conns.spawn(...)` |
| `SchemaSync` + `/tidb/server_id` | `crates/poc/domain/src/sync.rs:147, 148, 150, 620` | `tokio::spawn` 托管 `watch_global_version`、`sync_loop`、`watchdog`、`keep_alive`（若接入 `/tidb/server_id` 租约保活同样在控制面运行） |
| `PrivilegeSync` | `crates/poc/domain/src/privilege_loader.rs:97, 104, 105` | `tokio::spawn` 托管 watch 与 periodic loop |
| `SysVarSync` | `crates/poc/domain/src/sysvar_loader.rs:87, 97, 117` | 捕获主运行时 `Handle::current()` 给 OS 线程做 `fetch_rt.block_on(...)` |
| `StoreLiveness` | `client-rust/src/store/liveness.rs:179` | `tokio::spawn` 每秒探测死 TiKV 节点（对比：`batch.rs:154, 673` 与 `client.rs` 属数据面，留在 `ExecPool`） |
| 看门狗（尚未创建） | 待建 `ExpensiveQueryHandle` & `ServerMemoryLimitHandle` | 100ms 巡检必须运行在 `ControlRuntime` |

### 2.4 `layers.toml` 边界约束与并发组件放置限制

- 分层 crate 严格要求 `dep.rank < self.rank`，绝不依赖 `[legacy]` crate（`poc-server`、`tidb-mysql-protocol`、`poc-session`、`poc-domain`）。
- 通过 **`N0` §4 规则 2（接口反转）** 摆放：`ProcessInfo` / `SessionManager` 置 `tidb-sessionctx` (rank 7)；`tidb-executor::glue::SessionContext` (rank 14) 暴露 `session_manager()` 与 `mem_exceed_log_hook()`；`ExpensiveQueryHandle` / `ServerMemoryLimitHandle` 置 `tidb-session` (rank 15)；`ServerSessionManager` 与 `ConnIdAllocator` 置 `tidb-server` (rank 16，新增 `tidb-runtime` rank 1 与 `tidb-sessionctx` rank 7 依赖)；`tidb-mysql-protocol` `[legacy]` 向下依赖 `tidb-server`。

---

## 3. Rust 架构与分层设计

### 3.1 分层摆放与接口反转总览

```
[rank 1]  tidb-runtime        : ControlRuntime (2 threads), KillCancel (可复位), ChildToken (不可复位), CheckpointHook, ExecPool
[rank 1]  tidb-memory         : SqlKiller (set_on_kill, is_connection_alive, finish), Tracker (含 detach_without_resetting_killer),
                                ActionOnExceed, memstats (含 ReadMemInterval 300ms TTL 修复)
[rank 7]  tidb-sessionctx     : ProcessInfo, trait SessionManager, DistSQLContext.{sql_killer, kill_cancel}, SessionVars.{mem_root, disk_root}
[rank 11] tidb-store          : SelectResult / CopIterator / Backoffer 使用传入的 SqlKiller + KillCancel 协作取消与 time-slice checkpoint
[rank 13] tidb-planner        : PlanBuilder::build_kill 读取 PlanContext::session_manager() 做 1227 权限校验
[rank 14] tidb-executor       : ExecCtx (killer, cancel: KillCancel, cpu, store_windows, CheckpointHook); SimpleExec::execute_kill_stmt;
                                reset_context_of_stmt 经 SessionContext::mem_exceed_log_hook() 注入 LogHook
[rank 15] tidb-session        : Session 安装 on_kill->KillCancel、ProcessInfo、ExpensiveQueryHandle::run、ServerMemoryLimitHandle::run
[rank 16] tidb-server         : ServerSessionManager (impl SessionManager)、ConnIdAllocator（crates/server/Cargo.toml 增依赖 tidb-runtime, tidb-sessionctx）
[legacy]  tidb-mysql-protocol : 依赖 tidb-server；PacketIo 读写半部拆分、DisconnectDetector (poll_fill_buf)、conn_shutdown、Drop-safe 守卫
[legacy]  poc-session / server: Router 修复 router.take()、PoC 内部 cancel_select、ControlRuntime 托管 ctrl_kv/SchemaSync(+server_id)/PrivilegeSync/SysVarSync/status/rpc/watchdogs/StoreLiveness
```

### 3.2 `ExecCtx` 升级、双层取消原语（`KillCancel` vs `ChildToken`）与 `Checkpoint` 不变量

#### 3.2.1 `crates/runtime/src/cancel.rs`：可复位 `KillCancel` 与不可复位 `ChildToken`
为严格兼容 Go `SQLKiller.Reset()`（`conn.go:1314, 1384`、`select.go:1006`、`tracker.go:370`、`builtin_miscellaneous_vec.go:375` `SLEEP()`）与不可复位算子私有 `closeCh/finishCh`（`N3 §3.3` 第 2、4 条），在 `tidb-runtime` 实现两种轻量原语（`#![forbid(unsafe_code)]`，不引入 `tokio-util`）：

1. **会话级可复位取消通知器 `KillCancel`**（绑定共享 `signal: Arc<AtomicU32>`，即 `killer.signal_handle()`）：
   - 内部持有 `signal: Arc<AtomicU32>` 与 `notify_tx: tokio::sync::watch::Sender<u64>`（代际/事件序号，初始 `0`）。
   - **`notify(&self)`**：由 `SqlKiller::send_kill_signal` 的 `on_kill` 回调调用，在 `signal` 已 CAS `0 -> reason` 后递增 `notify_tx` 序号，唤醒所有异步等待者。
   - **`on_reset(&self)`**：由 `SqlKiller::reset()`（在将 `signal.swap(0, SeqCst)` 之后）调用，递增 `notify_tx` 或保持状态与 `signal == 0` 同步，**绝不留下永久卡死的 `true` 标志**。
   - **`is_killed(&self) -> bool`**：单次原子读 `self.signal.load(SeqCst) != 0`（随 `SqlKiller::reset()` 立刻复位为 `false`！）。
   - **`async fn cancelled(&self)`**（核心竞态防线）：
     ```rust
     pub async fn cancelled(&self) {
         if self.is_killed() { return; }
         let mut rx = self.notify_tx.subscribe();
         loop {
             // 每次被 watch 唤醒（或错过一次期间的变化）后，必须以真实 atomic signal != 0 为唯一真条件：
             // 若中途因 SLEEP() / Detach() 调过 SqlKiller::reset() 使 signal 重回 0，则继续挂起等待，绝不假唤醒！
             if rx.wait_for(|_| self.is_killed()).await.is_ok() {
                 if self.is_killed() { return; }
             } else {
                 std::future::pending::<()>().await;
             }
         }
     }
     ```
2. **算子级不可复位子令牌 `ChildToken`**（对应 `N3 §3.3` 第 4 条算子私有 `closeCh`/`finishCh`，如 `Limit` 提前结束）：
   - 由局部 `AtomicBool` + `watch::Sender<bool>` 实现，单向触发 `cancel()`、不随 `SqlKiller::reset()` 复位，仅用于通知算子内部子任务正常早停。

#### 3.2.2 `ExecCtx` 定义、`CheckpointHook`、`SLEEP` 与算子收敛
将 `crates/executor/src/exec.rs` 从 `{ stmt, pool }` 升级为 `foundation-interfaces §2` 完整形态：

```rust
#[derive(Clone)]
pub struct ExecCtx {
    pub stmt: Arc<StatementContext>,
    pub pool: Option<ExecPool>,
    pub killer: Arc<SqlKiller>,
    pub cancel: KillCancel,
    pub cpu: Option<CpuRecorder>,
    pub store_windows: Option<Arc<StoreWindows>>,
}
```

- **修复 `ExecCtx::check()` 实现（消除冗余分支与误用）**：
  ```rust
  impl CheckpointHook for ExecCtx {
      type Error = Error;

      #[inline]
      fn check(&self) -> Result<(), Error> {
          // 1) 1s 节流 is_connection_alive 探针；2) signal 1..=5 映射准确 Go 错误（含 8176 NeedKill 日志）；0 返回 Ok(())
          self.killer.handle_signal()
      }
  }
  ```
  提供便捷异步方法 `pub async fn checkpoint(&self) -> Result<(), Error> { tidb_runtime::checkpoint(self).await }`。算子内部的 `ChildToken` 只控制该算子自己的 `select!` 正常结束分支（如 `Limit` 截断），**绝不允许伪造 `ErrQueryInterrupted(1317)`**。
- **关键不变量——为什么 `ExecCtx::check()` 绝对不调用 `stmt.mem_tracker.checkpoint()`**：
  如 `crates/memory/src/tracker.rs:525-535`（`A1` §3.3）所证，`Tracker::checkpoint()` 会清空所有祖先 `grew` 标志。若祖先 `ExecCtx::check()` 提前调用，会破坏叶子 `TrackedAlloc` 的单次增长语义。所有分配了内存的叶子算子继续自行调 `self.mem_tracker.checkpoint()?`；需要组合检查的循环使用 `OpCheckpoint { ctx, mem }` 先 `mem.checkpoint()?` 后 `ctx.check()?`。
- **per-statement `SqlKiller` ↔ `KillCancel` 绑定**：
  `SessionShared` 与 `SqlKiller` 持有成对绑定的 `kill_cancel: KillCancel`（`on_kill` 触发 `kill_cancel.notify()`，`on_reset` 触发 `kill_cancel.on_reset()`）。即使语句发生跨 `await`、内部 `reset()`（如 `SLEEP`），所有持有 `ExecCtx.cancel` 或 `DistSQLContext.kill_cancel` 的等待点与 `SqlKiller` 状态永远保持单一事实来源。
- **`SLEEP()` 对齐契约（待内置 `SLEEP()` 移植时遵守 Go `builtin_miscellaneous_vec.go:370-378`）**：
  当无表且非 INSERT/UPDATE/DELETE 的 `SELECT SLEEP(N)` 在 `tokio::select! { _ = ctx.cancel.cancelled() => ... }` 唤醒并确认 `killer.handle_signal().is_err()` 时，必须调用 **`killer.reset()`** 并返回标量 `1`（连接与会话均正常继续，不返回 `1317`/`3024`）；若当前语句带有表或属于 DML，则不重置 `killer`，错误按正常链路返回。
- **`tidb-store`、`client-rust` 与所有算子 `checkpoint(&())` 统一替换**：
  - `crates/store/src/distsql/select_result.rs`：**删除全部 6 处 `checkpoint(&())`**（`:380, :637, :645, :650, :726, :1219`）。`tidb-sessionctx::distsql_ctx::DistSQLContext` 新增 `pub kill_cancel: Option<KillCancel>`；为 `(Option<&Arc<SqlKiller>>, Option<&KillCancel>)` 实现 `CheckpointHook<Error = Error>`：
    - `new_sorted_select_results`（`:170-176`）与 `SortedSelectResults`（`:197-209`）接入 `sql_killer` + `kill_cancel`；
    - `CopIterator::recv_from_resp_ch`（`copr/coprocessor.rs:1304-1337`）在 `tokio::select!` 增加 `_ = kill_cancel.cancelled() => ...` 分支；
    - `Backoffer::{backoff, backoff_with_max_sleep_txn_lock_fast}`（`driver/backoff.rs:89, 101`）与 `ClientRustBackoffer`（`copr/client_rust.rs:57-85`）接入 `select! { _ = kill_cancel.cancelled() => ..., res = ... => res }`。明确 **`client-rust` 不读取 `signal_handle()`**，而是靠外层 `select!` drop RPC future 触发 `Entry::is_canceled()`（`client-rust/src/store/batch.rs:108-112, 215-218`）与 tonic stream 取消。
  - `crates/executor/src/distsql.rs`（`:2781, :2860, :3185, :3207`）：合并为 `stride.checkpoint(ctx).await?;`。
  - `crates/executor/src/sortexec/sort.rs:1299`：`ResultGenerator` 改为 `stride.checkpoint(&self.ctx).await?;`。
  - `crates/executor/src/sortexec/parallel_sort_worker.rs:194, 323`：`ParallelSortWorker`（`:60`）改为 `stride.checkpoint(&self.ctx).await?;` 与 `self.ctx.checkpoint().await?;`。
  - `crates/executor/src/aggregate/agg_hash_final_worker.rs:82, 146, 195`：删除局部 `KillerHook`，改为 `stride.checkpoint(ctx).await?;`。
  - `crates/executor/src/internal/exec/executor.rs:635-641`：`exec::next` 视路径开销接入 `ctx.checkpoint().await?`。
  - `ProjectionExec` 与 `SortExec`：Part 1 将 `ProjectionExec` 迁至 `TrackedAlloc` + `checkpoint()?`；**同时必须为 `SortExec`（`sort.rs:768-780, 897-899, 1137-1138`）及移行期 `ProjectionExec` 补齐 `Drop` impl 内存归零安全网（`if let Some(t) = &self.mem_tracker { let _ = t.replace_bytes_used(0); }`）**。

### 3.3 `KILL` 全链路、`server_id`、权限校验、断连探测与 Drop-Safe 清理顺序

#### 3.3.1 Parser / AST / Plan / SimpleExec、`server_id` 与混部安全、Router 状态机
1. **Parser & AST（`tidb-parser` + `tidb-sql`）**：
   - 将 `crates/parser/grammar/tidb_typed.y:5212-5216` 中的 `KillStmt#0..3` 升格为 `ast::Stmt::Kill(Box<KillStmt>)`。
   - 表达式分支非 `CONNECTION_ID()` 时按 Go `simple.go:2672` 报错 `"Invalid operation. Please use 'KILL TIDB [CONNECTION | QUERY] [connectionID | CONNECTION_ID()]' instead"`。
2. **权限检查（`tidb-planner` `PlanBuilder::build_kill`）**：
   - 对齐 Go `pkg/planner/core/planbuilder.go:3927-3945`：自身连接/`CONNECTION_ID()` 放行（普通用户可自杀返 `1317`）；目标为他人时检查 `CONNECTION_ADMIN` / `SUPER` 动态权限，缺权返 `1227 (42000)`；不存在连接交执行器静默返回 `OK`（不处理 `GlobalAutoAnalyzeProcessList` 与 SEM，因只读节点无 auto-analyze 且未启用 SEM）。
3. **`server_id` 租约与 32/64 位连接 ID 分配（彻底消除多节点/LB/TiProxy 误杀风险）**：
   - **为什么必须消除共享 `server_id = 0`（及跨节点复用同一 `server_id`）**：
     - 如果所有 `tidb-rs` 节点共享 `server_id = 0`（或相同 `server_id`），当通过负载均衡（LB）或 TiProxy 发起 `KILL`（例如 `mysql` 客户端按 Ctrl-C 会新建一条连接到随机节点执行 `KILL QUERY <cid>`）时，接收 `KILL` 的节点见到 `gcid.ServerID == self.server_id()`，会把**另一台节点上的连接误判为本机连接**，从而杀掉本机恰巧分到相同 `local_id` 的**完全无关用户会话**！
     - 同时 Go `simple.go:2729` 对 `ServerID == 0` 直接拒杀报 `"Unexpected ZERO ServerID"`；且 `server_id = 0` 在 64 位升级时会触发 `id >> 32 == 0` 被 `ParseConnID` 误判为截断 ID 而无法被杀死（`globalconn.go:115, 269-296`）。
   - **已定方案（负责人 `zhigang-du` 2026-11-28 确认，见 §7.1 决策 7）**：
     - **单机部署（`--schema-sync=false` / 无 etcd）**：固定使用 `serverIDForStandalone = 1`（`domain.go:1380, 1485`），配合 `ConnIdAllocator`（`globalconn.go:50-317`，`local32` 20 位从 `1` 起分配，用尽或 `server_id > 2047` 自动升 64 位奇数），**首条连接 `CONNECTION_ID()` 为 `(1 << 21) | (1 << 1) = 2097154`**（对齐 Go 单机行为！）；
     - **集群部署（`--schema-sync=true`）**：在 `ControlRuntime` 上按 Go 协议实现 `/tidb/server_id/<id>` 租约申请（优先 `[1, 2047]`，冲突/90% 升级 `[2048, MaxServerID64]`、`12h` TTL、`5m` 续约、`6h` PD 失联自杀 `KillAllConnections()`、停机 `releaseServerID()`），并将真实 `server_id` 写入 `ServerInfo.server_id`；`SimpleExec::execute_kill_stmt` 对 `gcid.ServerID != sm.server_id()` 调用 `killRemoteConn` 向对应 `ServerInfo` 的 `ip:status_port` 发送 `ExecType::TypeKill`，`poc-server/src/rpc.rs` 支持处理 `ExecType::TypeKill`！
     - **防误杀底线**：在集群/多节点模式下，若节点尚未成功取得独立唯一 etcd `/tidb/server_id` 租约（或租约失联期间），绝不允许把共享 `server_id` 的全局格式 ID 当成本机连接静默杀掉，必须明确拒绝或告警。
     - **完整 Warning 对齐（`simple.go:2681-2714`）**：补齐关闭 `enable-global-kill` 时不带 `TIDB` 的 Warning、表达式错误、`ParseConnID` 失败 Warning、以及 `isTruncated` 的后台 WARN 日志 + `Warning 1105`。
4. **PoC / 新栈混合期状态机、Drop-Safe 与路由契约**：
   - **主路径 + 全路径 Drop-Safe（对齐 `N3 §3.1` 第 6 条与 `conn.rs:1082` `catch_unwind_future`）**：
     - 正常语句、超时、`KILL QUERY`、OOM 均以**内部协作式取消为主路径**（保证走完显式 `exec::close` 并收集运行时统计与写回 ERR 包）；
     - 同时因为 `catch_unwind_future`（`conn.rs:1082`）、停机 `KillAllConnections` 超时与连接异常关闭会在**任意 `.await` 点直接 drop 语句 future**，所有资源守卫（`RouterGuard`、`StmtTrackerGuard`、`OnStmtEnd`、`SortExec::Drop`、`ProjectionExec::Drop`、并行 worker `JoinHandle` abort）必须实现 **100% Drop-safe**（在任意 `.await` 点被 drop 时零泄漏、零 `router` 丢失、零内存漂移）。
   - **修复 `self.router.take()` 状态丢失缺陷（`crates/poc/session/src/lib.rs:1121-1159`）**：使用 RAII `RouterGuard` 自动把 `router` 放回 `self.router`（或拆分借用 `Session` 状态彻底去除 `.take()`）。
   - **PoC 内部取消摆放与 `--new-stack=off` 适配**：PoC 取消 `select!`（针对 `kill_cancel.cancelled()` / `conn_shutdown.cancelled()`）**必须放在 `poc-executor` 内部算子等待点**，**绝不**包裹外层 `router.query`！无论 `--new-stack=off|fallback|strict`，`ast::Stmt::Kill` 均经统一 `PlanBuilder` / `PrivilegeSource` 鉴权 + `ServerSessionManager::kill` 执行；`poc-session` 执行期间同步向 `ServerSessionManager` 注册/清空 `ProcessInfo`（含严格的 `InSelectStmt` 门控，仅 `SELECT` 填充 `MaxExecutionTime`）。
   - **Kill/OOM 永不触发 Router 回退**：`router/mod.rs:827-863` 对 `1317`/`3024`/`8175`/`8176`/`8253`（`blocked_by(&e) == None`）严禁回退至 PoC，直接返回给客户端。

#### 3.3.2 并发安全的断连探测（`DisconnectDetector`）与写结果集（`FinishResultSet`）边界契约

1. **读写拆分（`PacketReader<R>` / `PacketWriter<W>`，保留共用状态）**：
   - 握手与 TLS 升级阶段继续使用完整 `PacketIo`（保留 `into_parts()` 语义）；命令循环启动后用 `tokio::io::split(maybe_tls)` 拆分为 `ReadHalf<MaybeTls<S>>` 与 `WriteHalf<MaybeTls<S>>`：
     - **共享协议状态**：共享 `seq: Arc<AtomicU8>`（每条命令前 `reset_sequence()` 归 `0`）与 `counts`（读/写字节计数每命令刷新至 `tidb_server_packet_io_bytes`）。
     - **`PacketReader`（`Arc<tokio::sync::Mutex<PacketReaderInner<R>>>`）**：持有 `BufReader<ReadHalf<MaybeTls<S>>>`（16 KiB 容量），idle 时由 `read_packet().await` 持有，SQL 执行与写结果集期间未上锁。
   - **`is_connection_alive(&self) -> bool` 实现**（安装时机对齐 Go `conn.go:2087-2104, 2170-2173` 及 `conn_stmt.go:316-332`：解包 `ExecuteStmt` 后对非事务 autocommit DML 执行前、对其他语句在写回 `ResultSet` 期间挂载，1s 节流调用）：
     - `try_lock()` 失败或 `!reader.buffer().is_empty()` 直接返回 `true`；
     - 在 `BufReader<ReadHalf<MaybeTls<S>>>` 上非阻塞轮询 **`AsyncBufRead::poll_fill_buf`**（严禁绕过 `BufReader` 直接调 1 字节 `poll_read`！）：
       - `Poll::Ready(Ok(buf))` 且 `!buf.is_empty()` → 管道字节安全保留在 `BufReader` 中不丢包，返回 `true`；
       - `Poll::Ready(Ok(&[]))`（EOF / `close_notify`）或 `Poll::Ready(Err(_))` → 返回 `false`（触发 `QueryInterrupted`）；
       - `Poll::Pending`（含 `tokio::io::split` `BiLock` 写并发争用或 TLS 1.3 `KeyUpdate` 内部状态让出）→ 保守返回 `true`（未知视存活，对齐 Go `IsAlive() < 0`）。
2. **`KILL QUERY`（`FinishResultSet`）vs `KILL CONNECTION`（`conn_shutdown`）包边界规则**：
   - `ServerSessionManager`（位于 **`tidb-server` rank 16**）维护每连接 `ConnControl`：`killer: Arc<SqlKiller>`、`kill_cancel: KillCancel`、`finish_cancel: ArcSwapOption<ChildToken>`（仅写结果集期间存在）、`conn_shutdown: ChildToken`（`KILL CONNECTION` / `KillAllConnections` 触发）、`status: AtomicU8`。
   - **关键协议安全不变量（修正 Go `retryable` 类比）**：
     - **`KILL QUERY`（`FinishResultSet` / `finish_cancel`）只停止继续拉取行（`ResultSet::next`）与发送新包；绝对不允许打断正在向 socket 写入的单个完整 MySQL 包或 `flush`！** 也就是说，`finish_cancel` 仅在`ResultSet::next` 与**完整包边界之间**检查——**严禁**对底层 `PacketWriter::write_packet` / `flush` 的单次异步 IO 套 `select! { _ = finish_cancel => ... }`。已发送的完整行包之后再紧跟发送完整 `ERR` 包（`1317`/`3024`/`8175`/`8176`），客户端正常收到 ERR 且 TCP 连接**保持存活**！（注：Go `conn.go:2106, 2345` 返回的 `retryable == false` 表示“已在 `firstNext` 后开始向外产出行、存储侧不可自动重试”，而非半包破损。）
     - **仅 `KILL CONNECTION`（`conn_shutdown`，等价 Go `server.go:936-948` 设置读写 deadline）**允许直接在阻塞 socket 读写 `.await` 上通过 `select! { biased; _ = conn_shutdown.cancelled() => ... }` 强行打断并立刻关闭连接（客户端收到 `2013`）。

#### 3.3.3 严格的 7 步资源清理顺序（状态机与 Drop-Safe 不变量）

```
[1] SendKillSignal (CAS 0 -> reason)
      ├──> 打印 WARN "kill initiated" ("connection ID", "reason")
      └──> on_kill 触发 KillCancel::notify()
             (+ 若 KILL CONNECTION: status=WaitShutdown, conn_shutdown.cancel())
      │
      ▼
[2] 任务协同唤醒与下游终止
      ├──> Executor/DistSQL/SelectResult/Backoffer 在 select! 与 CheckpointHook 返回 Err(killed)
      └──> tidb-store Drop 被取消的 RPC future -> client-rust Entry::is_canceled() 与 tonic stream 中止上游
      │
      ▼
[3] 并行算子内部收尾（ProjectionExec / SortExec / HashAggExec / IndexLookUpExecutor）
      ├──> 置算子私有 ChildToken (finish_ch)，await 全部 worker JoinHandle，取回 fetcher 暂管的子执行器
      └──> 排空 channel Chunk；若整体 future 在任意 .await 被 Drop，则 Drop guard abort worker 并执行安全清理
      │
      ▼
[4] 自顶向下执行器树 Close 与内存归零
      ├──> exec::close(&mut root) 依次关闭各子算子
      └──> 析构 ColAlloc::Tracked 并调 alloc.flush()；SortExec / ProjectionExec 在 close() 与 Drop 均保障 replace_bytes_used(0)
      │
      ▼
[5] 语句收尾、全路径解绑与 SqlKiller::Reset 时序
      ├──> RecordSet 路径: CloseRecordSet -> sc.detach_mem_disk_tracker() (含 mem_root.unbind_actions + killer.reset())
      ├──> 无 RecordSet / 早退路径 (Compile/Open/PointGet Err、handle_no_delay Ok(None)/Err、future Drop):
      │    RAII StmtTrackerGuard::Drop 执行 sc.detach_mem_disk_tracker()
      ├──> 【Router 合法回退特例】StmtCheckpoint::restore() 对被弃置的 current stmt_ctx 调 detach_without_resetting_killer():
      │    摘除 mem_root.children 与 action，严禁调 killer.reset()，完整保留在途 KILL！
      └──> [注] COM_STMT_EXECUTE 开启的游标在 FETCH 期间保留其 tracker，于游标结束/错退/COM_STMT_RESET/COM_STMT_CLOSE 解绑
      │
      ▼
[6] 审计日志、协议回包与入口/出口复位
      ├──> 打印 WARN "kill finished" (由 SqlKiller::reset() 于 dispatch 出口 conn.go:1314 语义触发，若此前已被 detach 复位则以首处复位日志为准)
      ├──> 打印 INFO "command dispatched failed"（9 字段）
      ├──> 每次 dispatch 入口/出口均确保 SqlKiller::reset()（conn.go:1384, 1314；select.go:1006）并清连接存活/finish_func 钩子
      ├──> KILL QUERY / 超时 / OOM：向客户端发送完整 ERR 包，连接保持存活；KILL CONNECTION / 断连：注销连接与回收 ConnID
      │
      ▼
[7] 看门狗滞后解除
      └──> ExpensiveQueryHandle / ServerMemoryLimitHandle 在下个 100ms tick 清理状态与记录
```

### 3.4 内存配额、OOM Action、日志接线与 `D01.10` 边界

#### 3.4.1 会话根 Tracker、`ResetContextOfStmt`、全局 Sysvar 同步与全路径解绑
1. **会话根 Tracker 归属**：在 `SessionShared` / `SessCtx` 创建全局唯一持久的 `mem_root: Arc<Tracker>`（`is_root_tracker_of_sess = true`、`label = LabelForSession`、`bytes_limit = vars.mem_quota_query`）与 `disk_root: Arc<Tracker>`。
2. **`SysVarSync::rebuild` 对全局内存/OOM 原子量的白名单初始化与实时同步**：
   - 解决 `sysvar_loader.rs:161` 传 `None` 导致 `ServerMemoryLimit`（`tracker.rs:62`）永久为 `0` 的问题：在屏蔽写/DDL/PD 等副作用 hook 前提下，每次 rebuild（及启动默认值兜底）对白名单只读全局原子量同步：`tidb_server_memory_limit`（`"80%"`）、`tidb_server_memory_limit_sess_min_size`（`128 MiB`）、`tidb_mem_oom_action`（`"CANCEL"` / `"LOG"`）、`tidb_enable_tmp_storage_on_oom`、`tidb_query_log_max_len`。
   - **保持 `crates/base/src/config/defaults_gen.rs:301` 小写 `"cancel"` 不动**（对齐 Go `pkg/config/config.go:919`，它仅用于老配置升级 bootstrap，运行时默认值由 `DefTiDBMemOOMAction = "CANCEL"` 定义；比较端保持 `eq_ignore_ascii_case("CANCEL")` 即可）。
   - 明确 `tidb_expensive_query_time_threshold` 与 `tidb_expensive_txn_time_threshold`（`sysvar_data.rs:191, 193`）属 **`ScopeInstance`**（不存 `mysql.global_variables`），由单机 `SET GLOBAL` 更新原子量。
3. **`reset_context_of_stmt`（`crates/executor/src/select.rs:644-657`）与 `get_dist_sql_ctx`（`session.rs:853`）**：
   - `reset_context_of_stmt` 重置 `mem_root`、`disk_root`，调 `killer.reset()`（`select.go:1006`）后绑定 `conn_id` 与 `killer`，按 `OOMAction` 安装 `PanicOnExceed` 或 `LogOnExceed` 并注入 `ctx.mem_exceed_log_hook()`，将语句 `sc.mem_tracker` 挂载至 `mem_root`（`EnableTmpStorageOnOOM` 时将 `disk_tracker` 挂至 `disk_root`）；保留 `MEMORY_QUOTA` hint 覆盖入口；`get_dist_sql_ctx` 传入 `Some(Arc::clone(&mem_root))`。
4. **全路径解绑、回退保护与预处理/游标生命周期**：
   - 更正 `crates/executor/src/adapter.rs:681-684, 916-917` 错误注释（因 `mem_root.children` 持强引用，`Drop` 不会自动解绑！）；
   - `CloseRecordSet` 与 `StmtTrackerGuard`（覆盖 `compile`/`open_executor`/`select_for_update`/`handle_no_delay`/`txn_start_ts`/`point_get` 无 `RecordSet` 返回、早退错误与 future Drop）显式调用 `sc.detach_mem_disk_tracker()`；
   - **`StmtCheckpoint::restore()`（`checkpoint.rs:95-113`）特例**：当发生合法 `Blocked` 回退时，先对被放弃的当前 `vars.stmt_ctx()` 调用 **`detach_without_resetting_killer()`**（摘除 `children` 与 action，**绝不重置 `killer.signal`**），再覆盖 `vars.set_stmt_ctx(cp.stmt_ctx)`；而 Kill/OOM 错误绝不进入 `restore()`。
   - **预处理语句与游标生命周期（`conn_stmt.go:395-624`）**：`COM_STMT_CLOSE`（`conn.rs:1251`）、`COM_STMT_RESET`（`:1260`）、`RESET_CONNECTION`（`:1280`）触发时确保清理对应预处理语句与关联状态；待后续 `D12.12` 开放 `COM_STMT_FETCH` 时，游标 `RowContainer`/`ResultSet` 的 `MemTracker` 跨命令持续挂载在 `vars.MemTracker` 下直至游标耗尽、错误或 `STMT_RESET`/`STMT_CLOSE` 解绑（`SqlKiller` 仍每命令复位）。

#### 3.4.2 日志合约接线（`#464` & `D00.3` §4.4 黄金样本 2）
- 修复 `crates/session/src/expensive_query.rs:205-217`：`sm == None`（bootstrap 未注入 `SessionManager`）且 `logger.level() <= Warn` 时打 `[INFO] "expensive_query during bootstrap phase"`；`sm == Some(_)` 但 `get_process_info(conn_id)` 返回 `None` 时**直接静默返回**；命中则输出 `[WARN] "memory exceeds quota"` 14 字段黄金日志。

#### 3.4.3 与 `D01.10`（磁盘溢写）的边界
- **本 spec 不实现 `D01.10`**：因此虽然系统变量 `tidb_enable_tmp_storage_on_oom` 默认 `ON`，当前 Go 中可落盘通过的 `Sort`/`TopN`/`HashAgg` 超限查询在 `tidb-rs` 会直接报 **`8175`**（或 `LOG`），已在 §6 明确登记为临时用户可见差异。
- `Tracker` 优先级链 `DefRateLimitPriority(4) > DefCursorFetchSpillPriority(3) > DefSpillPriority(2) > DefLogPriority(1) > DefPanicPriority(0)` 保持 100% 就绪；`builder.rs` 暂保持 `HashAggExec.session_mem_tracker` 为 `None` 直至 `D01.10` 落地（防止执行中途触软限报错 `blocked-by:D01.10` 引发 Router `bug_blocked_after_open` `1235`）。

### 3.5 `servermemorylimit` 与 `expensive_query` 后台看门狗

#### 3.5.1 `ServerMemoryLimitHandle` 与 `memstats.rs` TTL 修复
- **修复 `crates/memory/src/memstats.rs:44-57`**：在 `read_mem_stats()` 中增加 `s.ts.elapsed() >= ReadMemInterval (300ms)` 自动刷新（也可由 `ControlRuntime` 300ms ticker 协同驱动，避免业务热路径调 `epoch::advance()`）。
- **状态机（100ms tick，读取 jemalloc `stats.resident` / `MemStats.sys`）**：
  1. `is_killing == true` 时：查 `sm.get_process_info(session_id)`；若存在且 `info.time == sql_start_time`，每 5s 打 `"global memory controller failed to kill the top-consumer in %ds"`；**仅在该 5s 日志分支内部**（严格对齐 `servermemorylimit.go:106-122`）判 `seconds >= 60` 时打强制告警并**仅直接调 `s.session_tracker.killer().finish_result_set()`**（不额外调 `sm.kill`），之后进入结束清理。
  2. 结束清理：照 Go `servermemorylimit.go:128-134` 移植（同时开具 `upstream-pitfall` + `go-bug-kept` issue，详见 §7.3 第 4 条）——置 `IsKilling = false`，打 `"global memory controller killed the top1 memory consumer successfully"`（**无 `runtime.GC()`**）。
  3. `bt = ServerMemoryLimit.load() > 0 && resident > bt`：若 `Top1 == None` 或消费 `< 128 MiB`（`servermemorylimit.go:151-193`），CAS 清空并每 5s 打 `"no one larger than tidb_server_memory_limit_sess_min_size is found"` WARN；否则发送 `ServerMemoryExceeded`（`8176`）、记录 `SessionKillTotal`/`SessionKillLast`/`IsKilling` 与 50 槽环形缓冲 `MEMORY_USAGE_OPS_HISTORY`。
  4. **关于 `8176` `NeedKill` WARN 节流**：Go `sqlkiller.go:153-156` 在每次 `HandleSignal` 都打 `"global memory controller, NeedKill signal is received successfully"`；由于 Rust 算子 `ExecCtx::check()` 检查密度更高，按每条语句/每秒节流（记录在 §6）。

#### 3.5.2 `ExpensiveQueryHandle`（100ms 巡检）
- 每 100ms 读 `ScopeInstance` 阈值并遍历 `sm.show_process_list()`：
  1. 事务 `>= 600s`：15s 报 metric、10m 打 `expensive_txn`；
  2. 语句非空、`cost_time >= 60s`、距上次 `> 60s` 且 `logger.level() <= Warn`：打 `expensive_query`（黄金样本 1）；
  3. `info.max_execution_time > 0 && cost_time > Duration::from_millis(info.max_execution_time)`：**无级别门控地**打 `[WARN] "execution timeout, kill it"`（**精确 3 字段：`costTime`、`maxExecutionTime`、`processInfo`**），并调 `sm.kill(info.id, true, true, false)` → `3024`（已修订 `N3 §3.3`，见 §7.1 决策 9）。

### 3.6 控制面运行时隔离（`#474 R-5` / `N3 §3.6`）与停机序

```
+-------------------------------------------------------------------+
| 数据面运行时: tidb_runtime::Runtime (ExecPool: Steal / NoSteal)    |
|   • TCP listener & MySQL Conn (连接 task == 语句 task, 零交接)    |
|   • 算子并行 worker + 数据面 KvClient (kv: TimestampOracle/batch)  |
+-------------------------------------------------------------------+
         │ 无锁原子 / Arc<SqlKiller> / KillCancel / ArcSwap
         ▼ (严禁控制面 .await 数据面执行体或跨越 await 持有数据面锁)
+-------------------------------------------------------------------+
| 控制面运行时: tidb_runtime::ControlRuntime (2 threads, tidb-rs-ctrl)|
|   • 控制面专用轻量 KvClient (ctrl_kv: 在 ctrl_rt 连接 1 条/store)  |
|   • SchemaSync (+ /tidb/server_id 租约/5m keepalive/6h 断连自杀)  |
|   • PrivilegeSync / SysVarSync (传入 ctrl_handle + ctrl_kv)        |
|   • /status HTTP 与 rpc.rs gRPC (+ TypeKill) / stmt-timing         |
|   • ExpensiveQueryHandle (100ms) & ServerMemoryLimitHandle (100ms) |
|   • client-rust StoreLiveness (注入 ctrl_handle 到 liveness.rs:179)|
+-------------------------------------------------------------------+
```

- **优雅停机顺序（对齐 `poc/server/src/main.rs:567-592` & `D03.1` §4.2 & `domain.go:3262`）**：
  1. 信号抵达 → 关闭 `/status` HTTP + gRPC（≤ 5s）；
  2. 停止 `priv_sync.shutdown()`、`sys_vars.shutdown()`、`sync.shutdown().await`（撤销 etcd DDL 租约与删除 `/tidb/server_id/<id>`）；
  3. 停止接收新连接，`Server::serve` 进入 `DrainClients`（≤ 15s）与 `KillAllConnections`（≤ 1s），此时两个看门狗仍运行；
  4. `Server::serve` 返回后关闭并 join `ExpensiveQueryHandle` 与 `ServerMemoryLimitHandle`；
  5. 停止 `StoreLiveness`、drop `ctrl_kv` 并调用 `ControlRuntime::shutdown_timeout(5s)`，随后 `ExecPool` 析构并 `tidb_log::sync()` 退出。

---

## 4. 外部契约（External Contract）

本节集中锁定 `#542` 涉及的全部跨进程、跨协议、资源阈值及可观测性外部契约（对齐 `CLAUDE.md` §4）。

1. **etcd 键、租约与停机顺序**：
   - `/tidb/server_id/<server_id>`（已定，见 §7.1 决策 7）：TTL `12h`，每 `5m` renew lease 并更新 `ServerInfo.server_id`（`/tidb/server/info/<ddl_id>`）；与 PD 断连超 `6h` 调用 `KillAllConnections()` 防止相同 `server_id` 重配引发误杀；优雅停机先撤销租约并删除 `/tidb/server_id/<server_id>` 后再关闭其余客户端（单机 `--schema-sync=false` 使用 `server_id = 1`，不访问 etcd）。
2. **新增后台资源占用与外部连接数**：
   - 控制面隔离引入固定 **2 线程** `ControlRuntime`（`tidb-rs-ctrl`）；
   - 控制面 `ctrl_kv` 向 PD 建立独立通道，向集群每个 TiKV store 仅多开 **1 条**轻量 gRPC 连接（仅在启动与 DDL/Privilege/SysVar 变更或 30s/periodic tick 时微流量使用）。
3. **HTTP / gRPC 路由变化**：
   - `/status`（`GET /status` 返回 `{"connections":N,"version":...,"git_hash":...}`）、`GET /metrics` 与 gRPC 服务整体迁移至 `ControlRuntime` 执行，重 SQL 下 P99 响应不再被 `ExecPool` 饿死；
   - `poc-server/src/rpc.rs` 新增支持 `ExecType::TypeKill`（接收来自其他 TiDB 节点转发的 `tipb.Kill{ConnID, Query}`，经 `ServerSessionManager::kill` 生效后返回空响应）。
4. **Wire 协议、错误码、Warning 与阈值汇总**：
   - **连接 ID 编码**：启用 `enable_global_kill=true` & `enable_32bits_connection_id=true` 时，32 位编码 `(server_id << 21) | (local_id << 1)`（单机 `server_id=1` 首连 `2097154`）；20 位耗尽或 `server_id > 2047` 切 64 位 `1 | (local_id << 1) | (server_id << 41)`；
   - **`KILL QUERY` 包边界**：只在整包之间生效，`ERR` 包跟在完整行包之后，连接不断；仅 `KILL CONNECTION` 立刻掐断 socket；
   - **核心错误码与 SQLSTATE**：`1317 (70100)`（`ErrQueryInterrupted`）、`3024 (HY000)`（`ErrMaxExecTimeExceeded`）、`8175 (HY000)`（`ErrMemoryExceedForQuery`）、`8176 (HY000)`（`ErrMemoryExceedForInstance`）、`8253 (HY000)`（`ErrResourceGroupQueryRunawayInterrupted`）、`1227 (42000)`（`ErrSpecificAccessDenied`, `"SUPER or CONNECTION_ADMIN"`）；
   - **核心 Warning**：截断 ID `1105`；`enable-global-kill=false` 非法格式 `Invalid operation...`；`Parse ConnectionID failed: ...`；跨节点失败 `KILL remote connection failed: ...`；
   - **默认阈值**：`tidb_mem_quota_query = 1 GiB`、`tidb_mem_oom_action = 'CANCEL'`、`tidb_server_memory_limit = '80%'`（按 jemalloc `stats.resident`）、`tidb_server_memory_limit_sess_min_size = 128 MiB`、`tidb_expensive_query_time_threshold = 60s`、`tidb_expensive_txn_time_threshold = 600s`、探活节流 `1s`（探测读阻塞超时等同瞬时 non-blocking）、看门狗周期 `100ms`、强制 `FinishResultSet` 超时 `60s`。
5. **Prometheus 指标与标签（对齐 `pkg/metrics/server.go`）**：
   - **新增**：`tidb_server_event_total{type="kill"}`（在 `crates/metrics/src/lib.rs` 注册并初始化 `type="kill"` 计数，在 `ServerSessionManager::kill` 递增）；
   - **复用**：`tidb_server_connections{resource_group="default"}`（`/status` 数据源）、`tidb_server_memory_usage{module,type}`、`tidb_server_execute_error_total`。

---

## 5. 分阶段实施计划（Part 1 – Part 4）

| Part | 范围与涉及 Crates / 文件 | 核心交付内容 | `layers.toml` & 整行黄金 / E2E 验收门槛 |
|---|---|---|---|
| **Part 1**<br>取消、双层令牌与 `KILL` 全链路 | `tidb-runtime`: `cancel.rs`, `lib.rs`<br>`tidb-memory`: `tracker.rs` (`detach_without_resetting_killer`), `sqlkiller.rs`<br>`tidb-sessionctx`: `Cargo.toml` (+`tidb-runtime`), `ProcessInfo`, `SessionManager`, `distsql_ctx.rs`<br>`tidb-store`: `distsql/select_result.rs`, `copr/coprocessor.rs`, `driver/backoff.rs`, `copr/client_rust.rs`<br>`tidb-parser` / `tidb-planner` / `tidb-executor`: `exec.rs`, `distsql.rs`, `sortexec/`, `agg_hash_final_worker.rs`, `projection.rs`, `simple.rs`, `internal/exec/executor.rs`<br>`tidb-metrics` / `tidb-server` / `tidb-mysql-protocol` / `poc-session` / `poc-domain` / `poc-server` | 1) 实现可复位 `KillCancel` 与不可复位 `ChildToken`，升级 `ExecCtx`（修复重复分支、`only leaf calls mem_tracker.checkpoint()`），消除 13 处 `checkpoint(&())` 与 2 处 `KillerHook`；<br>2) `tidb-store` 接入 `KillCancel`（通过 drop RPC future 中止 `client-rust`）；<br>3) `ProjectionExec` 迁 `TrackedAlloc`，`SortExec` / `ProjectionExec` 补 `Drop` 归零；<br>4) 修复 `router.take()`、`RouterGuard`、`poc-executor` 内部 `cancel_select`、`--new-stack=off` 兼容、PoC `SetProcessInfo`（含 `InSelectStmt` 门控）、Kill/OOM 永不回退，并在 `Compiler::compile` / `ExecStmt::exec` / `write_result_set` 遇 `1317`/`3024`/`8175`/`8176` 时照 Go 补打三条 `* panic` 日志（§7.1 决策 10）；<br>5) 实现单机 `server_id = 1`、集群 `/tidb/server_id` 租约（`12h` TTL / `5m` 保活 / `6h` 断连 `KillAllConnections` / 停机释放）+ `rpc.rs` `TypeKill` + `killRemoteConn`（§7.1 决策 7）、`ConnIdAllocator`、`ServerSessionManager`、`tidb_server_event_total{type="kill"}`、`PacketIo` 拆分（`poll_fill_buf` 断连、`KILL QUERY` 保完整包写 ERR、`KILL CONNECTION` 中断 socket）及任意 `.await` 点 future drop 单元测试。 | • `check_layers.py` 无违规；<br>• **任意 `.await` 点 Drop 单测**：验证中途 drop `Router` 不丢、`SortExec`/`ProjectionExec` 内存归零、worker `JoinHandle` 全部回收；<br>• **整行黄金日志比对**：`[kill]`、`"kill initiated"`、`"kill finished"`、`"command dispatched failed"`、截断 ID WARN 及三条 `* panic` 日志（`ERROR "compile SQL panic"`、`WARN "execute sql panic"`、`ERROR "write query result panic"`）；<br>• **真实 MySQL E2E（与 Go v8.5.8 对照）**：`KILL QUERY` 返 `1317` 且连接存活、非特权 `1227` / 自杀 `1317`、截断 ID `1105`、不存在 ID 静默、单机 `CONNECTION_ID()` 为 `2097154`、集群 `/tidb/server_id` + 跨节点 `TypeKill`、`KILL CONNECTION` `2013`、TCP `SHUT_RDWR` ~1s 断开后通过 **`ServerSessionManager::show_process_list()` 集成测试 + `/status` `connections` 归零**验证清除（不依赖尚属 stub 的 `SHOW PROCESSLIST` SQL）。 |
| **Part 2**<br>内存配额、OOM Action、日志与全路径解绑（`#464`） | `tidb-sessionctx`: `variable/{session.rs, tidb_vars.rs}`, `stmtctx.rs`<br>`tidb-executor`: `glue.rs`, `select.rs`, `adapter.rs`, `builder.rs`<br>`tidb-session`: `session.rs`, `checkpoint.rs`, `expensive_query.rs`<br>`poc-domain`: `sysvar_loader.rs` | 1) 为 `SessionShared` / `SessionVars` 增加会话根 `MemTracker` / `DiskTracker`；<br>2) `SessionContext::mem_exceed_log_hook()`、`reset_context_of_stmt` 安装 `PanicOnExceed`/`LogOnExceed` + `LogHook`、修复 `get_dist_sql_ctx` 传 `mem_root`；<br>3) `SysVarSync::rebuild` 只读白名单原子同步（保持 `defaults_gen.rs:301` 不改，比较端大小写兼容）；<br>4) 修复 `log_on_query_exceed_mem_quota`（严格区分 bootstrap `sm == None` 与查无进程静默）；<br>5) 全路径解绑（`CloseRecordSet` + `StmtTrackerGuard` + `StmtCheckpoint::restore()` 调 `detach_without_resetting_killer()` 保留在途 `KILL` + 修正 `adapter.rs:682, 916` 注释 + `COM_STMT_CLOSE`/`COM_STMT_RESET` 清理）；保持 `HashAggExec.session_mem_tracker` 暂不挂载直到 `D01.10`。 | • `check_layers.py` 通过；<br>• **整行黄金日志比对**：`["memory exceeds quota"]`（样本 2）与 `"expensive_query during bootstrap phase"`，同时单测查无进程时静默无日志；<br>• **真实 MySQL E2E（新栈 `SortExec` `ORDER BY` / `ProjectionExec`）**：`tidb_mem_quota_query=100000` + `CANCEL` 返回精确 `8175`，`mem_root.children` 清空且归 `0`；`LOG` 返回结果并输出样本 2；验证合法 `Blocked` 回退时未清空并发 `KILL`；**`HashAggExec` 用算子/`ExecStmt` 集成测试**覆盖 `4 > 2 > 1 > 0` 链。 |
| **Part 3**<br>`memstats` 修复与两大 100ms 看门狗 | `tidb-memory`: `memstats.rs`<br>`tidb-session`: `expensive_query.rs`, `server_memory_limit.rs`<br>`tidb-server` / `poc-session`: 接线看门狗与指标 | 1) 修复 `memstats::read_mem_stats()` 300ms `ReadMemInterval` TTL 刷新；<br>2) 100ms `ExpensiveQueryHandle::run`：`ScopeInstance` 阈值、15s/10m `expensive_txn`、60s `expensive_query`、无级别门控 `"execution timeout, kill it"`（3 字段）→ `3024`（PoC 与新栈均更新 `ProcessInfo` + `InSelectStmt` 门控；`COM_STMT_EXECUTE` 解包判定）；<br>3) 100ms `ServerMemoryLimitHandle::run`：jemalloc `resident`（`sys`）、`128 MiB` 门槛（含 `Top1 == None` 分支）、`8176`（`NeedKill` 节流打 WARN）、5s/60s（5s 分支内仅调 `FinishResultSet()`）、照 Go 顺序执行 `Succ:`（开 `upstream-pitfall`）、50 槽 `MEMORY_USAGE_OPS_HISTORY`。 | • `check_layers.py` 与 `govec` 全绿；<br>• **整行黄金日志比对**：`"expensive_query"`（样本 1）、`"expensive_txn"`、`"execution timeout, kill it"`、5 条 `servermemorylimit` WARN、`8176` `NeedKill` WARN；<br>• **真实 MySQL E2E 对照 Go v8.5.8**：慢 scan/`ORDER BY` 设 `max_execution_time=200` → `3024`（`SELECT SLEEP(5)` 行为待内置 `SLEEP()` 移植补全）；慢查超阈值输出样本 1；低 `tidb_server_memory_limit` 触发 `8176` 与 ops history 记录。 |
| **Part 4**<br>控制面运行时隔离（`#474 R-5`） | `tidb-runtime`: `ControlRuntime`<br>`poc-domain`: `sync.rs`, `privilege_loader.rs`, `sysvar_loader.rs`<br>`poc-server`: `main.rs`, `status.rs`, `rpc.rs`<br>`client-rust`: `store/liveness.rs`, `TikvConnect` | 1) 新增默认 2 线程 `ControlRuntime`（`tidb-rs-ctrl`）；<br>2) 在 `ControlRuntime` 上独立连接 **`ctrl_kv`** 并显式传递 `ctrl_handle` 给 `SchemaSync`（含 `/tidb/server_id` 保活）、`PrivilegeSync`、`SysVarSync`；<br>3) 迁移 `/status` HTTP + `rpc.rs` gRPC、`stmt-timing`、`ExpensiveQueryHandle`、`ServerMemoryLimitHandle`；<br>4) `client-rust` `StoreLiveness` 注入 `ctrl_handle`（`batch.rs` 与数据面 `kv` 留 `ExecPool`）；<br>5) 落地 5 步优雅停机序。 | • 在 **`--runtime-topology=steal` 与 `no-steal`** 下以 `worker_threads=2` 压满混合重负载验证：`/status` 与 `/metrics` `< 50ms`、etcd DDL/`server_id` 租约零超时、`KILL`/`3024`/`8176` `< 200ms` 强杀生效、优雅停机先释放 `server_id` 且零挂死；若与 Go 混部启用 TiProxy，实测 TiProxy 对 32/64 位连接 ID 的路由与 `KILL` 兼容性。 |

---

## 6. 与 Go TiDB v8.5.8 的已知差异清单

| # | 差异项 | Go v8.5.8 行为 | `tidb-rs` 行为 | 对用户可见影响 | 分类与追踪（Issue / 台账） |
|---|---|---|---|---|---|
| 1 | 全局内存限额指标与回收 | 读取 `runtime.MemStats.HeapInuse`，杀后调用 `runtime.GC()` | 按 `N3+A1 §4.4` 裁定 4 读取 jemalloc `stats.resident`（`MemStats.sys`），无 `runtime.GC()` | 按 RSS 常驻内存而非 Go 堆使用判定，无 GC 停顿 | **永久例外**（已裁定 `N3+A1`） |
| 2 | Kill / OOM 内部传播与三条 `* panic` 日志 | 内部 `panic(err)`，于 `compiler.go:62` / `adapter.go:539` / `conn.go:2364` recover 并打出 3 条日志 | 内部走 `Result<(), Error>`（`N3 §3.4`），并在相同三个边界遇 `1317`/`3024`/`8175`/`8176` 时照 Go 字段/级别补打三条日志（§7.1 决策 10） | 对外日志级别、消息、字段与 Go 逐字节一致，无用户可见差异 | **已对齐 Go（无外部差异）**（`zhigang-du` 2026-11-28 确认，不登记 `rulings.toml`） |
| 3 | 控制面线程模型 | 所有 goroutine 共享 M:N 抢占式 Go scheduler | 数据面 `ExecPool` + 控制面 2 线程 `ControlRuntime`（含专用 `ctrl_kv`） | 高负载下控制面探测、lease 续约与 OOM 强杀更稳定 | **永久架构例外**（`zhigang-du` 2026-11-28 确认，§7.1 决策 8，已同步修订 `N3 §3.1` 第 2 条） |
| 4 | **`servermemorylimit` Top1 CAS 顺序上游缺陷** | `servermemorylimit.go:128-130` 先 `s.reset()` 再 `CompareAndSwap(nil, nil)` | 照 Go 顺序移植（`:155-157` 与 `tracker.go:495` 提供自然兜底） | 与 Go v8.5.8 完全一致 | **临时保留 Go 缺陷**（开 `upstream-pitfall` + `go-bug-kept` issue，宪章第九条 2 款） |
| 5 | **`CONNECTION_ID()` 与 Go → Rust 跨节点 `KILL`** | 单机 `server_id = 1`，集群注册 `/tidb/server_id` `[1, 2047]` 并通过 `TypeKill` 互通 | Part 1/4 全量实现单机 `server_id = 1`、集群 `/tidb/server_id` 租约与 `TypeKill` 跨节点互通（§7.1 决策 7） | 落地后与 Go v8.5.8 完全一致；混部 TiProxy 仍需在 Part 4 实测验证（见 #12） | **已对齐 Go（无长期差异）**（`zhigang-du` 2026-11-28 确认） |
| 6 | **`--new-stack=off` 默认下的内存配额覆盖范围** | 所有 SQL 均受 `tidb_mem_quota_query` (`8175`) 与 `8176` 约束 | 当前二进制默认 `--new-stack=off`；细粒度内存限额与 `MemUsageTop1Tracker` **仅对 `--new-stack=fallback\|strict` 命中的新执行栈 SQL 生效** | 默认启动模式下不触发 `8175`/`8176`；开启新栈且命中支持语句时生效 | **临时追踪差异**（随 `VS1` 切流与旧栈退场 issue 消解） |
| 7 | **缺失落盘（`D01.10`）下的查询失败行为** | `tidb_enable_tmp_storage_on_oom` 默认 `ON`，超限优先落盘并继续完成查询 | `D01.10` 未实现（`blocked-by:D01.10`），当前 `Sort`/`TopN`/`HashAgg` 无实际落盘能力 | **在默认 ON 配置下，Go 中依靠落盘成功的重查询在 `tidb-rs` 会直接报 `8175`（或 `LOG`）** | **临时追踪差异**（`blocked-by:D01.10`） |
| 8 | **`CLUSTER_PROCESSLIST`、`SHOW PROCESSLIST` 与虚拟表** | `rpc.rs` 返回进程列表，支持 `SHOW PROCESSLIST` 与 `MEMORY_USAGE_OPS_HISTORY` | `rpc.rs` `TypeTableScan` 返回空表，`SHOW PROCESSLIST` SQL 仍为 stub，`MEMORY_USAGE_OPS_HISTORY` 仅先建内存环形缓冲 | `SELECT * FROM information_schema.CLUSTER_PROCESSLIST` 看不到 `tidb-rs` 会话 | **临时追踪差异**（单开 `CLUSTER_PROCESSLIST`/系统表暴露 issue，关联 `D06.3` / `D03.2`） |
| 9 | **`SysVarSync` 白名单与 `8176` `NeedKill` 日志节流** | `rebuild` 或集群变化跑完整 hook；`NeedKill` 每次 `HandleSignal` 打日志 | `SysVarSync::rebuild` 仅对内存/OOM 白名单做只读原子同步；`8176` `NeedKill` WARN 按语句/每秒节流 | 内存/OOM 全局变量同步正常，但其他非白名单变量仍按会话读取缓存；高频率 checkpoint 下避免日志风暴 | **临时 / 实现差异**（`SysVarSync` 随 `D06.5` 完整同步消解；节流记录于此） |
| 10 | **结果集写出、`PhysicalHashAgg`、Hints、游标与 `SLEEP(N)`** | 流式写回、`HashAgg` 全接入、支持 Hint/`COM_STMT_FETCH`/`SLEEP` | `D03.1` 尚待流式写回；`PhysicalHashAgg` 待 `D10.1`；Hint 待 `D02.2`/`D13.3`；游标 `COM_STMT_FETCH` 待 `D12.12`；`SLEEP(N)` 尚在 PoC | 带 Hint/Agg 暂走 PoC 或报 `1235` | **临时追踪差异**（随 `D03.1`、`D10.1`、`D02.2`、`D12.12` 消解） |
| 11 | **断连检测底层原语与 `#70776` 修复** | `net.Conn` `SetReadDeadline(30µs) + Peek(1)`；`SQLKiller.Reset` 为 load+store | `BufReader::poll_fill_buf` 非阻塞探测（`BiLock` / TLS `Pending` 保守判存活）；`SqlKiller.reset` 已含 `#70776` `swap(0)` 修复 | 外部感知一致（`~1s` 断连触发 `1317`）；消除极窄并发丢信号竞争 | **内部实现差异 / Post-v8.5.8 `go-fix`**（`sqlkiller.rs:14-16` 已记录） |
| 12 | **TiProxy 混部连接 ID 编码兼容性** | TiProxy 解析 32/64 位连接 ID 路由 `KILL` | 本期采用与 Go 相同的 bit0 / `serverID` 布局，需在混部验证阶段加入 TiProxy 实测 | 需验证无截断或跨节点路由错误 | **待验证混部兼容性**（纳入 Part 4 混部验收项） |

---

## 7. 决策点（严格遵守 `CONSTITUTION.md` 第九条）

### 7.1 负责人已决（不得再问）

1. **`servermemorylimit` 度量指标**：使用 jemalloc `stats.resident`（`MemStats.sys`）判定实例内存上限（`N3+A1 §4.4` 裁定 4）。
2. **Go GC 专属变量与 `release()`**：`tidb_gogc_tuner_*`、`tidb_server_memory_limit_gc_trigger`、`tidb_enable_gc_aware_memory_track` 保持合法校验但不触发 GC 动作；无 `runtime.GC()`（`N3+A1 §4.3` 裁定 3、`N8 §3.4`）。
3. **取消与 OOM 错误传播机制**：严格采用 `panic → Result`（`N3 §3.4`、`N3+A1 §4.5`），不在 Rust 中使用 panic + `catch_unwind` 控制取消流。
4. **SQL 执行零交接、协作让出与 Drop-Safe**：连接任务即语句任务在 `ExecPool` 运行（`N3 §3.1`），所有资源守卫在任意 `.await` 点被 drop 时必须零泄漏（`CLAUDE.md` rule 8）。
5. **仅叶子算子调 `mem_tracker.checkpoint()`**（`A1 §3.3`）。
6. **日志字段契约**：与 Go v8.5.8 整行/全字段字节级对齐（`D00.3 §4.4`、`CLAUDE.md` rule 11 & 12）。
7. **集群 `server_id` 租约与跨节点 Global KILL 全量交付（`approver: zhigang-du`, 2026-11-28，原决策点 1 = A）**：单机（`--schema-sync=false`）固定使用 `serverIDForStandalone = 1`（首条连接 ID `2097154`）；集群模式在 `#542` Part 1/4 直接按 Go 协议实现 etcd `/tidb/server_id/<id>` 租约（`[1, 2047]` / 64 位溢出、`12h` TTL、`5m` 保活、`6h` 断连触发 `KillAllConnections()`、停机 `releaseServerID()`）以及 `poc-server/src/rpc.rs` `ExecType::TypeKill` + 出站 `killRemoteConn`，保证与 Go 混部及 LB/TiProxy 下零误杀。
8. **控制面运行时隔离（`approver: zhigang-du`, 2026-11-28，原决策点 2 = A，同步修订 `N3 §3.1` 第 2 条）**：数据面维持单一 `ExecPool` 运行时（连接任务 = 语句任务，零跨运行时语句交接）；集群控制面与后台看门狗（`SchemaSync`/`PrivilegeSync`/`SysVarSync` 及其独立轻量 `ctrl_kv`、`/status` HTTP + gRPC、`ExpensiveQueryHandle`、`ServerMemoryLimitHandle`、`client-rust` `StoreLiveness`）隔离至专用 2 线程 `ControlRuntime`（`tidb-rs-ctrl`），不新增 Go TiDB 不存在的 SQL 系统变量。
9. **取消原语分层与周期巡检沿用 Go L1（`approver: zhigang-du`, 2026-11-28，原决策点 3 = A，同步修订 `N3 §3.3` 第 2、4、5 条）**：① 三个周期任务（`ExpensiveQueryHandle` 100ms 统一处理 60s/600s/`max_execution_time`、`ServerMemoryLimitHandle` 100ms 采样、`SQLKiller` 1s 节流连接探活）按 Go L1 实现，不再另建每语句 deadline 定时器；② 取消原语拆分为**会话级可复位取消器 `KillCancel`**（绑定 `SqlKiller.signal`，随 `reset()` 同步清除，唤醒必二次校验 `signal != 0`）与**算子级不可复位子令牌 `ChildToken`**（对应 `closeCh`/`finishCh`）。
10. **Kill / OOM 路径补打 Go 三条 `* panic` 日志（`approver: zhigang-du`, 2026-11-28，原决策点 4 = B）**：内部仍按 `N3 §3.4` 走 `Result<(), Error>` 传播，但在三个对应边界遇到 `1317` / `3024` / `8175` / `8176` 时照 Go 原级别、消息与字段补打三条日志，并以整行黄金测试钉住，**不登记 `rulings.toml` 差异**：
    - `Compiler::compile`（Go `pkg/executor/compiler.go:62`）：`ERROR` `"compile SQL panic"`，字段 `[conn] [session_alias] [SQL=<stmt text>] [stack=<stack>] [recover=<err>]`；
    - `ExecStmt::exec`（Go `pkg/executor/adapter.go:539`）：`WARN` `"execute sql panic"`，字段 `[conn] [session_alias] [sql=<text to log>] [stack=<stack>]`（注意小写 `sql` 且无 `recover` 字段）；
    - `write_result_set`（Go `pkg/server/conn.go:2364`）：`ERROR` `"write query result panic"`，字段 `[conn] [session_alias] [lastSQL=<last stmt>] [stack=<stack>] [recover=<err>]`。

### 7.2 需要负责人决定（严格限宪章第九条 第 1 款）

无（`#545` 的 4 项第九条 1 款决策点均已由项目负责人 `zhigang-du` 于 2026-11-28 确认并移入 §7.1）。

### 7.3 维护者已定（宪章第九条 第 2 款，不单独询问）

1. **Trait 反转与分层归属**：`ProcessInfo` 与 `SessionManager` 放在 `tidb-sessionctx`（rank 7）；`mem_exceed_log_hook()` 与 `session_manager()` 放在 `tidb-executor::glue::SessionContext`（rank 14）；`ExpensiveQueryHandle` 与 `ServerMemoryLimitHandle` 放在 `tidb-session`（rank 15）；`ServerSessionManager` 与 `ConnIdAllocator` 放在 `tidb-server`（rank 16，新增 `tidb-runtime` rank 1 与 `tidb-sessionctx` rank 7 依赖）；`tidb-mysql-protocol` `[legacy]` 向下依赖 `tidb-server`。
2. **混合路由过渡期 PoC 路径处理**：`KILL` / `max_execution_time` / TCP 断连在连接、`SqlKiller` 与 `poc-executor` 内部 `cancel_select` 统一覆盖新旧双栈（维护 `ProcessInfo` 含 `InSelectStmt` 门控，修复 `self.router.take()` 并禁止外层 `select!` 包裹 `router.query`）；细粒度 `tidb_mem_quota_query`（`8175`/`LOG`）与 `MemUsageTop1Tracker`（`8176`）仅对新执行栈生效，不为即将淘汰的 `[legacy] poc-executor` 补写 `TrackedAlloc`。
3. **`StmtCheckpoint::restore()` 保留 `KILL` 且 Kill/OOM 永不回退**：合法 `Blocked` 回退时 `restore()` 调用 `detach_without_resetting_killer()` 绝不重置 `killer`；任何 Kill/OOM 错误一律不触发回退。
4. **照 Go 移植 `servermemorylimit.go:128-130` 上游缺陷（第九条 2 款）**：因 `servermemorylimit.go:155-157` 与 `tracker.go:495` 提供自然兜底，不占用负责人决策，按 Go 原文移植并开具 `upstream-pitfall` + `go-bug-kept` issue。
5. **消除全工作区 `checkpoint(&())`、升级 `ProjectionExec` 与全路径解绑**：Part 1 替换 13 处 `checkpoint(&())` + 2 处 `KillerHook`、升级 `ProjectionExec` 并补齐 `SortExec`/`ProjectionExec` `Drop` 内存归零、全路径覆盖 `COM_STMT_CLOSE`/`COM_STMT_RESET`/`CloseRecordSet`/`StmtTrackerGuard` 解绑，并更正 `adapter.rs:682, 916` 注释。
6. **`HashAggExec.session_mem_tracker` 暂不挂载（直到 `D01.10`）与 `SysVarSync` 白名单同步**：防止误触 `blocked-by:D01.10`（`1235`），同时保障 `ServerMemoryLimit` 等全球内存/OOM 原子量生效；保留 `defaults_gen.rs:301` 不改。
7. **`PacketIo` 读写拆分、`poll_fill_buf` 探活与 `KILL QUERY` 整包保护**：共享 `seq`/`IoCounts`；通过 `BufReader::poll_fill_buf` 保留管道粘包；`finish_cancel` 仅在整包之间生效不撕裂包，仅 `conn_shutdown` 中断 socket 读写。
8. **`client-rust` 最小侵入注入与四阶段全量整行黄金门禁**：`StoreLiveness` 注入可选 `Handle`（`batch.rs` 数据面留 `ExecPool`）；按 Part 1–4 执行，强制 `check_layers.py`、任意 `.await` 点 Drop 单测及全部 10 类日志的整行黄金比对。

---

## 8. 本文所用 ID

- `#464`：Expensive Query 与 OOM 日志黄金合约对齐（`D00.3 §4.4`）
- `#474 R-5`：`N3` 架构收敛项——控制面运行时隔离
- `#542`：本规格对应 Issue——新执行栈运行时控制（取消与 `KILL`、内存配额与看门狗、控制面隔离）
- `#70776`：上游修复（commit `05c7dd7b96`，修复 `SqlKiller::Reset` swap 丢失信号竞争）
- `A1`：统一内存记账模型（`TrackedAlloc` + `Tracker::checkpoint` + jemalloc `resident`）
- `D00.3`：统一日志规范与黄金日志样本
- `D01.10`：磁盘溢写行容器（`RowContainer` / `DataInDiskByChunks` / `SpillDiskAction`）
- `D02.2`：Parser 语法扩展（含 optimizer hints 语法等）
- `D03.1` / `D03.2`：MySQL 协议服务端、流式写回与状态/信息系统表
- `D05.1` / `D05.2` / `D05.3`：系统变量、`SessionVars` / `StatementContext`、会话管理
- `D06.3` / `D06.5`：`InfoSchema` / `Domain` 与元数据同步
- `D07.1` / `D07.3`：TiKV store 驱动与 DistSQL 客户端（`SelectResult`）
- `D10.1`：物理聚合计划（`PhysicalHashAgg` 等）
- `D12.0` / `D12.2` / `D12.3` / `D12.5` / `D12.12`：执行器框架、Projection、HashAgg、Sort、游标结果集
- `D13.3`：Hint 解析与下推处理
- `D15.1` / `D15.3`：内存追踪与 cgroup 内存感知
- `N0`：Crate 分层规范（`layers.toml` 与 `scripts/check_layers.py`）
- `N2`：会话与语句上下文隔离规范（`SessionShared` / `Arc<StatementContext>`）
- `N3`：运行时并发、取消、`ExecPool`（`Steal`/`NoSteal`）、时间片 `checkpoint` 与 `panic → Result` 规范
- `N8`：运行时专属配置/变量处理规范（Go GC 等不产生实际运行行为）
- `N10`：资源管控与 Runaway Query 规范（`8253`）
- `VS1`：第一条纵向切片及 PoC / 新执行栈并行路由机制
