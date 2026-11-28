# #542 运行时控制设计规格：取消与 KILL、内存配额与看门狗、控制面运行时隔离

- **状态**：设计草案（待负责人确认 §6.2 决策点）
- **关联 Issue**：`#542`（新执行栈 Cancellation & KILL、Memory Quota & Watchdog、Control-Plane Isolation）、`#474 R-5`（控制面隔离）、`#464`（Expensive Query & OOM 日志接线）
- **Go 基线**：Go TiDB v8.5.8（commit `8b857efa2`；其中 `SqlKiller` 包含上游 `pingcap/tidb#70776` commit `05c7dd7b96` 修复）
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
| **32/64 位 ID 解析与不存在 ID** | `pkg/executor/simple.go:2665-2726`，`pkg/util/globalconn/globalconn.go:28-134`：<br>配置默认 `enable-global-kill = true`、`enable-32bits-connection-id = true`、`compatible-kill-query = false`。<br>• bit0=0 为 32 位 `[serverID(11b)\|localID(20b)\|0]`；bit0=1 为 64 位 `[0(1b)\|serverID(22b)\|localID(40b)\|1]`。<br>• 若 `id & 0x1 > 0 && id >> 32 == 0`，`ParseConnID` 返回 `isTruncated = true`，写入 Warning。<br>• 本机 `Server.Kill` 若 ID 不在 `s.clients` 且不在 `SysProcTracker` 则直接返回（`pkg/server/server.go:929-933`）。<br>• 若 `gcid.ServerID != sm.ServerID()`，走 `killRemoteConn`（`simple.go:2728-2777`）发 `tipb.ExecType_TypeKill` DAG 请求。 | • `enable-global-kill = true` 时，不带 `TIDB` 的 `KILL QUERY <cid>` 正常生效、无 warning。<br>• `KILL` 不存在的合法格式 ID **静默成功（OK），无错误也无 warning**（不返回 MySQL `1094`）。<br>• 传入被截断的 32 位奇数 ID（如 `99999999`）返回 **`Warning 1105`**: `Kill failed: Received a 32bits truncated ConnectionID, expect 64bits. Please execute 'KILL [CONNECTION \| QUERY] ConnectionID' to send a Kill without truncating ConnectionID.`。 |
| **`KILL QUERY` vs `KILL CONNECTION`** | `pkg/server/server.go:922-974`：<br>1) 打 `[INFO] [server.go:923] [kill] [conn=...] [query=...] [maxExecutionTime=...] [runawayExceed=...]`。<br>2) 若 `!query`：将连接状态置为 `connStatusWaitShutdown`，并对 `bufReadConn` 设置 `SetWriteDeadline(time.Now())` 与 `SetReadDeadline(time.Now())`（立刻终止阻塞的网络读写并在 `clientConn.Run` 退出断开连接）。<br>3) `killQuery`：① 调 `SQLKiller.SendKillSignal(reason)`（`CAS 0 -> reason`，首次成功打 `[WARN] [sqlkiller.go:66] ["kill initiated"] ["connection ID"=...] [reason=...]`)；② `conn.cancelDispatch()` 取消当前命令 `context.Context`；③ `SQLKiller.FinishResultSet()` 唤醒/结束正在写出的 `ResultSet`。 | • `KILL QUERY <cid>` 中断重查询：客户端收到 **`1317 (70100): Query execution was interrupted`**，连接保持存活（后续 `SELECT 1` 成功）；日志按序输出 `[INFO] [server.go:923] [kill]` → `[WARN] [sqlkiller.go:66] ["kill initiated"]` → `[WARN] [sqlkiller.go:171] ["kill finished"]` → `[INFO] [conn.go:1222] ["command dispatched failed"]`。<br>• `KILL [TIDB] <cid>`（`KILL CONNECTION`）：客户端收到连接断开（pymysql `2013 Lost connection to MySQL server during query`），后续发包失败。 |
| **执行检查点与错误传播** | `pkg/executor/internal/exec/executor.go:448, 462`（`exec.Next` 前后调 `HandleSQLKillerSignal()`），`pkg/util/sqlkiller/sqlkiller.go:118-174`：`HandleSignal()` 返回错误表：`QueryInterrupted(1) -> 1317`、`MaxExecTimeExceeded(2) -> 3024`、`QueryMemoryExceeded(3) -> 8175`、`ServerMemoryExceeded(4) -> 8176`、`RunawayQueryExceeded(5) -> 8253`。在 Go 中经 `panic(err)` 在 `Compiler.Compile`（`compiler.go:50-63`）、`ExecStmt.Exec`（`adapter.go:503-540`）和 `clientConn.writeResultSet`（`conn.go:2349-2365`）recover 转错误，`dispatch` `defer` 调 `SQLKiller.Reset()`（`conn.go:1312-1317`）。 | 算子内部与结果集写出期间均能立即响应，错误通过 MySQL ERR 包返回客户端，`dispatch` 结束后信号清零不影响下一条语句。 |
| **`max_execution_time` 超时** | `pkg/executor/adapter.go:622-631`：仅对 `StmtCtx.InSelectStmt == true` 将 `GetMaxExecutionTime()`（含 `MAX_EXECUTION_TIME(ms)` hint 覆盖）写入 `ProcessInfo.MaxExecutionTime`；`pkg/executor/select.go:317-337`（`checkMaxExecutionTimeExceeded`）；`pkg/util/expensivequery/expensivequery.go:96-100`：100ms 巡检发现 `costTime > MaxExecutionTime*ms` 后**无级别门控地**打 `[WARN] [expensivequery.go:97] ["execution timeout, kill it"]`（fields: `costTime`、`maxExecutionTime`、`processInfo`）并调 `sm.Kill(info.ID, true, true, false)`。 | 默认 `max_execution_time = 0`。设 `200` ms 后：<br>• 重查询返回 **`3024 (HY000): Query execution was interrupted, maximum statement execution time exceeded`**。<br>• `SELECT SLEEP(5)` 返回单行结果 `1`（不向客户端报错），后台仍输出 `[WARN] [expensivequery.go:97] ["execution timeout, kill it"]` → `[INFO] [server.go:923] [kill] [conn=...] [query=true] [maxExecutionTime=true] [runawayExceed=false]` → `"kill initiated"`（`[executor:3024]...`）→ `"kill finished"`。 |
| **客户端断连检测** | `pkg/server/conn.go:2055-2104, 2170-2173`：`handleStmt` 对非事务 autocommit DML 在执行前、对其他语句在进入 `writeResultSet` 前挂载 `IsConnectionAlive`，退出时清除；`pkg/server/internal/util/buffered_read_conn.go:56-88`：设 `30 * time.Microsecond`（`30µs`，第 63 行）read deadline 并 `Peek(1)`，仅当确定遇到 `io.EOF` 或非超时 `net.Error` 时返回 `0`（已死），并发锁被占或超时返回 `-1`/`1`；`SQLKiller.HandleSignal` 以 1s（`checkConnectionAliveDur`）节流调用，检测到死亡即发送 `QueryInterrupted`。 | 客户端突然 TCP 关闭（`SHUT_RDWR`）：Go 在约 **1s** 内由 `HandleSignal` 检出，触发 `SendKillSignal(QueryInterrupted)`，产出 `"kill initiated"`（`1317`）→ `"kill finished"` → `"command dispatched failed"`，并在 **2.5s** 内从 `SHOW PROCESSLIST` 彻底清除。 |

### 1.2 支柱二：内存配额、`OOMAction`、`servermemorylimit` 与 `expensive_query`

| 维度 | Go v8.5.8 源码位置与逻辑 | 真实 v8.5.8（unistore）实测行为 |
|---|---|---|
| **系统变量默认与作用域** | `pkg/sessionctx/variable/sysvar.go:519, 525, 1176, 1200, 1228, 1459, 1718, 2757`：<br>• `tidb_mem_oom_action = 'CANCEL'`（`ScopeGlobal`）<br>• `tidb_mem_quota_query = 1073741824`（1 GiB，`ScopeGlobal \| ScopeSession`）<br>• `tidb_server_memory_limit = '80%'`（`ScopeGlobal`）<br>• `tidb_server_memory_limit_gc_trigger = 0.7`（`ScopeGlobal`）<br>• `tidb_server_memory_limit_sess_min_size = 134217728`（128 MiB，`ScopeGlobal`）<br>• `tidb_expensive_query_time_threshold = 60`（`ScopeInstance`，不存 `mysql.global_variables`）<br>• `tidb_expensive_txn_time_threshold = 600`（`ScopeInstance`，不存 `mysql.global_variables`） | `SET SESSION tidb_mem_oom_action = 'LOG'` 报错 **`1229`**（全局作用域变量）；默认值与源码完全一致。 |
| **`ResetContextOfStmt`、`MemTracker` 树与全路径解绑** | `pkg/executor/select.go:997-1048`，`pkg/executor/adapter.go:589-591, 865, 1734`，`pkg/server/conn.go:2140-2156`：<br>1) 每次语句开始：`vars.MemTracker`（`IsRootTrackerOfSess=true`）`Detach`、`UnbindActions`、`SetBytesLimit(vars.MemQuotaQuery)`、`ResetMaxConsumed()`，绑定 `Killer = &vars.SQLKiller`、`SessionID = ConnectionID`；`vars.DiskTracker` 同样重置并按需挂到 `GlobalDiskUsageTracker`。<br>2) `sc.InitMemTracker(LabelForSQLText, -1)` 并 `AttachTo(vars.MemTracker)`；若有 `MEMORY_QUOTA` hint（`adapter.go:589`），覆写 `vars.MemTracker.SetBytesLimit(sc.StmtCtx.MemQuotaQuery)`。<br>3) 根据 `variable.OOMAction` 安装 `PanicOnExceed{ConnID, Killer}` 或 `LogOnExceed{ConnID}`，并**强制注入 `SetLogHook(dom.ExpensiveQueryHandle().LogOnQueryExceedMemQuota)`**。<br>4) 语句结束或任何早退错误（`handleNoDelay`、`compile`、`open_executor` 失败，`conn.go:2150-2155`）均显式调用 `sc.DetachMemDiskTracker()`，在 `Tracker.Detach()`（`pkg/util/memory/tracker.go:362-371`）从 `parent.children` 摘除、清空会话根 `actionOnExceed` 链并 `Killer.Reset()`。 | • 无落盘算子场景下，设 `tidb_mem_quota_query = 100000` + `tidb_mem_oom_action = 'CANCEL'`：返回 **`8175 (HY000): Your query has been cancelled due to exceeding the allowed memory limit for a single SQL query. Please try narrowing your query scope or increase the tidb_mem_quota_query limit and try again.[conn=<id>]`**。<br>• 设 `tidb_mem_oom_action = 'LOG'`：查询正常返回结果，通过 `LogOnExceed -> LogOnQueryExceedMemQuota` 输出 `["memory exceeds quota"]` WARN（`D00.3` §4.4 黄金样本 2）。 |
| **Action 优先级与落盘链** | `pkg/util/memory/action.go:104-112`：`DefRateLimitPriority(4) > DefCursorFetchSpillPriority(3) > DefSpillPriority(2) > DefLogPriority(1) > DefPanicPriority(0)`。算子通过 `FallbackOldAndSetNewAction` / `FallbackOldAndSetNewActionForSoftLimit`（`tracker.go:238-309`）合并优先级链；`softScale = 0.8`（`tracker.go:117`）。 | 高优先级行动（如限流 `4`、落盘 `2`）先触发并 `SetFinished()`，耗尽后自动降级至 `LogOnExceed(1)` / `PanicOnExceed(0)`。 |
| **`servermemorylimit` 实例看门狗** | `pkg/util/servermemorylimit/servermemorylimit.go:35-264`（100ms tick），配合 `pkg/domain/domain.go:2807, 2833`（每 `ReadMemInterval = 300ms` 调 `ForceReadMemStats()`）：<br>1) 读 `ReadMemStats().HeapInuse`，维护 `MemoryMaxUsed`。<br>2) 若 `HeapInuse > bt`：检查 `MemUsageTop1Tracker`（仅 `bytesConsumed >= tidb_server_memory_limit_sess_min_size` 128 MiB 才有资格 CAS 更新 Top1，见 `tracker.go:489-502`），查 `sm.GetProcessInfo(sessionID)` 后发送 `ServerMemoryExceeded`（**`8176`**），递增 `SessionKillTotal`、记录 `SessionKillLast` 与 `IsKilling`，并向 50 条环形缓冲 `GlobalMemoryOpsHistoryManager` 写入快照（`SQL_TEXT` 截断 256 字符，`MEMORY_USAGE_OPS_HISTORY` 表读取）。<br>3) 若杀后 Top1 会话仍存在（`info.Time == s.sqlStartTime`）：每 5s 打 `"global memory controller failed to kill the top-consumer in %ds"` WARN；达到 **60s** 后打强制告警并**仅直接调用 `s.sessionTracker.Killer.FinishResultSet()`**（**不再多打一次 `sm.Kill` 日志**）；确认结束后执行 `Succ`（注：Go 原文 `servermemorylimit.go:128-130` 先调 `s.reset()` 置 `s.sessionTracker = nil` 再调 `MemUsageTop1Tracker.CompareAndSwap(s.sessionTracker, nil)`，属上游缺陷，见 §3.5.1 与 §6.2 决策点 3）。<br>4) 若无会话 `>= 128 MiB`：每 5s 打 `"no one larger than tidb_server_memory_limit_sess_min_size is found"` WARN。 | 实例级强杀返回 **`8176 (HY000)`**（`ErrMemoryExceedForInstance`），记录进 `INFORMATION_SCHEMA.MEMORY_USAGE_OPS_HISTORY`。 |
| **`expensive_query` 100ms 巡检与 OOM 日志** | `pkg/util/expensivequery/expensivequery.go:50-154`：<br>1) 遍历 `sm.ShowProcessList()`；<br>2) 事务耗时 `>= ExpensiveTxnTimeThreshold`（600s）：每 15s 上报 `OngoingTxnDurationHistogram`，每 10m 打 `expensive_txn` WARN；<br>3) 查询耗时 `>= ExpensiveQueryTimeThreshold`（60s）且距上次 `> 60s` 且 `log.GetLevel() <= Warn`：打 `expensive_query` WARN（`D00.3` §4.4 黄金样本 1）；<br>4) `MaxExecutionTime > 0 && costTime > MaxExecutionTime*ms`：打 `"execution timeout, kill it"`（3 个字段：`costTime`、`maxExecutionTime`、`processInfo`，无级别门控）并调 `sm.Kill(info.ID, true, true, false)`（`3024`）；<br>5) `LogOnQueryExceedMemQuota(connID)`（`:126-145`）：先判 `log.GetLevel() <= Warn`；若 `eqh.sm.Load() == nil`（bootstrap 未注入 `sm`）打 `[INFO] "expensive_query during bootstrap phase" ["conn"=connID]`；若 `sm != nil` 但 `GetProcessInfo(connID)` 未找到则**静默返回**；否则打 `["memory exceeds quota"]` WARN。 | 与上述实测一致（60s 慢查、`3024`、`SLEEP(5)` 特例、`memory exceeds quota` 黄金日志）。 |

