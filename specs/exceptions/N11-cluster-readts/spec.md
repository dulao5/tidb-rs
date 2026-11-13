# N11：集群成员身份、schema 同步与 MDL、读时间戳、显式只读事务

状态：**调研草案**（2026-11-13）。类型：实现例外（只读节点参与集群协议的方式）+ 待决定的特性问题。来源：#128 第 1、6 项（外部评审），#136（`kill_tikv` 场景下整个节点停顿 7 s 的根因）。流量接入的四种方式见 [ADR 0009](../../../docs/adr/0009-traffic-entry.md)，本 spec 是它的前提。

本文只做调研和设计，**不写生产代码**。凡是没有 Go 行号支撑的结论都标"推断"。

引用记法：

| 记法 | 对象 |
|---|---|
| `pkg/...` | TiDB `v8.5.8`（`8b857efa2`） |
| `parser/...` | TiDB parser 模块 `v0.0.0-20260827064614-8b857efa2036`（v8.5.8 实际使用） |
| CG | client-go `c3b5079` |
| PD | PD client `v0.0.0-20260804033407-85a975a5ca78` |
| CR | client-rust fork `dulao5/client-rust` @ `36068d0`（tidb-rs `Cargo.toml:142` 所钉） |
| tidb-rs 路径 | `main` @ `7be21ec` |

## 0. 总结

1. **集群成员**（§2.1）：Go 在 etcd 里写三类东西：
   - `/tidb/server/info/<ddl_id>`（DDL 等待集合、`cluster_info`、cluster 表扇出、global KILL 路由都靠它）；
   - `/topology/tidb/<ip:port>`（Dashboard、TiProxy 的发现入口）；
   - `/tidb/server_id`（global connection ID 的高位）。

   tidb-rs 现在只写第一类（ADR 0005）。建议保持不进 `/topology/tidb`：TiProxy 只按连接路由，进去就会收到写流量。global KILL 和 server ID 见 D2。
2. **schema 同步**（§2.2–§2.3）：
   - Go 的 owner 对已注册节点**无限期等待**（MDL 开启时没有超时，`pkg/ddl/schemaver/syncer.go:389-406`）。
   - SchemaValidator 的 lease 按 **TSO** 计（`pkg/domain/schema_validator.go:134-148`、`:274-278`），但**只在写事务提交时检查**（`pkg/sessiontxn/isolation/base.go:560-583`），只读语句和只读事务从不检查（`pkg/session/session.go:510-513`）。

   tidb-rs 是只读节点，照搬 Go 等于"永远不检查"。本 spec 按 #136 的修复规则，把 lease 检查移到**语句准入**，并规定准入窗口 ≤ 注册窗口（§4.2）。
3. **升级安全**（§2.4）：Go 遇到未知的 `TableInfo.Version`、DDL action、更高的 bootstrap 版本时**都不拒绝**（`pkg/infoschema/builder.go:65-110`、`pkg/session/bootstrap.go:1611-1616`）。原因是 Go 的滚动升级会替换所有节点，而 tidb-rs 在升级后的集群里**始终是旧节点**。所以 tidb-rs 加一组"不认识就拒绝服务并报警"的规则（§4.3，D7）。
4. **读时间戳**（§2.5）：
   - Go 的 TSO 是异步预取（`AdviseWarmup`，`pkg/planner/optimize.go:256`），由 PD client 批量发送。
   - 低精度 TSO 由 client-go 后台每 2 s 刷新（CG `oracle/oracles/pd.go:458-512`）。
   - stale read、`tidb_snapshot`、`tidb_external_ts` 各有一套 ts 来源和校验。
   - replica read 只对只读语句生效（`pkg/sessionctx/variable/session.go:2469-2503`）。

   tidb-rs 设计一个统一的 `ReadTs` 来源（§4.4），并把 CR 里已有的 `LowResolutionTs` 扩展为后台刷新、可调周期、支持 stale ts 计算和 ts 校验。
5. **显式只读事务**（§2.6）：
   - Go v8.5.8 里 `START TRANSACTION READ ONLY`（不带 AS OF）是 noop 函数：`tidb_enable_noop_functions=OFF`（默认）时报 **1235**，打开后接受但**不强制只读**（`pkg/executor/simple.go:618-644`）。错误码 1792 定义了但从未使用（`pkg/errno/errcode.go:758`）。
   - **`BEGIN READ ONLY` 和 `BEGIN /*T! READ ONLY */` 在 Go 里是语法错误**（实测 parser v8.5.8，§2.6）。
   - tidb-rs 的显式事务语义见 §4.5，用户可见的选择见 D8。
6. **需要负责人决定**的 10 点见 §6。最重要的是 D3/D4（lease 移到准入、过期后怎么办）和 D8（显式只读事务的语法与错误码）。

## 1. 范围

| 在范围内 | 不在范围内 |
|---|---|
| etcd 注册（serverinfo、topology、server_id、minstartts）、owner 不参选；schema 版本上报、MDL、schema lease；升级时的元数据兼容检查；读时间戳的来源（TSO、MaxTS、低精度、stale、snapshot、external）、replica read 的选路参数；显式只读事务、autocommit=0；与 TiProxy 会话迁移相关的 `SHOW/SET SESSION_STATES`（只列需求，接入方式见 ADR 0009） | 写路径、DDL 执行、owner 职责；TiFlash；keyspace / next-gen（#128 第 6 项，负责人待定）；client-rust 内部的 replica selector 实现（[client-go 对等对照表](../../D07.1-store/client-go-parity.md) §3） |

替换的 Go 范围（tracker 标"例外"，不计为未移植）：

- `pkg/domain/schema_validator.go`：lease 判定的**位置**改变，公式不变；
- `pkg/domain/infosync/info.go`：只做 serverinfo 部分，topology 由 D1 决定；
- `pkg/ddl/schemaver/syncer.go`：节点侧，owner 侧不移植。

其余（`pkg/sessiontxn/**`、`staleread`、`pkg/session/txn.go` 的只读部分）按 L1 移植，属于 D05.4 / D06.5。

所在阶段：
- §4.2 的 lease 修复（#136）立即做，在 PoC 的 `crates/poc/domain` 上；
- 其余随 D06.5（domain）、D05.4（sessiontxn）、D03.1（协议层）移植，定稿在 D05.4 开工前。

## 2. 调研：Go v8.5.8

### 2.1 集群成员

