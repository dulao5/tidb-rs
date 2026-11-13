# D07.1：client-go 对等对照表（client-go ↔ client-rust fork）

对应 #128 第 1 项。client-rust 替代 client-go 是项目中最大的一项 L3 同源替换（宪章第二条 3），本表是 [ADR 0008](../../docs/adr/0008-client-rust-replaces-client-go.md) 的依据。写法同 [lock-resolver.md](lock-resolver.md)：只做对照，不改代码。修复按本表的优先级排进 #101 的后续或新的 issue。锁解析（R1–R12）已在 lock-resolver.md 中处理，这里不再重复。

## 0. 对照基准

| 对象 | 版本 |
|---|---|
| client-go | `c3b5079`（TiDB v8.5.8 所用，tracker `govec/go.mod`） |
| TiDB | v8.5.8（`v1.1.0-beta.0.20260827064614-8b857efa2036`），用于 TiDB 侧的调用方和系统变量 |
| client-rust | `dulao5/client-rust` 分支 `tidb-rs/metrics` @ `94442ea`（含 R1–R6、R12） |
| tidb-rs 侧配置 | PoC `crates/poc/kv/src/lib.rs`：`tidb_retry_options()`（region 退避 48 次，约 20.5 s；lock 退避 16 次）、`--tikv-connections`（默认 4）。超时和消息上限未设置，用 client-rust 默认值 |

路径记法：`G` = client-go 的 `internal/locate/`，其他 client-go 路径写全；`R` = client-rust 的 `src/`；TiDB 路径以 `pkg/` 开头。

优先级：**P0** = D07.1 / D07.2 的读路径在真实集群上可用之前必须修；**P1** = 进入 P2 阶段（性能、运维）之前修；**P2** = 有需要时再做。

影响分类：**正确性**（返回错误或结果不对）、**可用性**（Go 能扛过去的故障下 tidb-rs 报错）、**性能**、**运维**。

## 1. 先说结论

1. **三项 P0，都是"Go 能扛过去、tidb-rs 报错"的可用性问题**，详见 §9：
   - **P0-1 退避预算**。点查、max-ts 点查和锁解析用的是固定的 10 次退避（约 1.5 s，所有错误共用一条曲线）；Go 是 20 s 的时间预算，每种错误一条曲线。ServerIsBusy 2 ms 后就重试，Go 等 2–10 s，而且这部分睡眠最多 10 分钟不计入预算（`config/retry/config.go:144`），TiKV 持续繁忙时 Go 会一直等。leader 切换、store 宕机、TiKV 繁忙时，点查约 1.5 s 后报错。
   - **P0-2 KV RPC 超时 2 s**。Go 是 30 s / 60 s。负载高时大的 BatchGet / Scan 会误报超时。
   - **P0-3 gRPC 接收消息上限 4 MiB**。Go 没有上限。宽行表的 coprocessor 响应、大的 BatchGet 会直接失败。
2. **最大的结构差异**：
   - 退避按次数而不是按时间计（§4）。
   - 没有 replica read（follower / closest-replica）、没有 region bucket、没有 store 存活状态（§2、§3）。
   - 没有 BatchCommands 流（§5）。
   - 没有把超时告诉 TiKV（`max_execution_duration_ms`，§4）。

   这些都是性能和运维问题，正确性上没有发现会让读结果不同的缺口。
3. **工作量**：client-go 相关目录的 Go 代码约 1.7 万行（`internal/locate` 7967、`internal/client` 3306、`txnkv/txnsnapshot` 2286、`txnkv/txnlock` 1479、`oracle/oracles` 1133、`config/retry` 615、`internal/resourcecontrol` 180，不含测试）。client-rust 对应部分约 5.7 千行。P0 三项改动小（配置和一处 plan 参数，加测试）；P1 中最大的是按时间计的退避和 store 存活状态；replica read 和 BatchCommands 各是一个独立的中等项目。见 §10 与 ROADMAP。

## 2. region cache