### 1.3 支柱三：控制面与后台任务

- **Go 调度特点**：Go 将 HTTP `/status` & gRPC `rpcServer`（`pkg/server/http_status.go`、`pkg/server/rpc_server.go`）、`Domain` 的 schema sync（`loadSchemaInLoop`、`mdlCheckLoop`、`watchGlobalSchemaVer`）、privilege/sysvar reload、`expensivequery.Handle.Run`、`servermemorylimit.Handle.Run` 与 `client-go` 的 `StoreLiveness` 探测都作为普通 goroutine 跑在统一的抢占式 M:N Go scheduler 上。
- **Rust 协作式调度的结构性风险**：在 Tokio 协作式调度（尤其是 `N3` 的 `Topology::NoSteal` 单线程-per-worker 模型或有限 `ExecPool` worker 下），若控制面服务与它们所使用的 `KvClient`（包括内部 `TimestampOracle::run_tso`、`BatchConn::run` 与 tonic HTTP/2 I/O 任务）直接 `tokio::spawn` / `pool.spawn` 在业务 `ExecPool` 上，则重查询爆发、未完整让出时间片的路径或 `NoSteal` 分片不均会直接阻塞 etcd DDL lease `keep_alive`（引发节点退服掉出 `admissible` 状态）、饿死 `/status` 健康检查与远端 RPC、并延迟 `servermemorylimit` 与 `max_execution_time` 的强杀生效时间。

---

## 2. `tidb-rs` 现状盘点（已移植 vs 缺失）

### 2.1 分层底座与现有能力对照表

| 模块 / 能力 | 现状与位置（已核验行号） | 缺失 / 缺口 |
|---|---|---|
| **`SqlKiller`** | `crates/memory/src/sqlkiller.rs:1-411` 已完整移植信号枚举 `0..=5`、`Err*` 映射（含 `8253`）、`#70776` `Reset` swap 修复、`FinishResultSet`、`is_connection_alive`、`handle_signal`（1s 节流），且 **`:197` 已修复为 `"connection ID"`（带空格，无需再改）**，并 **`:175` 已预留 `set_on_kill(&self, f: Option<FinishFn>)` 唤醒钩子**。 | 1) 尚无上层调用 `set_on_kill` 绑定 `CancellationToken`；<br>2) 连接层未安装 `is_connection_alive`、`set_finish_func`、`in_write_result_set`；<br>3) `KillExec` 未移植，无外部入口发送 `QueryInterrupted`。 |
| **`Tracker` & `ActionOnExceed`** | `crates/memory/src/tracker.rs:1-1141`、`action.rs:1-313`、`alloc.rs:1-247` 已移植父子树（`parent.children` 持 `Arc<Tracker>` 强引用 L398，需显式 `detach()` 解绑 L406-426）、`TrackedAlloc`、`MemUsageTop1Tracker`、`ServerMemoryLimit`（L62 初值 **`0`**）、`ServerMemoryLimitSessMinSize`（128 MiB）、`ActionOnExceed` 优先级 `4..=0`、`LogOnExceed`（L192 `set_log_hook`）、`PanicOnExceed`（L260 `set_log_hook`）。 | 1) `SessionVars`/`SessionShared` 没有会话根 `MemTracker`/`DiskTracker`；<br>2) `crates/executor/src/select.rs:654-657` 仅调 `sc.init_mem_tracker(LabelForSQLText, -1)`，既未挂到会话根 tracker，也未安装 `PanicOnExceed`/`LogOnExceed` 与 `LogHook`；<br>3) `crates/session/src/session.rs:853` `get_dist_sql_ctx` 将 `session_mem_tracker` 临时填为 `sc.mem_tracker`（limit `-1`）；<br>4) `crates/executor/src/adapter.rs:682-684, 916-917` 误以为 `StmtCtx` drop 会自动解绑，未调用 `detach_mem_disk_tracker()`（含早退路径与 `StmtCheckpoint::restore()` 均未解绑）。 |
| **Heap Stats (`memstats.rs`)** | `crates/memory/src/memstats.rs:1-83` 已定义 `ReadMemInterval = 300ms`（L26），并用 `tikv_jemalloc_ctl` 读取 `allocated`/`active`/`mapped`/`resident`（L72-82）。 | 1) `GlobalMstats.ts` 标有 `#[allow(dead_code)]`（L44），`read_mem_stats()`（L53-57）在首次加载后永久返回旧缓存，且无人定时调 `force_read_mem_stats()`；<br>2) `servermemorylimit` 必须判定 **`stats.resident`（`MemStats.sys`）**（`N3+A1 §4.4` 裁定 4）。 |
| **`ExpensiveQuery` 日志** | `crates/session/src/expensive_query.rs:1-461` 已移植 `ProcessInfoForLog`（L24）、`gen_log_fields`（L66）、`log_expensive_query`（L183）、`log_on_query_exceed_mem_quota`（L205），且有黄金样本 1 & 2 字节级单测。 | 1) `log_on_query_exceed_mem_quota` 仅区分 `Option<&ProcessInfoForLog>`，**把 `sm != None` 但查无进程的情况与 bootstrap (`sm == None`) 混为一谈**（L214-217 误打 `"expensive_query during bootstrap phase"`）；<br>2) 生产代码无调用；缺少 100ms `ExpensiveQueryHandle::run`、`SessionManager` 与 `ServerMemoryLimitHandle`。 |
| **`ExecCtx` & `exec::next`** | `crates/executor/src/exec.rs:19-28` 仅有 `stmt` 与 `pool`；`crates/executor/src/internal/exec/executor.rs:410-430, 635, 641` 有 `ExecutorKillerHandler`，`exec::next` 前后查 `handle_sql_killer_signal()?`；`crates/executor/src/select.rs:121-151` 已移植 `check_max_execution_time_exceeded`。 | 1) `ExecCtx` 缺少 `killer`、`cancel: CancellationToken`、`cpu`、`store_windows` 及 `CheckpointHook` 实现；<br>2) `exec::next` 未调用时间片 `checkpoint`；<br>3) 新栈执行期间无 `max_execution_time` 后台杀除与 `ProcessInfo` 注册。 |
| **Parser / IR / SimpleExec / Router** | `crates/parser/grammar/tidb_typed.y:5212-5216` 将 `KillStmt#0..3` 降级为 `ph(...)` → `ast::Stmt::Unported`；`crates/sql/src/ir.rs:329-334` 报 `1235`。`crates/executor/src/simple.rs:1-56` 仅实现 `ADMIN FLUSH ... PLAN_CACHE`。 | 1) 需要提升 `ast::Stmt::Kill`、`PlanBuilder` `1227` 校验、`SimpleExec::execute_kill_stmt` 及在 `--new-stack=off/fallback/strict` 全模式放行；<br>2) **`crates/poc/session/src/lib.rs:1121-1159` 先 `self.router.take()` 再 `.await` 再放回**——若外层 `select!` drop future 会永久丢失 `self.router`！且外层 drop `NewSession::execute` / `ExecStmtResult` 会跳过 `exec::close`（`crates/session/src/exec_result.rs:13-15`）。 |
| **连接层与 ID 分配** | `crates/mysql-protocol/src/server.rs:82, 136` 使用 `next_connection_id: AtomicU64::new(1)` 顺序递增；`conn.rs:1082` 用 `catch_unwind_future` 保护 `dispatch`；`crates/server/src/conn.rs:78-149` 一次编码到 `ResultPackets`。 | 1) **关键 Bug 风险**：`next_connection_id = 1, 2, 3...` 产生奇数 ID `1, 3, ...`，若直接接入 Go `globalconn::ParseConnID` 会因 `id & 1 == 1 && id >> 32 == 0` 被误判为“截断的 32 位 64-bit ID”而报 Warning `1105`、无法 KILL；<br>2) `PacketIo`（`packet.rs:75-81`）为单向 `BufReader<BufWriter<S>>`，缺少并发安全的读端断连探测与写端可中止原语；<br>3) 无 `ServerSessionManager`、无 per-command 取消。 |
| **Sysvars & Config** | `crates/sessionctx/src/variable/sysvar_data.rs:191, 193, 359-460, 799` 与 `crates/sessionctx/src/hint.rs:22-40` 已对齐所有相关 sysvar；`sysvar_data.rs:191, 193` 标明 `tidb_expensive_query_time_threshold` 与 `tidb_expensive_txn_time_threshold` 为 `ScopeInstance`。 | 1) **`SysVarSync::rebuild`（`crates/poc/domain/src/sysvar_loader.rs:161`）传 `None` 跳过全部 `SetGlobal` hook**，导致 `ServerMemoryLimit` 永久为 **`0`**（未跑 `sg_tidb_server_memory_limit` `session_hooks_b2.rs:410` 设入 `"80%"`），且 `ServerMemoryLimitSessMinSize`、`OOMAction`、`EnableTmpStorageOnOOM`、`QueryLogMaxLen` 无法启动初始化与集群同步；<br>2) `crates/base/src/config/defaults_gen.rs:301` 默认 `oom_action` 为小写 `"cancel"`（而 sysvar `tidb_mem_oom_action` 为 `"CANCEL"`）。 |
| **全局集群 KILL** | `crates/poc/domain/src/serverinfo.rs:37, 74` 硬编码 `server_id: 0`、`disable_rpc: true`；`crates/poc/server/src/rpc.rs:292-314` 对 `ExecType::TypeKill` 返回 `"this exec type TypeKill doesn't support yet"`。 | 未申请 `/tidb/server_id`，未支持远程 `TypeKill` RPC。 |

### 2.2 全部 `checkpoint(&())` 空占位与手动 `consume` / 算子接入现状清单

在新执行栈与 `tidb-store` 中共有 **13 处 `checkpoint(&())` 空占位** 与 **2 处局部 `KillerHook`**（另含 legacy 2 处、测试 1 处），当前仅让出 CPU 或未统一通过 `ExecCtx` 检查取消与信号：

| 文件与行号 | 当前写法 | 缺陷分析 |
|---|---|---|
| `crates/store/src/distsql/select_result.rs:380` | `let Ok(()) = tidb_runtime::checkpoint(&()).await;` | `sortedSelectResults.next` k-way merge 循环内**无任何 kill 检查**；`new_sorted_select_results`（`:170-209`）未接收 `sql_killer` / `cancel`。 |
| `crates/store/src/distsql/select_result.rs:637, 645, 650` | `let Ok(()) = tidb_runtime::checkpoint(&()).await;` | `fetch_resp_with_intermediate_results` 仅在 `:615` 收到包后查过一次 `sql_killer`，空响应自旋与返回路径未集成到统一 hook。 |
| `crates/store/src/distsql/select_result.rs:726` | `let Ok(()) = tidb_runtime::checkpoint(&()).await;` | `read_from_default` 行解码循环内无 kill 检查。 |
| `crates/store/src/distsql/select_result.rs:1219` | `let Ok(()) = tidb_runtime::checkpoint(&()).await;` | `selectResultIter.next` 切换响应循环内无 kill 检查。 |
| `crates/executor/src/distsql.rs:2781, 2860, 3185, 3207` | `let _ = stride.checkpoint(&()).await;` 紧接下一行 `self.shared.base.killer.handle_sql_killer_signal()?;` | 拆成两次检查，未使用统一 `ExecCtx` `CheckpointHook`。 |
| `crates/executor/src/sortexec/sort.rs:1299` | `let Ok(()) = stride.checkpoint(&()).await;` | `ResultGenerator::generate_result_from_memory` 内存多路归并输出循环**完全无 Kill 检查**。 |
| `crates/executor/src/sortexec/parallel_sort_worker.rs:194, 323` | `:194` 每 100 行调 `sql_killer.handle_sql_killer_signal()?` 后调 `stride.checkpoint(&()).await`；`:323` 调 `tidb_runtime::slice::checkpoint(&()).await` | `ParallelSortWorker`（`:60`）未统一持 `ExecCtx` hook，`:323` 无 kill 检查。 |
| `crates/executor/src/aggregate/agg_hash_final_worker.rs:146, 195` | `stride.checkpoint(&kill_hook).await?;`（局部 `KillerHook(&killer)` L82） | 虽已检查 `ExecutorKillerHandler`，但使用局部 hook 而非统一 `ExecCtx`，且与 `:161` `mem_tracker.checkpoint()` 分离。 |
| Legacy / 测试：`crates/poc/executor/src/lib.rs:166`、`crates/distsql/src/lib.rs:140`、`crates/executor/src/testutil.rs:238` | `checkpoint(&()).await` | Legacy / 测试点位（testutil 可接 mock `ExecCtx`；legacy 路径随 `poc-executor` 取消透传中止）。 |

