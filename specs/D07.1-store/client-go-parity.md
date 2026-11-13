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
| `ReplicaReadType`：Leader / Follower / Mixed / Learner / PreferLeader（`pkg/kv` 的 `store_vars.go:60-71`）；TiDB 的 closest-replica、closest-adaptive 映射为带标签匹配的 Mixed（`G/replica_selector.go:181-190`、`:393-432`）；系统变量 `tidb_replica_read` | 只读 leader；`replica_read` 除测试外从不设为 true。所谓 follower 尝试只是把 leader 请求发给另一个 voter（`R/request/plan.rs:370-388`） | 不支持 `tidb_replica_read`：设成 follower 等值时 tidb-rs **默默读 leader**（已写进 `docs/compatibility.md`；修复项包括 SET 时给 warning） | 性能：读负载全落在 leader 上，跨 AZ 读没有节省；正确性无影响；运维：用户以为分流了实际没有 | P1（若 D07.1 / D07.2 需要 follower 或 closest 读则为 P0）。设计见 §3.1 |
| 短读超时之后的重试（`tikv_client_read_timeout` 的本意是"leader 慢就去 follower 读"）：第一次尝试用短超时；TiKV 到期返回 ServerIsBusy "deadline is exceeded" 或 gRPC DeadlineExceeded 时，不退避（`region_request.go:1941-1946`、`:1614`），给 leader 打上 `deadlineErrUsingConfTimeoutFlag`（`replica_selector.go:580-588`），之后 leader 不再是候选（`:242`），请求改发 follower 并设 `ReplicaRead = true`（`:162-165`，ReadIndex，仍线性一致） | client-rust#15 之后：不退避，立即重试，第二次用默认超时；但没有 replica read，**仍发回原 leader** | leader 真的慢时，tidb-rs 第二次仍等在这个 leader 上（最长 30 s / 60 s），Go 换到 follower 读 | 性能 / 可用性：该变量的主要用途失效；正确性无影响 | P1，与上一行（replica read）共用一块实现（设计见 §3.1，步骤 RR-1 至 RR-3）。tidb-rs 接入这个变量前，要么等 replica read 做完，要么在 `docs/compatibility.md` 写明"只缩短第一次尝试"（已写） |
| leader 不可用：`isLeaderCandidate` 在 leader 的 store 不可达、遇到 NotLeader / 超时、或已尝试 10 次 / 50 s 时跳过它（`G/replica_selector.go:233-248`、`region_request.go:709-713`），再按评分选副本；可经 follower 转发（`G/replica_selector.go:129-138`、`:436-461`） | 先试缓存的 leader，再按顺序试每个 voter（不含 learner、witness）（`R/request/plan.rs:558-680`）。Unavailable 等错误失效 store 并丢弃连接；DeadlineExceeded 换下一个 peer（`:331-368`、`:536-549`） | 没有存活记忆，没有转发 | 性能：PD 报告新 leader 之前，每个新请求都重复一次对宕机 leader 的超时 | P1 |
| 按负载选副本：`busyThreshold`、store 估计等待时间（`G/replica_selector.go:142-156`、`:339`） | 无。两次 `ServerIsBusy(0)` 后的一次性 probe 只能发现 leader 变化（`R/request/plan.rs:295-303`） | 无 | 性能：热点下 | P2 |
| stale read：第二次尝试回退到 leader（`G/replica_selector.go:170-179`） | 无 stale read | 无 | 只在 D07 需要 `AS OF TIMESTAMP` 时相关 | P2 |
| 发送失败：存活检查后 `invalidateReplicaStore`，BoTiKVRPC 100 ms–2 s 等抖动退避（`region_request.go:1599-1685`）；所有 peer 都失败后重新加载 region（`:1694-1719`） | 一轮全部失败后失效 region（`R/request/plan.rs:621-643`、`:912-922`），用同一条 2 ms 起步的预算（`:924-943`） | RPC 退避太短，见 P0-1 | 可用性 | **P0（P0-1）** |

### 3.1 replica read 设计

对应上表 `ReplicaReadType` 与"短读超时之后的重试"两行（都是 P1），两者用同一块实现：短读超时只是"leader 模式下 leader 被排除后改走 follower 读"的一个特例。本节只写设计，不改代码。本节的 `R` 路径基于 client-rust `tidb-rs/metrics` @ `36068d0`；TiKV 路径基于 v8.5.8（`a1fdd6a59`）。

