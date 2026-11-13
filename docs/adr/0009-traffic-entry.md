# ADR 0009：流量接入的四种模式

- 状态：proposed（2026-11-13）。四种模式本身由项目负责人决定；本 ADR 给出每种模式的实现要求和待决定的点（§6）。
- 背景：#128 第 1 项（"只读节点怎么接流量，没有设计"）。前提是 [N11](../../specs/exceptions/N11-cluster-readts/spec.md)：集群成员、schema 同步、读时间戳、显式只读事务。
- 引用记法：
  - `pkg/...`：TiDB v8.5.8；
  - CG：client-go `c3b5079`；
  - TP：TiProxy `pingcap/tiproxy` @ `0065729`（`v1.3.3-beta.2-5`，本机 `~/src/pingcap/tiproxy`）。
- 标"推断"的是没有代码或实测支撑的判断。

## 1. 决定

tidb-rs 支持四种流量模式：

| # | 模式 | 拓扑 | 读 ts 来源 | 谁的结果返回给应用 |
|---|---|---|---|---|
| 1 | 影子读 | app → TiProxy → {tidb-go, tidb-rs} | **TiProxy** 取 TSO，注入两边 | 只有 tidb-go；tidb-rs 比对并记录诊断 |
| 2 | 直连，`autocommit=1` 的 SELECT | app → tidb-rs | tidb-rs 自己取 TSO | tidb-rs |
| 3 | 直连，`autocommit=0`（`BEGIN; SELECT …`） | app → tidb-rs | tidb-rs 自己取 TSO | tidb-rs |
| 4 | TiProxy 分流 | app → TiProxy → tidb-go，部分语句和事务 → tidb-rs | tidb-rs 自己取 TSO | 执行它的那个后端 |

在模式 4 中，TiProxy 把 `autocommit=1` 的 SELECT 和标记为只读的显式事务路由到 tidb-rs。

模式 2、3 就是 #128 所说的"独立只读端点"（报表、BI 直连）；模式 1、4 是"TiProxy 分流"。两种定位都要，差别只在落地顺序（§6 D8）。

## 2. 现状：TiProxy 能做什么、不能做什么（TP @ 0065729）

| 能力 | 现状 | 位置 |
|---|---|---|
| 后端发现 | 只看 etcd `/topology/tidb/<ip:port>/{info,ttl}`，没有 `ttl` 键的后端被丢弃；SQL 地址取自键，JSON 只读 `ip`、`status_port`、`labels` 等 | TP `pkg/manager/infosync/info.go:33-37`、`:93-105`、`:259-311` |
| 健康检查 | `GET <ip>:<status_port>/status`（任何 HTTP 错误都算不健康；Go 优雅停机时返回 500），SQL 端口握手，`GET /config` 读 `security.session-token-signing-cert`，非空才允许迁移会话 | TP `pkg/balance/observer/health_check.go:108-184`、`:197-221` |
| 路由粒度 | **只按连接**：握手时选一次后端（`getBackendIO`），之后只能整体迁移；`ClientInfo` 只有地址和端口；没有读写分离，也没有按语句路由 | TP `pkg/proxy/backend/backend_conn_mgr.go:295-318`；`pkg/balance/router/backend_selector.go:8-14` |
| 会话迁移 | 旧后端 `SHOW SESSION_STATES` → 新后端用 `tidb_session_token` 插件认证 → `SET SESSION_STATES`；只在 `finishedTxn()`（不在事务中、没有未取完的游标或 long data）时迁移 | TP `backend_conn_mgr.go:55-58`、`:633-725`；`pkg/proxy/backend/cmd_processor.go:104-110`；`pkg/proxy/backend/authenticator.go:352-384` |
| 事务状态 | 从每个 OK / EOF 包的 `SERVER_STATUS_IN_TRANS` 得出，不单独跟踪 autocommit；不解析 `BEGIN READ ONLY` | TP `cmd_processor.go:41-68`；`pkg/util/lex/filter.go:103-110` |
| 镜像与比对 | **没有在线镜像**。流量回放丢弃结果（"the result may be huge and causes OOM"），只记录错误 | TP `pkg/sqlreplay/conn/packetio.go:22`；`pkg/sqlreplay/report/tables.go:9-21` |
| prepared ID | 迁移时依赖 TiDB 恢复原 ID；回放时有 `psIDMapping`，缺映射时按文本现场 prepare | TP `pkg/sqlreplay/conn/conn.go:83`、`:261`、`:299-321` |
| TSO / PD | **没有**。只连 etcd；PD client 只是间接依赖 | TP `pkg/util/etcd/etcd.go:30-68`；`go.mod:188` |

