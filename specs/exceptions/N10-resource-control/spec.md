# N10：资源管控（资源组、RU 计量、限流、runaway query）

状态：**调研草案**（2026-11-13）。类型：实现例外 + 待决定的特性问题。负责人已同意调研方向：Go 的资源管控做得不够好，tidb-rs 可以做得更好。所以本 spec 不是照搬：先列清 Go v8.5.8 的机制和有证据的问题，再给 Rust 的设计。

**兼容面保持不变**：资源组 DDL 与选项、`SET RESOURCE GROUP`、`RESOURCE_GROUP()` hint、`CURRENT_RESOURCE_GROUP()`、`QUERY WATCH`、`CALIBRATE RESOURCE`、相关系统变量、`information_schema` 表、`mysql.tidb_runaway_*` 表、错误码，以及与 PD resource manager、TiKV 之间的协议。变的只是 TiDB 一侧怎样计量、怎样限流、怎样判定 runaway。

引用：TiDB `v8.5.8`（`pkg/` 下的路径）；client-go `c3b5079`（记为 CG）；PD client `v0.0.0-20260804033407-85a975a5ca78`（v8.5.8 实际使用的版本，`resource_group/controller/` 下的路径，记为 PD）。

## 0. 总结

1. **Go 的计量缺口**（§2.3）：on-prem 的 TiDB 从来不把自身执行 SQL 的 CPU 计入 RU，单组也一样。PD client 只在 `EnableSingleGroupByKeyspace`（serverless）模式下取进程 CPU，v8.5.8 创建控制器时不传这个选项（`domain/runaway.go:37`），否则返回 0（PD `model.go:312-315`，带 TODO）。TiKV CPU 只对读请求计费（PD `model.go:108`）；写失败的退款永远不执行（CG `resource_control.go:177-179`，`Succeed()` 恒为 true）。
2. **Go 的限流粒度**（§2.2）：每个进程、每个资源组一个令牌桶，等待加在**每个 KV RPC 发出之前**。同组请求之间没有公平性，按抢锁顺序；读费用响应后才扣，可欠账；`BURSTABLE` 等于完全不限（tidb#54554）。
3. **Go 的 runaway 判定**（§2.4）：EXEC_ELAPSED 由 100 ms 的 tick 检查，刚过期就结束的语句会漏记（tidb#69254）；KILL 只是发信号，要等执行器下次检查才生效（tidb#70333：300 s 拖到 419 s）；RU 和 PROCESSED_KEYS 只在 cop 响应时检查。COOLDOWN（rule 或 watch 触发）和 rule 触发的 SWITCH_GROUP 只作用于 cop 请求。
4. **tidb-rs 设计**（§4）：
   - **计量**：放在 client-rust 的请求完成钩子里，与 N9 的 KV 统计同一处；TiDB 侧 SQL CPU 用 N9 的窗口分摊记到语句所在的资源组。
   - **限流**：从"每个 RPC 前睡眠"改为在 N3 运行时里按资源组排队、加权公平地调度语句的任务，令牌桶协议不变。
   - **runaway**：在 N3 的 checkpoint 和 poll 包装器里持续检查，TiDB 侧的计算也能及时发现。
5. **需要负责人决定**的 6 点见 §6。最重要的是 D1：建议 SQL CPU 默认只计入 RU 指标，参与限流作为显式开关（on-prem 的 Go 从不计 SQL CPU，直接计入会改变所有现有部署的 `RU_PER_SEC` 含义）。

## 1. 范围

| 在范围内 | 不在范围内 |
|---|---|
| TiDB 侧：RU 计量（KV 请求 + SQL CPU）、resource manager 客户端（令牌桶、向 PD 申请和上报）、限流的位置与粒度、runaway 判定与动作、QUERY WATCH、background 任务标记、相关 SQL 面 | TiKV 侧的优先级调度和 background 限速（同源 Rust，原样使用）；PD resource manager 服务端；TiFlash / MPP（tidb-rs 不支持）；写路径（tidb-rs 只读，写 RU 只为完整性记录） |