此外，注意两个算子级现状（直接影响 Part 1–2 的内存配额与 E2E 选型）：
1. **手写 `mem_tracker.consume(...)` 与缺失 `Drop` 归零**：
   - `ProjectionExec`（`crates/executor/src/projection.rs:249, 333, 360, 400, 446, 454, 497, 506, 605, 619, 623, 704`）仍使用手写 `consume(...)`，且 `Drop`（`:514-527`）仅 `abort()` 任务而未调用 `mem_tracker.replace_bytes_used(0)`；
   - `SortExec`（`crates/executor/src/sortexec/sort.rs:768-780, 897-899, 1137-1138`）同样仍使用手写 `mem_tracker.consume(...)`，仅在 `close()`（`:898`）调用 `replace_bytes_used(0)`，`Drop for ParallelTasks`（`:185`）与 `SortExec` 均**无 `Drop` 内存归零**。
2. **`PhysicalHashAgg` 尚未接入新栈路由**：
   - `crates/executor/src/builder.rs:186-279, 675-683` 尚未将 `PhysicalHashAgg`（`D10.1`）接入 `ExecutorBuilder::build`，`crates/poc/session/src/router/precheck.rs:33-34` 将 `GROUP BY` / `HAVING` 退回 PoC；而 `PhysicalSort`（`builder.rs:237`）与 `PhysicalTopN`（`:247`）已接入且 `precheck.rs:59-68` 放行 `ORDER BY` / `LIMIT`。因此新栈 E2E 内存超限实验需以 **`SortExec`（大表 `ORDER BY`） / `ProjectionExec`** 为准，`HashAggExec` 用 `ExecStmt` / 算子集成测试覆盖，直到 `D10.1` 落地。

### 2.3 未隔离的控制面任务（含隐式 `KvClient` 内部任务）清单

目前所有控制面后台循环及其依赖的 `KvClient` 内部任务均运行在主运行时（`ExecPool` / `Handle::current()`）上：

| 组件 | 源码与 spawn 行号 | 当前调度位置与风险 |
|---|---|---|
| **共享 `KvClient` 内部循环** | `crates/poc/server/src/main.rs:437-442`（在 `ExecPool` 上连接 `kv`），触发：<br>• `client-rust/src/pd/timestamp.rs:81`（`TimestampOracle::new` `tokio::spawn(run_tso)`）<br>• `client-rust/src/store/batch.rs:154`（`BatchConn::start` `tokio::spawn(send_loop.run())`）及 tonic HTTP/2 连接 task | 即使仅把 `SchemaSync::start` 等逻辑挪走，若它们继续复用在 `ExecPool` 上建立的共享 `kv`，其 TSO、TiKV gRPC 与 HTTP/2 IO 仍受 `ExecPool` 重负载影响！必须为控制面在 `ControlRuntime` 上**单独连接轻量 `ctrl_kv`**。 |
| `stmt-timing` | `crates/poc/server/src/main.rs:520` | `pool.spawn("stmt-timing", ...)` |
| `/status` + gRPC 服务 | `crates/poc/server/src/main.rs:540` + `crates/poc/server/src/status.rs:111` | `pool.spawn("status-server", ...)` + `conns.spawn(...)`（HTTP 健康探针与 `rpc.rs` gRPC 请求） |
| `SchemaSync` | `crates/poc/domain/src/sync.rs:147, 148, 150, 620` | `watch_global_version`、`sync_loop`（含 `read_mdl_jobs`）、`watchdog`、`keep_alive`（全用 `tokio::spawn`） |
| `PrivilegeSync` | `crates/poc/domain/src/privilege_loader.rs:97, 104, 105` | `watch_loop`、`periodic_refresh_loop`、`periodic_sysvar_refresh_loop`（全用 `tokio::spawn`） |
| `SysVarSync` | `crates/poc/domain/src/sysvar_loader.rs:87, 97, 117` | 捕获 `Handle::current()`（主运行时）给独立 OS 线程 `"sysvar-cache"` 做 `fetch_rt.block_on(...)`，并在 `rt.spawn(watch_loop)` |
| `StoreLiveness` | `client-rust/src/store/liveness.rs:179` | `tokio::spawn` 每秒探测死 TiKV 节点的 `grpc.health.v1.Health/Check`（控制面探针）。**对比**：`client-rust/src/store/batch.rs:154, 673`（`SendLoop` / `recv_loop`）与 `client.rs` 属数据面 RPC，**应留在数据面运行时** |
| 看门狗（尚未创建） | 待建 `ExpensiveQueryHandle` & `ServerMemoryLimitHandle` | 100ms 巡检必须运行在控制面运行时 |

### 2.4 `layers.toml` 边界约束与并发组件放置限制

根据 `layers.toml`（`N0`）：
- 分层 crate 严格要求 `dep.rank < self.rank`，且**绝对不允许依赖 `[legacy]` crate**（`poc-server`、`tidb-mysql-protocol`、`poc-session`、`poc-domain` 等）。
- 相关 rank：`tidb-base` (0)、`tidb-metrics` (0)、`tidb-memory` (1)、`tidb-runtime` (1)、`tidb-log` (1)、`tidb-types` (2)、`tidb-execdetails` (4)、`tidb-sessionctx` (7)、`tidb-store` (11)、`tidb-domain` (12)、`tidb-planner` (13)、`tidb-executor` (14)、`tidb-session` (15)、`tidb-server` (16，当前拥有 `D03.1`, `D03.2`)。
- **`tidb-planner` (13)** 的 `PlanBuilder` 需要调用 `SessionManager::get_process_info()` 进行 `KILL` `1227` 权限校验；**`tidb-executor` (14)** 的 `SimpleExec` 需要调用 `SessionManager::kill()` / `server_id()`，而 `reset_context_of_stmt` 需要从 `SessionContext` 注入来自 **`tidb-session` (15)** 的 `LogHook`（`LogOnQueryExceedMemQuota`）；真正的活跃连接注册表由 **`tidb-server` (16)**（`ServerSessionManager`、`ConnIdAllocator`）与 **`tidb-mysql-protocol` `[legacy]`** 协同持有（`tidb-mysql-protocol` 向下依赖 `tidb-server`）。因此必须通过 **`N0` §4 规则 2（接口反转）** 严格分层放置。

---

## 3. Rust 架构与分层设计

### 3.1 分层摆放与接口反转总览

```
[rank 1]  tidb-runtime        : ControlRuntime (2 threads), CancellationToken, CheckpointHook, ExecPool, CpuRecorder
[rank 1]  tidb-memory         : SqlKiller (set_on_kill, is_connection_alive, finish), Tracker, ActionOnExceed,
                                memstats (含 ReadMemInterval 300ms TTL 修复)
[rank 7]  tidb-sessionctx     : ProcessInfo, trait SessionManager, SessionVars.{mem_tracker, disk_tracker, sql_killer}
[rank 11] tidb-store          : SelectResult / CopIterator / Backoffer 使用传入的 SqlKiller + CancellationToken
[rank 13] tidb-planner        : PlanBuilder::build_kill 读取 PlanContext::session_manager() 做 1227 权限校验
[rank 14] tidb-executor       : ExecCtx (killer, cancel, cpu, store_windows, CheckpointHook); SimpleExec::execute_kill_stmt;
                                reset_context_of_stmt 通过 SessionContext::mem_exceed_log_hook() 接入 LogHook
[rank 15] tidb-session        : Session 安装 on_kill->cancel、ProcessInfo、ExpensiveQueryHandle::run、ServerMemoryLimitHandle::run
[rank 16] tidb-server         : ServerSessionManager (impl SessionManager)、ConnIdAllocator（在 crates/server/Cargo.toml 增依赖 tidb-runtime[1]、tidb-sessionctx[7]）
[legacy]  tidb-mysql-protocol : 依赖 tidb-server；PacketIo 读写半部拆分（共享 seq/IoCounts）、DisconnectDetector (poll_fill_buf)、
                                per-command cancel、Conn 注册/注销
[legacy]  poc-session / server: Router 修复 router.take()、全模式放行 KillStmt、PoC 取消透传；ControlRuntime 独立连接 ctrl_kv
                                并托管 SchemaSync / PrivilegeSync / SysVarSync / status / watchdogs / StoreLiveness
```

- **依赖校验**：
  - `tidb-sessionctx` (7) 仅依赖 `tidb-base` (0)、`tidb-memory` (1)、`tidb-execdetails` (4) 等下层 crate，可承载 `ProcessInfo` 与 `pub trait SessionManager: Send + Sync`；
  - `tidb-executor` (14) 绝不依赖 `tidb-session` (15)，只通过 `glue::SessionContext` 新增两个方法：
    - `fn session_manager(&self) -> Option<Arc<dyn SessionManager>> { None }`
    - `fn mem_exceed_log_hook(&self) -> Option<tidb_memory::action::LogHook> { None }`
  - `tidb-session` (15) 依赖 `tidb-types` (2)、`tidb-memory` (1)、`tidb-sessionctx` (7)、`tidb-runtime` (1)，合法承载 `ExpensiveQueryHandle` 与 `ServerMemoryLimitHandle`（含 `MEMORY_USAGE_OPS_HISTORY` 环形缓冲）；
  - `tidb-server` (16) 位于 `D03.1` 归属层，仅新增比自己 rank 更低的 `tidb-runtime` (1) 与 `tidb-sessionctx` (7) 依赖，承载 `ServerSessionManager` 与 `ConnIdAllocator`；`tidb-mysql-protocol` `[legacy]` 向下依赖 `tidb-server` 完成连接托管；
  - `poc-server` `[legacy]` 在启动时装配 `ControlRuntime`、`ctrl_kv`、`ServerSessionManager`、`ExpensiveQueryHandle`、`ServerMemoryLimitHandle`，并把 `ControlRuntime::handle()` 传给 `poc-domain` 与 `client-rust`。

### 3.2 `ExecCtx` 升级、`CancellationToken` 与 `Checkpoint` 不变量

#### 3.2.1 `tidb_runtime::cancel::CancellationToken`（零新增外部 crate）
在 `crates/runtime/src/cancel.rs` 新增轻量级取消令牌（遵循 `#![forbid(unsafe_code)]`，不引入 `tokio-util` 新依赖）：
- 内部状态为 `Arc<CancelInner>`，包含 `flag: AtomicBool` 与 `tx: tokio::sync::watch::Sender<bool>`（初始 `false`）。
- **`cancel(&self)`**：`if !self.flag.swap(true, Ordering::SeqCst) { let _ = self.tx.send_replace(true); }`（幂等、无锁、不会丢唤醒）。
- **`is_cancelled(&self) -> bool`**：单次 `self.flag.load(Ordering::Acquire)`（热路径零锁）。
- **`async fn cancelled(&self)`**：若 `is_cancelled()` 立即返回；否则订阅 `let mut rx = self.tx.subscribe(); let _ = rx.wait_for(|v| *v).await;`（`watch::Receiver::wait_for` 天然 cancel-safe）。

#### 3.2.2 `ExecCtx` 定义与 `CheckpointHook` 实现
将 `crates/executor/src/exec.rs` 从 `{ stmt, pool }` 升级为 `foundation-interfaces §2` 完整形态：

```rust
#[derive(Clone)]
pub struct ExecCtx {
    pub stmt: Arc<StatementContext>,
    pub pool: Option<ExecPool>,
    pub killer: Arc<SqlKiller>,
    pub cancel: CancellationToken,
    pub cpu: Option<CpuRecorder>,
    pub store_windows: Option<Arc<StoreWindows>>,
}
```

- **实现 `tidb_runtime::CheckpointHook`**：
  ```rust
  impl CheckpointHook for ExecCtx {
      type Error = Error;

      #[inline]
      fn check(&self) -> Result<(), Error> {
          if self.cancel.is_cancelled() || self.killer.get_kill_signal() != 0 {
              return self.killer.handle_signal();
          }
          self.killer.handle_signal() // 含 1s 节流 is_connection_alive 探针
      }
  }
  ```
  提供便捷异步方法 `pub async fn checkpoint(&self) -> Result<(), Error> { tidb_runtime::checkpoint(self).await }`。
- **关键不变量——为什么 `ExecCtx::check()` 绝对不调用 `stmt.mem_tracker.checkpoint()`**：
  如 `crates/memory/src/tracker.rs:525-535`（`A1` §3.3）所证，`Tracker::checkpoint()` 会读取自身的 `grew` 标志并在回溯中清除**所有祖先**的 `grew` 标志。若祖先 `stmt.mem_tracker.checkpoint()` 在叶子 `TrackedAlloc` 之前清空自身 `grew`，会导致一次增长触发多次动作链或祖先提前越权/漏记；因此，所有分配了内存的叶子算子必须自行在各自循环中调用 `self.mem_tracker.checkpoint()?`（在向父级回溯时原子一次性触发动作链并清理祖先 `grew` 标志）。对同时持有 `ExecCtx` 与算子局部 `mem_tracker` 的密集循环，提供组合适配器 `pub struct OpCheckpoint<'a> { pub ctx: &'a ExecCtx, pub mem: &'a Arc<Tracker> }`，先 `self.mem.checkpoint()?` 再 `self.ctx.check()?`。
- **per-statement `SqlKiller` ↔ `CancellationToken` 绑定**：
  - `Session::execute_stmt` 每次语句开始时创建新的 `cancel = CancellationToken::new()`（确保上一条被杀语句的 token 不会污染下一条），并在 `reset_context_of_stmt` 重置 `killer` **之后**注册：
    ```rust
    let cancel_clone = cancel.clone();
    self.core.sctx.killer.set_on_kill(Some(Box::new(move || cancel_clone.cancel())));
    ```
  - 配合 RAII drop guard 在语句退出或 future drop 时清 `set_on_kill(None)`。如果并发在 `set_on_kill` 注册瞬间前刚好已写入 `killer.signal != 0`，构建 `ExecCtx` 时立即检查 `if killer.get_kill_signal() != 0 { cancel.cancel(); }`，实现零竞态窗口。