tidb-go 一侧（v8.5.8）与本 ADR 相关的事实：

- **会话状态**：`SHOW SESSION_STATES` 返回状态 JSON 和签名 token（`pkg/executor/show.go:2383-2417`）。
  - token 有效 1 分钟，用集群共享的 `session-token-signing-cert` / `-key` 签名（`pkg/sessionctx/sessionstates/session_token.go:53`、`:77-123`；`pkg/config/config.go:608-609`）。
  - `SET SESSION_STATES` 恢复 prepared statement 时**保留原 ID**（`pkg/server/driver_tidb.go:494-496`；`pkg/sessionctx/variable/session.go:3051`）。
  - 有事务、临时表、advisory lock、`LOCK TABLES`、未取完的游标时报 8146（`pkg/session/session.go:4665-4692`；`driver_tidb.go:466-473`）。
- **精确 ts**：现有的能让语句读在一个**精确 TSO** 上的方式只有两个，语义都不是"新鲜读"：
  - `tidb_snapshot`（SESSION）：用历史 schema、不持有 MDL、不进入事务、拒绝写（`pkg/sessionctx/variable/varsutil.go:436-465`；`pkg/sessiontxn/isolation/base.go:183-205`、`:303-305`）；
  - `tidb_external_ts`（GLOBAL，存在 PD，所有会话共享，`sysvar.go:2940-2956`）。

  `AS OF TIMESTAMP` 只有毫秒精度（`pkg/sessiontxn/staleread/util.go:36-61`）。
- **事后取 ts**：`tidb_last_query_info` 在每条语句后记录 `start_ts`、`for_update_ts`（`pkg/executor/adapter.go:1701-1709`）。
- **只读标记的语法**：`BEGIN /*T! READ ONLY */` 在 tidb-go 中是**语法错误**；`START TRANSACTION READ ONLY` 默认报 1235（N11 §2.6，实测）。

## 3. 各模式的要求

### 3.1 所有模式共同的 tidb-rs 前提

| 项 | 内容 | 所属 |
|---|---|---|
| schema 同步 | N11 §4.2（lease 按 TSO、准入窗口 ≤ 注册窗口） | N11、D06.5 |
| 升级安全 | N11 §4.3 | N11 |
| 读 ts | N11 §4.4 | N11、D05.4 |
| `/status` | 能准入语句时返回 200，否则 500（lease 过期、升级拒绝、关闭中）。TiProxy 和负载均衡据此摘除节点（TP `health_check.go:156-184`） | D03.1 |
| 认证 | 照 Go 的认证插件（D04.1）；模式 1、4 另需 `tidb_session_token` | D04.1 |
| 系统变量 | 驱动和 ORM 初始化时 `SET` 的变量都要接受（N8 注册表），不支持的值报错 | N8 |
| 写语句 | 拒绝，错误码见 §6 D7 | N11 D8 |
| `SERVER_STATUS_IN_TRANS` 等状态标志 | 照 Go 设置。TiProxy 靠它判断能否迁移 | D03.1 |

### 3.2 模式 1：影子读

**流程**（每条可比对的语句；粗体是对现有组件的改动）