所在阶段：读路径接入 TiKV 之后（D07.1、D07.2），会话主流程（D05.3）之前定稿；实现在 P2。

## 2. 调研：Go v8.5.8

### 2.1 资源组怎样到达每个请求

- **会话到语句**：`SessionVars.ResourceGroupName` 复制到 `StmtCtx.ResourceGroupName`（`executor/select.go:982`）。hint（`planner/optimize.go:162-191`，未知组退回会话组，`session/session.go:2113-2123`）或 watch 的 SWITCH_GROUP（`executor/adapter.go:604-606`）可以改写。
- **语句到请求**：
  - 读请求：`StmtCtx` → `DistSQLContext.ResourceGroupName`（`session/session.go:2830`）→ `kv.Request.ResourceGroupName`（`distsql/request_builder.go:371`）→ cop 请求的 `kvrpcpb.ResourceControlContext{ResourceGroupName}`（`store/copr/coprocessor.go:1325-1341`）。
  - 事务和快照：`kv.SetTxnResourceGroup`（`session/txn.go:317`）、`snapshot.SetOption(kv.ResourceGroupName)`（`executor/builder.go:2263`）→ client-go 各请求构造处（CG `txnsnapshot/snapshot.go:529`、`prewrite.go:209`、`commit.go:88`）。
- **同一条路径上的 Top SQL 标签**：`resource_group_tag` 由 `StmtCtx.GetResourceGroupTagger()` 生成（`sessionctx/stmtctx/stmtctx.go:868-875`），挂在同一个 `kv.Request` 上（`request_builder.go:361`），与组名一起写进 cop 请求（`coprocessor.go:1350-1351`）。这正是 N9 §6.4 已经决定照搬的部分。

### 2.2 限流：client-go 拦截器 + PD resource manager 客户端

- **安装**：TiDB 在 `domain/runaway.go:37` 创建 PD 的 `ResourceGroupsController`，`tikv.SetResourceControlInterceptor`（`:50`、CG `tikv/kv.go:901`）。每个 store 的 client 都包一层 `NewInterceptedClient`（CG `tikv/kv.go:314`）。
- **每个 RPC**：发出前同步调用 `OnRequestWait`，收到响应后调用 `OnResponseWait`（CG `internal/client/client_interceptor.go:45-88`）。`OnResponseWait` 出错时，已经拿到的响应被丢弃，返回错误（`:79-80`）。
- **不经过拦截器的请求**：组名为空、总开关关闭、background 请求、request source 含 `internal_others`（`client_interceptor.go:148-178`，CG `resource_control.go:46`）。
- **令牌桶**：每个进程、每个资源组一个 `Limiter`（PD `controller.go:139`、`:714`、`:913-925`）。
  - **取令牌**：一个 goroutine 每秒检查一次，把所有组合并成一次 `AcquireTokenBuckets`（`:236-428`、`:605-636`），令牌低于阈值时提前触发（`:332-338`）。每次申请 `平均 RU/s × 5 s × 1.1 − 余量`（`:1369-1379`）；PD 可以一次给足，也可以按 trickle 慢慢放，放的期间标记为 throttled（`:1222-1257`）。
  - **等待**：在发请求**之前**（`onRequestWaitImpl` → `acquireTokens`，`:1451-1483`）。按欠的令牌算出等待时间，超过 `LTBMaxWaitDuration`（默认 30 s，PD `config.go:54`）就失败（PD `limiter.go:240-244`、`:402-438`），报 `ErrClientResourceGroupThrottled` → TiDB 错误 8252（`store/driver/error/error.go:204-205`）。另有最多 20 次、每次 50 ms 的重试（PD `config.go:56-60`）。
  - **公平性**：没有。一个桶一把互斥锁，同组请求按抢锁顺序预约（PD `limiter.go:402-404`）；组与组完全独立。唯一的"优先级"只是透传：`OverridePriority` 由 TiKV 执行（PD `controller.go:1499`），client-go 的批量发送优先队列在等完令牌之后才起作用（CG `internal/client/priority_queue.go:41-42`）。
  - **欠账**：读费用在响应后扣。响应小于 4 MB 或组未被限流时允许欠账，桶可以变成负数（PD `controller.go:1552`、`limiter.go:330`）。
  - **`BURSTABLE`**：`BurstLimit < 0` 时完全跳过令牌（PD `controller.go:900-905`、`:1464`、`:1551`，`limiter.go:406`），只记录消耗。