- **`tidb-store`、`client-rust` 与所有算子 `checkpoint(&())` 统一替换**：
  - `crates/store/src/distsql/select_result.rs`：**删除全部 6 处 `checkpoint(&())`**（`:380, :637, :645, :650, :726, :1219`）。`tidb-store`（rank 11）不依赖 `tidb-executor`（rank 14）：
    - 在 `tidb-sessionctx::distsql_ctx::DistSQLContext` 中新增 `pub cancel: Option<CancellationToken>`（由 `SessionCore::get_dist_sql_ctx` 传入当前语句 `cancel`）；
    - 在 `tidb-store` 为 `(Option<&Arc<SqlKiller>>, Option<&CancellationToken>)` / `DistSQLContext` wrapper 实现 `CheckpointHook<Error = Error>`；
    - **`new_sorted_select_results`（`select_result.rs:170-176`）与 `SortedSelectResults`（`:197-209`）**：透传并持有 `sql_killer` 与 `cancel`，在 k-way merge 循环（`:380`）同时响应取消与信号检查；
    - **`CopIterator::recv_from_resp_ch`（`crates/store/src/copr/coprocessor.rs:1304-1337`）**：不再 `let _ = ctx;`，而是将 `ctx.cancel.cancelled()` 加入 `tokio::select!` 退出分支，并在唤醒时返回对应被杀信号或错误；
    - **Backoff 与 `client-rust` 取消契约**：在 `crates/store/src/driver/backoff.rs:89, 101`（`Backoffer::backoff`、`Backoffer::backoff_with_max_sleep_txn_lock_fast`）与 `crates/store/src/copr/client_rust.rs:57-85`（`ClientRustBackoffer`）接入 `select! { _ = cancel.cancelled() => ..., res = ... => res }`。明确：**`client-rust` 不读取 `SqlKiller.signal_handle()`**，而是通过外层 `select!` 直接 drop 在途 RPC future，触发 `client-rust/src/store/batch.rs:108-112, 215-218` 的 `Entry::is_canceled()`（`self.res.is_closed()`）与 tonic stream 取消，避免空等 backoff 或 RPC 响应。
  - `crates/executor/src/distsql.rs`（`:2781, :2860, :3185, :3207`）：全部改为 `stride.checkpoint(ctx).await?;`，删除重复的下一行 `self.shared.base.killer.handle_sql_killer_signal()?;`。
  - `crates/executor/src/sortexec/sort.rs:1299`：`ResultGenerator` 持 `ctx: ExecCtx`，改为 `stride.checkpoint(&self.ctx).await?;`。
  - `crates/executor/src/sortexec/parallel_sort_worker.rs:194, 323`：**`ParallelSortWorker`**（`:60`）持 `ctx: ExecCtx`，`:194` 合并为 `stride.checkpoint(&self.ctx).await?;`，`:323` 改为 `self.ctx.checkpoint().await?;`。
  - `crates/executor/src/aggregate/agg_hash_final_worker.rs:82, 146, 195`：删除局部 `KillerHook(&killer)`，统一持 `ExecCtx` 调 `stride.checkpoint(ctx).await?;`。
  - `crates/executor/src/internal/exec/executor.rs:635-641`：`exec::next` 保留前后快路径信号检查并视热点情况接入 `ctx.checkpoint().await?`。
  - `ProjectionExec` 与 `SortExec`：
    - Part 1 将 `ProjectionExec`（`crates/executor/src/projection.rs:249..704`）迁至 `TrackedAlloc` + 叶子 `mem_tracker.checkpoint()?`；
    - 对 `SortExec`（`sort.rs:768-780, 897-899, 1137-1138`）当前仍使用的手写 `mem_tracker.consume(...)`（以及移行期间的 `ProjectionExec`），**必须在其 `Drop` impl 中补齐 `if let Some(t) = &self.mem_tracker { let _ = t.replace_bytes_used(0); }` 安全网**（`sort.rs:185` 仅有 `Drop for ParallelTasks` abort 任务、`projection.rs:514-527` 仅有 `Drop` abort 任务，两者目前均缺 `Drop` 内存归零），确保异常退出不残留负值或泄漏字节数。

### 3.3 `KILL` 全链路、权限校验、连接 ID 修复、断连探测与严格清理顺序

#### 3.3.1 Parser / AST / Plan / SimpleExec、`ConnIdAllocator` 与 Router 协调
1. **Parser & AST（`tidb-parser` + `tidb-sql`）**：
   - 将 `crates/parser/grammar/tidb_typed.y:5212-5216` 中的 `KillStmt#0..3` 从 `ph(...)` 升格为正式 AST 变体 `ast::Stmt::Kill(Box<KillStmt>)`：
     ```rust
     pub struct KillStmt {
         pub connection_id: u64,
         pub query: bool,
         pub tidb_extension: bool,
         pub expr: Option<Box<ExprNode>>,
     }
     ```
   - 若为表达式分支且不是 `CONNECTION_ID()`，按 Go `simple.go:2672` 返回明确错误。
2. **权限检查（`tidb-planner` `PlanBuilder::build_kill`）**：
   - 对齐 Go `pkg/planner/core/planbuilder.go:3927-3945`：
     - 若目标为 `CONNECTION_ID()`（或查 `sm.get_process_info(raw.connection_id)` 得 `pi.user == self.user.username`）：无需额外全局权限，**允许普通用户自杀并立刻中止当前 `KILL` 语句返回 `1317`**；
     - 若目标连接属于其他用户：调用 `PrivilegeManager::request_dynamic_verification(active_roles, "CONNECTION_ADMIN", false)`（需有 `SUPER` 或 `CONNECTION_ADMIN`；只读节点未启用 SEM 且无 `GlobalAutoAnalyzeProcessList`，不构造假桩），不满足返回 `ErrSpecificAccessDenied`：**`1227 (42000)`: `Access denied; you need (at least one of) the SUPER or CONNECTION_ADMIN privilege(s) for this operation`**；
     - 若目标连接不存在：不报错，放行至执行器静默返回 `OK`（无错误也无 warning）。
3. **连接 ID 分配 Bug 修复与 `ParseConnID` 对齐（`tidb-server`）**：
   - **必须修复 `crates/mysql-protocol/src/server.rs:82, 136` 的连接 ID 分配规则**！当前递增 `1, 2, 3...` 产生奇数 ID，而 Go `globalconn::ParseConnID(id)`（`pkg/util/globalconn/globalconn.go:114-123`）把 bit0=1 解释为 64 位 GlobalConnID 并因高 32 位全 0 判定 `isTruncated = true`，导致 `KILL 1` / `KILL QUERY 1` 报错 Warning `1105` 而不能杀死连接 1！
   - **方案**：在 **`tidb-server` (rank 16)** 实现 `ConnIdAllocator`（对齐 Go `pkg/util/globalconn/globalconn.go:50-134`）并供 `tidb-mysql-protocol` 使用：
     - 单机 / `server_id == 0` 且 32 位模式（`enable_32bits_connection_id = true`）：分配 `(server_id << 21) | (local_id << 1)`（即始终产生偶数 `2, 4, 6...`，`local_id` 从 `1` 开始且按 `20 bits` 池复用）；若未启用 `enable_global_kill` 则支持 `SimpleAllocator`（顺序 `1, 2, 3...` 不走 `ParseConnID`）；
     - 这样在默认 `enable_global_kill = true`、`enable_32bits_connection_id = true` 下，既能正确执行 `KILL <cid>` / `KILL QUERY <cid>` 杀死任意本机连接，又能对用户手传被截断的奇数（如 `99999999`）精确返回 Warning `1105`。
4. **PoC / 新栈混合期状态机修复与 Router 行为（`crates/poc/session`）**：
   - **修复 `self.router.take()` 状态丢失缺陷（`crates/poc/session/src/lib.rs:1121-1159`）**：当前 `query`/`prepare`/`execute` 均先 `self.router.take()`、跨 `.await` 调用 `router.*(self, ...)` 再 `self.router = Some(router)`。若外层发生 future drop（或 unwind），`self.router` 将永久变 `None`、后续所有语句静默退化！**Part 1 必须先改成 RAII `Drop` 守卫自动放回 `router`（或拆分借用 `Session` 核心状态，完全移除 `.take()`）**。
   - **新栈严禁外层 `select!` drop future（维护 `exec::close` 不变量）**：在 `crates/mysql-protocol/src/conn.rs` 中，**禁止**对外层 `handler.query`/`handler.execute`（以及下游 `NewSession::execute` / `ExecStmtResult::next`）套用 `tokio::select! { _ = cancel => ... }` 去强行 drop 语句 future——否则会跳过 `ExecStmtResult` 与 `exec::close(&mut root)`（`crates/session/src/exec_result.rs:13-15`）！新执行栈一律走内部 `SqlKiller` + `CancellationToken` 协作取消并经正常 `Err` 返回，确保完整执行 7 步清理。
   - **PoC 路径取消透传与 `--new-stack=off` 下的 `KillStmt` 适配**：
     - 无论 `--new-stack=off|fallback|strict`，所有连接均注册到同一个进程级 `ServerSessionManager`，并将 `killer: Arc<SqlKiller>` 与 `stmt_cancel: CancellationToken` 透传进 `poc_session` / `poc_executor`；对于尚未迁入新栈而落入 PoC 的长查询/睡眠查询，允许在 PoC 层对 `stmt_cancel.cancelled()` 安全中止并返回 `1317` / `3024` / `2013`（在 `Drop` 守卫保证 `router` 不丢失的前提下）。
     - 对于 `ast::Stmt::Kill`：`--new-stack=fallback|strict` 直接经新栈 `PlanBuilder::build_kill` + `SimpleExec::execute_kill_stmt` 处理；在 `--new-stack=off` 时，于 `poc_session` 的 `query_inner` 入口先解析识别 `ast::Stmt::Kill`，复用同一套 `PrivilegeSource`（`CONNECTION_ADMIN`/`SUPER` 动态权限校验，确保同样报 `1227` 与普通用户自杀）及 `ServerSessionManager::kill`，保证三种路由模式行为 100% 一致。

#### 3.3.2 并发安全的断连探测（`DisconnectDetector`）与写结果集强制中止（`FinishResultSet`）

目前 `crates/mysql-protocol/src/packet.rs:75-81` 的 `PacketIo<S>` 为单结构 `BufReader<BufWriter<S>>`，在 `dispatch()` 执行期间被连接任务独占且外部不可访问。必须改造为：

1. **读写拆分（`PacketReader<R>` / `PacketWriter<W>`，保留共用状态）**：
   - 在握手与 TLS 升级阶段继续使用完整 `PacketIo`（保留原有 `into_parts()` 语义）；完成 TLS 握手进入命令循环后，使用 `tokio::io::split(maybe_tls)` 拆分为 `ReadHalf<MaybeTls<S>>` 与 `WriteHalf<MaybeTls<S>>`：
     - **共享协议状态**：`PacketReader` 与 `PacketWriter` 共享 `seq: Arc<AtomicU8>`（每条命令前 `reset_sequence()` 置 `0`，读写包统一原子 `fetch_add(1)` 递增，严格维持单流时的包序列号语义）与 `counts: Arc<Mutex<IoCounts>>`（或在 `IoCounts` 中拆为 `AtomicU64` 读/写字节计数，每命令结束刷新到 `tidb_server_packet_io_bytes` 指标）。
     - **`PacketReader`（`Arc<tokio::sync::Mutex<PacketReaderInner<R>>>`）**：内部持有 **`BufReader<ReadHalf<MaybeTls<S>>>`**（16 KiB 默认容量，对齐 Go `DefaultReaderSize = 16 * 1024`）。
       - **Idle 读取期间**：`Conn::command_loop` 持有读锁执行 `read_packet().await`；
       - **SQL 执行与写结果集期间**：`PacketReader` 不持有读锁，协议层绝不主动消费普通查询期间的数据。
   - **`is_connection_alive(&self) -> bool` 实现**（安装时机对齐 Go `conn.go:2087-2104, 2170-2173`：非事务 autocommit DML 执行前、其余语句在写回结果集阶段；在 `SqlKiller` 内置 1s 节流下调用）：
     - 用 `try_lock()` 获取 `PacketReader`；若正被占用，按 Go 保守返回 `true`（未知视为存活）；
     - 若 `!reader.buffer().is_empty()`，直接返回 `true`；
     - **关键要求：绝对不要绕过 `BufReader` 直接对 `ReadHalf` 调 1 字节 `poll_read`！** 必须通过一次性无阻塞 `now_or_never` 轮询 **`AsyncBufRead::poll_fill_buf`**（作用于 `BufReader<ReadHalf<MaybeTls<S>>>`）：
       - `Poll::Ready(Ok(buf))` 且 `!buf.is_empty()`（pipelined 提前到达的下一命令字节）→ **该数据直接安全留在 `BufReader` 内部缓冲区中**，零丢失、零额外缓冲拼装，返回 `true`；
       - `Poll::Ready(Ok(&[]))`（TCP EOF / TLS `close_notify`）或 `Poll::Ready(Err(_))`（`ECONNRESET`/`EPIPE`）→ 连接已断，返回 `false`（触发 `QueryInterrupted`）；
       - `Poll::Pending` → 返回 `true`。
     - **`tokio::io::split` `BiLock` 争用与 TLS 1.3 `KeyUpdate` 并发安全说明**：`tokio::io::split` 内部用 `BiLock` 保护 `MaybeTls<S>`。当 `PacketWriter` 正在异步写响应或 TLS 1.3 触发 `KeyUpdate` 控制帧交互时，读半部 `poll_fill_buf` 遇到 `BiLock` 争用或内部状态让出均会返回 `Poll::Pending`。因此将 `try_lock()` 失败与 `Poll::Pending` 一律保守判为 `true`（对齐 Go `IsAlive() < 0` 未知视存活），既无死锁风险又绝不会误杀活跃 TLS/流式连接。
     - 全程零 `unsafe`，兼容明文 `TcpStream` 与 `TlsStream`。