**serverinfo**（DDL 等待集合的来源）
- 键：`/tidb/server/info/<ddl_id>`（`pkg/domain/infosync/info.go:68`、`:265`），绑定 TTL 90 s 的 session lease（`info.go:710-722`、`:927-938`，`pkg/ddl/util/util.go:66-67`）。
- 内容：`StaticServerInfo`（`info.go:174-191`）：`ddl_id`、`ip`、`listening_port`、`status_port`、`lease`、`start_timestamp`、`server_id`，以及 `labels`（`:198`）。
- session 丢失时，`infoSyncerKeeper` 重建 session 并重写（`pkg/domain/domain.go:919-944`）。
- 读者：
  - `GetAllServerInfo`（`info.go:452-462`、`:695-707`）；
  - `TIDB_SERVERS_INFO`（`pkg/executor/infoschema_reader.go:2715-2716`）；
  - `CLUSTER_INFO` 等通过 `GetClusterServerInfo`（`pkg/infoschema/tables.go:1840-1906`）；
  - DDL 的版本等待（§2.2）、job 版本探测（§2.4）、global KILL 路由（下文）。
- **cluster 内存表扇出**：
  - `CLUSTER_PROCESSLIST`、`CLUSTER_SLOW_QUERY`、`CLUSTER_STATEMENTS_SUMMARY*`、`CLUSTER_TIDB_TRX` 等（`pkg/infoschema/cluster.go:60-72`）由 `buildTiDBMemCopTasks` 对 `GetAllServerInfo` 中每个节点的 `IP:StatusPort` 发一个 `CmdCop` 请求（`pkg/store/copr/coprocessor.go:597-619`）。
  - 接收端是 status 端口上的 gRPC `Coprocessor`（`pkg/server/rpc_server.go:96`、`:196`；注册于 `pkg/server/http_status.go:496`）。

**topology**（Dashboard、TiProxy 的入口）
- 键：`/topology/tidb/<ip:port>/info`（**无 lease**），以及 `/topology/tidb/<ip:port>/ttl`（绑 45 s 的 lease，每 30 s 刷新）（`info.go:80-84`、`:776-782`、`:941-966`；`domain.go:966-990`）。
- 开关：配置 `skip-register-to-dashboard`（`pkg/config/config.go:241-242`，`domain.go:1555-1558`）。
- TiDB 内部只有 plan replayer 读它（`info.go:906-924`，`pkg/server/handler/optimizor/plan_replayer.go:142`）。
- TiProxy 只从这里发现后端，并且没有 `ttl` 键的后端会被丢弃（TiProxy `pkg/manager/infosync/info.go:33-37`、`:306-311`，见 ADR 0009 §2）。

**server ID 与 global connection ID**
- server ID 存在 `/tidb/server_id/<id>`：
  - 用 etcd 事务 `CreateRevision==0` 抢占，lease 12 h（`domain.go:3125-3143`、`:3215-3260`）；
  - 32 位 ID 优先在 `[1, 2047]` 中随机选（`:3279-3318`）。
  - 连续 6 h 续不上 lease 时 `KillAllConnections()`，防止 ID 被别人拿走（`:3376-3397`）。
- 连接 ID 布局（`pkg/util/globalconn/globalconn.go:30-73`）：
  - 32 位：`serverID(11) | localConnID(20) | 0`；
  - 64 位：`0 | serverID(22) | localConnID(40) | 1`。
- `KILL`（`pkg/executor/simple.go:2665-2774`）：
  - 解析出 serverID，不是本机的就构造 `ExecType_TypeKill` 的 DAG，经 `buildTiDBMemCopTasks` 发到该节点 status 端口的 gRPC；
  - 32 位 ID 被截断时只给 warning。
- 配置：`enable-global-kill` 默认 true（`config.go:1075`）。

**owner 选举**：节点默认参选的 owner 和关闭方式（`pkg/ddl/ddl.go:72`、`:865`、`:911-915`、`:1072-1095`；`domain.go:2175-2185`、`:2540-2547`；`pkg/autoid_service/autoid.go:330`）：

| owner | 默认 | 关闭方式 |
|---|---|---|
| DDL | 参选 | `tidb_enable_ddl` / `run-ddl` |
| stats | 参选 | `tidb_enable_stats_owner` |
| bindinfo | 参选 | `bind-info-lease=0` |
| autoid 服务 | 参选（status server 上） | 没有开关 |
| dist task、TTL | 跟随 DDL owner | 同 DDL |

只读节点一个都不应参选。tidb-rs 现在不参选任何 owner（ADR 0005）。

**minstartts**
- 每 30 s，把本节点最老的活跃事务 start_ts 写到 `/tidb/server/minstartts/<id>`，绑 session lease（`info.go:814-878`；`domain.go:926-931`）。
- GC 把 safepoint 压到它之下（`pkg/store/gcworker/gc_worker.go:515-560`）。
- 只统计 `now - tidb_gc_max_wait_time` 之后的事务（`info.go:826-829`）。

### 2.2 schema 版本同步与 MDL

**上报与等待**
- 节点：
  - MDL 开时，写 `/tidb/ddl/all_schema_by_job_versions/<job>/<ddl_id>`（无 lease，`PutKVToEtcdMono`）；
  - MDL 关时，写 `/tidb/ddl/all_schema_versions/<ddl_id>`（绑 lease）（`pkg/ddl/schemaver/syncer.go:277-293`）。
- owner（`WaitVersionSynced`，`syncer.go:319-435`）：
  - MDL 开时**每轮重读 `GetAllServerInfo`** 作为等待集合（`:344-367`）。集合中每个节点都上报了 ≥ 目标版本才返回。
  - 每 1 s 只打一条 "someone is not synced" 日志，然后继续等（`:389-406`）。
  - **没有 `2*lease` 上限**，只在 ctx 取消时退出（`:334-338`）。
  - MDL 关时，`DeadlineExceeded` 被当成成功（`pkg/ddl/job_worker.go:1139-1142`）。
- 死节点退出等待集合的唯一途径是它的 serverinfo lease 过期（90 s）（推断，依据上面两处 lease 绑定）。

**MDL**
- `tidb_enable_metadata_lock`：GLOBAL，默认 ON（`pkg/sessionctx/variable/tidb_vars.go:791`、`:1576`；`sysvar.go:1512-1522`）。
- 节点：
  - 每次 reload 后读 `mysql.tidb_mdl_info`（`domain.go:1019-1054`）。
  - `mdlCheckLoop` 每 50 ms 检查一次（`domain.go:1056-1136`）。
  - 本节点没有事务持有该 job 所涉表的旧版本时，才上报（`pkg/session/session.go:4936-4961`）。
- **谁持有 MDL**（`tryLockMDLAndUpdateSchemaIfNecessary`，`pkg/planner/core/preprocess.go:2057-2175`）：
  - **autocommit 的只读语句不持有**（`:2101`）；
  - `tidb_snapshot`、stale read 不持有（`:2089-2094`）；
  - **显式事务里的只读语句持有**，直到事务结束（`TxnCtx` 清理，`pkg/sessionctx/variable/session.go:415`）。