### 2.3 RU 模型

默认常数（PD `config.go:66-83`）：读 base 1/8 + per-batch 1/2 × 0.7、读字节 1/64 KiB、写 base 1 + per-batch 1 × 0.7、写字节 1/1 KiB（乘副本数）、CPU 1 ms = 1/3 RU。

| 计费项 | 何时计 | 来源 | 位置 |
|---|---|---|---|
| 读/写 base、写字节 × 副本数 | 请求前 | 请求内容（写只算 Prewrite、Commit） | PD `model.go:81-103`，CG `resource_control.go:42-71` |
| 读字节 | 响应后 | `ScanDetailV2.ProcessedVersionsSize`，否则 cop `Data.Size()` 或整个 Scan 响应大小 | CG `resource_control.go:110-164`；PD `model.go:106-135` |
| TiKV CPU | 响应后，**只对读** | `TimeDetailV2.ProcessWallTimeNs`：TiKV 的注释说它"在大多数情况下非常接近 CPU 时间"，但包含 RocksDB 内部的 mutex 等待，所以叫墙钟；TiKV 8.5 的回包里没有真正的 CPU 字段 | PD `model.go:108`："For now, we can only collect the KV CPU cost for a read request."；CG `resource_control.go:152` TODO |
| TiDB SQL CPU | 每秒，按整个进程 | 只在 `EnableSingleGroupByKeyspace`（serverless，一个 keyspace 一个组）模式下取进程 CPU，其余情况返回 0。v8.5.8 创建控制器时只传 `WithMaxWaitDuration`，不开这个模式（`domain/runaway.go:37`），所以 on-prem 部署**无论几个组**都不计 | PD `model.go:158-165`、`:295-315`（`// TODO: Need a way to calculate in the case of multiple groups.`） |
| 写失败退款 | — | `Succeed()` 恒为 true，退款逻辑走不到 | CG `resource_control.go:177-179`，PD `model.go:110-112` |

### 2.4 runaway query

- **规则**（`resourcegroup/runaway/checker.go:45-47`）：EXEC_ELAPSED（执行开始时间 + 阈值，`:82-84`）、RU（`WRU+RRU >= 阈值`，`:406`）、PROCESSED_KEYS（所有 cop 任务之和，`:355`、`:414`）。
- **检查点**：
  - 执行前查 QUERY WATCH：SQL 文本、SQL digest、plan digest（`adapter.go:596-607`，`checker.go:322-324`）。
  - 每个 cop 请求前（`store/copr/coprocessor.go:1362-1366`）：KILL 时，把剩余时间写进 `MaxExecutionDurationMs` 交给 TiKV（`checker.go:195-200`）。
  - 每个 cop 响应后（`coprocessor.go:1410`、`:1989-1996`）：检查 RU 和 keys。
  - expensivequery 的 100 ms tick（`util/expensivequery/expensivequery.go:57`、`:109-114`）：只检查时间（`checker.go:250` 传入的 RU 为 nil，keys 为 0）。
  - 没有 plan digest 的语句不检查（`checker.go:99`）。
- **动作**：
  - KILL：报错 8253 / 8254（`checker.go:147-149`、`:210`、`:365`）。
  - COOLDOWN：之后的 cop 请求 `OverridePriority = 1`（`:186`、`:212`），cop 并发降到 1（`coprocessor.go:229-232`，注释 `:222`：cooldown 可能阻塞 SQL，issue56916）。
  - SWITCH_GROUP：watch 命中时改语句的组（`adapter.go:604-606`）；rule 触发时**只改 cop 请求**的组（`checker.go:214-217`）。
  - DRYRUN：只记录（`:153-155`）。