1. TiProxy 收到语句，判断是否可比对（下文"范围"），**向 PD 取 TSO `ts`**。
2. TiProxy 把语句和 `ts` 发给 tidb-go（**注入**，见 D1），把结果流式返回应用，同时**计算结果摘要 `g`**。
3. 应用拿到结果之后，TiProxy 把 `(语句, ts, g)` 发到这个客户端连接对应的 **tidb-rs 影子连接**。
4. tidb-rs 在 `ts` 上执行，计算自己的摘要 `r`：
   - 比对 `g` 和 `r`，不一致时记录诊断：SQL、参数、`ts`、两边摘要、tidb-rs 的计划和结果样本；`ts` 在 GC 之前都可以重放；
   - 给 TiProxy 返回一个 OK，不返回行。

tidb-rs 的执行在应用的关键路径之外。影子连接积压时，TiProxy 丢弃影子请求并计数，不反压应用。

**注入 ts 的语义**（D1）：注入的 `ts` 取代 tidb-go 中该语句（RR 事务则是整个事务）的**所有**读 ts 来源，包括 MaxTS 点查（`optimistic.go:128-172`）和低精度 TSO（`base.go:643-660`）。tidb-rs 侧同样用 `External(ts)`，不走 MaxTS。

**TSO 的正确性**（D2）
- 应用看到的是 tidb-go 的结果，所以注入的 `ts` 必须和 tidb-go 自己取的 TSO 一样新：**在语句到达 TiProxy 之后发出请求**。只有这样，应用在此之前确认过的提交才对它可见。
- 多个连接同一时间段到达的语句**合批**取 TSO，是安全的（PD client 本来就这样做，PD `pkg/batch/batch_controller.go:68-183`）。
- **复用**语句到达之前就拿到的 TSO，等于 `tidb_low_resolution_tso`，会让应用读不到自己刚提交的数据。所以"TSO 缓存"只能是合批，除非该会话本来就开着 `tidb_low_resolution_tso`。
- 显式事务：RR 下 `ts` 在 BEGIN 时取一次，整个事务两边都用它；RC 下每条语句一个。

**范围**（只比对两边语义上应该相同的语句）
- **比对**：
  - `autocommit=1` 的 SELECT；
  - 只读显式事务里的 SELECT。
- **不比对**：
  - 事务中已有写之后的读：tidb-go 会读到自己的未提交写（membuffer），tidb-rs 读不到；
  - 加锁读、`SHOW`、`INFORMATION_SCHEMA`、`PROCESSLIST` 一类节点相关的查询；
  - 读会话状态的函数：`LAST_INSERT_ID()`、`FOUND_ROWS()`、`ROW_COUNT()`、`CONNECTION_ID()`、`@@tidb_*` 信息变量。
- **比对但归类为"不确定"、不计为不一致**：
  - 非确定函数（`NOW()`、`RAND()`、`UUID()`）；
  - 没有全序 `ORDER BY` 的 `LIMIT`；
  - 浮点聚合（求和顺序不同，最后几位可能不同）。
  - TiProxy 只做粗分类（`lex`），细分类由 tidb-rs 在规划后做。

**摘要**
- 两种都算，tidb-rs 按自己的计划决定用哪种：结果有全序就用顺序敏感的，否则用顺序无关的。
  - 顺序敏感：按行哈希链；
  - 顺序无关：行哈希的和加行数。
- 另外分别比对：列定义（名字、类型、flags、小数位）、warning 数、错误码。
- 摘要在协议包的字节上计算（text / binary 协议各一种），这样数值格式、时间精度、字符集的差异都会被发现。
- TiProxy 侧的代价是对结果字节做一次哈希。可以按采样率开启（推断：与转发本身的拷贝相比开销不大，需要测）。