- 显式事务里第一次访问某表时，取 **domain 当前最新**的表定义，而不是事务开始时的（`preprocess.go:2105-2131`）：
  - 新的 public 索引如果不在旧定义里，标成 `WriteReorganization`，不用于读（`:2132-2158`）；
  - public 列的 ID 变了，报 `ErrInfoSchemaChanged`（8028）（`:2159-2175`）。

### 2.3 SchemaValidator 与 schema lease

- **lease**：配置 `lease`，默认 45 s（`pkg/config/config.go:109`、`:912`）。`loadSchemaInLoop` 每 lease/2 = 22.5 s，以及每次 `global_schema_version` 变化时 reload（`domain.go:1138-1163`）。
- **按 TSO 计**：
  - `Reload` 先取一个 TSO `ver`（`domain.go:810`），加载成功后 `SchemaValidator.Update(ver, ...)`（`:851`）。
  - `Update` 把过期时间设为 `GetTimeFromTS(ver) + lease - 1ms`（`schema_validator.go:134-148`）。
  - `Check(txnTS, ...)` 在 schema 没有变化时，如果 `GetTimeFromTS(txnTS)` 晚于过期时间，返回 `ResultUnknown`（`:274-278`）。
  - 比较双方都是 TSO 的物理部分，与本地时钟和 reload 循环的快慢无关。
- **reload 失败**只记日志（`domain.go:1153-1163`），已加载的 schema 继续使用。
- **etcd session 丢失**时，`SchemaValidator.Stop()` → 重建 syncer → `mustReload`（每 200 ms 重试直到成功）→ `Restart()`（`domain.go:1169-1193`、`:1226-1241`）。注释（`:1173-1178`）说明这是为了"禁止 DML"：节点从等待集合中消失后，owner 可以连推两个版本。
- **检查发生在哪里**：
  - 只在提交时检查，通过 `SchemaChecker`（`pkg/sessiontxn/isolation/base.go:560-583`；CG `txnkv/transaction/2pc.go:1933-1938`、`:2097-2119`）。
  - `ResultUnknown` 重试 10 次 × 500 ms 后报 **8027 `ErrInfoSchemaExpired`**；`ResultFail` 报 **8028 `ErrInfoSchemaChanged`**（`pkg/domain/schema_checker.go:41-45`、`:63-80`；`pkg/errno/errcode.go:975-976`）。
  - **只读事务提交直接返回**，不检查（`pkg/session/session.go:510-513`）。`Restart` 的注释也承认："the problem still exists for read-only txns"（`schema_validator.go:117-119`）。
- **推论**：Go 节点失去 etcd session、或 reload 一直失败时，读语句继续用旧 schema 执行，没有任何截止时间。对 Go 来说这是已知的缺口，影响面限于读结果；但对 tidb-rs 这个**只做读**的节点来说，这就是全部。

### 2.4 升级与不认识的元数据

| 项 | Go 的行为 | 位置 |
|---|---|---|
| `TableInfo.Version` 高于 `CurrLatestTableInfoVersion`（5） | 不拒绝；读者只做 `>=` 比较 | `pkg/meta/model/table.go:59-85`；`pkg/infoschema/builder.go:880`、`:893` |
| 不认识的 `SchemaDiff` action | `default` 分支按"更新一张表"处理，不报错 | `pkg/infoschema/builder.go:65-110`、`:459-466`、`:509-511` |
| DDL job 版本 | 所有 serverinfo 的版本都 ≥ 8.4.0 才用 v2，否则 v1 | `pkg/ddl/ddl.go:93`、`:957-1046`；`pkg/meta/model/job.go:301-334` |
| bootstrap 版本高于本二进制（`currentBootstrapVersion = version232`） | 按 Normal 模式启动，`upgrade()` 直接返回，不拒绝 | `pkg/session/bootstrap.go:1323`、`:1611-1616`；`session.go:3883-3890` |
| JSON 中不认识的字段 | 忽略（Go `encoding/json` 的默认行为） | 推断 |

### 2.5 读时间戳

**TSO**
- PD client 把并发请求合批：
  - 批上限 10000（PD `clients/tso/client.go:44-45`）；
  - 等待窗口由 `tidb_tso_client_batch_max_wait_time` 设，默认 0，最大 10 ms（`pkg/sessionctx/variable/sysvar.go:740-746`；PD `pkg/batch/batch_controller.go:68-183`）。
  - 另有 `tidb_enable_tso_follower_proxy`、`tidb_tso_client_rpc_mode`（DEFAULT / PARALLEL / PARALLEL-FAST，`sysvar.go:747-751`、`:3614-3618`；`pkg/domain/domain_sysvars.go:65-105`）。
- 语句的 TSO 是 future：
  - 规划开始时发出（`AdviseWarmup`，`pkg/planner/optimize.go:256`；`base.go:321-332`、`:386-392`）；
  - 首次需要读 ts 时等待（`ActivateTxn`，`base.go:272-315`）。

**provider**（`pkg/session/txnmanager.go:330-376`）
- 选择：stale → `StalenessTxnContextProvider`；optimistic → 一个 start_ts；pessimistic 下按隔离级别选 RC / RR / Serializable。
- autocommit 语句总是 optimistic（`session.go:4196-4202`），所以 RC 只在 `autocommit=0` 或显式事务、且 `tidb_txn_mode=pessimistic`（默认，`tidb_vars.go:1432`）时生效。

**RC**
- 每条语句一个新 TSO；事务第一条语句用 start_ts（`pkg/sessiontxn/isolation/readcommitted.go:142-176`）。
- `tidb_rc_read_check_ts`（INSTANCE，默认 OFF）复用上一个 ts，遇到写冲突再取新的（`sysvar.go:593-598`；`readcommitted.go:107-115`、`:202-213`）。
- RR / optimistic 的读都用事务 start_ts（`repeatable_read.go:66-67`；`optimistic.go:46-47`）。

**MaxTS 点查**
- 条件：`IsPointGetWithPKOrUniqueKeyByAutoCommit`，要求 autocommit 且不在事务中（`pkg/planner/core/common_plans.go:1450-1502`）。
- 满足时，optimistic provider 用 `MaxUint64`，并提为高优先级（`optimistic.go:128-172`）。
- 没有开关。

**低精度 TSO**
- client-go 维护 `lastTSMap`：
  - 每次取 TSO 时更新（只增），后台每 `defaultOracleUpdateInterval` = 2 s 刷新一次（CG `tikv/kv.go:107`；`oracle/oracles/pd.go:120-130`、`:176-200`、`:281-309`、`:458-512`）。
  - 有 stale read 需要更新的 ts 时，刷新间隔自适应缩短，下限 500 ms（`pd.go:99-113`、`:335-456`）。