- **记录与同步**：`mysql.tidb_runaway_queries` 每 30 s 或满 512 条刷一次（`manager.go:46`、`:513-514`）；watch 表每 1 s 刷、每 1 s 同步到其他 TiDB（`manager.go:53`、`syncer.go:38`）；通道满时记录被静默丢弃（`manager.go:277`、`:353` TODO）。

### 2.5 background 任务与 SQL 面

- **background**：没有单独的 background 组。`default` 组的 `BACKGROUND` 选项给出任务类型和利用率上限（`ddl/resourcegroup/group.go:71-74`）；请求用 request source 标记任务类型（DDL `ddl/job_worker.go:517`，导入 `lightning/tikv/tikv.go:157`）。这些请求不在 TiDB 侧计量，交给 TiKV 记录和上报（CG `client_interceptor.go:167-172`）。
- **系统变量**：
  - `tidb_enable_resource_control`：GLOBAL，默认 ON（`sessionctx/variable/tidb_vars.go:1195`、`:1641`）。
  - `tidb_resource_control_strict_mode`：GLOBAL，默认 ON（`:1198`、`:1642`）。
  - `tidb_request_source_type`：SESSION，默认空（`:298`）。
- **系统表**：`information_schema.RESOURCE_GROUPS`、`RUNAWAY_WATCHES`（`infoschema/tables.go:214`、`:216`；后者读本节点内存中的 watch 列表，`executor/infoschema_reader.go:3824`）；`mysql.tidb_runaway_queries`、`tidb_runaway_watch`、`tidb_runaway_watch_done`（`session/bootstrap.go:639`、`:655`、`:672`）。
- **错误码**（`errno/errcode.go:1158-1166`）：8248–8255、8257。

## 3. Go 的问题（证据）

| # | 问题 | 证据 | 影响 |
|---|---|---|---|
| P1 | on-prem TiDB 从不把 SQL CPU 计入 RU（单组也是） | `domain/runaway.go:37`（不开 `EnableSingleGroupByKeyspace`）；PD `model.go:312-315`；pd#10940（serverless 下 SQL 层 CPU 计入 RU 指标却不进本地令牌桶，口径不一致） | TiDB 侧计算重的查询（大 join、聚合、排序）几乎不受 RU 限制；一个组可以占满 TiDB CPU。反过来说，现有部署的 `RU_PER_SEC` 配额和 `CALIBRATE RESOURCE` 的估算都是在"不含 SQL CPU"的口径下得出的 |
| P2 | TiKV CPU 只对读计费；口径含 RocksDB mutex 等待 | PD `model.go:108`；CG `resource_control.go:152`；tidb#42141（早期用 1 ms 粒度的 TimeDetail，已改用 V2 的 ns） | 写请求的 CPU 不计（tidb-rs 只读，影响小）；锁争用重时读的 CPU 偏大。TiKV 回包没有更准的字段，TiDB 侧无法单独修 |
| P3 | 读字节口径粗糙 | CG `resource_control.go:137`、`:143` TODO（拿整个响应大小当读字节） | RU 与实际读取量偏离 |
| P4 | 写失败退款失效 | CG `resource_control.go:177-179` | 写冲突重试被重复计费（tidb-rs 只读，影响小） |
| P5 | 限流等待在每个 RPC 之前、桶内无公平性 | PD `limiter.go:402-404`、`controller.go:1451-1483` | 一条语句的多个 cop 任务各自等待，尾延迟叠加；同组内大查询可以饿死小查询；语句已经占着运行时资源（内存、并发槽）却在 RPC 前睡眠 |
| P6 | 欠账无下限、大响应才事后等待 | PD `controller.go:1552`、`limiter.go:330` | 突发读可以把桶打成很深的负数，之后整组长时间被限 |
| P7 | `BURSTABLE` 等于不限 | PD `controller.go:1464`；tidb#54554（一个组打开 burstable 后吃掉其他组约 100K RU/s） | 打开 burstable 的组没有上限 |
| P8 | runaway 的判定粒度粗、生效晚 | (1) EXEC_ELAPSED 由 expensivequery 每 100 ms 检查（`expensivequery.go:57`），能发现，但 100 ms 的粒度会漏掉刚过期就结束的语句：tidb#69254（阈值 200 ms、查询 220 ms，时有时无）。(2) KILL 只是发信号（`server.go:965-966`），要等执行器下一次检查 kill 标志才生效：tidb#70333（300 s 的 KILL 拖到 419 s）。(3) RU 和 PROCESSED_KEYS 只在 cop 响应时检查（`coprocessor.go:1989-1996`，tick 传入的 RU 为 nil，`checker.go:250`） | 刚超阈值的语句漏记；KILL 延迟取决于执行器的检查间隔；TiDB 侧计算累计的 RU（若 D1 计入）不会触发 RU 规则 |
| P9 | COOLDOWN（rule 或 QUERY WATCH 触发）和 rule 触发的 SWITCH_GROUP 只作用于 cop 请求 | COOLDOWN 只在 `BeforeCopRequest` 里设置 `OverridePriority`，watch 命中也一样（`checker.go:184-187`，注释"Other watch actions should already been handled in `BeforeExecutor`"，即 COOLDOWN 没有在执行前处理）；rule 的 SWITCH_GROUP 见 `checker.go:214-217` | 点查、批量点查、事务请求仍按原组计费、原优先级执行 |
| P10 | runaway 记录与 watch 同步可能丢失 | `manager.go:277`、`:353`（通道满静默丢弃）；tidb#70036（watch 刷新出错会静默关闭跨节点隔离）；tidb#67772（`tidb_runaway_watch_done` 无限增长） | 隔离不可靠、系统表膨胀 |
| P11 | 令牌申请的 trickle 实现 | pd#11317（截止后客户端继续按 trickle 速率加 PD 没给的令牌）；pd#10985（写消耗上报时机） | 实际速率超过配额 |