**会话状态**
- 影子连接建立时，用 tidb-go 的 `SHOW SESSION_STATES` 初始化：用 token 认证，再 `SET SESSION_STATES`。
- 之后，TiProxy 把改变会话状态的命令也发给影子连接：`SET`、`USE` / `COM_INIT_DB`、`PREPARE`、`COM_STMT_PREPARE` / `CLOSE` / `RESET`、`COM_RESET_CONNECTION`、`COM_CHANGE_USER`。写语句不发。
- 怀疑漂移时（例如连续不一致），在事务边界再做一次 `SET SESSION_STATES`。
- prepared ID：初始化时两边相同（Go 恢复原 ID）；之后各自分配，由 TiProxy 映射（同回放的 `psIDMapping`）。

**各方改动**

| 组件 | 改动 |
|---|---|
| TiProxy | PD TSO client（合批、不复用，D2）；每个客户端连接一个影子后端连接；会话状态命令的转发与 prepared ID 映射；结果摘要；把 `(语句, ts, g)` 送到 tidb-rs；限流与丢弃；tidb-rs 的发现（D3） |
| tidb-go | 接受注入的读 ts（D1），语义等同"刚从 PD 取到的 TSO"：用最新 schema、持有 MDL、照常走 SchemaValidator。**注入的 ts 取代所有读 ts 来源**：该语句不走 MaxTS 点查优化（否则 Go 读最新数据、tidb-rs 读在 `ts` 上，每个 autocommit 点查都是假的不一致；`pkg/sessiontxn/isolation/optimistic.go:128-172`），也不用低精度 TSO（`base.go:643-660`）；校验 `ts` 不晚于 PD 当前时间（CG `ValidateReadTS`，`oracle/oracles/pd.go:667-713`），或只对可信连接免校验（D1） |
| tidb-rs | 影子连接的命令（接收 `ts` 和 `g`，执行，比对，记录，只回 OK）；N11 §4.4 的 `External(ts)` 读 ts（准入照 §4.2，用同步的 schema）；诊断记录的存储与查询（N9 的语句记录加一个"影子比对"字段）；会话状态的 `SET SESSION_STATES` 和 token 认证 |

### 3.3 模式 2：直连，`autocommit=1`

- tidb-rs 已经能做的：PoC 上的 autocommit SELECT、MaxTS 点查、规划前发 TSO（ADR 0003、0005）。
- 还要补：
  - §3.1 的全部；
  - 驱动、ORM 连接时的初始化查询（`SET NAMES`、`SHOW VARIABLES`、`SELECT @@...`）；
  - `information_schema` 中客户端常用的表（D06.6、D12.12）。
- TiProxy 和 tidb-go 不用改。
- 负载均衡由应用侧负责（HAProxy、DNS、驱动的多主机），用 `/status` 摘除节点。

### 3.4 模式 3：直连，`autocommit=0`

在模式 2 的基础上，加 N11 §4.5：
- `SET autocommit=0` 后的隐式事务；
- `BEGIN` / `START TRANSACTION [READ ONLY]`；
- RR 一个快照、RC 每条语句一个；
- MDL 跨语句持有，minstartts 上报；
- COMMIT / ROLLBACK 是 no-op；
- 事务内的写语句报错（D7）。

ORM 和 JDBC 常把读包在 `autocommit=0` 的事务里（#128），这是模式 3 存在的原因。TiProxy 和 tidb-go 不用改。

### 3.5 模式 4：TiProxy 分流

**路由规则**（TiProxy 新增，按语句或事务决定）

| 会话状态 | 语句 | 去向 |
|---|---|---|
| 不在事务中，`autocommit=1` | SELECT。排除：`FOR UPDATE` / `FOR SHARE`、`INTO`、用户变量赋值（`:=`）、有副作用的函数（`GET_LOCK`、`RELEASE_LOCK`、`NEXTVAL`、`LAST_INSERT_ID(expr)`、`SLEEP`）、读会话状态的函数（见下） | tidb-rs |
| 不在事务中 | 带只读标记的事务开始语句（D6） | 整个事务到 tidb-rs，直到 COMMIT / ROLLBACK |
| 其他 | 所有 | tidb-go |