**范围**：follower read，即 `Context.replica_read = true`。TiKV 用 ReadIndex 服务，结果线性一致，与 leader 读相同。stale read（`stale_read = true`，按 safe-ts 读，结果可能旧）和 learner / TiFlash 不在范围内，见 §3.1.4。

#### 3.1.1 Go 的做法

**类型与映射。** client-go 有 5 种 `ReplicaReadType`：Leader / Follower / Mixed / Learner / PreferLeader（`kv/store_vars.go:58-71`）。TiDB 的 `tidb_replica_read` 有 7 个值（`pkg/sessionctx/variable/sysvar.go:2479-2496`），由 `GetTiKVReplicaReadType` 映射过去（`pkg/store/driver/options/options.go:23-41`）：

| `tidb_replica_read` | client-go 类型 | 附加条件 |
|---|---|---|
| `leader`（默认） | Leader | |
| `follower` | Follower | |
| `leader-and-follower` | Mixed | |
| `closest-replicas` | Mixed | 匹配标签 `zone = <TiDB 的 labels.zone>`（`placement.DCLabelKey`，`pkg/ddl/placement/common.go:64`）。cop 在 `pkg/distsql/request_builder.go:69-76` 设置，点查和 BatchGet 在 `pkg/executor/builder.go:2266`。TiDB 没有 zone 标签时退化为不带标签的 Mixed，只打一条 warning（`pkg/executor/adapter.go:463-465`） |
| `closest-adaptive` | 按请求决定：Mixed 加 zone 标签，或 Leader | 估算的响应大于 `tidb_adaptive_closest_read_threshold` 才走 closest（cop：`pkg/executor/builder.go:4892-4910`；BatchGet：`:5656-5669`）。功能关闭时等于 leader（`pkg/sessionctx/variable/session.go:2499-2501`） |
| `learner` | Learner | 不在范围内 |
| `prefer-leader` | PreferLeader | |

**哪些语句会用。** 只有只读语句。写语句、RcCheckTs 隔离级别一律读 leader（`GetReplicaRead`，`pkg/sessionctx/variable/session.go:2469-2502`）。读类型按语句设到事务或 snapshot 上（`pkg/sessiontxn/isolation/base.go:439-449` → `KVSnapshot.SetReplicaRead`，`txnkv/txnsnapshot/snapshot.go:976-981`）。cop 请求则各自带一份（`pkg/store/copr/coprocessor.go:1332`、`:1381-1383`）。

**选 peer。** 每个 region 请求（一次 `SendReqCtx`）有一个 `replicaSelector`，在这次请求的所有重试中保留每个副本的尝试次数和标志（notLeader、deadlineErrUsingConfTimeout、serverIsBusy、dataIsNotReady）（`G/replica_selector.go:32-73`）。每次尝试调用 `next`（`:101-126`）：

- **Leader**：`isLeaderCandidate` 成立就选 leader（`:224-249`）。leader 不可用时（store 不可达、已试 10 次或 50 s（`region_request.go:709-713`）、带 deadline 或 notLeader 标志、epoch 过期），改用 Mixed 打分选一个 follower（`:158-166`）。**只有 leader 带 deadline 标志时**，发出的请求才设 `ReplicaRead = true`（`:162-165`）。其他情况下发给 follower 的仍是 leader 请求，目的是找新 leader。
- **Follower / Mixed / PreferLeader / Learner**：都走 `ReplicaSelectMixedStrategy`（`:168-217`）。候选条件（`isCandidate`，`:320-347`）：epoch 未过期、store 不是 unreachable、本次请求里没试过（每个副本只试 1 次）。然后按位打分（`:351-361`、`calculateScore` `:393-432`），从高位到低位依次是 NotSlow > LabelMatches > PreferLeader > NormalPeer > NotAttempted，同分时随机选（`:263-308`）：
  - Follower：follower 有 NormalPeer，leader 没有，所以 leader 只在 follower 都不可用时才会被选到；
  - Mixed：leader 和 follower 同分，随机选；
  - 带标签（closest）：标签匹配的副本优先；同样匹配时 leader 带 PreferLeader，优先于 follower（`:407-414`）；
  - PreferLeader：leader 不慢就选 leader；慢的 follower 不当候选（`:340-345`、`:399-406`）。