## 4. tidb-rs 设计

### 4.1 原则

1. **协议和兼容面不变**：与 PD resource manager 的 `AcquireTokenBuckets` 协议、`ResourceControlContext` 字段、错误码、系统表都照 Go。
2. **RU 模型的公式和常数默认照 Go**：PD 侧的配额（`RU_PER_SEC`）是按这个模型标定的，`CALIBRATE RESOURCE` 也按它估算。改进的是"计量什么、什么时候计、在哪里等"。凡是改变用户可观察到的限流结果的地方，都列为负责人决定（§6）。
3. **计量、观测、限流共用一条路径**：
   - 语句和任务级的状态（组名、累计 RU、runaway 检查器）放在 N9 的 `ObsCtx` 里，随 task 继承。
   - 请求级的组名、标签随请求上下文传递，同 N9 §6.4 的 `resource_group_tag`。
   - KV 请求完成时只有一个钩子，同时写 N9 的执行统计和 RU。

### 4.2 计量

| 计费项 | tidb-rs 做法 | 相对 Go |
|---|---|---|
| KV 请求 base、字节 | client-rust fork 的请求完成钩子（N9 §3.3 的同一钩子），公式照 PD `model.go` | 同 Go |
| TiKV CPU | 照 Go 用 `TimeDetailV2.process_wall_time_ns`（TiKV 8.5 回包中最接近 CPU 的字段，已核对 kvrpcpb） | 同 Go；P2 需要 TiKV 回包加字段，不在本 spec 范围 |
| TiDB SQL CPU | N9 §6.1 的窗口分摊已经把 CPU 记到每个任务的 `ObsCtx`；同一份数字按语句的资源组累计，乘 `CPUMsCost` 记为 RRU | 修 P1。**默认只计入指标**（上报和 `information_schema`），不进本地令牌桶；进入限流由显式开关控制，见 D1 |
| 写失败退款 | 按响应真实结果判断成功，失败时退款 | 修 P4（tidb-rs 目前只读，主要为完整性） |
| background 请求 | 照 Go 不在 TiDB 侧计量 | 同 Go |