| client-go（Go 行为） | client-rust 现状 | 缺口 | 影响 | 优先级 |
|---|---|---|---|---|
| B 树加按版本、按 id 的映射（`G/region_cache.go:79`、`:540`） | `BTreeMap` 加 HashMap，一把 tokio `RwLock`（`R/region_cache.rs:29-61`、`:184`）；按 key 的并发加载不去重（按 id 的有 `on_my_way_id`，`:100-171`） | 冷启动或热点 region 刚失效时，同一 region 被并发地向 PD 请求多次 | 性能 | P2 |
| 范围查询一次 PD 调用批量加载多个 region（`BatchLoadRegionsWithKeyRanges`，`G/region_cache.go:1876`） | 每个 region 一次 PD 调用，串行（`R/pd/client.rs:126-151`） | 无批量加载 | 性能：大表第一次扫描的拆分延迟高 | P1 |
| 缓存的 region 有 600 s（加最多 60 s 抖动）的空闲过期（`G/region_cache.go:141-187`、`checkRegionCacheTTL` `:453`），后台每秒回收 50 个（`:2814`） | 没有过期：`// TODO: does it need TTL?`（`R/region_cache.rs:32`） | 只在出错时失效 | 运维：内存只增不减；leader 迁走后要靠一次失败的请求才发现 | P1 |
| EpochNotMatch：TiKV 落后时 BoRegionMiss 退避；否则直接用错误里的 `CurrentRegions` 更新缓存，不访问 PD（`G/region_cache.go:2636-2700`） | 同样判断"落后"；否则一律失效、退避、再从 PD 加载（`R/request/plan.rs:1023-1057`） | 不用 `CurrentRegions` | 性能：region 频繁分裂时，每次分裂都多一次退避和一次 PD 调用 | P1 |
| NotLeader 带 leader 提示：切换并立即重试；提示的 peer 不在 region 里则失效（`G/region_cache.go:1924-1953`） | 更新 leader 并立即重试（`R/request/plan.rs:959-970`）；不检查 peer 是否属于 region（`R/region_cache.rs:225-238`） | 少一个检查 | TiKV 会拒绝错误的目标，正确性不受影响；多一次重试 | P2 |
| NotLeader 不带提示：BoRegionScheduling 退避后换一个 peer（`G/replica_selector.go:481-484`） | 失效 region，退避（`R/request/plan.rs:971-977`） | 每次选举都回 PD | 性能：选举期间 PD 负载 | P2 |
| StaleCommand：不退避、不失效，换下一个 peer（`client-go/internal/locate/region_request.go:1969-1981`） | 失效加退避（`R/request/plan.rs:987`） | 多一次 PD 调用 | 性能 | P2 |
| KeyNotInRegion：失效 region，不退避（`region_request.go:1910-1914`） | 落入兜底分支：失效 region **和 store**、丢弃 gRPC 连接、退避（`R/request/plan.rs:1006-1016`） | 不必要地丢掉 store 和连接 | 性能、运维 | P1 |
| ReadIndexNotReady、ProposalInMergingMode、RegionNotInitialized：退避后重试同一 peer（`region_request.go:2013-2051`） | 兜底分支（同上） | 分裂、合并期间反复重建连接 | 性能、运维 | P1 |
| MaxTimestampNotSynced：BoMaxTsNotSynced 退避后重试（`region_request.go:2003-2010`） | 立即失败（`R/request/plan.rs:1004`） | 不重试 | 无：TiKV 只在 prewrite（写路径）返回这个错误，tidb-rs 只读 | P2 |
| FlashbackInProgress / NotPrepared：立即失败（`region_request.go:1870-1897`） | 兜底分支，重试到预算用完 | 失败得慢 | 小 | P2 |
| region bucket：加载（`G/region_cache.go:2134`）、`LocateBucket`（`:1197`）、`UpdateBucketsIfNeeded`（`:2771`），用于按 bucket 拆 cop 任务 | 无 | cop 任务不能按 bucket 拆 | 性能：大 region 上并行度低（D07.2） | P2 |
| store 后台维护：定期重新解析 store 地址、标签（`client-go/internal/locate/store_cache.go:485-534`）；每 10 s 刷新 store 列表（`G/region_cache.go:785`） | 无，只在出错时丢弃并按需重新加载（`R/request/plan.rs:500`、`:982`） | store 地址变化要等一次失败才更新 | 运维：k8s 中 TiKV pod 重新调度后，第一批请求失败或变慢 | P1 |
| store 存活状态（reachable / unreachable / unknown）与健康检查循环（`store_cache.go:603-686`） | 无；只按 gRPC 状态码分类（`R/request/plan.rs:331-368`） | 每个请求自己重新发现宕机的 store | 性能、可用性：每个新请求都要先等一次对宕机 store 的连接超时。**实测（`82fd763` 的 kill_tikv）**：tidb-rs 有 2 次读在约 20 s 时用完预算报错，Go 0 次。诊断（Go 慢日志，那两条 27.8 s 的语句）表明主因**不是**存活状态，而是预算只有 Go 的一半：Go 的退避共 27.6 s、38.2 s，其中 tikvRPC 只占 0.6 s，见 P0-1 一行的 backoff weight。存活状态在 Go 里只省掉了对死 store 的 tikvRPC 退避（每条语句不到 1 s） | P1（维持；诊断后不再升 P0） |
| — | `read_through_region_by_id` 在 PD 调用失败时提前返回，留下 `on_my_way_id` 条目，之后的调用方会一直等（`R/region_cache.rs:150-160`、`:116-118`） | client-rust 自身的缺陷（依据代码推断，未复现） | 正确性：挂起。唯一调用方是 raw client 的 `store_for_id`（`R/raw/client.rs:821`），不在事务读路径上 | P2，但改动小，与 P0-1 一起修（§9） |