**只读标记**（D6，实测见 N11 §2.6）
- 负责人提出的 `BEGIN /*T! READ ONLY */` 在 tidb-go 中是语法错误：`/*T!` 不带 feature 列表时，内容会被解析，而 `BEGIN READ ONLY` 不是合法语法。这样 tidb-rs 不可用、TiProxy 退回 tidb-go 时，应用会报错。
- 建议改用 `BEGIN /*T![tidb_rs_read_only] READ ONLY */`：
  - tidb-go 遇到不认识的 feature ID，整段当普通注释，结果是普通 BEGIN（`parser/lexer.go:560-571`、`parser/tidb/features.go:54-61`，实测）；
  - TiProxy 识别这个标记，转发给 tidb-rs 时改写成 `START TRANSACTION READ ONLY`。
  - 这样 tidb-rs 的 parser 不用加 feature ID，仍按 Go 移植；`READ ONLY` 的语义由 N11 D8 决定。

**钉在 tidb-go 上的会话**：TiProxy 不解析表名，所以出现以下情况之一后，会话在剩余的生命周期里不再分流（直到 `COM_RESET_CONNECTION` / `COM_CHANGE_USER` 清除这些状态）：
- `CREATE [GLOBAL] TEMPORARY TABLE`：之后发往 tidb-rs 的 SELECT 会找不到该表，或读到同名的普通表；
- `LOCK TABLES`；
- advisory lock（`GET_LOCK`，Go 的 `session.advisoryLocks`，`pkg/session/session.go:233`、`:1670`）。

这与 tidb-go 拒绝迁移会话的条件一致（8146，`pkg/session/session.go:4665-4692`）。

**标记可能在途中丢失**：`mysql` 命令行客户端默认去掉注释（只保留 `/*!` 和 hint，除非加 `--comments` / `-c`），有些代理和 ORM 也会这样做。`/*T![...]` 被去掉后，事务成了普通 `BEGIN`，静默地走 tidb-go。这是安全的方向（结果正确，只是没有分流），但测试和排查时要知道：用 `mysql` 命令行验证分流时必须加 `--comments`。TiProxy 为每个会话计数"带标记的事务"，便于确认标记是否到达。

**会话状态在两个后端之间**（D5）
- 每个客户端连接同时保持一个 tidb-go 连接和一个 tidb-rs 连接。
- **广播**：改变会话状态的命令（同 §3.2 的列表）两边都发。
  - 应用看到的是 tidb-go 的回复；
  - tidb-rs 失败时（例如它不支持的系统变量值），这个会话**钉在 tidb-go 上**，不再分流，并计数。不让应用报错。
- **prepared statement**：
  - 总是在 tidb-go 上 prepare，应用看到的是 Go 的 ID 和列定义；
  - 只读语句在第一次被路由到 tidb-rs 时，按文本在 tidb-rs 上 prepare，TiProxy 维护 `(客户端 ID → tidb-rs ID)` 的映射，做法同回放（TP `conn.go:299-321`）。
  - 两边返回的列定义必须相同，否则客户端会解错数据。这正是模式 1 要验证的内容之一。
- **"上一条语句"状态**：`SHOW WARNINGS`、`SHOW ERRORS`、`@@warning_count`、`FOUND_ROWS()`、`ROW_COUNT()`、`LAST_INSERT_ID()`、`@@tidb_last_query_info` 读的是上一条语句的结果，TiProxy 要把它们发到执行上一条语句的后端。用户变量：`SET @a` 广播；在 tidb-go 上用 `SELECT ... INTO @a` 或 `:=` 赋值后，tidb-rs 的会话标为"需同步"，下一次分流前，在事务边界用 `SET SESSION_STATES` 同步一次（D5）。
- **整体迁移**（扩缩容、tidb-rs 下线）：照现有机制，用 `SHOW/SET SESSION_STATES` 和 token。tidb-rs 要实现这两条语句，JSON 格式与 Go 字节兼容（D05.2 的类型已移植，编码在 D12.12），并在 `/config` 暴露 `security.session-token-signing-cert`（TP `health_check.go:213-221`）。