- **Context**：`ReplicaRead = isReadOnlyReq && target != leader`，`StaleRead = false`（`:205-208`）。所以选到 leader 时永远发普通 leader 读。`tikvrpc.Request` 内嵌 `kvrpcpb.Context`，这两个字段就是 Context 里的 `replica_read` 和 `stale_read`（`tikvrpc/tikvrpc.go:309-324`）。
- 所有候选都用完后，选择器失效 region，回到 PD 重新加载。例外：有副本带 deadline 标志时不失效，直接快速重试（`:296-307`、`:310-318`）。
- follower 成功应答一个 replica read，**不会**被当成新 leader：只有非 replica、非 stale 的请求成功时才 `switchWorkLeaderToPeer`（`onSendSuccess`，`:627-639`）。

**TiKV 怎么服务 follower read。** `replica_read = true` 的读请求到了 follower，follower 向 leader 发 ReadIndex，等本地 apply 到 read index 后读本地 snapshot，所以是线性一致的。隔离级别为 SI 时，ReadIndex 还带上 `start_ts` 和 key 范围（`src/storage/mod.rs:3501-3503`、`src/server/raftkv/mod.rs:734-739`）。leader 先 `update_max_ts(start_ts)`，再查内存锁（async commit / 1PC 的锁），有锁就让 follower 返回 KeyIsLocked（`ReplicaReadLockChecker`，`src/server/raftkv/mod.rs:854-905`）。这一步的检查不带 `resolved_locks` 豁免（传入空集合），但内存锁存在的时间很短。follower 本地读时按普通读处理锁，`Context.resolved_locks` / `committed_locks` 照常生效。MaxTS 的读不更新 max_ts（`components/concurrency_manager/src/lib.rs:227-229`）。

**follower read 特有的错误**（client-go 处理）：

| 错误 | 来源 | Go 的处理 |
|---|---|---|
| ReadIndexNotReady | split / merge 进行中（`peer.rs:4184-4194`），或 follower 的 apply 落后 read index 超过 `follower_read_max_log_gap`（`peer.rs:3665-3673`） | BoRegionScheduling（2–500 ms）退避后重试，不失效 region（`region_request.go:2028-2041`）。下一次由选择器另选一个没试过的副本 |
| ServerIsBusy | raftstore 队列满（`peer.rs:3689-3694`）或读线程池满 | `onServerIsBusy`：带 `EstimatedWaitMs` 时记到 store 上，否则把 store 标为慢（`replica_selector.go:534-567`）。非 leader 模式下 `canFastRetry` 为真，不在原地睡，把退避挂起后立即换一个副本（`:569-578`） |
| NotLeader | follower 不知道 leader（例如正在选举） | 目标标 notLeader。错误里给出的 leader 可用时，**本次请求改为 Leader 模式**（`:474-493`） |
| RegionNotFound | 副本还没建好（例如 split 时） | 若本次请求还没发过 leader，改为 Leader 模式重试一次（`:513-533`） |
| FlashbackInProgress | | replica read 改为 Leader 模式重试（`:495-505`） |
| DataIsNotReady | **只有 stale read 会遇到**（safe-ts 未推进）；ReadIndex 读不会返回 | 打标志，不退避，下次改用 replica read（`region_request.go:2056-2070`、`replica_selector.go:507-511`、`:326-331`） |

**短读超时的重试。** 第一次尝试带 `tikv_client_read_timeout` 的短超时（`MaxExecutionDurationMs` 小于 `ReadTimeoutShort`，`replica_selector.go:590-597`）。超时有两种：gRPC DeadlineExceeded（`region_request.go:1613-1618`），或 TiKV 返回 ServerIsBusy "deadline is exceeded"（`:1941-1946`）。两种都调用 `onReadReqConfigurableTimeout`，给 target 打 deadline 标志（`replica_selector.go:580-588`），然后不退避、不失效 region，立即重试。下一次 `next` 时 leader 已不是候选（`:242`），于是选一个 follower 发 `ReplicaRead = true`（`:162-165`）。重试用默认超时。

**stale read**（仅作对照）：`EnableStaleWithMixedReplicaRead` 设 `StaleRead = true` 并改用 Mixed（`tikvrpc/tikvrpc.go:335-339`）。第一次尝试按标签选副本发 stale read，第二次回到 leader 读（`replica_selector.go:170-179`、`:186-204`）。遇到锁后改读 leader（`snapshot.go:893-898`）。

#### 3.1.2 client-rust 的设计