## 3. 副本选择与故障切换

| client-go | client-rust | 缺口 | 影响 | 优先级 |
|---|---|---|---|---|
| `ReplicaReadType`：Leader / Follower / Mixed / Learner / PreferLeader（`pkg/kv` 的 `store_vars.go:60-71`）；TiDB 的 closest-replica、closest-adaptive 映射为带标签匹配的 Mixed（`G/replica_selector.go:181-190`、`:393-432`）；系统变量 `tidb_replica_read` | 只读 leader；`replica_read` 除测试外从不设为 true。所谓 follower 尝试只是把 leader 请求发给另一个 voter（`R/request/plan.rs:370-388`） | 不支持 `tidb_replica_read`：设成 follower 等值时 tidb-rs **默默读 leader**（已写进 `docs/compatibility.md`；修复项包括 SET 时给 warning） | 性能：读负载全落在 leader 上，跨 AZ 读没有节省；正确性无影响；运维：用户以为分流了实际没有 | P1（若 D07.1 / D07.2 需要 follower 或 closest 读则为 P0） |
| 短读超时之后的重试（`tikv_client_read_timeout` 的本意是"leader 慢就去 follower 读"）：第一次尝试用短超时；TiKV 到期返回 ServerIsBusy "deadline is exceeded" 或 gRPC DeadlineExceeded 时，不退避（`region_request.go:1941-1946`、`:1614`），给 leader 打上 `deadlineErrUsingConfTimeoutFlag`（`replica_selector.go:580-588`），之后 leader 不再是候选（`:242`），请求改发 follower 并设 `ReplicaRead = true`（`:162-165`，ReadIndex，仍线性一致） | client-rust#15 之后：不退避，立即重试，第二次用默认超时；但没有 replica read，**仍发回原 leader** | leader 真的慢时，tidb-rs 第二次仍等在这个 leader 上（最长 30 s / 60 s），Go 换到 follower 读 | 性能 / 可用性：该变量的主要用途失效；正确性无影响 | P1，与上一行（replica read）共用一块实现。tidb-rs 接入这个变量前，要么等 replica read 做完，要么在 `docs/compatibility.md` 写明"只缩短第一次尝试"（已写） |
| leader 不可用：`isLeaderCandidate` 在 leader 的 store 不可达、遇到 NotLeader / 超时、或已尝试 10 次 / 50 s 时跳过它（`G/replica_selector.go:233-248`、`region_request.go:709-713`），再按评分选副本；可经 follower 转发（`G/replica_selector.go:129-138`、`:436-461`） | 先试缓存的 leader，再按顺序试每个 voter（不含 learner、witness）（`R/request/plan.rs:558-680`）。Unavailable 等错误失效 store 并丢弃连接；DeadlineExceeded 换下一个 peer（`:331-368`、`:536-549`） | 没有存活记忆，没有转发 | 性能：PD 报告新 leader 之前，每个新请求都重复一次对宕机 leader 的超时 | P1 |
| 按负载选副本：`busyThreshold`、store 估计等待时间（`G/replica_selector.go:142-156`、`:339`） | 无。两次 `ServerIsBusy(0)` 后的一次性 probe 只能发现 leader 变化（`R/request/plan.rs:295-303`） | 无 | 性能：热点下 | P2 |
| stale read：第二次尝试回退到 leader（`G/replica_selector.go:170-179`） | 无 stale read | 无 | 只在 D07 需要 `AS OF TIMESTAMP` 时相关 | P2 |
| 发送失败：存活检查后 `invalidateReplicaStore`，BoTiKVRPC 100 ms–2 s 等抖动退避（`region_request.go:1599-1685`）；所有 peer 都失败后重新加载 region（`:1694-1719`） | 一轮全部失败后失效 region（`R/request/plan.rs:621-643`、`:912-922`），用同一条 2 ms 起步的预算（`:924-943`） | RPC 退避太短，见 P0-1 | 可用性 | **P0（P0-1）** |