- `tidb_low_resolution_tso`：GLOBAL|SESSION，默认 OFF（`sysvar.go:330-333`）。
  - 打开后，语句 ts 取自缓存（`base.go:643-660`）；内部 SQL 不用（`pkg/sessionctx/variable/session.go:3748-3750`）。
  - 写语句报错 "can not execute write statement when 'tidb_low_resolution_tso' is set"，`SELECT FOR UPDATE` 也报错（`pkg/executor/adapter.go:643-646`、`:1047-1055`）。
- `tidb_low_resolution_tso_update_interval`：GLOBAL，默认 2000 ms，范围 10–60000（`sysvar.go:1603-1610`；`domain_sysvars.go:128-130`）。设置后最多再过一个旧周期生效（CG `pd.go:541-575`）。

**stale read**
- `AS OF TIMESTAMP expr`：
  - 毫秒精度（`pkg/sessiontxn/staleread/util.go:36-67`）。
  - 优先级：语句 AS OF → `tx_read_ts` → `tidb_read_staleness` → external ts（`staleread/processor.go:233-277`）。
  - 事务内写 AS OF 报 8135 `ErrAsOf`（`processor.go:159-162`）。
- `START TRANSACTION READ ONLY AS OF`：不取 TSO；加载该 ts 的历史 schema；事务标为 staleness（`staleread/provider.go:91-138`；`planbuilder.go:3962-3987`）。
- `SET TRANSACTION READ ONLY AS OF`：设隐藏变量 `tx_read_ts`，只作用于下一个事务（`parser/parser.y:10902-10910`；`varsutil.go:467-486`）。
- `tidb_read_staleness`：SESSION，≤ 0 秒（`sysvar.go:155-157`）。取 `now + staleness`，再按 store 的最小 safe ts 收紧（`staleread/util.go:70-90`）。`tidb_bounded_staleness()` 同理（`pkg/expression/builtin_time.go:6793-6852`）。min safe ts 来自 client-go 每 2 s 的 `safeTSUpdater`（CG `tikv/kv.go:84`、`:622`、`:708-728`）。
- 校验（`ValidateReadTS`，CG `pd.go:667-713`）：
  - ts 晚于缓存时，取一个新 TSO 比较，晚于它报 `ErrFutureTSRead`；
  - stale read 用 MaxUint64 报 `ErrLatestStaleRead`。
  - 两者 TiDB 都没有映射，用户看到 1105（推断）。
- 选路：stale 请求带 `StaleRead=true`、`ReplicaReadType=Mixed`（CG `tikvrpc/tikvrpc.go:335-339`）。第二次尝试退回 leader、普通读（CG `internal/locate/replica_selector.go:170-203`）。
- 写入：staleness 事务里 DML 报 "only support read-only statement during read-only staleness transactions"，加锁读报 "select lock hasn't been supported in stale read yet"（`session.go:2240-2272`）。
- schema：用 `GetSnapshotInfoSchema(ts)`（`pkg/domain/domain.go:710-718`），不持有 MDL。

**`tidb_snapshot`**
- SESSION 变量，接受**精确的 TSO 数字**或时间（`sysvar.go:209-215`；`varsutil.go:436-465`）。
- 设置时检查 GC safepoint，比 safepoint 旧报 8055（`pkg/executor/set.go:226-239`；`pkg/util/gcutil/gcutil.go:53-76`）。
- 读和 start_ts 都用它（`base.go:183-204`、`:326-328`），不设 InTxn（`:303-305`），用历史 schema。
- 写语句报错（`adapter.go:1047-1051`）。

**`tidb_external_ts`**
- 由 GLOBAL 变量写进 PD（`sysvar.go:2940-2952`；PD `client.go:1530-1570`）。
- `tidb_enable_external_ts_read`（GLOBAL|SESSION）打开后，事务外的语句读在这个 ts 上，会话变为只读（`sysvar.go:2953-2956`；`processor.go:307-317`；`session.go:2242`）。

**GC**
- 读时，client-go 检查 `startTS >= safePoint`，否则报 9006 `ErrGCTooEarly`；safepoint 缓存超过约 100 s 未刷新则报 9001（CG `tikv/kv.go:164-185`；`pkg/store/driver/error/error.go:117-119`、`:153-155`）。
- 长事务靠 minstartts（§2.1）阻止 GC 越过它。

**replica read**
- `tidb_replica_read`：GLOBAL|SESSION，取值 leader / prefer-leader / follower / leader-and-follower / closest-replicas / closest-adaptive / learner，默认 leader（`sysvar.go:2479-2496`；`pkg/kv/option.go:133-156`）。
- 只对**只读语句**生效：非只读语句退回 leader，带 hint 时给 warning（`pkg/sessionctx/variable/session.go:2469-2503`）。
- closest 读：用本节点的 `zone` 标签匹配 store（`pkg/config/config.go:353`；`base.go:167-180`；`pkg/distsql/request_builder.go:69-76`）。
- closest-adaptive：按 cop 任务预估字节数，阈值 `tidb_adaptive_closest_read_threshold`（默认 4096）（`pkg/executor/builder.go:4892-4909`）；本 zone 没有 TiKV 时关闭（`domain.go` 约 `:1655-1745`）。

### 2.6 显式事务与只读

**语法**（`parser/parser.y:3436-3484`）

| 写法 | 解析结果 |
|---|---|
| `BEGIN` / `START TRANSACTION` | 空的 `BeginStmt` |
| `BEGIN PESSIMISTIC` / `BEGIN OPTIMISTIC` | `Mode` |
| `START TRANSACTION READ WRITE` / `WITH CONSISTENT SNAPSHOT` | 与 `BEGIN` 相同，选项被丢弃 |
| `START TRANSACTION READ ONLY [AS OF ...]` | `ReadOnly`（加 `AsOf`） |
| **`BEGIN READ ONLY`** | **不存在** |

**实测**（parser v8.5.8，脚本见 PR 描述）：

| SQL | 结果 |
|---|---|
| `BEGIN /*T! READ ONLY */` | `syntax error ... near "READ ONLY */"`（1064） |
| `START TRANSACTION /*T! READ ONLY */` | 解析为 `ReadOnly=true` |
| `BEGIN /*T![tidb_rs_read_only] READ ONLY */` | 解析为普通 `BEGIN`：feature ID 不认识时，整段当普通注释（`parser/lexer.go:560-571`；`parser/tidb/features.go:54-61`） |

**执行**（`executeBegin`，`pkg/executor/simple.go:618-644`）
- `READ ONLY` 不带 AS OF 时，按 `tidb_enable_noop_functions` 处理：
  - OFF（默认，`tidb_vars.go:1466`）：报 **1235**（`ErrFunctionsNoopImpl`，"function READ ONLY has only noop implementation in tidb now..."，`pkg/expression/errors.go:33`）；
  - WARN：给 warning；
  - ON：接受。