2. **连接级取消、`FinishResultSet` 写阻塞打断与半包处理**：
   - `ServerSessionManager`（位于 **`tidb-server` rank 16**）为每条连接保存 `ConnControl`：
     - `killer: Arc<SqlKiller>`
     - `stmt_cancel: ArcSwap<CancellationToken>`（当前在飞语句 token）
     - `finish_cancel: ArcSwapOption<CancellationToken>`（仅在写结果集期间存在）
     - `conn_shutdown: CancellationToken`（`KILL CONNECTION` / `KillAllConnections` 触发，永远不可逆）
     - `status: AtomicU8`（`Reading` / `Dispatching` / `WaitShutdown` / `Shutdown`）
   - `ServerSessionManager::kill(conn_id, query, max_exec_time, runaway)`：
     1. 打 `[INFO] [server.go:923-style] [kill]`，递增 `tidb_server_event_total{type="kill"}`。
     2. 若 `!query`（`KILL CONNECTION`）：置 `WaitShutdown`、触发 `conn_shutdown.cancel()`（打断空闲 `read_packet().await` 与阻塞写），并向下继续触发 `killQuery`。
     3. 触发 `killQuery`：调用 `killer.send_kill_signal(reason)`（内部触发 `on_kill` → `stmt_cancel.cancel()`），随后调用 `killer.finish_result_set()`。
   - 写回结果集阻塞处理（覆盖当前 `ResultPackets` 物化路径以及后续 `D03.1` 流式 `write_chunks`）：
     - 进入写回前设置 `killer.in_write_result_set.store(true, SeqCst)`、`finish_cancel` 及 `killer.set_finish_func(...)`；
     - 在 `PacketWriter::write_packet` / `flush` 的每个 `.await` 点套用 `tokio::select! { biased; _ = conn_shutdown.cancelled() => ..., _ = finish_cancel.cancelled() => ..., res = io_op => res }`；
     - **半截包强制断连不变量**：若是 `KILL QUERY` 且在**尚未向 socket 写出任何数据前**被执行/准备阶段中断，连接保持存活并完整写出 ERR 包（`1317`/`3024`/`8175`/`8176`）；但若在 `PacketWriter` 已经向 `BufWriter`/socket 写出**部分结果包或包头字节后**被 `finish_cancel` 或 `conn_shutdown` 打断（等价 Go `conn.go:2349-2365` `retryable == false`），**绝不可复用错位的半截包写 ERR 包**，必须立即把该次故障标记为连接致命错误、`shutdown` 写半部并退出连接循环。

#### 3.3.3 严格的 7 步资源清理顺序（状态机与不变量）

为保证在 `KILL QUERY`、`KILL CONNECTION`、`max_execution_time`、OOM (`8175`/`8176`) 与 TCP 断连下绝不发生任务泄漏、内存归零漂移或下一条语句误杀，**严格规定清理顺序如下（任何阶段均不可跳过）：**

```
[1] SendKillSignal (CAS 0 -> reason)
      ├──> 打印 WARN "kill initiated" ("connection ID", "reason")
      └──> on_kill 触发 CancellationToken::cancel()
             (+ 若 KILL CONNECTION: status=WaitShutdown, conn_shutdown.cancel())
      │
      ▼
[2] 任务协同唤醒与下游终止
      ├──> Executor/DistSQL/SelectResult/Backoffer 在 select! 与 CheckpointHook 立刻返回 Err(killed)
      └──> tidb-store Drop 被 select! 取消的 RPC future -> client-rust 探测 Entry::is_canceled() 与 stream 丢弃中止请求
      │
      ▼
[3] 并行算子内部收尾（ProjectionExec / SortExec / HashAggExec / IndexLookUpExecutor）
      ├──> 置 finish_ch / finish_tx，通知全部 worker/fetcher 退出
      ├──> 逐个 await JoinHandle 汇合（若整体 future 被直接 Drop，则由 Drop guard send_replace(true) + abort()）
      ├──> 取回 fetcher 暂管的子执行器 (set_all_children) 以便后续 Exec::Close 传播
      └──> 排空内部输入/输出 channel 中的 Chunk，释放携带 TrackedAlloc / consume 记账的对象
      │
      ▼
[4] 自顶向下执行器树 Close 与内存归零
      ├──> exec::close(&mut root) 依次关闭各子算子
      └──> 析构含 ColAlloc::Tracked 的区并调 alloc.flush()；SortExec / ProjectionExec 在 close() 与 Drop 中均保证 replace_bytes_used(0)
      │
      ▼
[5] 语句收尾与全路径 Tracker 解绑（覆盖 RecordSet、handle_no_delay、编译/Open 早退及 future Drop）
      ├──> RecordSet 路径: FinishExecuteStmt -> CloseRecordSet -> 显式调用 sc.detach_mem_disk_tracker()
      ├──> 无 RecordSet 路径 (handle_no_delay Ok(None)/Err、Compile/Open/PointGet Err、Router 回退 StmtCheckpoint::restore):
      │    由 RAII StmtTrackerGuard 与 restore() 显式对当前 aborted stmt_ctx 调 detach_mem_disk_tracker()
      ├──> Tracker::detach() 将 stmt tracker 从 mem_root.children 摘除，清空会话根 action 链，并调 SqlKiller::reset() (swap(0))
      └──> 清除 set_on_kill(None)、is_connection_alive、finish_func
      │
      ▼
[6] 审计日志与协议回包
      ├──> 打印 WARN "kill finished" 与 INFO "command dispatched failed"
      ├──> KILL QUERY / 超时 / OOM（未写出半包结果集）：向客户端发送 ERR 包 (1317/3024/8175/8176/8253)，连接继续就绪
      └──> KILL CONNECTION / 客户端断连 / 写半包中断：注销 SessionManager、关闭连接并回收 ConnID
      │
      ▼
[7] 看门狗滞后解除
      └──> ExpensiveQueryHandle / ServerMemoryLimitHandle 在下个 100ms tick 发现 ProcessInfo.time 已清除/变更，
           结束 killing 状态、清理 MemUsageTop1Tracker 并打印清理成功日志
```

### 3.4 内存配额、OOM Action、日志接线与 `D01.10` 边界

#### 3.4.1 会话根 Tracker、`ResetContextOfStmt`、全局 Sysvar 同步与全路径解绑
1. **会话根 Tracker 归属**：在 `SessionShared` / `SessCtx` 创建全局唯一持久的根追踪器（与 `sql_killer: Arc<SqlKiller>` 同生命周期）：
   - `mem_root: Arc<Tracker>`：`is_root_tracker_of_sess = true`、`label = LabelForSession`、`bytes_limit = vars.mem_quota_query`（暴露给 `reset_context_of_stmt` 与 `SessionCore::get_dist_sql_ctx`）；
   - `disk_root: Arc<Tracker>`：`label = LabelForSession`、限额 `-1`。
2. **`SysVarSync::rebuild` 对全局内存/OOM 原子量的白名单初始化与实时同步（解决 `ServerMemoryLimit == 0` 陷阱）**：
   - `crates/poc/domain/src/sysvar_loader.rs:161` 当前传 `cache.rebuild(None, ...)` 跳过所有 `SetGlobal` hook，导致 `tidb_memory::tracker::ServerMemoryLimit`（`tracker.rs:62`）启动后永久为 `0`、`ServerMemoryLimitSessMinSize`/`OOMAction`/`EnableTmpStorageOnOOM`/`QueryLogMaxLen` 也无法感知启动默认与集群 `SET GLOBAL` 变更！
   - 方案：在**继续屏蔽写系统表 / DDL / PD 等有副作用 hook** 的前提下，令 `SysVarSync::rebuild` 在每次加载（含启动兜底默认初始化）时，对白名单内的**纯只读全局原子量**同步刷新：
     - `tidb_server_memory_limit` → 解析 `"80%"` / 字节数写入 `ServerMemoryLimit` & `ServerMemoryLimitOriginText`（按 `sg_tidb_server_memory_limit`，`session_hooks_b2.rs:410-419`）
     - `tidb_server_memory_limit_sess_min_size` → 写入 `ServerMemoryLimitSessMinSize`
     - `tidb_mem_oom_action` → 写入 `OOMAction`（**统一规范为大写 `"CANCEL"` / `"LOG"`**；同时处理 `crates/base/src/config/defaults_gen.rs:301` 的小写 `"cancel"`：在配置默认值或读取/比较端做 `to_ascii_uppercase()` 规范化并保证 sysvar 默认值与 `OOMAction` 比较大小写一致）
     - `tidb_enable_tmp_storage_on_oom` → 写入 `EnableTmpStorageOnOOM`
     - `tidb_query_log_max_len` → 写入 `QueryLogMaxLen`
   - 同时明确区分：`tidb_expensive_query_time_threshold`（`sysvar_data.rs:191`）与 `tidb_expensive_txn_time_threshold`（`:193`）属于 **`ScopeInstance`**（不存于 `mysql.global_variables`），直接由单机 `SET GLOBAL` hook 更新实例原子量并由 `ExpensiveQueryHandle` 每次 tick 读取。
3. **`crates/executor/src/select.rs:644-657` `reset_context_of_stmt` 接线**：
   - 对齐 Go `pkg/executor/select.go:997-1048`：
     1) 重置 `mem_root`：`detach()`、`unbind_actions()`、`set_bytes_limit(vars.mem_quota_query)`、`reset_max_consumed()`；
     2) 绑定身份与信号：`mem_root.session_id.store(vars.connection_id, SeqCst)`、`mem_root.set_killer(Some(Arc::clone(&killer)))`、`disk_root.set_killer(Some(Arc::clone(&killer)))`；
     3) 安装动作与日志钩子（读取 `OOMAction`，统一 `eq_ignore_ascii_case("CANCEL")` 判断）：
        - 若等于 `"CANCEL"`：构造 `PanicOnExceed::new(Some(killer.clone()), vars.connection_id)`，调 `action.set_log_hook(ctx.mem_exceed_log_hook())`，调用 `mem_root.set_action_on_exceed(Some(action))`；
        - 否则（`"LOG"`）：构造 `LogOnExceed::new(vars.connection_id)`，调 `action.set_log_hook(ctx.mem_exceed_log_hook())`，调用 `mem_root.set_action_on_exceed(Some(action))`；
     4) 创建语句 tracker 并挂载：`sc.init_mem_tracker(LabelForSQLText, -1)` → `sc.mem_tracker.attach_to(&mem_root)`；`sc.init_disk_tracker(LabelForSQLText, -1)` → 若 `EnableTmpStorageOnOOM.load(SeqCst)` 开启则 `sc.disk_tracker.attach_to(&disk_root)`；
     5) 当未来 `D02.2`/`D13.3` 解锁 `MEMORY_QUOTA` hint 时，在 `ExecStmt::exec` 前检查 `if sc.stmt_hints.has_mem_quota_hint { mem_root.set_bytes_limit(sc.stmt_hints.mem_quota_query); }`（对齐 Go `pkg/executor/adapter.go:589-591`）。
4. **全路径强制 `detach_mem_disk_tracker()`（防止 `mem_root.children` 泄漏强引用）**：
   - 如 `crates/memory/src/tracker.rs:398, 406-426` 所证，`mem_root.children` 持有子 tracker 的强引用 `Arc<Tracker>`，**`StatementContext` 析构绝不会自动解绑！** 必须修改以下 4 类路径并更正 `crates/executor/src/adapter.rs:681-684, 916-917` 的错误注释：
     1) **`ExecStmt::close_record_set`（`adapter.rs:908-918`）**：显式调用 `sctx.get_session_vars().stmt_ctx().detach_mem_disk_tracker()`；
     2) **无 `RecordSet` 返回路径与早退错误路径（`adapter.rs` `exec`/`handle_no_delay` 及 `session.rs:1530-1682` `execute_stmt_started`）**：包括 `compile` 失败、`open_executor` 失败、`select_for_update` 拒绝、`handle_no_delay` 返回 `Ok(None)` 或 `Err`、`txn_start_ts` 失败、`point_get` 失败以及 `async` future 被 Drop 时，一律由 RAII `StmtTrackerGuard`（`Drop` 调用 `detach_mem_disk_tracker()`、`mem_root.unbind_actions()`、`killer.set_on_kill(None)`、`killer.reset()`）兜底解绑（`RecordSet` 成功构造时移交 `CloseRecordSet` 负责，或者在 `CloseRecordSet` 同样幂等调用 `detach_mem_disk_tracker()`）；
     3) **`StmtCheckpoint::restore()`（`crates/session/src/checkpoint.rs:95-113`）**：当 Router 发生回退或重置时，**必须先对当前准备放弃的 `vars.stmt_ctx()` 调用 `detach_mem_disk_tracker()`，再调用 `vars.set_stmt_ctx(cp.stmt_ctx)`**（切勿误对旧的 `cp.stmt_ctx` 调用！），并清理 `mem_root.unbind_actions()`、`killer.set_on_kill(None)` 与 `killer.reset()`；
     4) **修复 `SessionCore::get_dist_sql_ctx()`（`crates/session/src/session.rs:853`）**：将 `session_mem_tracker` 从临时传入 `sc.mem_tracker` 改为传入 **`Some(Arc::clone(&mem_root))`**，关闭 `D07.3 spec §7 item 18` 缺口。