**一致性**
- 应用在 tidb-go 上 autocommit 写入并收到 OK 后，下一条 SELECT 才会被 TiProxy 发往 tidb-rs。tidb-rs 在收到语句后才取 TSO，所以能读到这次写入。MaxTS 点查也能读到。
- 前提是不开 `tidb_low_resolution_tso`，这与 Go 的语义相同。

**退回**
- tidb-rs 不健康（`/status` 500）时，TiProxy 不再分流，全部发往 tidb-go。只读标记对 tidb-go 是普通 BEGIN，所以应用不受影响。
- 语句已经发给 tidb-rs，但 tidb-rs 在返回任何行之前报了一个"可改道"错误（准入失败、不支持的语法或函数）时，TiProxy 在 tidb-go 上重试。错误码见 D7。

**各方改动**

| 组件 | 改动 |
|---|---|
| TiProxy | 按语句和事务路由（新功能，现有架构只按连接）；双后端连接；广播与 prepared ID 映射；只读标记的识别和改写；"可改道"错误的重试；tidb-rs 的发现（D3）。不需要 TSO |
| tidb-go | 不用改（标记对它是注释） |
| tidb-rs | §3.1；N11 §4.5；`SHOW/SET SESSION_STATES`、token 认证、`/config`；"可改道"错误 |

## 4. 各模式需要的能力汇总

| 能力 | 1 影子 | 2 直连 ac=1 | 3 直连 ac=0 | 4 分流 |
|---|---|---|---|---|
| N11 §4.2 schema lease / §4.3 升级安全 | 要 | 要 | 要 | 要 |
| 自己取 TSO | — | 要 | 要 | 要 |
| 外部注入 ts（`External`） | 要 | — | — | — |
| 显式只读事务（N11 §4.5） | 要（事务内的读） | — | 要 | 要 |
| `SHOW/SET SESSION_STATES` + token | 要 | — | — | 要 |
| `/status` 反映准入状态 | 要 | 要 | 要 | 要 |
| tidb-rs 的服务发现（D3） | 要 | — | — | 要 |
| TiProxy 改动 | 大 | 无 | 无 | 大 |
| tidb-go 改动 | 中（D1） | 无 | 无 | 无 |

## 5. 不做的事

- tidb-rs 不加入 `/topology/tidb`（N11 D1）。加入后，现有 TiProxy 会把含写的整条连接分给它。
- 不在 tidb-rs 上实现写的转发（把写转给 tidb-go）：那是代理的职责，放在 TiProxy。
- 模式 1 不把 tidb-rs 的结果返回给应用，即使 tidb-go 失败也不返回。

## 6. 需要负责人决定