**开销**：KV 计费是每个请求的几次加法。SQL CPU 计费复用 N9 窗口分摊的结果，不另外读时钟；按 N9 §6.1b 的测量，窗口方案每次 poll 的上界约 0.7%，RU 累计只多一次任务局部加法。

### 4.3 限流：语句级准入 + 加权公平调度

**令牌桶**：resource manager 客户端（Rust 新写，相当于 PD `controller.go` / `limiter.go`）。每个组一个桶，后台一个任务批量申请令牌、上报消耗，协议照 Go。同时修 P11 的 trickle 截止问题，不加 PD 没给的令牌。

**在哪里等**：Go 是每个 KV RPC 发出前睡眠（P5）。tidb-rs 分三层，**只有第 3 层（RPC 层）真正扣令牌**，前两层只读桶的状态（余量、是否欠账、是否在 trickle 中）做调度决定：

1. **语句准入**（N3 的 Q1 准入队列，`crates/runtime/src/admission.rs`）：语句开始执行前按资源组排队。组的桶为负或欠账超过下限时，新语句在这里等，而不是执行到一半在 RPC 前睡眠。被限流的语句不占并发槽和内存预算。等待超过 `LTBMaxWaitDuration`（30 s）时报 8252。Go 只在 RPC 层报 8252，这是用户可见的变化，见 D6。
2. **任务调度**（N3 运行时）：已经在执行的语句，任务按资源组加权公平调度，权重为 `RU_PER_SEC`，同组内 FIFO，PRIORITY 高的组优先。这一层分配的是 **TiDB 的 CPU**（poll 的机会），不是 TiKV 的 RU。超出配额的组，其任务被延后 poll，而不是阻塞线程。
3. **RPC 层**：照 Go 在每个请求前预约令牌、响应后结算，计量和最长等待（30 s 后报 8252）与 Go 相同。正常情况下第 1、2 层已经让请求的速率接近配额，这一层很少真的等待。

**优先级反转**：第 2 层延后 poll 一个组的任务时，如果这个任务持有别的组正在等的资源，别的组也会被拖慢。例如 R12 合并请求的 leader：其他组的语句正等着它的 ResolveLock 结果。缓解：持有共享资源的任务（合并请求的 leader、后台清锁、schema 同步等运行时内部任务）不归属任何资源组，不参与按组延后。见 §8。

`BURSTABLE`：照 Go 不受本组上限限制，但在第 2 层仍参与加权公平调度，不会饿死其他组（P7）。这改变了 burstable 组与其他组之间的相对速度，见 D2。

欠账（P6）：允许欠账，但设下限（例如一个上报周期的配额）。超过下限后，第 1 层不再放行新语句，直到还清。见 D3。

### 4.4 runaway

- **检查点**：
  - N3 的 `checkpoint`：每个 chunk、每个时间片（默认 1 ms）调用，已经承担 KILL 检查。
  - poll 包装器：每次 poll 结束时检查 EXEC_ELAPSED。
  - RU 与 PROCESSED_KEYS：在 KV 请求完成钩子里累计，超过阈值时设置语句的 runaway 标志，下一个 checkpoint 生效。

  修 P8：EXEC_ELAPSED 在每次 poll 结束时比较截止时间，不受 100 ms 粒度影响；KILL 走取消令牌，在下一个 checkpoint（不超过一个时间片，默认 1 ms）生效；RU 和 PROCESSED_KEYS 在每个 KV 请求完成时、以及 SQL CPU 刷出时（若 D1 打开限流开关）检查，不依赖 cop 响应。
- **KILL**：走 N3 的取消令牌（与 `KILL QUERY` 相同的路径），错误码照 Go（8253 / 8254）。仍按 Go 把剩余时间写进 cop 请求的 `max_execution_duration_ms`，让 TiKV 也能及时停止。
- **COOLDOWN / SWITCH_GROUP**：作用于**语句的全部请求**，包括点查、批量点查，以及语句发起的事务请求。组名和优先级放在语句的请求上下文里，生效后对之后发出的所有请求都适用（修 P9）。这是用户可见的行为差异，见 D4。
- **QUERY WATCH**：匹配规则照 Go（SQL 文本、SQL digest、plan digest）。watch 列表同步照 Go 每 1 s 一次。写入 `mysql.tidb_runaway_*` 改为有界队列：满时计数并在日志和指标中报出，不再静默丢弃（P10）。`watch_done` 的清理见 D5。