**现状。** `Context.replica_read` 从不设为 true。`set_leader` 只写 `peer`（`R/store/request.rs:181-190`）。点查、BatchGet、Scan 走 `RetryableMultiRegion`：每次尝试一轮候选，先缓存的 leader，再其余 voter（`region_request_candidates`，`R/request/plan.rs:474-492`；`execute_candidate_round`，`:673-777`）。发给其他 voter 的仍是 leader 请求，用来在冷 region 上找新 leader。`CandidateState` 的 "follower probe"（`:366-395`）也只是把 leader 请求发给其他 voter。cop 有自己的循环，只发 leader（`R/transaction/coprocessor.rs:593-640`）。

短读超时的现状（依据代码推断，未复现）：
- 遇到 ServerIsBusy "deadline is exceeded" 时，`ShardStep::Again` 原样再发 leader（`R/request/plan.rs:995-1008`）；
- 遇到 gRPC DeadlineExceeded 时，同一轮里接着把 leader 请求发给下一个 voter（`TryNextPeer`），它答 NotLeader。所有 voter 都试过后，以 LeaderNotFound 失效 region，再从 PD 加载（`:749-757`），最后还是回到 leader。

**1. 选择逻辑放在哪里。** 新建 `ReplicaSelector`（建议放在 `R/request/replica.rs`），取代 `CandidateState`：

- 一个 region 请求一个选择器，在 `single_shard_handler` 的循环里传递（与现在的 `candidate_state` 一样），重新拆分（`single_plan_handler`）时新建。这和 Go 每次 `SendReqCtx` 新建一个选择器对应。
- 状态：每个 peer 的尝试次数，以及 not_leader / deadline / server_busy 三个标志。
- 每次尝试的候选列表由它给出：`next() -> Option<(peer, replica_read: bool)>`。
- Leader 模式下保持现有行为（先 leader，再按顺序试其余 voter，发 leader 请求）。只在 leader 带 deadline 标志时，改为发 `replica_read = true` 的 follower 读。
- 其他模式照 Go 的打分（`calculateScore`）。没有 store 健康分数之前，NotSlow 位一律视为 1；不做 `busyThreshold`（§3 中为 P2）。
- `execute_candidate_round` 改为循环调用 `next()`，不再用固定的候选迭代器。
- cop 的 `handle_task_once` 也用同一个选择器（`CopTask` 带一个），这样两条读路径的行为一致。

**2. 请求里要设的字段。**
- `set_leader` 改为 `set_target(region, peer, replica_read)`，写入 `peer`、`replica_read`，并显式写 `stale_read = false`。
- `CandidateResponse::accepted_fallback_leader`（`R/request/plan.rs:348-353`）只在 `replica_read = false` 时更新 leader 缓存，对应 Go 的 `onSendSuccess`。否则一次成功的 follower read 会把 follower 写成 leader。
- 写请求、锁解析的 RPC（CheckTxnStatus、ResolveLock、CheckSecondaryLocks 等）和 raw 请求一律走 Leader 模式。选择器只对 Get / BatchGet / Scan / Coprocessor 生效（Go `isReadReq`，`replica_selector.go:599-607`）。

**3. 需要的数据。**
- peer 角色：`metapb::Peer.role`、`is_witness`，已在 `RegionWithLeader` 里。
- store 标签：已缓存在 `RegionCache.store_cache`（`metapb::Store`，`R/region_cache.rs:56`、`:128-134`）。选择器拿到候选 peer 后，经 `get_store_by_id` 读标签。不需要新的 PD 调用。
- 客户端所在 zone：由 tidb-rs 配置给出（对应 TiDB 的 `labels.zone`），以 `match_labels` 的形式随请求传入。client-rust 自己不读配置。

**4. API。**

```rust
pub enum ReplicaReadType { Leader, Follower, Mixed, PreferLeader }   // 不含 Learner
pub struct ReplicaRead { pub kind: ReplicaReadType, pub match_labels: Vec<metapb::StoreLabel> }
```

- `TransactionOptions::replica_read(ReplicaRead)`（默认 Leader），并加 `Transaction::set_replica_read` / `Snapshot::set_replica_read`。tidb-rs 按语句设置，对应 Go 的 `KVSnapshot.SetReplicaRead`。它作用于 `get`、`batch_get`、`scan` 和 max-ts 点查。
- `CopRequest::replica_read(ReplicaRead)`：每个 cop 请求一份，与 Go 的 `kv.Request.ReplicaRead` / `MatchStoreLabels` 对应。closest-adaptive 由 tidb-rs 按请求决定 Leader 还是 Mixed 加标签，client-rust 不做估算。
- **不放进 `RetryOptions`**：`RetryOptions` 会传给锁解析和写路径，而这两处必须读 leader。
- tidb-rs 侧的映射：`sessionctx` 已移植 `GetReplicaRead`（`crates/sessionctx/src/variable/session_methods.rs:1007`），按上面的映射表转换。`closest-replicas` 加 zone 标签；tidb-rs 没有配置 zone 时，像 Go 一样退化为 Mixed 并打 warning。`learner` 退化为 leader 并给 warning。