## 4. 退避、超时与语句截止时间

| client-go / TiDB | client-rust | 缺口 | 影响 | 优先级 |
|---|---|---|---|---|
| `Backoffer` 按总睡眠毫秒数（`maxSleep`）计预算，各种退避共享预算、各自维护指数状态（`client-go/config/retry/backoff.go:63`、`:146`、`:188-195`）；`Fork` / `Clone`（`:259`、`:278`）；ServerBusy 的睡眠不计入预算（`config/retry/config.go:144`） | `Backoff` 按次数计（`R/backoff.rs:21-35`），无总预算、无排除、无 fork | 模型不同 | 运维：重试时间只能按次数推算，与 TiDB 的配置无法对应 | P1 |
| 每种退避一条曲线（`config/retry/config.go:122-140`）：RegionMiss 2/500 ms；TiKVRPC 100/2000 ms 等抖动；TxnLock 100/3000 ms；ServerBusy 2000/10000 ms；StaleCmd 2/1000 ms 等 | region 错误、ServerIsBusy、gRPC 错误共用一条（`R/request/plan.rs:686-692`、`:891-900`、`:927-939`）；锁退避见 R3 / R7 | ServerIsBusy 2 ms 后重试，Go 等 2–10 s | 可用性：对过载的 TiKV 形成重试风暴，预算很快用完 | **P0（P0-1）** |
| 预算：Get / BatchGet / Scan 20000 ms（`txnkv/txnsnapshot/snapshot.go:183`、`:698`，`scan.go:115`）；coprocessor 每个 region `CopNextMaxBackoff` 20000 ms（`pkg/store/copr/coprocessor.go:72-73`） | BatchGet、Scan、coprocessor 用调用方的 `RetryOptions`（tidb-rs 设为 48 次，约 20.5 s）。**`Transaction::get` 和 max-ts 点查写死 `DEFAULT_REGION_BACKOFF`**（10 次，约 1.5 s，`R/transaction/transaction.rs:185`、`:1212`），锁解析的 CheckTxnStatus 等也是（`R/transaction/lock.rs`） | 点查只有约 1.5 s | 可用性 | **P0（P0-1）** |
| `tikv_client_read_timeout`（v8.5.8 中的名字，默认 0，`pkg/sessionctx/variable/sysvar.go:1733`）：设置每个 RPC 的超时，并把 `MaxExecutionDurationMs` 告诉 TiKV（`snapshot.go:630-635`、`:844-849`；copr `coprocessor.go:1353-1356`）。未设置时由 RPC 超时填入（`region_request.go:1393-1397`）。读超时后换副本重试（`:1614-1618`） | 源码中没有 `max_execution_duration`；所有 KV RPC 用一个 `Config.timeout`，默认 **2 s**（`R/config.rs:34`、`R/store/request.rs:116`）；coprocessor 60 s（`R/transaction/coprocessor.rs:80`） | 没有每语句的读超时；TiKV 不知道截止时间；2 s 默认值远低于 Go 的 30 s（Get）/ 60 s（BatchGet、Scan、cop）（`client-go/internal/client/client.go:87-88`；Get `snapshot.go:844`，BatchGet `:630`，Scan `scan.go:265`） | 可用性（P0-2）；性能：TiKV 继续执行客户端已经放弃的请求 | **P0（P0-2）**；`max_execution_duration_ms` 与读超时为 P1 |
| `max_execution_time`：expensivequery 看门狗 kill 会话、取消 context，`Backoff` 检查 `ctx.Done`（`pkg/util/expensivequery/expensivequery.go:96-99`） | 无截止时间接口；只能由调用方丢弃 future 取消（`R/transaction/coprocessor.rs:243`） | 需要 SQL 层（N3 取消令牌）自己包 | 由 tidb-rs 的 N3 承担 | P1 |