- 无论哪种，`ReadOnly` 都**不传给事务**（`:638-643`）。
- `tx_read_only` / `transaction_read_only` 同样是 noop 变量（`pkg/sessionctx/variable/noop.go:29-37`；`varsutil.go:150-175`）。
- **1792 `ErrCantExecuteInReadOnlyTransaction` 在整个代码树里没有被使用**（只在 `pkg/errno/errcode.go:758`、`errname.go:764` 出现）。

**真正拒绝写的地方**

| 场景 | 错误 | 位置 |
|---|---|---|
| `tidb_restricted_read_only` / `tidb_super_read_only` | **1836** "Running in read-only mode" | `pkg/planner/optimize.go:152-160`、`:433-471`；提交时再查一次 `session.go:514-523` |
| staleness、external ts | 无错误码的文本错误 | `session.go:2240-2272` |
| `tidb_snapshot` | 无错误码的文本错误 | `adapter.go:1047-1051` |
| 低精度 TSO | 无错误码的文本错误 | `adapter.go:1047-1055` |

**start_ts 何时取**
- 显式 `BEGIN` 在 BEGIN 时就取并激活（`base.go:96-108`、`:140-141`）。
- `autocommit=0` 的隐式事务在第一条语句激活，并设 InTxn（`session.go:4190-4212`；`base.go:110-111`、约 `:302-304`）。
- 只读事务的 COMMIT 是 no-op（`session.go:510-513`）。

**锁子句**
- `FOR UPDATE` 只在 `PessimisticLockEligible`（非 autocommit 或在事务中）时加锁，否则忽略（`pkg/executor/builder.go:775-794`；`pkg/sessionctx/variable/session.go:3755-3764`）。
- `LOCK IN SHARE MODE` / `FOR SHARE` 默认报 1235（noop），除非打开 noop 或 `tidb_enable_shared_lock_promotion`（`preprocess.go:1195-1230`）。

**同一快照内的锁处理集合**
- client-go 的 `resolvedLocks` / `committedLocks` 挂在 `KVSnapshot` 上（CG `txnkv/txnsnapshot/snapshot.go:124-125`、`:591`、`:786`），只对该快照的 start_ts 有效。
- RR 事务的各语句共用一个快照（推断，依据 `base.go:439` 由事务构造快照）。

**会话迁移**
- `SHOW SESSION_STATES`：返回状态 JSON 和签名 token（`pkg/executor/show.go:2383-2417`）。
- `SET SESSION_STATES`：恢复，包括 prepared statement 的**原 ID**（`pkg/server/driver_tidb.go:494-496`；`pkg/sessionctx/variable/session.go:3051`）。
- 在事务中、有临时表、advisory lock、`LOCK TABLES`、未取完的游标等情况下，报 **8146**（`session.go:4665-4692`；`driver_tidb.go:466-473`）。
- 细节见 ADR 0009。

## 3. tidb-rs 现状与缺口

| 方面 | 现状（main） | 缺口 |
|---|---|---|
| serverinfo 注册 | 写 `/tidb/server/info/<ddl_id>` 和 `all_schema_versions`，lease 90 s；`server_id=0`；不写 topology、server_id、minstartts；不参选任何 owner（ADR 0005，`crates/poc/domain/src/sync.rs`） | 与 Go 一致的部分保留；server_id / global KILL、minstartts 缺（D2、§4.1） |
| 健康判定 | **墙钟**：最近一轮完整循环在 `max_staleness` = 5 s 内，且 lease 至少还剩 3 s（`crates/poc/domain/src/registry.rs:19-37`、`:79-108`） | #136：5 s ≪ 45 s，meta region 选举（约 12 s）就足以让节点注销 |
| 注销后 | watchdog 每 100 ms 检查，满足条件就先换 epoch、再撤 lease（`sync.rs:607-645`），之后语句走 ADR 0003 回退路径：每条语句在 TiKV 上读 schema 版本键（`crates/meta/src/infoschema.rs:249`），meta region 无 leader 时所有语句一起停顿 | 回退路径的可用性取决于 meta region（#136 实测 7.08 s 全节点停顿） |
| 升级检查 | 无；不认识的 meta 在解码时报错，语句失败（ADR 0005 "Rolling upgrades"） | 无主动检测、无告警（§4.3） |
| TSO | `KvClient::current_ts` / `ts_future`，规划前发出、规划后等待（`crates/poc/kv/src/lib.rs:139-160`；ADR 0003）。CR 的 TSO 流每批最多 64 个请求（CR `src/pd/timestamp.rs:41`），断流重连已修（CR#17） | 批量窗口、follower proxy、RPC 并发等 sysvar 未接 |
| 低精度 TSO | CR `pd::LowResolutionTs`：每次取 TSO 时更新，超过 2 s 视为不可用，由调用方重新取（**惰性**，无后台循环；CR `src/pd/low_resolution_ts.rs:1-52`），目前只给锁解析用（CR `src/transaction/lock.rs:915-927`） | 没有后台刷新、周期不可调、没有 stale ts / safe ts / ts 校验；`tidb_low_resolution_tso` 未实现 |
| MaxTS 点查 | 已有（ADR 0005） | 只覆盖 handle 点查 |
| stale read / `tidb_snapshot` / external ts | 无 | CR 不支持 `StaleRead` 标志，也不取 store safe ts（CR 源码中无 `stale_read` / `safe_ts`，除 metrics 标签外） |
| replica read | 接受后**静默忽略**（`docs/compatibility.md`） | 对照表 §3 |
| 显式事务 | `BEGIN` / `START TRANSACTION` / `autocommit=0` 报 1235 "explicit transactions are not supported"（`crates/poc/session/src/lib.rs:289-293`、`:407`、`:654`）；`transaction_read_only` 等被接受并忽略（`:323-335`） | §4.5 |
| 会话状态 | `SessionStates` 类型已移植（`crates/sessionctx/src/sessionstates.rs`，D05.2）；编码在 D12.12，token 在 D04.1 | 未接入；见 ADR 0009 |
| 连接 ID | 进程内计数器（`crates/mysql-protocol/src/server.rs:125`） | 不是 globalconn 布局（D2） |

## 4. tidb-rs 设计

### 4.1 集群成员

- **serverinfo**：保持 ADR 0005 的做法（字段、lease、`disable_rpc=true`）。
- **topology**：默认不写 `/topology/tidb`（D1）。
  - 原因：TiProxy 从这里发现后端并**按整个连接**路由（ADR 0009 §2）；tidb-rs 一旦进入，会收到含写语句的连接。
  - Dashboard 因此看不到 tidb-rs。tidb-rs 的发现另用一个前缀，见 ADR 0009 D3。