### 4.5 与 N9 的关系（同一条路径）

| 数据 | N9 | N10 |
|---|---|---|
| 语句的 SQL digest / plan digest | Top SQL 键、语句记录 | QUERY WATCH 匹配、runaway 记录 |
| 任务级 CPU（窗口分摊） | `Tidb_cpu_time`、Top SQL | SQL CPU → RRU（§4.2） |
| KV 请求完成钩子 | `ExecDetails`、RPC 统计 | KV RU、PROCESSED_KEYS |
| 请求上下文 | `resource_group_tag`、`request_source` | `resource_group_name`、`override_priority`、`penalty` |
| `ObsCtx`（任务局部） | Top SQL 键、CPU 累计 | 组名、RU 累计、runaway 检查器 |
| checkpoint / poll 包装器 | 时间片、停顿检测、CPU 窗口 | runaway 检查 |

N9 的采样率（§6.1a）只决定 Top SQL 是否聚合。资源管控的 RU 计量**不受采样率影响**：未采样的语句同样累计 SQL CPU。窗口分摊本来就要读线程 CPU 时钟，所以开销不变。实现时要注意：N9 只对采样的任务做分摊。若 D1 选择计入 SQL CPU，资源管控开启时需要对所有任务做分摊，开销按 N9 §6.1b 的 100% 档计算，上界 0.7%。

## 5. 兼容性

| 项 | 与 Go 相同 | 与 Go 不同（用户可见） |
|---|---|---|
| DDL、hint、`SET RESOURCE GROUP`、`CURRENT_RESOURCE_GROUP()`、`QUERY WATCH`、`CALIBRATE RESOURCE` | 语法、选项、权限检查、错误 | — |
| 系统变量、系统表、`mysql.tidb_runaway_*` | 名字、类型、默认值、列 | — |
| 错误码 8248–8255、8257 | 相同 | 8252（Throttled）在语句准入层等待超时时也会出现，Go 只在 RPC 层出现（D6） |
| RU 数值 | KV 部分公式和口径相同 | 指标中多出 SQL CPU 的 RRU（默认只计入指标）；开关打开后也参与限流（D1） |
| 限流结果 | 单组、无 burstable、无 TiDB 侧重计算时基本相同 | 同组内更公平；burstable 组不再挤占其他组（D2）；欠账有下限（D3） |
| runaway | 规则、动作、记录格式相同 | 发现得更早（TiDB 侧计算也能发现）；COOLDOWN / SWITCH_GROUP 作用于语句全部请求（D4） |

这些差异写进 `docs/compatibility.md`（实现时）。

## 6. 需要负责人决定

| # | 问题 | 选项 | 建议 |
|---|---|---|---|
| D1 | TiDB SQL CPU 是否计入 RU | (a) 照 Go 完全不计；(b) 计入 RRU，只用于指标和 `information_schema`，不参与限流；(c) 计入并参与限流 | **默认 (b)，(c) 作为显式开关**（例如新 GLOBAL 系统变量，名字待定，默认关）。理由：on-prem 的 Go 从不计 SQL CPU（P1），现有部署的 `RU_PER_SEC` 都是在这个口径下设的，直接改成 (c) 会改变**所有** on-prem 部署（单组也一样）的 RU 含义。`CALIBRATE RESOURCE` 在 (c) 下的含义：它按 TiKV 侧的消耗估算集群容量（不含 TiDB CPU），打开 (c) 后同一负载每秒消耗的 RU 更多，估算出的容量需要按 SQL CPU 占比调高，或者 `CALIBRATE` 也把 TiDB 节点的 CPU 计入容量；开关文档要说明这一点，具体做法在实现前定 |
| D2 | `BURSTABLE` 组是否参与加权公平调度 | (a) 照 Go 完全不限；(b) 不受本组上限限制，但调度时与其他组按权重公平 | (b)，回应 tidb#54554 |
| D3 | 欠账下限 | (a) 照 Go 无下限；(b) 下限为一个上报周期（5 s）的配额，超过后语句准入暂停 | (b) |
| D4 | rule 触发的 COOLDOWN / SWITCH_GROUP 的作用范围 | (a) 照 Go 只作用于 cop 请求；(b) 作用于语句之后发出的全部请求 | (b)：Go 的行为更像实现限制，不是设计 |
| D5 | `mysql.tidb_runaway_watch_done` 的清理 | (a) 照 Go 不清理；(b) 按 `tidb_runaway_queries` 的 7 天规则一并清理 | (b)，回应 tidb#67772 |
| D6 | 语句准入层的限流（§4.3 第 1 层） | (a) 不要第 1 层，只在 RPC 层等待（同 Go）；(b) 要第 1 层，等待超时同样报 8252 | (b)：被限流的语句不再占着并发槽和内存等在 RPC 前。用户可见的变化是 8252 可能在语句开始前出现（Go 只在 RPC 时出现），错误码和文字不变 |