## 5. BatchCommands 与连接

| client-go | client-rust | 缺口 | 影响 | 优先级 |
|---|---|---|---|---|
| BatchCommands 双向流：`MaxBatchSize > 0` 且目标是 TiKV 时启用，承载 Get / BatchGet / Scan / Cop（`client-go/internal/client/client.go:309`、`:588`、`:622-625`；发送循环 `client_batch.go:610-664`）。默认 MaxBatchSize 128、OverloadThreshold 200、BatchWaitSize 8、GrpcConnectionCount 4（`config/client.go:51`、`:155`、`:170-174`）；TiDB v8.5.8 默认开启（`pkg/config/config.go:1047`） | 每个请求一次 unary gRPC；没有 BatchCommands 代码。连接数默认 1，tidb-rs 设为 4（`R/config.rs:36`、`R/store/client.rs:37-62`） | 无请求批处理 | 性能：高并发下每个请求的 HTTP/2 帧和系统调用更多，TiKV 侧占用更多 gRPC 处理。**可能与"16 线程时性能持平而非领先"有关（推测，未测量）**：要用 16 / 32 / 64 线程的 sysbench 加 TiKV gRPC CPU 指标验证 | P1（先测量）；实现为独立项目 |
| gRPC 接收消息上限 `math.MaxInt64-1`（`client.go:82`、`:318`） | 4 MiB（`R/config.rs:35`、`R/store/client.rs:56`） | 超过 4 MiB 的响应解码失败 | 正确性 / 可用性 | **P0（P0-3）** |
| HTTP/2 keepalive：每 10 s ping，3 s 超时（`config/client.go:156-157`、`client.go:343-346`） | `tcp_keepalive(10s)` + `keep_alive_timeout(3s)`，但没有 `http2_keep_alive_interval`（`R/common/security.rs:172-173`）。tonic 中没有 interval 时 timeout 不起作用（推测） | 没有 HTTP/2 ping | 运维：半开连接只能靠 RPC 超时或系统 TCP keepalive 发现。#132 断网 10 s 时，tidb-rs 的 cop 读比 Go 多挂起约 3 s（13.3 s 对 10.4 s），点查相同；**是否由此引起，原因待查**（另一种解释是 TCP 重传退避），验证方法见 `fault-scenarios.md` | P1 |
| 拨号超时 5 s，重连退避 100 ms→3 s（`client.go:86`、`:335-342`） | 连接超时 = `Config.timeout` = 2 s（`R/store/client.rs:54`） | 拨号超时更短 | 运维：慢链路或 TLS 握手时误判 store 故障 | P2 |
| 初始流和连接窗口 128 MiB（`config/client.go:48-49`） | tonic / hyper 默认值（推测为几 MB） | 流控窗口小 | 性能：高延迟链路上的大 cop 响应 | P2 |