#### 3.4.2 日志合约接线（`#464` & `D00.3` §4.4 黄金样本 2）
- 修复 `crates/session/src/expensive_query.rs:205-217` 的语义混用，严格对齐 Go `expensivequery.go:126-145`：
  - `ExpensiveQueryHandle::log_hook(&self) -> tidb_memory::action::LogHook` 返回闭包，触发 `handle.log_on_query_exceed_mem_quota(conn_id)`；
  - 内部首先检查 `logger.level() <= LogLevel::Warn`；
  - 若 **`self.sm` 尚未注入（即 bootstrap 阶段 `sm == None`）** → 打 `[INFO] "expensive_query during bootstrap phase" ["conn"=conn_id]`；
  - 若 **`sm` 已注入（`Some(sm)`）** 但 `sm.get_process_info(conn_id)` 返回 `None`（语句/连接已退出）→ **直接静默返回**（不得误打 `"expensive_query during bootstrap phase"`！）；
  - 否则构造 `ProcessInfoForLog` 并输出 `[WARN] ["memory exceeds quota"]` 14 字段黄金日志（`D00.3 §4.4` 黄金样本 2）。

#### 3.4.3 与 `D01.10`（磁盘溢写）的清晰边界与接入契约
- **本 spec（#542）不实现 `D01.10`**（`RowContainer` / `DataInDiskByChunks` 实盘写文件、spill partition 读回等仍保持 `blocked-by:D01.10`）。
- **动作链优先级契约完全就绪**：`Tracker::fallback_old_and_set_new_action` 与 `re_arrange_fallback` 严格维持优先级序：
  $$\text{DefRateLimitPriority}(4) > \text{DefCursorFetchSpillPriority}(3) > \text{DefSpillPriority}(2) > \text{DefLogPriority}(1) > \text{DefPanicPriority}(0)$$
- **关键工程防坑（与 `SortExec` / `TopNExec` / `HashAggExec` 的交互）**：
  - `crates/executor/src/sortexec/sort.rs:429, 810` 与 `topn.rs:513` 目前**故意没有**把 `SortPartitionSpillDiskAction` / `topNSpillAction` 挂到会话 `MemTracker`（标有 `blocked-by:D01.10`）；因此当前新栈已放行的 `SortExec`（`ORDER BY`）与 `TopNExec` 在超出 `tidb_mem_quota_query` 时会稳定触发硬限制 `PanicOnExceed`（`8175`）或 `LogOnExceed`（黄金日志），**并需按 §3.2.2 要求补齐 `SortExec::Drop` 内存归零安全网**。
  - **`HashAggExec` 特殊风险**：`crates/executor/src/aggregate/agg_hash_executor.rs:609-612, 888-891` 已写入 `if let Some(session_tracker) = self.session_mem_tracker.clone() { session_tracker.fallback_old_and_set_new_action_for_soft_limit(Some(action)); }`，而 `builder.rs` 尚未接入 `PhysicalHashAgg`（`D10.1`）且 `session_mem_tracker` 传 `None`。**在 `D01.10` 落地前，`builder.rs` 应保持暂不传入 `HashAggExec.session_mem_tracker`**（仅用 `stmt_mem_tracker` 记账与算子级集成测试）——否则一旦接上 `session_tracker` 且 `EnableTmpStorageOnOOM = ON`（默认），`HashAgg` 会在执行中途达到 `0.8 * quota` 软限时进入 `in_spill_mode` 并调 `DataInDiskByChunks::add()` 返回 `blocked-by:D01.10`（`1235`），触发 Router 报错 `"VS1 router bug: blocked after the new stack's executors opened"`！待 `D01.10` 就绪后，在 `builder.rs` 传入 `mem_root` 即可实现“先限流 → 再溢写 → 溢写耗尽降级为 `LOG`/`8175`”。

### 3.5 `servermemorylimit` 与 `expensive_query` 后台看门狗

#### 3.5.1 `ServerMemoryLimitHandle`（`crates/session/src/server_memory_limit.rs`）与 `memstats.rs` TTL 修复
- **修复 `crates/memory/src/memstats.rs:44-57` 缓存永不刷新问题**：
  - Go 在 `pkg/domain/domain.go:2807, 2833` 依靠后台循环每 `ReadMemInterval (300ms)` 调一次 `ForceReadMemStats()` 刷新全局统计，而 `ReadMemStats()` 只读取缓存。当前 Rust 未移植该 `domain.go` 周期任务，`GlobalMstats.ts` 也带有 `#[allow(dead_code)]`（`memstats.rs:44`），导致 `read_mem_stats()` 首次加载后永远静止不变！
  - Part 3 修复 `memstats::read_mem_stats()`：当 `stats` 为 `None` 或 **`s.ts.elapsed() >= ReadMemInterval (300ms)`** 时自动调用 `force_read_mem_stats()` 刷新（同时可保留原子/短锁防惊群，避免热路径频繁调 `epoch::advance()`）。
- **进程总内存指标与看门狗状态机**（按 `N3+A1 §4.4` 裁定 4 读取 **`MemStats.sys` = jemalloc `stats.resident`**；100ms tick 对齐 `pkg/util/servermemorylimit/servermemorylimit.go:67-264`）：
  1. 若 `is_killing == true`：查 `sm.get_process_info(session_id)`；若仍存在且 `info.time == sql_start_time`：
     - 每 **5s** 打 WARN：`"global memory controller failed to kill the top-consumer in {s}s"`（附带 `conn`、`sql digest`、`sql text` 截断 100、`sql memory usage`）；
     - 达到 **60s**：打强制关闭告警并**仅直接调用 `s.session_tracker.killer().finish_result_set()`**（严格对齐 `servermemorylimit.go:119-121`，**不额外调用 `sm.kill`**，避免产生多余 `[INFO] [kill]` 日志），同时推进清理分支；
     - 会话消失或换条 SQL 后：复位状态、`IsKilling.store(false)`、清理 `MemUsageTop1Tracker`、打 `"global memory controller killed the top1 memory consumer successfully"`（**无 `runtime.GC()`**）。
     - **关于 Go `servermemorylimit.go:128-130` 上游缺陷的处理**：Go 在 `Succ:` 分支先调 `s.reset()`（置 `s.sessionTracker = nil`）再调 `MemUsageTop1Tracker.CompareAndSwap(s.sessionTracker, nil)`，导致 `CompareAndSwap(nil, nil)` 未清除实际已杀掉的 Top1 pointer。该差异涉及实例级资源杀除状态，**已列入 §6.2 决策点 3 由负责人裁定**（维护者建议先保存 `prev = s.session_tracker` 再复位并 `compare_and_swap(prev, None)`，单开 `upstream-pitfall` issue 与登记 `rulings.toml`）。
  2. 若 `bt = ServerMemoryLimit.load() > 0` 且 `resident > bt`：
     - 读 `MemUsageTop1Tracker`，若当前消耗 `< ServerMemoryLimitSessMinSize (128 MiB)` 则 CAS 清空并每 5s 打 `"global memory controller tries to kill the top1 memory consumer, but no one larger than tidb_server_memory_limit_sess_min_size is found"` WARN；
     - 否则查 `sm.get_process_info(session_id)`，打 WARN `"global memory controller tries to kill the top1 memory consumer"`，发送 `SqlKiller::send_kill_signal(ServerMemoryExceeded)`（**`8176`**）并触发 `on_kill` 唤醒 `CancellationToken`；
     - 更新 `MemoryMaxUsed`、`SessionKillTotal`、`SessionKillLast`、`IsKilling`，并将本次强杀快照（`kill_time`、`"SessionKill"`、`limit`、`resident`、`ProcessInfo` 全字段，其中 `SQL_TEXT` 截断 256 字符）写入 50 槽环形缓冲 `GlobalMemoryOpsHistoryManager`。

#### 3.5.2 `ExpensiveQueryHandle`（升级 `crates/session/src/expensive_query.rs`）
- 100ms 周期执行（对齐 `pkg/util/expensivequery/expensivequery.go:50-154`）：
  1. 刷新 `ExpensiveQueryTimeThreshold`（`ScopeInstance`，默认 `60s`）与 `ExpensiveTxnTimeThreshold`（`ScopeInstance`，默认 `600s`）；
  2. 遍历 `sm.show_process_list()`：
     - 若 `cur_txn_start_ts != 0` 且 `txn_cost >= txn_threshold`：每 15s 上报 `OngoingTxnDurationHistogram`，每 10m 打 `expensive_txn` WARN；
     - 若语句非空且 `cost_time >= threshold` 且距上次打印 `> 60s` 且 `logger.level() <= Warn`：调用 `log_expensive_query(cost_time, &info_for_log)`（输出 `D00.3 §4.4` 黄金样本 1）；
     - 若 `info.max_execution_time > 0 && costTime > Duration::from_millis(info.max_execution_time)`：**无级别门控地**打 `[WARN] "execution timeout, kill it"`（**精确 3 字段：`costTime`、`maxExecutionTime`、`processInfo`**，对齐 `expensivequery.go:97-98`），并调 `sm.kill(info.id, true, true, false)` → **`MaxExecTimeExceeded` (`3024`)**（关于与 `N3 §3.3` 第 5 条 `M-TICK` 协调，见 §6.2 决策点 2）；
     - 若遇到 `SELECT SLEEP(N)`：内置 `SLEEP()`（待后续移植时）需监听 `ExecCtx.cancel` 在被杀时返回标量 `1`（对齐 Go 与 §1.1 实测），而底层仍正常走 `sm.kill` 流程。

### 3.6 控制面运行时隔离（`#474 R-5` / `N3 §3.6`）与停机序

```
+-------------------------------------------------------------------+
| 数据面运行时: tidb_runtime::Runtime (ExecPool: Steal / NoSteal)    |
|   线程数 = worker_threads (默认核数), 线程名 tidb-rs-worker        |
|   • TCP listener & MySQL Conn (连接 task == 语句 task, 零交接)    |
|   • 算子并行 worker (Projection/Sort/HashAgg/IndexLookUp)         |
|   • 数据面 KvClient (kv): TimestampOracle / batch.rs SendLoop &   |
|     recv_loop / client.rs 数据面 KV 请求                           |
+-------------------------------------------------------------------+
         │ 无锁原子 / Arc<SqlKiller> / CancellationToken / ArcSwap
         ▼ (严禁控制面 .await 数据面执行体或跨越 await 持有数据面锁)
+-------------------------------------------------------------------+
| 控制面运行时: tidb_runtime::ControlRuntime                         |
|   线程数 = 2 (默认), 线程名 tidb-rs-ctrl                           |
|   • 控制面专用轻量 KvClient (ctrl_kv: 在 ctrl_rt 上 connect 1条连接,|
|     隔离其内 TimestampOracle / BatchConn / tonic HTTP/2 任务)      |
|   • SchemaSync (传 ctrl_handle + ctrl_kv: watch/sync_loop/        |
|     read_mdl_jobs/watchdog/keep_alive)                             |
|   • PrivilegeSync (传 ctrl_handle + ctrl_kv: watch/periodic loop)  |
|   • SysVarSync (传 ctrl_handle + ctrl_kv + 白名单原子刷新)         |
|   • /status HTTP 与 rpc.rs gRPC 服务 (status-server + conns)      |
|   • ExpensiveQueryHandle (100ms tick)                              |
|   • ServerMemoryLimitHandle (100ms tick)                           |
|   • stmt-timing 报告                                               |
|   • client-rust StoreLiveness (注入 ctrl_handle 到 liveness.rs:179)|
+-------------------------------------------------------------------+
```

- **控制面专用 `ctrl_kv` 与显式 `Handle` 传递（修复隐式 `tokio::spawn` 泄露）**：
  - `poc/server/src/main.rs:437-442` 原本在主运行时上连接单一 `kv`，这会把 `TimestampOracle::new`（`client-rust/src/pd/timestamp.rs:81` `tokio::spawn(run_tso)`）、`BatchConn::start`（`client-rust/src/store/batch.rs:154` `tokio::spawn(send_loop.run())`）与 tonic HTTP/2 I/O 全部固定在 `ExecPool` 上；
  - 同时 `SchemaSync::start`（`sync.rs:147, 148, 150, 620`）、`PrivilegeSync::start`（`privilege_loader.rs:97, 104, 105`）、`SysVarSync::start`（`sysvar_loader.rs:87, 97`）均调用裸 `tokio::spawn` / `Handle::current()`；
  - 因此 Part 4 规定：**在 `ctrl_rt.spawn`（`ControlRuntime` 上下文）中连接一条专用轻量控制面 `ctrl_kv`（`store_connections = 1`）并传递显式 `ctrl_handle: &tokio::runtime::Handle` 启动 `SchemaSync`、`PrivilegeSync` 与 `SysVarSync`**，彻底断绝控制面后台循环及其底层 PD/TiKV gRPC task 对 `ExecPool` 的任何依赖。
- **与 `N3 §3.1` 第 2 条的协调关系**：
  - 核心原则“**所有用户 SQL 连接与语句执行在同一个 `ExecPool` 上完成，零跨运行时任务迁移/消息交接**”保持 100% 不变（修订表述见 §6.2 决策点 1）。
  - `client-rust` 严格分界：`StoreLiveness::spawn_health_check_loop`（`liveness.rs:179`）支持可选注入 `Option<Handle>`（控制面探针），而 `batch.rs:154, 673` 与 `client.rs`（数据面 `kv` 上的 RPC 收发）继续留在数据面运行时执行，避免业务 KV 读写多一次跨运行时切换。