**5. 与锁解析的关系。**
- 带锁提示的读可以发给 follower：`resolved_locks` / `committed_locks` 在 follower 本地读时生效，`Dispatch` 每次发送都带上（`R/request/plan.rs:164-177`）。
- leader 在 ReadIndex 时的内存锁检查不认这些提示（§3.1.1），最多让读在锁解析后多重试一次，并消耗锁退避的预算。
- 遇到锁后**保持** replica read，不回到 leader：Go 只在 stale read 时回 leader（`snapshot.go:680-683`、`:893-898`）。
- 锁解析自己的 RPC 总是发给 leader。

**6. 与时间预算的关系。**
- 换副本本身不退避，也不计入预算：deadline、NotLeader 带 leader、follower ServerIsBusy 都立即换副本。这和 Go 的快速重试一致。
- ReadIndexNotReady 按 BoRegionScheduling 退避，RegionNotInitialized 按 BoMaxRegionNotInitialized（2–1000 ms）退避，都计入同一份 `Backoff::time_budget`。这两项和 ProposalInMergingMode 一起，就是 §2 中"兜底分支"那一行，要先修：现在它们会失效 region 和 store。
- 为防止快速重试无限循环，沿用 Go 的上限：每个副本在一次 region 请求中只试 1 次，leader 最多 10 次 / 50 s。所有副本用完后走 region 重新加载，那一步照常计入预算。

**7. 与 store 存活状态的关系。**
- 另一个进行中的 PR（`tidb-rs/store-liveness`）会给 store 标 unreachable。选择器只需要一个查询接口 `is_unreachable(store_id)`：Mixed 模式下 unreachable 的 store 不当候选，Leader 模式下 leader 的 store unreachable 时 leader 不当候选。这对应 Go 的 `isCandidate`（`:321-324`）和 `isLeaderCandidate`（`:238-240`）。
- 这个接口在存活状态 PR 合并之前一律返回 false。replica read 不依赖它，只是有了它之后少等一次连接超时。

#### 3.1.3 实施步骤与测试

client-rust 侧每步一个 PR，按顺序：

| # | 内容 | 依赖 |
|---|---|---|
| RR-1 | ReadIndexNotReady / ProposalInMergingMode / RegionNotInitialized 退避后重试，不失效 region 和 store（§2 那一行 P1） | 无 |
| RR-2 | 抽出 `ReplicaSelector`，取代 `CandidateState` 和 `region_request_candidates`，只有 Leader 模式。纯重构，现有测试不变 | 无 |
| RR-3 | `set_target`（写 `replica_read`、`stale_read`）；follower 读成功不更新 leader；短读超时路径：两种 deadline 都给 leader 打标志，下一次以 `replica_read = true` 发给 follower，不退避、不失效 region。完成后 §3 第二行关闭 | RR-2 |
| RR-4 | Follower / Mixed / PreferLeader、标签匹配、`TransactionOptions` / `Snapshot` / `Transaction` 的 API；NotLeader、RegionNotFound、FlashbackInProgress 回退到 Leader 模式 | RR-3 |
| RR-5 | cop 改用选择器，加 `CopRequest::replica_read` | RR-4 |
| RR-6 | 接入 store 存活状态 | RR-2，存活状态 PR |

tidb-rs 侧：
- **RR-7**：接入 `tidb_replica_read` 和 `tikv_client_read_timeout`，传 zone 标签；更新 `docs/compatibility.md` 中的两行，去掉 SET 时的 warning（`learner` 除外）。
- closest-adaptive 在 D07.2 有了大小估算之后再接。

**单测**（mock `KvClient`）。用 `MockKvClient::with_dispatch_hook` 记录每次尝试的 `ctx.peer`、`ctx.replica_read`、`ctx.stale_read`、`ctx.max_execution_duration_ms`。`MockPdClient` 要加一个返回 store 标签的 hook。每种类型和错误各至少一例：