## 6. coprocessor paging

| client-go / TiDB | client-rust | 缺口 | 影响 | 优先级 |
|---|---|---|---|---|
| `tidb_enable_paging` 默认开；`tidb_min_paging_size` 128、`tidb_max_paging_size` 50000（`pkg/sessionctx/variable/tidb_vars.go:1393-1394`、`:1565`）；页大小每轮翻倍，上限至少 50000（`pkg/util/paging/paging.go:28`、`:34-46`）；index lookup 关闭 paging（`pkg/executor/distsql.go:1093`）；`LimitSize` 小于页大小时关闭（`pkg/store/copr/coprocessor.go:436-441`）；剩余范围按响应的 `Range` 重算（`:1521-1535`） | 可选的 `paging(min, max)`，默认关（`R/transaction/coprocessor.rs:105`、`:137`、`:167-170`）；翻倍到调用方给的上限（`:717-725`）；`calculate_remain` 支持正反序（`:827`）；region 错误时重新拆分剩余范围（`:757-763`） | 默认关；无 50000 的下限、无按 LIMIT 关闭 | 性能（SQL 层忘了打开时，TiKV 整 region 扫描、内存尖峰）。行为本身一致。**P0-3 修复（client-rust#10）去掉 4 MiB 接收上限之后，paging 也是内存的上限**：一个响应的全部字节在内存跟踪看到之前就已经在 tonic 的缓冲区里，不开 paging 时一个 region 的整段扫描就是一个响应（Go 同样不设上限，靠默认开的 paging 限住单个响应） | **P1**（原 P2）：tidb-rs 的 coprocessor 像 Go 一样默认开 paging（`tidb_enable_paging`、min 128 / max 50000、index lookup 与小 LIMIT 关闭）。实现排进 D07.2 |

## 7. RU 拦截器

见 [N10](../exceptions/N10-resource-control/spec.md) §2.2–§2.3、§4。client-rust 没有资源管控拦截器：

- 请求不带 `ResourceControlContext`（组名、priority、penalty）；
- 不向 PD resource manager 申请令牌，也不上报消耗。

这是功能缺失，不影响读结果。设计和优先级由 N10 和 block issue #129 决定，这里不重复。

## 8. 其他（与本表相邻、已在别处处理）

| 项 | 位置 |
|---|---|
| 锁解析（ForRead、后台清锁、TTL 退避、shared lock、lite、缓存、请求合并、低精度时间戳；锁等待已按睡眠时间计预算） | lock-resolver.md R1–R12，client-rust#1–#9 |
| `resource_group_tag`、`request_source` | N9 §6.4 |
| 低精度时间戳、stale read、`tidb_low_resolution_tso` | [N11](../exceptions/N11-cluster-readts/spec.md) §4.4；组件 `pd::LowResolutionTs` 已在 R7 加入 |

## 9. P0 的复现方法与修复范围