- **优雅停机严格顺序（对齐 `poc/server/src/main.rs:567-592` & `D03.1` §4.2）**：
  1. 信号抵达 → 触发 `stop_status` 关闭 `/status` HTTP + gRPC，等待 `status_done_rx`（≤ 5s），打 Go 规范三行停机 WARN；
  2. 停止集群监听与注销租约：`priv_sync.shutdown()`、`sys_vars.shutdown()`、`sync.shutdown().await`（立即撤销 etcd 租约避免阻塞 DDL）；
  3. 停止接受新 MySQL 连接并进入 `Server::serve` `DrainClients`（≤ 15s）与 `KillAllConnections`（≤ 1s）——**在此阶段 `ExpensiveQueryHandle` 与 `ServerMemoryLimitHandle` 保持运行**；
  4. `Server::serve` 返回后，发送 `stop_watchdogs` 信号并优雅 join 两个看门狗；
  5. 停止 `StoreLiveness`、drop `ctrl_kv` 并调用 `ControlRuntime::shutdown_timeout(5s)`，随后 `runtime`（`ExecPool`）析构并 `tidb_log::sync()` 刷盘退出。

---

## 4. 分阶段实施计划（Part 1 – Part 4）

| Part | 范围与涉及 Crates / 文件 | 核心交付内容 | `layers.toml` & 验收门槛 |
|---|---|---|---|
| **Part 1**<br>取消与 `KILL` 全链路打通 | `tidb-runtime`: `cancel.rs`, `lib.rs`<br>`tidb-sessionctx`: `ProcessInfo`, `SessionManager`, `distsql_ctx.rs`<br>`tidb-store`: `distsql/select_result.rs`, `copr/coprocessor.rs`, `driver/backoff.rs`, `copr/client_rust.rs`<br>`tidb-parser` / `tidb-planner` / `tidb-executor`: `exec.rs`, `distsql.rs`, `sortexec/`, `agg_hash_final_worker.rs`, `projection.rs`, `simple.rs`, `internal/exec/executor.rs`<br>`tidb-server` / `tidb-mysql-protocol` / `poc-session`: `Cargo.toml`, `ServerSessionManager`, `ConnIdAllocator`, `packet.rs`, `conn.rs`, `poc/session/src/lib.rs`, `router/` | 1) 实现 `CancellationToken`、升级 `ExecCtx`（遵循“仅叶子调 `mem_tracker.checkpoint()`”），消除新栈与 `tidb-store` 13 处 `checkpoint(&())` 与 2 处 `KillerHook`；<br>2) `tidb-store`（`SortedSelectResults`、`CopIterator::recv_from_resp_ch`、`Backoffer`）接入 `cancel`（通过 drop future 取消 `client-rust`）；<br>3) `ProjectionExec` 迁至 `TrackedAlloc`，`SortExec` / `ProjectionExec` `Drop` 补齐 `replace_bytes_used(0)`；<br>4) 修复 `self.router.take()` 丢失缺陷；**严禁在 `Conn` 外层 `select!` drop 新栈语句 future**（强制协作完成 7 步清理）；PoC 透传取消与 `--new-stack=off` `KillStmt` 适配；<br>5) `tidb-server`（rank 16）实现偶数 `ConnIdAllocator` 与 `ServerSessionManager`、`Parser`/`PlanBuilder` (`1227`)/`SimpleExec` (`1105`/静默)、`PacketIo` 读写拆分（共享 `seq`/`IoCounts`、`poll_fill_buf` 断连探针、半包中断直接断连）。 | • `python3 scripts/check_layers.py` 无违规；<br>• 单测覆盖全部算子与 `SelectResult`/`Backoffer` `< 50ms` 响应 kill、worker 100% 汇合、归零无漏；<br>• **真实 MySQL E2E 对照 Go v8.5.8**：重查询 `KILL QUERY` 返 `1317` 且连接存活（`SELECT 1` 成功）；非特权 `KILL` 他人返 `1227`、自杀返 `1317`；截断奇数 ID 出 `1105` Warning；不存在 ID 静默 OK；`KILL CONNECTION` 返 `2013`；TCP `SHUT_RDWR` ~1s 终止且从 processlist 清除。 |
| **Part 2**<br>内存配额、OOM Action、日志与全路径解绑（`#464`） | `tidb-sessionctx`: `variable/{session.rs, tidb_vars.rs}`<br>`tidb-executor`: `glue.rs`, `select.rs`, `adapter.rs`, `builder.rs`<br>`tidb-session`: `session.rs`, `checkpoint.rs`, `expensive_query.rs`<br>`tidb-base`: `config/defaults_gen.rs`（或大小写标准化）<br>`poc-domain`: `sysvar_loader.rs` | 1) 为 `SessionShared` / `SessionVars` 增加会话根 `MemTracker`（`is_root_tracker_of_sess=true`）与 `DiskTracker`；<br>2) `SessionContext` 新增 `mem_exceed_log_hook()`，`reset_context_of_stmt` 安装 `PanicOnExceed`/`LogOnExceed` + `LogHook`，修复 `get_dist_sql_ctx` 传 `mem_root`；<br>3) `SysVarSync::rebuild` 增设只读白名单全局原子量同步（`ServerMemoryLimit`、`ServerMemoryLimitSessMinSize`、`OOMAction`、`EnableTmpStorageOnOOM`、`QueryLogMaxLen`）并规范 `"CANCEL"`/`"cancel"`；<br>4) 修复 `log_on_query_exceed_mem_quota`（严格区分 bootstrap `sm == None` 与连接已失联）；<br>5) 全路径 `detach_mem_disk_tracker()`（`CloseRecordSet` + RAII 守卫覆盖无 `RecordSet`/早退失败 + **`StmtCheckpoint::restore()` 先对 aborted `stmt_ctx` 解绑再覆盖** + 修复 `adapter.rs:682, 916` 注释）；保持 `HashAggExec.session_mem_tracker` 暂不挂载直到 `D01.10`。 | • `check_layers.py` 通过；<br>• **真实 MySQL E2E（用新栈已接入的 `SortExec` 大表 `ORDER BY` / `ProjectionExec`）**：设 `tidb_mem_quota_query=100000` + `CANCEL` 返回精确 **`8175`** 文案，且验证 `mem_root.children` 为空、消费归 `0`；`LOG` 正常返回结果并断言字节级对齐 `D00.3 §4.4` 黄金样本 2；<br>• **`HashAggExec` 用算子/`ExecStmt` 集成测试**验证 `RateLimit(4) > Spill(2) > Log(1) > Panic(0)` 与 `LogOnExceed`（直至 `D10.1` 接入 `PhysicalHashAgg`）；验证早退错误 & Router 回退无 tracker 泄漏。 |
| **Part 3**<br>`memstats` 修复与两大 100ms 看门狗 | `tidb-memory`: `memstats.rs`<br>`tidb-session`: `expensive_query.rs`, `server_memory_limit.rs`<br>`tidb-server`: 接线看门狗启停与统计指标 | 1) 修复 `memstats::read_mem_stats()` 检查 `ts.elapsed() >= ReadMemInterval (300ms)` 自动刷新 `force_read_mem_stats()`；<br>2) 启动 100ms `ExpensiveQueryHandle::run`：读取 `ScopeInstance` 阈值、15s/10m `expensive_txn`、60s `expensive_query` 黄金样本 1、`max_execution_time` 杀除（`"execution timeout, kill it"` 精确 3 字段 `costTime`/`maxExecutionTime`/`processInfo`，无级别门控，触发 `3024`）；<br>3) 启动 100ms `ServerMemoryLimitHandle::run`：基于 jemalloc `resident`（`MemStats.sys`）、`128 MiB` 门槛、`8176` 强杀、5s 卡住告警、60s **仅调 `FinishResultSet()`**、CAS 顺序按 §6.2 决策点 3 实现、写入 50 槽 `MEMORY_USAGE_OPS_HISTORY`。 | • `check_layers.py` 与 `govec` 全绿；<br>• **真实 MySQL E2E 对照 Go v8.5.8**：对新栈慢 scan/`ORDER BY` 查询设 `max_execution_time=200` 触发 **`3024`** 与 `"execution timeout, kill it"` 日志（注：`SELECT SLEEP(5)` 行为待内置 `SLEEP()` 移植后补全 E2E）；慢查过阈值输出 `D00.3 §4.4` 黄金样本 1；配低 `tidb_server_memory_limit` 触发 **`8176`**、写入 50 槽历史并在杀后正确清理 `MemUsageTop1Tracker`。 |
| **Part 4**<br>控制面运行时隔离（`#474 R-5`） | `tidb-runtime`: `ControlRuntime`<br>`poc-domain`: `sync.rs`, `privilege_loader.rs`, `sysvar_loader.rs`<br>`poc-server`: `main.rs`, `status.rs`<br>`client-rust`: `store/liveness.rs`, `TikvConnect` | 1) 新增默认 2 线程 `ControlRuntime`（`tidb-rs-ctrl`）；<br>2) 在 `ControlRuntime` 内连接独立轻量 **`ctrl_kv`**，并将显式 `ctrl_handle` 传入 `SchemaSync::start`、`PrivilegeSync::start`、`SysVarSync::start`；<br>3) 迁移 `/status` HTTP + `rpc.rs` gRPC、`stmt-timing`、`ExpensiveQueryHandle`、`ServerMemoryLimitHandle` 至 `ControlRuntime`；<br>4) `client-rust` `StoreLiveness` 支持注入 `ctrl_handle`（`batch.rs` 与数据面 `kv` 留在 `ExecPool`）；<br>5) 落地 5 步优雅停机序。 | • 在 **`--runtime-topology=steal` 与 `--runtime-topology=no-steal`** 两种模式下均以 `worker_threads=2` 压满重 SQL 负载验证：`/status` 与 `/metrics` 延迟 `< 50ms`、etcd DDL lease 零超时、`KILL`/`max_execution_time`/`servermemorylimit` 在 `< 200ms` 内完成强杀、优雅停机零挂死。 |

---

## 5. 与 Go TiDB v8.5.8 的已知差异清单

| # | 差异项 | Go v8.5.8 行为 | `tidb-rs` 行为 | 对用户可见影响 | 分类与追踪 Issue |
|---|---|---|---|---|---|
| 1 | 全局内存限额指标与回收 | 读取 `runtime.MemStats.HeapInuse`，杀后调用 `runtime.GC()` | 按 `N3+A1 §4.4` 裁定 4 读取 jemalloc `stats.resident`（`MemStats.sys`），无 `runtime.GC()`（Rust `Drop` 立即释放） | 按 RSS 常驻内存而非 Go 堆使用判定实例超限，无 GC 停顿 | **永久例外**（已裁定 `N3+A1`） |
| 2 | Kill / OOM 内部传播机制 | `Consume`/`HandleSignal` 内 `panic(err)`，上层 `recover()` | 全链路 `Result<(), Error>` 返回（`N3 §3.4` `panic → Result`） | 外部返回与日志相同，不产生误导性的 `execute sql panic` / `write query result panic` 内部栈日志 | **永久例外**（已裁定 `N3`） |
| 3 | 控制面线程模型 | 所有 goroutine 共享 M:N 抢占式 Go scheduler | 数据面 `ExecPool`（连接与 SQL 零交接）+ 控制面 2 线程 `ControlRuntime`（含专用 `ctrl_kv`） | 无功能差异，高负载下控制面探测、lease 续约与 OOM 强杀更稳定 | **永久架构例外**（待 §6.2 决策点 1 确认） |
| 4 | `servermemorylimit` Top1 CAS 清理顺序 | `servermemorylimit.go:128-130` 先 `s.reset()`（置 `sessionTracker = nil`）再 `CompareAndSwap(nil, nil)` | 待 §6.2 决策点 3 裁定（建议保存原 `tracker` 再 CAS 清空，或照搬 Go 缺陷） | 避免刚杀掉的连接 Tracker 残留于 `MemUsageTop1Tracker` | **待裁定（上游缺陷）**（若修复需单开 `upstream-pitfall` issue + 登记 `rulings.toml`） |
| 5 | 跨节点 Global KILL | 节点注册 `/tidb/server_id`，支持跨节点 `ExecType_TypeKill` RPC | 本期 `server_id = 0`、`rpc.rs` `TypeKill` 未实现，仅支持本机节点连接 KILL | 跨节点发送 `KILL` 会产生 `"KILL remote connection failed: ..."` Warning | **临时追踪差异**（单开集群 Global KILL follow-up issue，关联 `D06.5` / `D03.1`） |
| 6 | 结果集写出阻塞窗口 | `writeChunks` 边计算边流式 write TCP | 当前 `tidb-server`（`D03.1` Part 1）先物化 `ResultPackets` 再由协议层写 socket | 在 `D03.1` 流式改造前，大结果集主要在 `rs.next` 计算阶段被取消；写满 TCP 缓冲区后的取消效果需待流式发送补全 | **临时追踪差异**（随 `D03.1` / `#464` 后续流式写出消解） |
| 7 | 磁盘溢写、`HashAgg` 新栈接入与 `MEMORY_USAGE_OPS_HISTORY` 视图 | 超限可落盘、`HashAgg` 走新栈并支持查询 `MEMORY_USAGE_OPS_HISTORY` | `D01.10` 落盘未实现（`blocked-by:D01.10`）；`PhysicalHashAgg` 待 `D10.1`；50 条环形缓冲内存结构就绪，虚拟表待 `D06.3`/`D03.2` 暴露 | 无落盘缓冲能力（超限直接按 `CANCEL`/`LOG` 动作）；系统表 SQL 视图暂未暴露 | **临时追踪差异**（随 `D01.10`、`D10.1`、`D06.3`、`D03.2` 消解） |
| 8 | `MEMORY_QUOTA` / `MAX_EXECUTION_TIME` Hints 与 `SLEEP(N)` | 支持 hints 覆盖会话配置，`SELECT SLEEP(N)` 被杀返回 `1` | Hint 解析阻塞于 `D02.2` / `D13.3`；内置 `SLEEP()` 尚在 PoC | 带 Hint 语句暂在 `fallback` 走 PoC（`strict` 报 `1235`） | **临时追踪差异**（随 `D02.2`、`D13.3` 及 `SLEEP` 移植消解） |
| 9 | 断连检测底层原语 | `net.Conn` `SetReadDeadline(30µs) + Peek(1)` | 分离 `ReadHalf` 通过 `BufReader::poll_fill_buf` 非阻塞探测（`BiLock` / TLS `Pending` 保守判存活） | 客户端无感知（同为 ~1s 节流触发 `1317`） | **内部实现差异**（`N3` 异步 I/O 映射，无外部追踪需求） |