| # | 问题 | 选项 | 建议 |
|---|---|---|---|
| D1 | tidb-go 怎样接受 TiProxy 给的 start_ts（模式 1） | (a) 新增隐藏的一次性会话变量，取精确 TSO，语义等同新取的 TSO（持有 MDL、用最新 schema），并**取代所有读 ts 来源**（关闭该语句的 MaxTS 点查和低精度 TSO），由 TiProxy 在语句前设置（用 multi-statement 或紧跟的一个包，避免多一次往返）；(b) COM_QUERY 用语句注释，二进制协议用变量；(c) 不改 tidb-go：tidb-go 照常执行，TiProxy 事后读 `@@tidb_last_query_info` 得到 `start_ts` 再交给 tidb-rs，但多一条语句，并会覆盖 `SHOW WARNINGS` 等"上一条语句"状态；(d) 用 `tidb_snapshot`：语义不同（历史 schema、无 MDL、不进事务），**不可用** | **(a)**；校验方式二选一：照 `ValidateReadTS` 校验（ts 新于缓存时多一次 PD 请求），或者只对持有新权限（如 `RESTRICTED_SHADOW_READ`，名字待定）的连接免校验。建议后者。(c) 可作为不改 Go 的早期验证手段 |
| D2 | TiProxy 的 TSO 缓存 | (a) 只合批，每个请求在语句到达之后发出；(b) 允许复用最近的 TSO（低精度语义） | **(a)**；只有会话本身开着 `tidb_low_resolution_tso` 时才允许 (b) |
| D3 | TiProxy 怎样发现 tidb-rs | (a) tidb-rs 写 `/topology/tidb-rs/<ip:port>/{info,ttl}`，JSON 与 Go 的 topology 相同，TiProxy 增加一个前缀；(b) TiProxy 静态配置 tidb-rs 地址 | **(a)**：与现有的发现和健康检查代码同构，也可供 Dashboard 后续使用 |
| D4 | 比对在哪里做（模式 1） | (a) TiProxy 算 tidb-go 的摘要，交给 tidb-rs，由 tidb-rs 比对和记录；(b) TiProxy 比对；(c) 两边各自落日志，离线比对 | **(a)**，与负责人"tidb-rs 做比对和诊断"一致；tidb-rs 有计划，能判断顺序和非确定性 |
| D5 | 模式 4 的会话状态同步 | (a) 广播会话命令 + prepared ID 映射，必要时在事务边界 `SET SESSION_STATES`；(b) 每次切换后端都 `SHOW/SET SESSION_STATES`（每次 2 个往返加 JSON） | **(a)** |
| D6 | 只读事务的标记 | (a) `BEGIN /*T![tidb_rs_read_only] READ ONLY */`，TiProxy 改写后发给 tidb-rs；(b) `START TRANSACTION READ ONLY`，tidb-go 打开 `tidb_enable_noop_functions`（会同时放开其他 noop 函数，如 `LOCK IN SHARE MODE`）；(c) `BEGIN /*T! READ ONLY */`：tidb-go 报语法错误，**不可用** | **(a)**；feature ID 的名字待定。标记被客户端去掉时（`mysql` 命令行默认如此），事务安全地走 tidb-go |
| D7 | 写语句和"可改道"的错误码 | autocommit 写：1836 "Running in read-only mode"（Go 的只读模式）；事务内写：1792（N11 D8）；"可改道"：一个固定的错误码加固定的文字前缀，TiProxy 据此重试 | 如左。"可改道"建议用 1105 加前缀 `tidb-rs: reroute:`，不占用 TiDB 的错误码段 |
| D8 | 落地顺序与外部改动的归属 | 顺序：(a) 2 → 3 → 1 → 4；(b) 1 优先（先验证一致性）。TiProxy、tidb-go 的改动：在 `dulao5/tiproxy`、`dulao5/tidb`（v8.5.8 + 补丁）的 fork 中维护，或向上游提 | 顺序 **(a)**：2、3 只依赖 tidb-rs 自身，能先给出可用的只读端点；1 需要改 TiProxy 和 Go，但能验证 4 所需的列定义和会话状态兼容性，所以排在 4 之前。外部改动先放 fork，与 ADR 0008 的 client-rust fork 同样的维护方式 |

## 7. 后果

- 模式 2、3 只需 tidb-rs 自身的工作（N11、D03.1、D04.1、D05.4、D12.12），可以按现有路线图推进。
- 模式 1、4 需要在 tidb-rs 之外维护 TiProxy 的改动，模式 1 还需要维护 tidb-go 的改动。ROADMAP 要单独计入这部分工作量，并像 client-rust fork 一样定期与上游同步。
- 模式 1 产出的不一致记录，会成为 #128 第 3 项故障场景测试之外的第二个系统级对照来源。