- **owner**：不参选任何 owner，作为启动断言（不是配置项）。
- **server ID 与 global KILL**（D2 选 (b) 时）：
  - 照 Go 抢占 `/tidb/server_id`，按 `globalconn` 布局分配连接 ID；
  - status 端口提供 gRPC `Coprocessor`，只实现 `ExecType_TypeKill` 和 cluster 内存表需要的部分（N9 的 `CLUSTER_*`）；
  - `KILL` 不是本机的 ID 时，照 Go 转发。
  - 在此之前（D2 选 (a)），连接 ID 至少避开 Go 节点可能使用的 serverID 段。理由（推断，未实测）：从 Go 节点对一个 tidb-rs 连接 ID 执行 `KILL`，会按 ID 的高位找节点，可能落到某个 Go 节点上。
- **minstartts**：显式只读事务（§4.5）会让读的寿命超过 `tidb_gc_life_time`，需要照 Go 每 30 s 上报（`info.go:814-878`）。autocommit 读不需要。

### 4.2 schema 同步：lease 按 TSO、准入窗口 ≤ 注册窗口（#136）

**规则**（#136 Lucky 的三点，本 spec 写成不变式）：

1. **lease 按 TSO 计**，照 `schema_validator.go:134-148`：
   - 每轮完整同步循环开始时取一个 TSO `sync_ts`，用它读版本并 reload（ADR 0005 已经这样做）。
   - 循环成功后，`expire_ts = phys(sync_ts) + lease - 1ms`。
2. **语句准入**：语句的读 ts `read_ts` 满足 `phys(read_ts) <= expire_ts` 才用同步的 schema。
   - 这是 Go 写事务提交检查（`:274-278`）**移到了读语句的准入**：Go 不检查读，tidb-rs 只有读，见 D3。
   - MaxTS 点查没有 `read_ts`，用 `LowResolutionTs` 的最新值加上其最大年龄比较（推断：这是保守的上界）。
   - stale read / `tidb_snapshot` 用历史 schema，不受此限（与 Go 一致，§2.2）。
3. **先停止准入，再注销；准入期间保持注册**。不变式：

   > 任何被准入的语句，在它整个执行期间，本节点都在 owner 的等待集合中（serverinfo 键存在），或者该语句的结果被丢弃。

   实现：
   - 注销流程固定为：(1) 在 registry 锁内置"关闭准入"并换 epoch；(2) 撤 lease。ADR 0005 已有这个顺序和结果 fencing（`StmtGuard::results_valid`，`registry.rs:182-192`）。
   - 新增要求：**注销条件不早于准入条件失效**。`max_staleness` ≥ lease（默认都是 45 s），并且 watchdog 的判定以 `expire_ts` 为准：用最新的 `LowResolutionTs` 判断 `phys(now_ts) > expire_ts`；拿不到 TSO 时退回墙钟加一个余量。
   - 加 `debug_assert!`：注销时不能存在 `epoch == current` 的已准入语句仍在准入窗口内。
   - 加一个单元测试：构造"准入后立即注销"的交错，断言结果被拒绝。
4. **`max_staleness` ≥ lease**：
   - 启动时校验，`--schema-sync-max-staleness` 小于 `--schema-lease` 时拒绝启动。
   - 默认 lease 取 Go 的 45 s，可配置（D5）。

**效果**
- meta region 选举（#136 中约 12 s）期间，节点继续用已加载的 schema 准入语句，不注销，也不走回退路径。
- owner 在这段时间等待本节点（Go 节点在同样情况下也会让 owner 等待，§2.2）。

**lease 过期之后**（D4）
- 节点停止准入，注销，然后：(a) 照现在走 ADR 0003 回退路径（每条语句读版本键）；或 (b) 照 Go 写事务的语义报 8027，直到重新同步。
- 建议 (a)：它是正确的，代价只在可用性。

**显式只读事务与 MDL**（§4.5）
- 事务在 registry 中的条目从**第一次访问表**开始，到 COMMIT / ROLLBACK 为止；表集合随语句累加。这与 Go 一致（`preprocess.go:2101`）。
- 事务内第一次访问某表时，用当前同步的 schema，并照 Go 处理新索引和列 ID 变化（`preprocess.go:2132-2175`）。
- 长事务会挡住 DDL，与 Go 一致；`mysql.tidb_mdl_view` 看不到它（ADR 0005 已列）。D6 决定是否加上限。

**上报**：MDL 开时照 Go 上报 `all_schema_by_job_versions`（ADR 0005 已做）；关时上报 `all_schema_versions`。

### 4.3 升级安全：不认识就拒绝服务并报警

tidb-rs 在升级后的集群里始终是旧节点，Go 的"忽略未知"对它不安全：它会按旧规则解读新元数据。规则如下：

| 检测点 | 条件 | 动作 |
|---|---|---|
| 启动和每轮同步 | `mysql.tidb.tidb_server_version` > 232（v8.5.8 的 `currentBootstrapVersion`） | **节点级**：停止准入、注销、所有语句报明确错误（D7 定错误码），指标 `tidb_rs_unknown_metadata{kind="bootstrap"}` = 1，ERROR 日志 |
| reload | 不认识的 `SchemaDiff.Type` | 节点级（同上）。tidb-rs 现在每次都全量 reload，所以这一项用来发现"集群跑了新 DDL"；改成增量 reload 后，这是必需的 |
| reload | `TableInfo.Version` > 5，或 JSON 中有不认识的字段 | **表级**：该表报错（`ErrTableNotExists` 不合适，D7 定错误码），其他表照常；指标 `kind="table"` |
| reload | `mysql.tidb_mdl_info` 及 tidb-rs 读取的其他系统表多了或少了列 | 节点级：MDL 协议依赖这些列 |
| 注册时 | 集群中有 serverinfo 的 `version` 高于 v8.5 的大版本 | 只报警，不拒绝：升级过程中的正常状态 |

"不认识的 JSON 字段"需要在 D06.1 的 Go-JSON 层加一个**只在 reload 时启用**的严格模式（L2 胶水，标注）。普通解码仍照 Go 忽略未知字段，保证 L1 测试不变。

### 4.4 读时间戳

**统一的读 ts 来源**（D05.4 的 provider 照 Go 移植，这里只定 tidb-rs 侧的载体）：

```rust
// glue（D05.4）：一条语句的读 ts 从哪里来
enum ReadTsSource {
    Tso,                    // 新 TSO（future，规划前发出，规划后等待）
    LowResolution,          // tidb_low_resolution_tso
    MaxTs,                  // autocommit 点查（Go IsPointGetWithPKOrUniqueKeyByAutoCommit）
    TxnStart(u64),          // RR / optimistic 显式事务
    Stale(u64),             // AS OF / tx_read_ts / tidb_read_staleness / external ts
    Snapshot(u64),          // tidb_snapshot
    External(u64),          // ADR 0009 的外部注入 ts（影子读）
}
```