| # | 问题 | 复现方法 / 真实集群上的触发条件 | 建议修复范围 |
|---|---|---|---|
| P0-1 | 点查、max-ts 点查、锁解析的退避只有 10 次（约 1.5 s），所有错误共用 2 ms 起步的曲线；ServerIsBusy 2 ms 后重试。**预算大小**：Go 的每个 `Backoffer` 都乘以 `tidb_backoff_weight`（`NewBackofferWithVars`，`config/retry/backoff.go:106-110`；默认 2，`kv/variables.go:71`），所以 Get / BatchGet / scan / cop 的实际预算是 40 s，不是 20 s。tidb-rs 按 20 000 × weight 设置（默认 40 s），会话变量接入前用默认值（D05.2 §4）。**P0-1a 已修复**（client-rust#11，tidb-rs 升到 `2ca139e`）：点查和清锁改用调用方的预算，kill_tikv 复跑 tidb-rs 0 个错误。**P0-1b 已修复**（client-rust#12，tidb-rs 升到 `de641f8`）：Go 的 Backoffer，每种错误一条曲线，每次读一份共享的 20 s 时间预算，ServerIsBusy 不计入；同一 region 的重试改成循环，修了栈溢出。kill_tikv 复跑最长挂起 14.0 s（Go 13.2 s） | **触发**：(a) 3 TiKV 集群上 `kill -9` 一个 TiKV，其上 leader 的 region 要经过选举（默认选举超时约 10 s）；期间对这些 region 的点查约 1.5 s 后报错，Go 等到新 leader 后成功。(b) `pd-ctl operator add transfer-leader` 并发大量点查。(c) TiKV 的 `server_is_busy` 故障注入（failpoint）或 CPU 打满。**复现**：#128 第 2 项的 3 TiKV 故障场景测试里加这几种，对比 Go；单测可用 mock 连续返回 11 次 `ServerIsBusy` / `NotLeader`，tidb-rs 报错而按 Go 的预算应成功。**实测（#132，`tools/ab/faults/run.sh -s kill_tikv`）**：(a) 复现，两次都复现：tidb-rs 两种点查各报错约 30 次（`Leader of region N is not found` / `peer is not leader`），Go 0 次（最长挂起 14–18 s 后成功）；tidb-rs 的范围扫描和聚合（48 次退避）没有报错。(b) 40 个 transfer-leader 加 evict-leader 没有触发。(c) CPU 限到 3% / 0.5% 没有触发 ServerIsBusy，ServerIsBusy 部分未证实也未否定 | (1) `Transaction::get`、`get_at_max_ts` 和锁解析改用调用方的 `RetryOptions`（小改动）；(2) 按 Go 为 ServerIsBusy、gRPC 错误、region 错误分开退避曲线，**并照 Go 把 ServerIsBusy 的睡眠排除在预算外（最多 10 分钟，`config/retry/config.go:144`）**（中等）；(3) 长期：按时间计的 Backoffer（P1，与 R3 / R7 的 `LockBackoff` 统一）；(4) 一并修 §2 的 `on_my_way_id` 挂起缺陷（小） |
| P0-2 | 所有 KV RPC 超时默认 2 s。**已修复**：client-rust#14 照 Go 按请求类型设 30 s / 60 s 超时，把截止时间作为 `max_execution_duration_ms` 发给 TiKV，并支持 `tikv_client_read_timeout`（只用于第一次尝试）；client-rust#15 在短读超时到期时不再按 ServerIsBusy 退避 | **触发**：TiKV 负载高、大的 BatchGet（几千个 key）、Scan 跨大 region、跨 AZ 高延迟。**复现**：`tc qdisc add dev lo root netem delay 2500ms`（或 TiKV 的 grpc 延迟 failpoint）后点查和 BatchGet 报 DeadlineExceeded，Go 成功。**实测（#132）未复现**：SIGSTOP 一个 TiKV 5 s、CPU 限到 3%，两边都没有报错，因为超时的请求换 peer 重试（`DeadlineExceeded` 不失效缓存）。实际触发条件更窄：单个请求在 TiKV 上处理超过 2 s，而且所有 peer 都如此 | (1) 默认值按 Go 改为 Get 30 s（`ReadTimeoutShort`，`txnkv/txnsnapshot/snapshot.go:844`）、BatchGet / Scan / coprocessor 60 s（`ReadTimeoutMedium`，`snapshot.go:630`、`scan.go:265`；cop `CoprReqTimeout` 60 s），或在 tidb-rs 侧设置（小改动）。注意 `client.go:87-88` 两个常量的注释容易读反；(2) P1：`tikv_client_read_timeout` 与 `max_execution_duration_ms` |
| P0-3 | gRPC 接收消息上限 4 MiB。**已修复**：client-rust#10 去掉上限（与 Go 一致），tidb-rs 升到 `efe655f` 后复跑 `wide_rows` 5 / 5 成功 | **触发**：宽行表（例如单行 64 KiB 的 text / json）上的全表扫描，一次 cop 响应超过 4 MiB；或几百个大 key 的 BatchGet。**复现**：建表 `t(id int primary key, v longtext)`，插入 200 行 64 KiB 的值（同一 region），经 tidb-rs `select * from t`，报解码错误；Go 正常。**实测（#132，`run.sh -s wide_rows`）复现**：100 行 × 64 KiB，一次约 6 MiB 的 cop 响应，tidb-rs 5 / 5 报 `OutOfRange: decoded message length too large`，Go 5 / 5 成功 | 把上限设为 Go 的值（`usize::MAX` 或足够大），可在 tidb-rs 侧的 `Config` 设置（小改动），并加测试 |
| P0-4 | TSO 流断开后不恢复。client-rust 的 `run_tso` 在流出错或结束时直接退出，在途请求失败，之后的请求报 `TimestampRequest channel is closed`；`RetryClient` 的 `retry!` 两次尝试之间不睡，`reconnect` 在 1 s 内又不重连，10 次重试几毫秒就用完，错误直接抛给语句。Go 的 PD client（`tsoDispatcher`，PD client `85a975a` 的 `clients/tso/dispatcher.go:253-262`、`:353-391`）释放坏流、检查成员变化后用新流继续；每批 3 s 没有应答就断流（`:324`）。**已修复**：client-rust#17，tidb-rs 升到 `36068d0` | **发现**：第一次正式 weekly 运行（run 43414370518），wide_rows 期间所有需要 TSO 的读各报错 8–18 次，max-ts 点查不受影响；断流的根因还在查（#142 之后会拿到完整消息和 PD 日志）。**复现**：mock PD 在一批请求中途断流（client-rust#17 的测试） | client-rust#17：断流后作废旧流（generation），未应答的请求放到新流最前面重发（PD 只会给更大的时间戳）；重建流退避 20 ms – 1 s；连续 3 条流没有任何应答就让等待中的请求失败、关闭请求通道，交给 `RetryClient` 重连；每批 3 s 截止；`get_timestamp` 总等待上限 15 s 兜底；`retry!` 两次尝试之间退避 10 ms – 1 s。**后续**：N3 有了语句截止时间之后，`get_timestamp` 应受语句 context 约束（Go 用语句的 ctx 等 TSO，`clients/tso/request.go:76-96`），届时 15 s 兜底改为取语句截止时间与它的较小值（Lucky 在 #17 的意见） |