---

## 6. 决策点（严格遵守 `CONSTITUTION.md` 第九条）

### 6.1 负责人已决（不得再问）

1. **`servermemorylimit` 度量指标**：使用 jemalloc `stats.resident`（`MemStats.sys`）判定实例内存上限，同时记录 `allocated`/`active` 用于归因（`N3+A1 §4.4` 裁定 4）。
2. **Go GC 专属变量与 `release()`**：`tidb_gogc_tuner_*`、`tidb_server_memory_limit_gc_trigger`、`tidb_enable_gc_aware_memory_track` 保持合法校验与状态读取但不产生 GC 动作；无 finalizer，内存随 `Drop` 释放且无 `runtime.GC()`（`N3+A1 §4.3` 裁定 3、`D05.1`/`N8 §3.4`）。
3. **取消与 OOM 错误传播机制**：严格采用 `panic → Result`（`N3 §3.4`、`N3+A1 §4.5`），不在 Rust 中使用 panic + `catch_unwind` 控制取消流。
4. **SQL 执行零交接与时间片让出**：连接任务即语句任务，全部在 `ExecPool` 运行并协作式 `CheckpointStride` 让出（`N3 §3.1` 第 2、5 条）。
5. **仅叶子算子调 `mem_tracker.checkpoint()`**（`A1 §3.3`）。
6. **日志字段契约**：`SqlKiller`（`"connection ID"`）、`expensive_query`、`memory exceeds quota` 与 Go v8.5.8 字节级对齐（`D00.3 §4.4`、§9.1）。

### 6.2 需要负责人决定（严格限宪章第九条 第 1 款，供直接点选）

#### 决策点 1：控制面运行时隔离（`#474 R-5`）对 `N3 §3.1` 第 2 条的修订确认（第九条 1(b) 基础设计修订）
- **背景**：`N3 §3.1` 第 2 条原文为“进程里只有一个 tokio runtime”。在 Tokio 协作式调度（尤其 `NoSteal` 或有限 worker 下），将集群元数据同步（`SchemaSync`/`PrivilegeSync`/`SysVarSync` 及其专属 `KvClient`）、`/status`+gRPC、OOM/超时看门狗与 `client-rust` `StoreLiveness` 留在 SQL 执行池上，会导致重 SQL 负载饿死 etcd DDL lease 续约、健康检查与强杀动作。
- **选项**：
  - **A（推荐）**：修订 `N3 §3.1` 第 2 条为“**数据面**保持单一 `ExecPool` 运行时（连接任务 = 语句任务，零跨运行时语句交接）；**控制面**、控制面轻量 `ctrl_kv` 与后台看门狗服务隔离至专用 2 线程 `ControlRuntime`（`tidb-rs-ctrl`）”，且不新增 Go TiDB 不存在的 SQL 系统变量。
  - **B**：严格维持单 runtime，控制面服务与 `ctrl_kv` 继续跑在 `ExecPool` 上。
- **维护者建议与理由**：选 **A**。既守住了零 SQL 语句交接（`N3` 核心性能与拓扑保证），又彻底消除重查询饿死集群控制面与看门狗的线上可用性风险。

#### 决策点 2：`max_execution_time` 触发机制对 `N3 §3.3` 第 5 条（`M-TICK`）的修订确认（第九条 1(b) 基础设计修订）
- **背景**：`N3 §3.3` 第 5 条（`specs/exceptions/N3-A1-concurrency-memory/spec.md:100`）原定将 Go `expensive_query` 每 100ms 扫描 `ShowProcessList` 改为每条语句挂一个独立 deadline 定时器（`M-TICK`）。但实际实现中，`ExpensiveQueryHandle` 为了 60s 慢查和 600s 慢事务本就必须每 100ms 扫描一次会话表；若 `max_execution_time` 再另起一套独立语句定时器，不仅会与 `expensive_query` 巡检产生双路径并发触发竞态（重复触发 `execution timeout, kill it` 与 `sm.kill`），而且在 `costTime` / 日志时机上易偏离 Go 实测契约。
- **选项**：
  - **A（推荐）**：修订 `N3 §3.3` 第 5 条（`M-TICK`），对 `max_execution_time` **按 L1 忠实采用 `ExpensiveQueryHandle` 100ms 统一巡检触发**（并保留 `check_max_execution_time_exceeded` 算子检查点），不再另设每语句独立超时定时器。
  - **B**：维持 `N3` 原方案为每语句设置独立 deadline 定时器，并在 `ExpensiveQueryHandle` 巡检中跳过 `max_execution_time` 检查（需重新设计避免 `"execution timeout, kill it"` 日志与 Go 产生差异）。
- **维护者建议与理由**：选 **A**。既然 `ExpensiveQueryHandle` 100ms 循环已固定存在，统一由它判定并执行 `"execution timeout, kill it"` + `sm.kill` 实现零额外定时器开销、零竞态且日志字段（`costTime`、`maxExecutionTime`、`processInfo`）与 Go 100% 等价。

#### 决策点 3：Go `servermemorylimit.go:128-130` 上游缺陷（`s.reset()` 后再 `CompareAndSwap`）的处理方式（第九条 1(a) & 第 3 款例外）
- **背景**：Go TiDB v8.5.8 `pkg/util/servermemorylimit/servermemorylimit.go:128-130` 在确认 Top1 消费者退出或达 60s 强杀后，于 `Succ:` 分支先调用 `s.reset()` 将 `s.sessionTracker` 置为 `nil`（第 97 行），下一行才调用 `memory.MemUsageTop1Tracker.CompareAndSwap(s.sessionTracker, nil)`（第 130 行），导致实际执行的是 `CompareAndSwap(nil, nil)` 而未能清除原本的 `Top1Tracker` 指针。由于修正该行为会改变实例级 OOM 强杀的后续资源状态，属于宪章第一条 2 与第九条 3 排除事项（“修正会改变...资源行为的...仍按第 1 款先问负责人”）。
- **选项**：
  - **A（推荐）**：按正确语义实现（在 `s.reset()` 前暂存 `prev = s.session_tracker`，复位后执行 `MemUsageTop1Tracker.compare_and_swap(prev, None)`），同时开具 `upstream-pitfall` issue 并在偏差台账 `docs/testing/rulings.toml` 登记。
  - **B**：严格照搬 Go v8.5.8 缺陷（`s.reset()` 后传 `None` 做无效 CAS，同样开具 `upstream-pitfall` issue）。
- **维护者建议与理由**：选 **A**。修复 2 行局部指针顺序即可防止已死亡会话持续占据 `MemUsageTop1Tracker`（尤其当其它会话消耗未达覆盖阈值时阻塞下一轮 OOM 救援），风险可控且收益明确。

### 6.3 维护者已定（宪章第九条 第 2 款，不单独询问）

1. **Trait 反转与分层归属**：`ProcessInfo` 与 `SessionManager` 放在 `tidb-sessionctx`（rank 7）；`mem_exceed_log_hook()` 放在 `tidb-executor::glue::SessionContext`（rank 14）反转注入；`ExpensiveQueryHandle` 与 `ServerMemoryLimitHandle` 放在 `tidb-session`（rank 15）；`ServerSessionManager` 与 `ConnIdAllocator` 放在 `tidb-server`（rank 16，新增 `tidb-runtime` rank 1 与 `tidb-sessionctx` rank 7 依赖）；`tidb-mysql-protocol` `[legacy]` 向下依赖 `tidb-server`。理由：严格满足 `layers.toml` `dep.rank < self.rank` 且不引入 `legacy` 反向依赖。
2. **本期阶段边界：单机本机 KILL 先行，集群 `TypeKill` 单开 issue 追踪（第九条 1(a) 暂不支持例外 & 第 2 款）**：`#542` 本期实现本机节点 KILL 与与 Go `globalconn` 兼容的偶数连接 ID 分配（消除奇数 `1` 被误判 `1105` 的冲突），对跨节点 `server_id != self.server_id()` 返回 `"KILL remote connection failed: ..."` Warning；`/tidb/server_id` 租约注册与 `rpc.rs` `TypeKill` 单开 follow-up issue 在后续阶段补齐。
3. **混合路由过渡期 PoC 路径处理（第九条 2 款临时边界处理）**：`KILL` / `max_execution_time` / TCP 断连在连接与 `SqlKiller` 层统一覆盖新旧双栈（透传 `killer`/`cancel` 至 `poc-executor`，并修复 `self.router.take()` 状态丢失）；细粒度 `tidb_mem_quota_query`（`8175`/`LOG`）与 `MemUsageTop1Tracker`（`8176`）仅对新执行栈生效，不为即将淘汰的 `[legacy] poc-executor` 补写 `TrackedAlloc`。
4. **`CancellationToken` 自实现于 `tidb-runtime`**：基于 `AtomicBool` + `tokio::sync::watch` 实现约 40 行安全组件，不引入 `tokio-util` 新外部依赖。
5. **`ExecCtx::checkpoint()` 与叶子内存检查分离**：`ExecCtx::check()` 仅检查取消与 `SqlKiller`（不调用 `stmt.mem_tracker.checkpoint()`），算子叶子继续各自调用 `self.mem_tracker.checkpoint()?`，维护 `A1 §3.3` 单次增长触发动作链不变量。
6. **消除全工作区 `checkpoint(&())`、升级 `ProjectionExec` 与全路径解绑**：Part 1 收敛新栈及 `tidb-store` 中 13 处 `checkpoint(&())` 与 2 处局部 `KillerHook`、迁 `ProjectionExec` 至 `TrackedAlloc` 并为 `SortExec`/`ProjectionExec` 补 `Drop` 内存归零；Part 2 对 `CloseRecordSet`、早退错误与 `StmtCheckpoint::restore()` 全面接入 `detach_mem_disk_tracker()`（并更正 `adapter.rs:682, 916` 注释）。
7. **`HashAggExec.session_mem_tracker` 保持暂不挂载（直到 `D01.10` 落地）** 与 **`SysVarSync::rebuild` 只读白名单同步**：保证当前 `tidb_mem_quota_query` 稳定落入 `SortExec`/`ProjectionExec` 的硬限路径（`8175`/`LOG`）而不误触 `blocked-by:D01.10`（`1235`），同时确保 `ServerMemoryLimit`（默认 `"80%"`）在启动及集群变更时正确生效。
8. **`PacketIo` 读写拆分与非阻塞 `poll_fill_buf` 探活**：共享 `seq: Arc<AtomicU8>` 与 `IoCounts`；通过 `BufReader::poll_fill_buf` 检查 `EOF`/`RST` 并保留提前到达的管道字节在 `BufReader` 内部缓冲；写半部 `select!` 监听 `conn_shutdown` 与 `finish_cancel`，半截包中断一律强制断开连接。
9. **`client-rust` 最小侵入注入与四阶段门禁**：`StoreLiveness` 通过可选 `Option<Handle>` 接入控制面（`batch.rs` 数据面不动），按 Part 1 → Part 2 → Part 3 → Part 4 拆分实施，每步均以 `scripts/check_layers.py`、单元测试与 Go v8.5.8 行为对照测试作为合并门槛。

---

## 7. 本文所用 ID

- `#464`：Expensive Query 与 OOM 日志黄金合约对齐（`D00.3 §4.4`）
- `#474 R-5`：`N3` 架构收敛项——控制面运行时隔离
- `#542`：本规格对应 Issue——新执行栈运行时控制（取消与 `KILL`、内存配额与看门狗、控制面隔离）
- `#70776`：上游修复 `pingcap/tidb#70776`（commit `05c7dd7b96`，修复 `SqlKiller::Reset` swap 丢失信号问题与日志 reason 竞态）
- `A1`：统一内存记账模型（`TrackedAlloc` + `Tracker::checkpoint` + jemalloc `resident`）
- `D00.3`：统一日志规范与黄金日志样本
- `D01.10`：磁盘溢写行容器（`RowContainer` / `DataInDiskByChunks` / `SpillDiskAction`）
- `D02.2`：Parser 语法扩展（含 optimizer hints 语法等）
- `D03.1` / `D03.2`：MySQL 协议服务端、流式写回与状态/信息系统表
- `D05.1` / `D05.2` / `D05.3`：系统变量、`SessionVars` / `StatementContext`、会话管理
- `D06.3` / `D06.5`：`InfoSchema` / `Domain` 与元数据同步
- `D07.1` / `D07.3`：TiKV store 驱动与 DistSQL 客户端（`SelectResult`）
- `D10.1`：物理聚合计划（`PhysicalHashAgg` 等）
- `D12.0` / `D12.2` / `D12.3` / `D12.5`：执行器框架、Projection、HashAgg、Sort
- `D13.3`：Hint 解析与下推处理
- `D15.1` / `D15.3`：内存追踪与 cgroup 内存感知
- `N0`：Crate 分层规范（`layers.toml` 与 `scripts/check_layers.py`）
- `N2`：会话与语句上下文隔离规范（`SessionShared` / `Arc<StatementContext>`）
- `N3`：运行时并发、取消、`ExecPool`（`Steal`/`NoSteal`）、时间片 `checkpoint` 与 `panic → Result` 规范
- `N8`：运行时专属配置/变量处理规范（Go GC 等不产生实际运行行为）
- `N10`：资源管控与 Runaway Query 规范（`8253`）
- `VS1`：第一条纵向切片及 PoC / 新执行栈并行路由机制