**`LowResolutionTs` 的扩展**（CR `src/pd/low_resolution_ts.rs`，接口不变，加方法）：

| 能力 | Go | 扩展 |
|---|---|---|
| 后台刷新 | `updateTS` 每 2 s（CG `pd.go:458-512`） | 可选的后台任务；周期由 `tidb_low_resolution_tso_update_interval` 设（`set_update_interval`，最小 10 ms）。打开 `tidb_low_resolution_tso` 或有 stale read 时启用（D9）。`MAX_AGE` 改为"当前周期 + 一个余量"，惰性路径保留为兜底 |
| 自适应缩短 | stale read 需要更新的 ts 时缩短，下限 500 ms（CG `pd.go:335-456`） | 先不做，记入对照表；只在 stale read 实现时一起做 |
| stale ts | `GetStaleTimestamp(prevSecond)`（CG `pd.go:607-632`） | `stale_ts(prev: Duration)`：`phys(last) + (now - prev - arrival)` |
| ts 校验 | `ValidateReadTS`（CG `pd.go:667-713`），晚于缓存时单飞取新 TSO | `validate_read_ts(ts, is_stale)`，同样单飞；错误文字照 CG |
| min safe ts | `safeTSUpdater` 每 2 s（CG `tikv/kv.go:708-728`） | 另一个组件（`SafeTsCache`），按 store 取 safe ts；随 stale read 一起做 |

- **共享范围**：一个 `TransactionClient` 一个实例，锁解析、语句 ts、stale 计算共用。与 Go 每个 `KVStore` 一个 oracle 相同。
- **TSO 批量**：
  - CR 的批上限 64 改为可配（Go 10000）；
  - 接 `tidb_tso_client_batch_max_wait_time`（0–10 ms 窗口）。
  - `tidb_enable_tso_follower_proxy`、`tidb_tso_client_rpc_mode` 先报"不支持"（宪章第五条），记入对照表。

**schema 与读 ts 的对应**

| 读 ts 来源 | 用的 schema | MDL |
|---|---|---|
| Tso / LowResolution / MaxTs / TxnStart | 同步的 schema（§4.2 准入） | 显式事务持有，autocommit 不持有 |
| Stale / Snapshot | `GetSnapshotInfoSchema(ts)`：ADR 0003 的"在 ts 上读版本并加载"，带缓存 | 不持有 |
| External | 同 Tso（ADR 0009 模式 1 中，注入的 ts 是刚从 PD 取的 TSO） | 同 autocommit |

**replica read**
- 照 Go 只对只读语句生效（tidb-rs 全是只读语句）。
- 需要 CR 实现 `ReplicaReadType` 和 closest 的 label 匹配（对照表 §3，P1 或 P2）。
- 在那之前，设置非 `leader` 值改为**报错**，而不是现在的静默忽略（宪章第五条，见 D10）。

**RC**
- 照 Go：pessimistic + RC 的显式事务每条语句取新 TSO。
- `tidb_rc_read_check_ts` 照 Go 复用上一个 ts。tidb-rs 没有写冲突，复用总是成立：只读事务里不会遇到 `WriteConflict`（推断）。

### 4.5 显式只读事务

**接受的写法**（D8）
- `BEGIN`、`START TRANSACTION [READ ONLY | WITH CONSISTENT SNAPSHOT | READ WRITE]`；
- `SET autocommit=0` 之后的隐式事务；
- `START TRANSACTION READ ONLY AS OF ...`（stale，随 stale read 实现）。

**语义**（照 Go 的 provider）

| 项 | 规则 |
|---|---|
| start_ts | `BEGIN` 时取（`base.go:96-108`）；`autocommit=0` 在第一条语句时取 |
| RR / optimistic | 所有语句读同一个 start_ts，共用一个快照 |
| pessimistic + RC | 每条语句一个新 ts（§4.4） |
| MaxTS | 显式事务内**不用** MaxTS（Go 要求 `!InTxn`，`common_plans.go:1500-1502`） |
| 锁处理集合 | 快照的 `resolved_locks` / `committed_locks` 在事务内跨语句共享，只对这一个固定的 start_ts 有效。RC 下随每条语句的新 ts 重建；MaxTS 读的集合只在该语句内有效（CR `src/transaction/transaction.rs:2168-2182`）。这个约束写进 D07.1 的快照接口：集合挂在 `(start_ts)` 上，不能挂在会话上 |
| COMMIT / ROLLBACK | no-op（`session.go:510-513`），释放 MDL 条目 |
| 写语句 | 拒绝，错误码见 D8 |
| `SELECT ... FOR UPDATE` / `FOR SHARE` | autocommit 下照 Go 忽略锁子句（`builder.go:775-794`）。事务内 Go 会加锁（写 TiKV），tidb-rs 不能，所以报错（D8） |
| 状态标志 | OK / EOF 包的 `SERVER_STATUS_IN_TRANS` 照 Go 设置。TiProxy 依赖这个标志判断能否迁移（ADR 0009 §2） |
| MDL | §4.2 |
| minstartts | 有显式事务时上报（§4.1） |

## 5. 兼容性

| 项 | 与 Go 相同 | 与 Go 不同（用户可见） |
|---|---|---|
| serverinfo、MDL 上报、owner 等待 | 协议、键、lease | — |
| topology | — | 不在 Dashboard、TiProxy 的默认发现中（D1） |
| global KILL | (b) 时相同 | (a) 时从其他节点 `KILL` tidb-rs 的连接无效（D2） |
| schema lease | 45 s、按 TSO | 读语句也受 lease 约束：Go 不检查读（D3） |
| 升级 | — | 遇到不认识的元数据时拒绝服务，Go 会继续运行（D7） |
| `START TRANSACTION READ ONLY` | 解析相同 | 默认 noop 关闭时 Go 报 1235，tidb-rs 接受并强制只读（D8 建议） |
| 显式事务里的写 | — | Go 执行，tidb-rs 拒绝（只读节点） |
| `tidb_replica_read` | (CR 支持后) 相同 | 支持前报错（D10） |

这些差异在实现时写进 `docs/compatibility.md`。

## 6. 需要负责人决定