- Leader：只发 leader，`replica_read = false`；leader 失败后发给 voter 的仍是 `replica_read = false`（冷 region 的现有行为不变）。
- Follower：只发 follower，`replica_read = true`；两个 follower 都答 ServerIsBusy 后发 leader，`replica_read = false`。
- Mixed：多次运行后 leader 和 follower 都被选到；带 zone 标签时只选匹配的副本，匹配的 leader 优先。
- PreferLeader：leader 正常时只发 leader。
- 短读超时：分别注入 ServerIsBusy "deadline is exceeded" 和 gRPC DeadlineExceeded。第二次尝试发 follower，`replica_read = true`，`max_execution_duration_ms` 为默认值；中间没有睡眠；region 缓存没有失效。
- follower 答 NotLeader（带 leader）：下一次发 leader，`replica_read = false`。
- follower 答 ReadIndexNotReady：退避后换副本，region 不失效。
- follower 读成功后 leader 缓存不变；写请求和锁解析的 RPC 在任何类型下都是 `replica_read = false`。
- follower 返回 KeyIsLocked：锁解析后重试仍是 replica read，并带上锁提示。
- cop 走同样几例。

**真实集群**（`tools/ab/faults`，新场景 `slow_leader`）：
1. 把 `sbtest1` 的 leader 都调度到一个 store。
2. 让这个 store 只在读上变慢，同时不拖慢 raftstore；否则 ReadIndex 也会变慢，follower read 测不出来。候选做法有两种，见开放问题：(a) 用大扫描占满它的 unified read pool；(b) 只对 TiDB 进程到它的连接加 netem 延迟（cgroup 加 fwmark）。
3. 两边会话都设 `tikv_client_read_timeout = 100`、`tidb_replica_read = leader`。

期望：Go 的点查和 cop 读在约 100 ms 加一次 follower 读后返回。tidb-rs 在 RR-3 之前每次都等满 leader；RR-3 之后应与 Go 相当。用 follower 上的 `tikv_raftstore_proposal_total{type="read_index"}` 确认读确实发到了 follower。另跑一组 `tidb_replica_read = follower` / `closest-replicas`（playground 给 TiKV 配 zone 标签），检查 leader 的读 QPS 下降，结果与快照一致（现有的结果检查即可）。

#### 3.1.4 不做的事与开放问题

**不做：**
- stale read（`AS OF TIMESTAMP`、`tidb_read_staleness`、DataIsNotReady 的处理），见 §3 stale read 一行和 N11。
- learner 读和 TiFlash（MPP / batch cop）。
- 按负载选副本（`busyThreshold`，§3 P2）。
- 经 follower 转发（forwarding）。
- store 的慢分数（NotSlow 位）：等存活状态和健康状态做完再看。

**开放问题：**
1. **D07.1 / D07.2 是否需要 follower 或 closest 读**（决定 P1 还是 P0）？若只需要"leader 慢时换 follower"，RR-1 到 RR-3 就够了，RR-4 / RR-5 可以推后。
2. **closest-adaptive** 依赖 TiDB 的大小估算（`avgRowSize`、`netDataSize`）。tidb-rs 在 D07.2 之前是先当作 leader，还是先当作 closest-replicas？
3. **zone 配置的来源**：tidb-rs 的配置项是否沿用 TiDB 的 `labels.zone`？
4. **slow_leader 场景的注入方式**：只拖慢读、不拖慢 raft 的那种（读线程池占满，还是按进程加网络延迟），要在 RR-3 的 PR 里定。
5. leader 在 ReadIndex 时的内存锁检查不认 `resolved_locks`：async commit 冲突多的负载下，会不会让 follower read 的锁重试明显多于 leader 读？这需要实测；TiKV 侧不改。
6. 保持 Leader 模式的冷 region voter 遍历（发 leader 请求找新 leader），还是像 Go 那样直接靠 NotLeader 提示和存活状态？本设计先保持现状，等存活状态合并后再评估。

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
| replica read（§3、§3.1） | 中到大 | RR-1 至 RR-7；短读超时只需 RR-1 至 RR-3（中） |
| BatchCommands（§5） | 大 | 先测量（16 / 32 / 64 线程）再决定 |
| region bucket（§2） | 中 | 随 D07.2 的 cop 任务拆分 |
| RU 拦截器（§7） | 大 | 见 N10 |

client-go 相关的 Go 代码约 1.7 万行（§1）。L3 替换不按函数逐个移植，工作量按上表的项计，不计入 tracker 的"Go 函数体行数"。