## 7. 验收

1. **兼容面**：tools/ab 对资源组 DDL、`QUERY WATCH`、系统变量、系统表、错误码做 Go/tidb-rs 对照，结果相同。
2. **计量**：同一负载下，tidb-rs 报给 PD 的 KV 部分 RU 与 Go 相差 ≤ 2%。SQL CPU 部分单独列出（D1）。
3. **限流**：
   - 两个组，配额 1:3，饱和负载下实际 RU 比例为 1:3 ± 5%。
   - 同组内一个大查询加多个点查时，点查的 p99 延迟不超过单独运行时的 2 倍（Go 同场景作对照）。
   - burstable 组不使其他组掉到配额以下（D2）。
4. **runaway**：
   - EXEC_ELAPSED 为 1 s 的 KILL 规则，对只在 TiDB 侧计算的查询（大 hash join，所有 cop 请求很早就完成），在 1 s + 50 ms 内终止。Go 同场景作对照（tidb#69254）。
   - COOLDOWN 之后的点查带低优先级（D4）。
5. **开销**：资源管控开启、无限流时，sysbench oltp_read_only 的 CPU / 查询相对关闭时增加 ≤ 1%，测量方法同 N9 §6.1b。

## 8. 风险

- **resource manager 客户端是新写的**：client-rust 没有对应实现，令牌申请、trickle、上报周期的细节多，PD 侧也在变（pd#10940、#10985、#11317）。缓解：先照 Go 的协议和测试移植一版，再做改进；与 PD 的交互用集成测试覆盖。
- **语句准入层限流可能让短语句排在长语句之后**：在第 1 层按组分队，组内 FIFO，并对超时报 8252；验收第 3 项专门测。
- **第 2 层的优先级反转**（§4.3）：被延后的任务若持有其他组在等的资源（R12 合并请求的 leader、后台清锁、锁等），会拖慢其他组。缓解：运行时内部任务和合并请求的 leader 不归属资源组；实现时用一个测试覆盖"低配额组持有合并请求的 leader，高配额组等待其结果"。
- **D1 选 (c) 后与 Go TiDB 混部**：同一集群中 Go 和 tidb-rs 对同一负载的 RU 计数不同，配额消耗速度不同。这需要在文档中说明，或只在纯 tidb-rs 集群中启用。

## 9. 与其他 spec 的关系

- **N3**：准入队列（Q1）、运行时调度、checkpoint、取消令牌。本 spec 在其上加按组的排队与加权。
- **N9**：`ObsCtx`、窗口分摊的 CPU、KV 请求完成钩子、请求上下文（§4.5）。
- **#101（D07.1 lock resolver）**：解析锁的 RPC 也带资源组名（Go 的 `ResourceGroupNameFromCtx`），计入语句的 RU；lock-resolver.md R11 一并处理。
- **D07.2（coprocessor 客户端）**：在 cop 请求上设置 `ResourceControlContext`、`max_execution_duration_ms`。