| # | 问题 | 选项 | 建议 |
|---|---|---|---|
| D1 | 是否写 `/topology/tidb` | (a) 不写（ADR 0005 现状）；(b) 写，Dashboard 和 TiProxy 都能看到 | **(a)**：TiProxy 按连接路由，没有读写分离（ADR 0009 §2），进入 topology 会收到写连接。tidb-rs 的发现另走 ADR 0009 D3 |
| D2 | server ID 与 global KILL | (a) 维持 `server_id=0`，不支持跨节点 KILL；(b) 照 Go 抢占 server ID，globalconn 连接 ID，status 端口 gRPC 接收 KILL | **(b)**，随 D03.1 / status 端口 gRPC 一起做；之前按 (a)，并按 §4.1 避开 Go 的 ID 段 |
| D3 | schema lease 是否用于读语句准入 | (a) 照 Go，读不检查（节点失联后无限期用旧 schema）；(b) 按 TSO 在准入时检查（§4.2） | **(b)**：只读节点的读就是全部流量；这是 #136 的修复方向 |
| D4 | lease 过期后的语句 | (a) ADR 0003 回退路径（每条语句读版本键，正确但依赖 meta region）；(b) 报 8027 直到重新同步 | **(a)** |
| D5 | lease 与 `max_staleness` 默认值 | (a) 都取 45 s（Go 的 lease），卡住的 tidb-rs 最多挡 DDL 约 45 s；(b) 更短（如 15 s），可用性窗口也更短 | **(a)**，两者可配置，启动时校验 `max_staleness ≥ lease` |
| D6 | 显式只读事务对 DDL 的阻挡 | (a) 照 Go，事务多长就挡多久；(b) 加上限（超过后事务报 8028，释放 MDL） | **(a)** 照 Go；监控里给出阻挡 DDL 的 tidb-rs 事务 |
| D7 | 升级检查的粒度和错误码 | (a) 全部节点级；(b) bootstrap 版本、diff 类型、系统表列为节点级，表元数据为表级（§4.3） | **(b)**。错误码：8027 的文字是固定的（"schema failed to update in 1 lease, please make sure TiDB can connect to TiKV"，`pkg/errno/errname.go:984`），不能说明原因，会误导排查。建议用 1105 加明确文字（"tidb-rs v8.5.8 cannot read metadata written by a newer TiDB: ..."），不新增错误码 |
| D8 | 显式事务的语法与错误码 | **语法**：(a) 照 Go，`READ ONLY` 受 `tidb_enable_noop_functions` 控制，默认 1235；(b) tidb-rs 始终接受 `READ ONLY` 并强制只读。**写语句**：用 1792（MySQL 的语义，Go 定义了但没用）或 1836。**事务内加锁读**：1792 或 1235 | 语法 **(b)**（特性例外，用户可见）；写语句和事务内加锁读都用 **1792** |
| D9 | 低精度 TSO 后台刷新 | (a) 保持惰性；(b) 照 Go 加后台刷新，周期可调 | **(b)**，按需启用（§4.4） |
| D10 | `tidb_replica_read` 在 CR 支持前的行为 | (a) 静默忽略（现状）；(b) 非 `leader` 值报错 | **(b)**（宪章第五条） |

## 7. 验收

1. **#136 回归**：
   - `tools/ab/faults` 的 `kill_tikv`：先用 `pd-ctl operator add transfer-leader <schema 版本键所在 region> <将被杀的 store>`，把 meta region 的 leader 放到将被杀的 store 上，使场景确定。
   - 期望：节点不注销，语句不停顿超过数据 region 自身的选举时间（Go 同场景作对照，#136 中为 13.2 s）。
2. **不变式**：
   - 单元测试：准入与注销交错时，结果被拒绝；
   - `debug_assert!` 覆盖 §4.2 第 3 条；
   - 启动参数 `max_staleness < lease` 时拒绝启动。
3. **lease 按 TSO**：用 failpoint 冻结同步循环。`phys(read_ts) > expire_ts` 的语句不被准入；注销发生在准入停止之后（日志顺序加上 etcd 键消失的时间）。
4. **MDL**：
   - 显式只读事务读表 t 时，Go 上的 `ALTER TABLE t ADD COLUMN` 等到事务结束才完成；
   - autocommit 读不阻挡；
   - 事务内首次访问一个新加了索引的表，不使用该索引（与 Go 对照）。
5. **升级**：在 playground 里手工把 `tidb_server_version` 改成 233，节点在一个同步周期内拒绝服务，指标为 1；改回后恢复。
6. **读 ts**：
   - `tidb_low_resolution_tso=ON` 时语句不发 TSO 请求（PD 请求计数）；
   - 后台刷新周期跟随 sysvar；
   - RC 显式事务每条语句一个 TSO，RR 一个。
7. **显式事务**：
   - 两条 SELECT 之间 Go 提交了一行，RR 事务内看不到，RC 看得到；
   - 写语句报 D8 的错误码；
   - `SERVER_STATUS_IN_TRANS` 在 BEGIN 之后置位、COMMIT 之后清除（抓包）。
8. **global KILL**（D2 (b)）：从 Go 节点 `KILL` tidb-rs 的连接，语句在一个 checkpoint 内结束（N3）。

## 8. 风险

- **lease 变长后 DDL 被挡得更久**：卡住的 tidb-rs 最多挡 45 s（现在 5 s）。Go 节点在 etcd 还活着的情况下会挡更久（无上限），所以这仍不差于 Go，但会被用户看到。缓解：D5 可配置；指标和日志报出"本节点正在挡 DDL"。
- **TSO 与本地时钟**：准入用 TSO，watchdog 拿不到 TSO 时（PD 不可用）只能用墙钟加余量来判断。这正是 PD 不可用的时候，语句本身也取不到 TSO，影响有限（推断）。
- **严格模式误报**：Go 小版本也可能在 JSON 中加字段，导致表级拒绝。缓解：上游同步流程把新增字段登记为"已知"；报警而不是静默。
- **显式事务的生命周期**：tidb-rs 第一次需要跨语句的会话状态（快照、MDL 条目、minstartts）。客户端忘了 COMMIT 时，资源一直被占着，与 Go 相同；靠 `wait_timeout` 断开连接，需要协议层支持。

## 9. 与其他 spec 的关系

- **ADR 0003 / 0005**：本 spec 修订 ADR 0005 的健康判定（墙钟 5 s → TSO lease 45 s），注册协议不变；回退路径仍是 ADR 0003。实现时在 ADR 0005 加一节"由 N11 修订"。
- **ADR 0008 / 对照表**：`LowResolutionTs` 扩展、stale read、replica read、safe ts 是 CR 的工作项，计入对照表 §3、§8。
- **ADR 0009**：四种流量模式依赖本 spec 的 §4.4 外部 ts、§4.5 显式事务、会话状态。
- **N3**：KILL 的取消令牌；显式事务的会话状态在 N2 的会话上下文里。
- **N9**：`CLUSTER_*` 扇出需要的 status 端口 gRPC 与 D2 共用。