P0 修复本身先不做，按本表评审后排期（Maya）。

## 10. 工作量估计（计入 ROADMAP）

| 项 | 大小 | 说明 |
|---|---|---|
| P0-1 / P0-2 / P0-3 | 小（各 1 个 PR） | 配置与 plan 参数，加单测；P0-1 的分曲线退避为中等 |
| 按时间计的 Backoffer（§4） | 中 | 替换 `Backoff` 的次数模型，与 `LockBackoff` 统一；影响所有 plan |
| store 存活状态、store 后台刷新（§2） | 中 | 对应 `store_cache.go` |
| region cache：TTL、`CurrentRegions`、批量加载、兜底分支细分（§2） | 中 | |
| `max_execution_duration_ms`、读超时、HTTP/2 keepalive（§4、§5） | 小 | |
| replica read（§3） | 中到大 | 若产品需要 follower / closest 读 |
| BatchCommands（§5） | 大 | 先测量（16 / 32 / 64 线程）再决定 |
| region bucket（§2） | 中 | 随 D07.2 的 cop 任务拆分 |
| RU 拦截器（§7） | 大 | 见 N10 |

client-go 相关的 Go 代码约 1.7 万行（§1）。L3 替换不按函数逐个移植，工作量按上表的项计，不计入 tracker 的"Go 函数体行数"。
