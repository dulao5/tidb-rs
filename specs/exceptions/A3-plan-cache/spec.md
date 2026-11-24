# A3（plan cache）：实例级计划缓存（实现例外 spec）

状态：**已确认（2026-11-23，负责人在 #305）**（2026-11-23 起草）。类型：实现例外（A3"缓存"的 plan cache 部分；A3 其余三项 coprocessor cache、infoschema v2、统计信息缓存各自另写 spec），含几处用户可见的特性例外（默认值、OFF 时不缓存、`ADMIN FLUSH INSTANCE PLAN_CACHE` 生效，§4）。所属块：D13.1 plan cache 的缓存部分（VS1-39 起），crate `tidb-planner`（缓存本体与重建）、`tidb-domain`（持有与后台淘汰）、`tidb-sessionctx`（接口）。

修订记录：

- 2026-11-23：按 Lucky 对 #304 的评审（批准，附修改意见）修订：§2.3 补上漏列的执行期写入，第 1 阶段的两处列入完成条件；§3.3 可缓存条件补上 Go 克隆按字段返回 `false` 的情况；§3.6 代价缓存改为"封存节点上照常计算、不写回"；§3.4 淘汰在锁外释放计划树、Put 在写锁下复查淘汰状态、哈希加随机种子、写明命中路径不分配的前提；§8 风险 1 增加"只换分片内部结构"的后备。§6 的 11 项决定在 #305 等负责人确认（Lucky 全部同意建议）。
- 2026-11-23：按 Lucky 对 #309 的评审（批准，附非阻塞意见）：§3.4 加 flush 代数，防止 flush 之前开始优化的语句在 flush 之后放入，§5.2 加确定性测试；§6 决定 6 写明没有按条目的 TTL。
- 2026-11-23：负责人在 #305 决定 §6 的 11 项并确认本 spec：1–5、7、9、11 按建议；6 先改为淘汰检查周期可配置（新变量），当日撤销（见下一条）；8 改为 `ADMIN FLUSH INSTANCE PLAN_CACHE` 真正清空实例级缓存（按 Go 缺陷修正，#307）；10 改为缓存设计覆盖非预处理语句（Go 中两者共用实例级缓存），先做预处理语句。相应修改 §0、§1、§3.4、§3.8、§4、§6、§7、§8、§9，偏差台账加 R14–R16，`docs/compatibility.md` 加 Plan cache 一节。
- 2026-11-23：负责人在 DM 中确认（"选 a"）：淘汰触发周期照 Go v8.5.8 固定 30 秒，不可配置，撤销新增变量 `tidb_instance_plan_cache_evict_interval`。修改 §1.2、§3.8、§4、§6 决定 6、§7、§9；台账 R17 撤销（编号不复用），`docs/compatibility.md` 去掉该行。

负责人的要求（2026-11-23）："concept follows Go's instance-level plan cache design, but make full use of Rust's capabilities"。负责人之前已决定：tidb-rs 只做实例级 plan cache（各会话共享），不做 Go 的会话级 prepared plan cache（#128 §一.5；N1 §3.5 记为 2026-11-14 的决定）。

相关：[N1](../N1-plan-representation/spec.md) §3.4、§3.5（路径复制）、§3 第 9 条（链与扁平列表）、§7 第 2 条（交给本 spec 的待定项）；[N4](../N4-expr-eval/spec.md) §3.5（求值的可变状态）；[N13](../N13-shared-metadata/spec.md)（元数据 `Arc`）；[N3+A1](../N3-A1-concurrency-memory/spec.md) §3.7（后台任务）、A1 §3.5（内存数值的兼容契约）；[N8](../N8-sysvar/spec.md)（系统变量）；[D09.7 试点报告](../../D09.7-column-pruning/pilot-report.md) §5 风险 1–4；[D13.1 spec](../../D13.1-plancache/spec.md)（已移植的非缓存部分）；[VS1 plan](../../VS1-vertical-slice/plan.md) §8 风险 2、§10-3，VS1-39。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/`。

## 0. 总结

- **行为照 Go 的实例级 plan cache**（`tidb_enable_instance_plan_cache = ON` 时的 Go）：哪些语句、哪些计划能缓存，key 由哪些东西组成，命中与未命中的判定，range 重建失败时怎样退回，warning 文本，`@@last_plan_from_cache`，三个实例级变量的取值与校验，淘汰算法（软限额、按最近使用时间的阈值），都与 Go 相同。非预处理语句（`tidb_enable_non_prepared_plan_cache`）在 Go 中走同一个查找与放入，同样使用本缓存（§1.3）。与 Go 不同、用户可见的只有负责人决定的几处（§4 第 5–8 条）。
- **重新设计承载方式**：Go 为了并发安全，放入缓存时深拷贝一次，**每次命中再深拷贝一次**（含全部表达式），然后在私有副本上原地改 range。Rust 中物理计划本来就是不可变的 `Arc` 树（N1 §3.4），所以：
  1. 放入缓存不复制，执行中的语句和缓存持有同一棵树；
  2. 命中时只按本次参数重建 range，用 N1 的路径复制只浅拷贝从根到被改写节点的那几个节点，其余子树、表达式、schema 全部共享；参数得到的 range 与缓存中相同时一个节点也不复制；
  3. 缓存中的计划在类型层面不能被改：唯一的写入口 `make_mut_plan` 遇到共享节点必然先复制，执行器 crate 用 lint 禁止调用它；计划节点不保存会话上下文，执行时需要的会话状态由调用方传入；
  4. 查找用分片哈希表，读锁只在取出一个 `Arc` 的几十纳秒内持有；命中路径不分配内存（key 写进会话复用的缓冲区）；
  5. 内存按缓存条目实际持有的字节计（决定 4），淘汰沿用 Go 的软 / 硬限额与阈值算法；被淘汰的计划在最后一个使用它的语句结束时确定性释放。
- **验收**：Go 打开实例级 plan cache 时的行为向量与 ab 对照（含每条 EXECUTE 的 `last_plan_from_cache` 与 warning）；并发与内存限额测试；性能按负责人的门槛：sysbench read_only 不慢于打开 plan cache 的 Go。
- **第 1 阶段只做 VS1 形态的预处理语句**：点查、按 handle 的表范围扫描、Selection / Projection / Sort / TopN / Limit / 聚合（§7）。非预处理语句在第 1 阶段之后接上（§7 第 4 阶段）。

## 1. 范围

### 1.1 照 Go 保持不变的行为（L1）

| 行为 | Go 位置 | 说明 |
|---|---|---|
| 是否尝试使用缓存：语句可缓存（`StmtCacheable`，PREPARE 时由 `IsASTCacheable` 判定）且会话变量 `tidb_enable_prepared_plan_cache` 为 ON | `planner/core/plan_cache.go:203-216` | 实例级模式下 Go 仍然看这个**会话**变量；它是"本会话是否用缓存"，`tidb_enable_instance_plan_cache` 是"缓存放在哪里"。D13.1 已移植。非预处理语句看的是 `tidb_enable_non_prepared_plan_cache`（`:204-206`），见下一行 |
| 非预处理语句同走这条路径：`getPlanFromNonPreparedPlanCache` 把常量参数化成 `?`，按参数化后的 SQL 取会话的 `PlanCacheStmt`，再以 `isNonPrepared = true` 调同一个 `GetPlanFromPlanCache`；`lookupPlanCache` / `generateNewPlan` 不区分两者，所以打开实例级缓存时非预处理语句的计划也放进实例级缓存 | `planner/optimize.go:72-135`、`plan_cache.go:194-241`、`:251-263` | 可缓存判定（`NonPreparedPlanCacheableWithCtx`）、参数化（`plan_cache_param.go`）、warning `skip non-prepared plan-cache: …`、metrics 的 non-prepared 计数照 Go。实现顺序见 §1.3、§7 第 4 阶段 |
| 计划是否可缓存：`isPlanCacheable` / `isPhysicalPlanCacheable`，含 `tidb_plan_cache_max_plan_size` | `plan_cacheable_checker.go:547-634` | D13.1 已移植；原因文本逐字节相同 |
| **实例级额外的可缓存条件**：计划中每个节点都必须能 `CloneForPlanCache`。只有 27 种算子实现了它（`plan_clone_generated.go`：TableScan、IndexScan、Selection、Projection、Sort、TopN、StreamAgg、HashAgg、HashJoin、MergeJoin、TableReader、IndexReader、PointGet、BatchPointGet、Limit、IndexJoin、IndexHashJoin、IndexLookUpReader、LocalIndexLookUp、IndexMergeReader、Update、Delete、Insert、Lock、UnionScan、UnionAll、TableDual），其余算子走默认实现返回 `false`（`operator/baseimpl/plan.go:150`） | `plan_cache.go:320-329`、`plan_cache_utils.go:437-449` | Put 时克隆失败就**静默**不放入，不发 warning；之后每次都未命中。Rust 不需要克隆，但必须保留这个集合（§3.3 `instance_cacheable`），否则会多缓存 Go 不缓存的计划，`last_plan_from_cache` 不同 |
| cache key 的组成与顺序 | `plan_cache_utils.go:257-415` | 见 §2.1 |
| 参数类型匹配：`parseParamTypes`、`checkTypesCompatibility4PC`（VARCHAR / VAR_STRING 互通、charset、collation、整数的 unsigned、DECIMAL 的 flen / decimal 只能不大于缓存中的） | `plan_cache_utils.go:661-688`、`:778-796` | 同一 key 下可以有多个计划（参数类型不同）；按**最新放入的优先**的顺序找第一个兼容的（`plan_cache_instance.go:89-99`，Put 插在链表头 `:122-125`） |
| 命中后的处理：权限再检查（`checkPreparedPriv`）→ `RebuildPlan4CachedPlan` → `FoundInPlanCache = true`、`FoundInBinding`、命中计数、`SetPlanDigest`、恢复 `StmtHints` | `plan_cache.go:265-288` | 重建失败时发 `skip plan-cache: plan rebuild failed, <原因>` 并退回重新优化（`plan_cache_rebuild.go:39-47`）；重建中 `UseCache` 由 true 变 false 时同样退回 |
| range 重建 `rebuildRange` 的分支、安全检查 `isSafeRange`、`convertConstant2Datum` 的"转换后必须相等" | `plan_cache_rebuild.go:77-450` | 逻辑 L1；写入方式按 §3.2 |
| 未命中：`generateNewPlan` 优化、`isPlanCacheable`、Put | `plan_cache.go:292-336` | Put 失败（超过硬限额、正在淘汰、已有兼容计划）都是静默的 |
| Put 的条件：正在淘汰时不放；`本条目大小 + 当前总量 > 硬限额` 时不放；已有参数类型兼容的计划时不放 | `plan_cache_instance.go:103-131` | |
| 淘汰：总量低于软限额时什么也不做；否则按"平均每条大小"算出要淘汰的条数 n，取第 n 小的最近使用时间作阈值，淘汰最近使用时间 ≤ 阈值的条目；删掉变空的 key | `plan_cache_instance.go:137-208` | 软限额 = 硬限额 ×（1 − `tidb_instance_plan_cache_reserved_percentage`）（`domain/domain.go:3436-3438`） |
| 失效：key 中含 schema 版本、每张表的 revision（`RelateVersion`）、RC / `FOR UPDATE` 读时的最新 schema 版本、统计信息版本（`tidb_plan_cache_invalidation_on_fresh_stats`）等，变化后自然不命中；旧条目留在缓存里直到被淘汰 | `plan_cache.go:114-166`、`plan_cache_utils.go:263-271`、`:381-388` | Go 实例级缓存**没有** Delete 和 DeleteAll（`plan_cache_instance.go` 只有 Get / Put / Evict） |
| `ADMIN FLUSH ... PLAN_CACHE` 的校验与 warning：`GLOBAL` 报 `Do not support the 'admin flush global scope.'`；本会话 `tidb_enable_prepared_plan_cache` 为 OFF 时发 `The plan cache is disable. So there no need to flush the plan cache` 并返回 | `executor/simple.go:3134-3145` | INSTANCE 范围的效果**不照 Go**：Go 只设置时间戳（各会话下次 EXECUTE 时清空自己的会话级缓存，`plan_cache.go:173-177`），实例级缓存不变；tidb-rs 真正清空实例级缓存（§1.2、决定 8） |
| 系统变量：`tidb_enable_instance_plan_cache`（GLOBAL，默认 OFF）、`tidb_instance_plan_cache_max_size`（GLOBAL，默认 104857600，`parseByteSize`，负数报错）、`tidb_instance_plan_cache_reserved_percentage`（GLOBAL，默认 0.1，[0, 1]，越界截断并发 1292） | `sessionctx/variable/sysvar.go:1425-1458`、`tidb_vars.go:925-931`、`:1587-1588`、`:1757-1759` | v8.5.8 **没有**淘汰间隔的变量：淘汰固定每 30 秒一次（`domain.go:3483`），tidb-rs 照 Go 固定、不加变量（决定 6）；限额每 15 秒从变量刷新一次（`domain.go:3452-3467`）。tidb-rs 的默认值见决定 1，§1.2、§3.8 |
| 可观测面：`@@last_plan_from_cache`（会话只读，取上一条语句的 `FoundInPlanCache`，`executor/select.go:1249-1250`）、`@@last_plan_from_binding`、慢日志 `Plan_from_cache`、语句摘要 `PLAN_IN_CACHE` / `PLAN_CACHE_HITS` / `PLAN_CACHE_UNQUALIFIED` / `PLAN_CACHE_UNQUALIFIED_LAST_REASON`；metrics `tidb_server_plan_cache_total{type="prepared"}`、`tidb_server_plan_cache_miss_total`、`tidb_server_plan_cache_instance_memory_usage` 与 `tidb_server_plan_cache_instance_plan_num_total`（标签带前导空格：`" instance-plan-cache"`、`" instance-plan-cache-last-evict"`） | `sessionctx/variable/sysvar.go:387-392`、`executor/adapter.go:2104`、`adapter_slow_log.go:253`、`infoschema/tables.go:1397-1415`、`planner/core/metrics/metrics.go:44-58` | v8.5.8 没有列出 plan cache 内容的 INFORMATION_SCHEMA 表。metrics 名称和标签照抄（N9） |
| warning 文本：`skip prepared plan-cache: <原因>`（`util/context/plancache.go:83`）、`skip plan-cache: plan rebuild failed, …`、`force plan-cache: may use risky cached plan: …` | 同左 | |
| 会话私有的点查执行器复用：`PlanCacheStmt.PointGet.Executor`（只在 autocommit、非 stale read、schema 版本未变、读时间戳为 MaxTS 时） | `plan_cache.go:351-363`、`executor/compiler.go:133-139`、`executor/adapter.go:366-391` | 它属于会话的 `PlanCacheStmt`，不在共享缓存里，照 Go 移植（不在本例外的替换范围） |

### 1.2 重新设计的部分（本例外替换的 Go 范围，宪章第三条 4）

| Go | 替换为 | tracker 标记 |
|---|---|---|
| `planner/core/plan_cache_instance.go` 全部（`instancePlanCache`、`instancePCNode`、无锁链表、`Get`/`Put`/`Evict`/`foreach`/`headNodes`/`createNode`/`GetLimits`/`SetLimits`/`MemUsage`/`Size`） | `plan_cache_instance.rs`：分片哈希表（§3.4）；`calcEvictionThreshold` 与 Evict 的两遍扫描按 L1 移植算法 | 例外（A3），`calcEvictionThreshold` L1 |
| `plan_clone_generated.go` 的 27 个 `CloneForPlanCache`、`clonePhysicalPlansForPlanCache`、`BasePhysicalPlan.CloneForPlanCacheWithSelf`、`baseimpl.Plan.CloneForPlanCache`、`mutableIndexJoinRange.CloneForPlanCache`、`PlanCacheValue.CloneForInstancePlanCache` | 不复制。可缓存集合由 `instance_cacheable()` 表达（§3.3）；命中时的私有化由路径复制完成（§3.2，N1 的 `clone_node`） | 例外（A3） |
| `lookupPlanCache` 中命中后的克隆（`plan_cache.go:255`）、`generateNewPlan` 中放入前的克隆（`:321-327`） | 去掉克隆，其余 L1 | 函数 L1，这两行登记为例外 |
| `PlanCacheValue.MemoryUsage`（Go 的估算公式，`plan_cache_utils.go:456-491`） | 决定 4：条目实际持有的字节（§3.5）。`isPlanCacheable` 中 `tidb_plan_cache_max_plan_size` 的判断**仍用 Go 公式**（各算子 `memory_usage()` 已按 L1 移植），保证 warning 相同 | 例外（A3，决定 4） |
| `domain.InitInstancePlanCache`、`planCacheMetricsAndVars`、`planCacheEvictTrigger`（`domain.go:3435-3500`） | 运行时上的后台任务（N3 §3.7），逻辑 L1，限额生效时机见决定 5 | L2 |
| `planCacheEvictTrigger` 的固定周期 `time.NewTicker(time.Second * 30)`（`domain.go:3483`） | 照 Go：固定 30 秒，不可配置（§3.8，决定 6） | L1（不是例外；负责人 2026-11-23 确认） |
| `executeAdminFlushPlanCache` 的 INSTANCE 范围（`executor/simple.go:3146-3153`：只清本会话缓存并 `SetExpiredTimeStamp4PC`，实例级缓存从不读这个时间戳） | 另外清空实例级缓存（`delete_all`，§3.4）；校验、warning 照 Go | 例外（A3，Go 缺陷的修正，决定 8；#307，台账 R15） |
| `tidb_enable_instance_plan_cache` 默认 OFF；OFF 时用会话级缓存 | 默认 ON；OFF 时完全不缓存 | 例外（A3，特性例外，决定 1、2；台账 R14） |
| `NewPlanCacheKey` 中用户名、主机名、库名、语句文本直接拼接（`plan_cache_utils.go:308-312`） | 字符串字段带长度前缀（§3.4） | 例外（A3，决定 9；#308，台账 R16） |
| `plan_cache_lru.go`（会话级 LRU）、会话级 `DeleteAll` / `Delete` 的实际删除 | 不移植（负责人决定只做实例级）。D13.1 的 `SessionPlanCache` 替身保留 | 例外（A3，不移植） |

### 1.3 非预处理语句（决定 10）

非预处理 plan cache（`tidb_enable_non_prepared_plan_cache`，Go 默认 OFF）**在本 spec 范围内**。Go v8.5.8 中 `lookupPlanCache` 由预处理和非预处理语句共用，打开实例级缓存时两者的计划都放在实例级缓存中（§1.1）。缓存本体、key、放入、淘汰、命中时的重建对两者相同，本 spec 的设计（§3）直接覆盖，不另做设计。

实现顺序：先预处理语句（第 1 阶段）；非预处理语句特有的部分（常量参数化 `plan_cache_param.go`、`NonPreparedPlanCacheableWithCtx`、会话的参数化语句缓存 `GetNonPreparedPlanCacheStmt` / `AddNonPreparedPlanCacheStmt`、快速解析路径）放在之后的阶段（§7 第 4 阶段）。在那之前保持 D13.1 现状：设为 ON 时照常接受、不起作用（`last_plan_from_cache` 为 0）。D13.1 spec §1.2 中"非预处理 plan cache 不移植"一行与 §6 对应的边界桩，在第 4 阶段开工时改写。

### 1.4 不在本 spec

- 预处理去重缓存（`tidb_enable_cache_prepare_stmt`，Go 默认 OFF）：它缓存的是会话的 `PlanCacheStmt`，不是计划；D13.1 已让新会话不写它，本 spec 不改。
- 绑定（bindinfo）、真实统计信息的版本：key 中照 Go 留出位置，块到位前按"无绑定""pseudo 统计信息版本为 0"计算，与 Go 在同样条件下相同（§7）。

## 2. Go 行为的细节（Rust 照做的部分）

### 2.1 cache key（`NewPlanCacheKey`，`plan_cache_utils.go:257-415`）

按顺序：

1. 绑定：`MatchSQLBindingForPlanCache`；绑定要求忽略缓存时返回不可缓存，原因 `ignore plan cache by binding`（L258-261）。
2. 用户名、主机名（`AuthUsername` / `AuthHostname`，"不同用户不共享计划"，L298-303）、语句的库名（`StmtDB`，空则用当前库）、语句文本 `StmtText`。
3. `stmt.SchemaVersion`、`RelateVersion`（表 ID → revision，按 ID 排序，L236-251）、分区裁剪模式。
4. 最新 schema 版本：只在 READ COMMITTED 或 `ForUpdateRead` 时取 `domain.InfoSchema().SchemaMetaVersion()`，否则为 0（L263-271）。
5. `sql_mode`、时区偏移（按**当前时刻**算出的 offset，L292-295）、隔离读引擎（TiDB / TiKV / TiFlash；非只读语句临时去掉 TiFlash，L273-280）、`sql_select_limit`、绑定 SQL、连接的 charset 与 collation、`InRestrictedSQL`、`tidb_restricted_read_only`、`tidb_super_read_only`、表达式下推黑名单的重载时间戳（L321-340）。
6. 子查询：有子查询时 `tidb_enable_plan_cache_for_subquery` 为 OFF 则不可缓存；`foreign_key_checks`（L342-353）。
7. `LIMIT ?`：`tidb_enable_plan_cache_for_param_limit` 为 OFF 则不可缓存；参数值必须是整数且 ≤ `MaxCacheableLimitCount`，值本身进 key（L355-379）。所以 Limit / TopN 的 count、offset 固化在缓存计划中，命中时不需要重建。
8. 统计信息版本：`tidb_plan_cache_invalidation_on_fresh_stats`（默认 ON）时，语句所涉各表最新统计版本之**和**（L381-388）。
9. 脏表（本事务写过的表 ID，排序后，L390-404）。
10. 事务状态：`InTxn`、autocommit、`pessimistic-auto-commit` 配置、`ForShareLockEnabledByNoop`、`SharedLockPromotion`（L406-412）。

**不在 key 中的**：绝大多数优化器变量（`tidb_opt_*`、`tidb_cost_model_version`、`tidb_opt_fix_control`、`tidb_isolation_read_engines` 以外的引擎偏好等）。Go 的实例级缓存因此会让设置不同的会话共享同一计划；计划中规划时决定、依赖会话的值（例如 PointGet 的 `LockWaitTime` 来自规划时的 `innodb_lock_wait_timeout`，`StmtHints`）也随计划共享。**照搬**，在 `docs/compatibility.md` 不需要另写（与 Go 相同）。

### 2.2 命中、未命中的完整顺序（`GetPlanFromPlanCache`，`plan_cache.go:194-241`）

`planCachePreprocess`（参数个数、参数值、MDL 与逐表 revision、schema 版本、过期时间戳、脏表；D13.1 已移植）→ 设置 `UseCache` 并发出 `StmtCacheable` 的原因 → `NewPlanCacheKey`（不可缓存时 `SetSkipPlanCache`）→ `parseParamTypes` → 查找 → 命中则 `adjustCachedPlan`（失败或重建失败则继续）→ `generateNewPlan`。Rust 的函数结构与这个顺序逐行对应，只有"查找"和"放入"两步换成本 spec 的缓存。

### 2.3 Go 在执行期间修改计划的位置（D09.7 试点报告 §5 风险 2）

Go 放入前克隆的理由写在 `plan_cache.go:322-326`："execution (might modify the plan)"。逐处列出（只看能进实例级缓存的 27 种算子；不可缓存的也列出，因为 N1 下所有物理计划都是 `Arc` 树）：

| # | Go 位置 | 改了什么 | 可缓存 | Rust 的去处 |
|---|---|---|---|---|
| 1 | `rebuildRange` 及 `updateRange`（`plan_cache_rebuild.go:51-308`）；`PhysicalIndexJoin.Ranges.Rebuild`（`index_join_path.go`） | `Ranges`、`rangeInfo`、`Handle`、`Handles`、`IndexValues`；IndexJoin 的可变 range | 是 | 命中路径上的路径复制（§3.2）。经 `TablePlans[0]` / `IndexPlans[0]` 写的地方，按 N1 §3 第 9 条改为写链上的节点 |
| 2 | `PointGetPlan.PrunePartitions`（`point_get_plan.go:355`，L369 写 `p.PartitionIdx`），由 `executor/point_get.go:57` 在构建执行器时调用 | 分区下标 | 是 | 返回值交给执行器（`PointGetExecutor` 的分区下标字段），不写计划。签名改为返回 `(is_table_dual, Option<usize>)`，L2，块 D12.1 / D10.8 移植分区时照此写。BatchPointGet 的 `PartitionIdxs` 移植时同样核对 |
| 3 | 计划节点保存的会话上下文：`baseimpl.Plan.ctx`、`PointGetPlan.ctx`、`BatchPointGetPlan.ctx`，克隆时换成新会话（`base_physical_plan.go:261` `SetSCtx`，`plan_clone_generated.go` 中 `cloned.ctx = newCtx`） | 上下文指针 | 是 | 节点不保存上下文（`M-CTX`，已在 `crates/planner/src/core/point_get_plan.rs` 等落实）；`rebuildRange`、`PrunePartitions`、`OperatorInfo` 等由调用方传入 `&dyn PlanContext` |
| 4 | 代价缓存：`PlanCostInit` / `PlanCostVer2` 在 EXPLAIN 时补算，`EXPLAIN ANALYZE FORMAT='true_card_cost'` 用 `CostFlagRecalculate` 重算（`common_plans.go:936`） | 节点上的代价 | 是 | Rust 的代价缓存是节点内的 `Mutex`（经 `&self` 写，`crates/planner/src/physicalop/base_physical_plan.rs` 文件头）。缓存中的节点写它会让别的会话看到本会话代价因子算出的值。规则见 §3.6 |
| 5 | `buildApply` 写 `v.OuterSchema`（`executor/builder.go:2619`） | Apply 的关联列 | 否（`plan_cacheable_checker.go:623-624`） | 执行器局部计算，不写计划（N1 下计划共享的规则对所有计划成立） |
| 6 | `buildShuffle` 写 `v.Tails[j].SetChildren(stub)`（`builder.go:5457`） | Shuffle 子节点 | 否（`:602-603`） | 执行器自己持有 stub，不写计划 |
| 7 | 表达式求值的缓存 `builtinFuncCache`（按 `CtxID`，`expression/builtin.go`）；`CorrelatedColumn.Data`（Apply 内层逐行写） | 函数节点内部 | 前者是 | §3.7 |
| 8 | `RAND()` 的随机数发生器：`builtinRandSig.Clone` 共享 `mysqlRng`（`expression/builtin_math.go:1064`） | 发生器状态（内部加锁） | 是 | 与 Go 相同：多个会话命中同一计划时共享一个发生器（Go 每次命中克隆表达式，但克隆仍共享发生器）。N4 §3.5 已定为 `Arc<Mutex<MysqlRng>>` |
| 9a | 唯一索引点查执行器：`executor/point_get.go:212` 让 `e.idxVals` 直接引用 `p.IndexValues`，`:303` 调 `EncodeUniqueIndexValuesForKey`（`planner/core/util.go:369`），后者把各值转换成列类型（字符串、ENUM、`table.CastValue`）**写回同一个切片** | `PointGetPlan.IndexValues` 的元素 | 是 | **第 1 阶段完成条件**。执行器持有一份自己的 `Vec<Datum>`（从计划复制，或转换时生成新 `Vec`），转换结果写进它，不写计划。结果与 Go 相同：Go 写回的是本语句私有克隆（命中）或之后不再使用的原计划（未命中，缓存中是放入前的克隆） |
| 9b | `buildTableReader` 在 reader 的 schema 为空而表计划的 schema 非空时调用 `v.SetSchema(v.GetTablePlan().Schema())`（`executor/builder.go:3840-3843`，上游 `#50358`） | `PhysicalTableReader` 的 schema | 是 | **第 1 阶段完成条件**。执行器构建时用局部变量取"有效 schema"（为空则取表计划的 schema），执行器与结果列都用它，不写计划。若规划侧能保证 reader 的 schema 不为空，改在规划侧补齐（写在语句独占的树上，放入缓存之前），并加断言 |
| 9c | BatchPointGet：`PrunePartitionsAndValues`（`planner/core/point_get_plan.go:734`，由 `executor/builder.go:5555` 调用）在**非分区表**上也会原地过滤掉含 NULL 的 `IndexValues`（L746-753），分区表上还会原地改 `PartitionIdxs`（L743、L777-780） | `IndexValues`、`PartitionIdxs` | 是 | 第 2 阶段（BatchPointGet）：过滤结果作为返回值交给执行器，计划不变 |
| 9d | 外键级联：`fkc.plan.CascadePlans = append(...)`（`executor/foreign_key.go:752`） | `FKCascade.CascadePlans` | 否（带外键的 DML 克隆返回 `false`，§3.3） | 执行器局部保存级联计划（写路径阶段） |
| 10 | `generateNewPlan` 中 `stmtCtx.SetPlan`、`NormalizePlan` 写入 `PlanCacheStmt`；`planCachePreprocess` 写 `RelateVersion` / `tbls` / `SchemaVersion` | 语句上下文、会话的 `PlanCacheStmt` | — | 都是会话私有对象，不是计划节点，照 Go |

以上是按 `executor/*.go` 对计划类型字段赋值、`SetChild(ren)`、`SetSchema`、计划方法调用，以及**经别名写入**（执行器字段引用计划中的切片后写元素，如 9a）检查得到的清单；初稿漏了 9a–9d，所以不声称它是完整的。保证缓存不被污染的不是这张清单，而是 §3.1：执行器拿不到 `&mut` 计划节点，`make_mut_plan` 等在执行器 crate 中被 lint 禁止，计划中的 `Vec` 只能经 `&` 读，所以 Go 中这类写入在 Rust 里要么编译不过、要么 lint 报错，只能改写成执行器局部状态（上表最后一列）。D12 各块移植时，块 plan 逐处列出本块遇到的这类写入及其改写方式。

## 3. Rust 设计

### 3.1 不变式：缓存中的计划不可改

1. **计划是不可变的 `Arc` 树**（N1 §3.4），`PhysicalPlan: Send + Sync`。唯一的写入口是 `make_mut_plan` / `map_children`：节点被共享（引用计数 ≥ 2）时先 `clone_node`，再写副本。缓存始终持有一个引用，所以**任何**经这两个函数的写入都只会落在语句私有的副本上。这是 Go 需要"每次命中深拷贝"才能得到的保证，Rust 由所有权直接给出。
2. **节点内的内部可变性只允许三种**，每处在代码中注明属于哪一种，由 CI 检查：把现有的 `scripts/check_model_mutability.py`（N13 对元数据类型的同类检查）扩展到计划节点和表达式类型，出现 `Cell`、`RefCell`、`Mutex`、`RwLock`、`Atomic*`、`OnceCell`、`OnceLock` 时必须带 `// interior: <类别>` 注释：
   - (a) 只由不可变字段决定的惰性值（`OnceLock`，如表达式 hashcode）：共享无害；
   - (b) 与 Go 一样跨克隆共享的状态（`RAND()` 发生器）；
   - (c) 代价缓存：按 §3.6 处理。
   其他内部可变状态（例如求值缓存）不得放在节点里。
3. **执行器 crate 不能写计划**：`tidb-executor` 中用 `clippy::disallowed_methods` 禁止 `make_mut_plan`、`map_children`、`Arc::get_mut`（对计划类型）。执行器只拿 `&dyn PhysicalPlan` 或 `PhysicalPlanRef`。§2.3 第 2、5、6 行的 Go 写入因此只能写成执行器局部状态。
4. **节点不保存会话上下文**（`M-CTX`）。执行时计划引用会话状态的方式只有三种：
   - 参数：`Constant` 带 `param_marker` 序号，求值时从当前语句的 `EvalContext` 读本次的参数值（D13.1 §2 第 2 条、N4 §3.4）；
   - 推迟求值的表达式（`NOW()` 等 `DeferredExpr`）：按当前会话的上下文求值；
   - 其余（读时间戳、事务、内存 tracker、kill 信号、warning）都在执行器构建时由 `ExecCtx` 提供（N2）。
   表和索引的元数据是 N13 的 `Arc`，版本由 key 中的 schema 版本和 revision 固定。
5. **静态断言**：`PlanCacheValue: Send + Sync`、`InstancePlanCache: Send + Sync`；`RpnState: !Sync`（N4）。

### 3.2 命中时按参数改写：路径复制

```rust
// go: RebuildPlan4CachedPlan L1 [M-PLAN, M-CTX]
pub fn rebuild_plan_for_cached_plan(
    sctx: &dyn PlanContext,
    cached: &PhysicalPlanRef,          // 缓存中的根，只读
) -> Result<Option<PhysicalPlanRef>, Error> {
    let sc = sctx.stmt_ctx();
    if !sc.use_cache() { return Ok(None); }
    sc.set_in_prepared_plan_building(true);
    let _reset = scopeguard::guard((), |_| sc.set_in_prepared_plan_building(false));
    let mut p = cached.clone();        // 引用计数加一，不复制
    if let Err(err) = rebuild_range(sctx, &mut p) {
        sc.append_warning(errors::no_stack(format!("skip plan-cache: plan rebuild failed, {err}")));
        return Ok(None);
    }
    if !sc.use_cache() { return Ok(None); }
    Ok(Some(p))                        // 语句私有的根；未改写的子树与缓存共享
}
```

- `rebuild_range` 的分支与 Go 逐行对应。下降用 `map_children`，写字段用 `make_mut_plan`。因为缓存持有引用，被写的节点和它到根的路径上的节点被浅拷贝（字段复制，子节点、表达式、schema 是引用计数加一）。
- **相同则不写**：新算出的 range（handle、index values）与节点中现有的相等时不调用 `make_mut_plan`。重复相同参数的 EXECUTE 不复制任何节点。结果与 Go 相同（Go 写入的是相等的值）。
- 安全检查 `isSafeRange` 用的"缓存中的 range"就是缓存节点上的值，与 Go 在克隆上读到的相同（克隆时复制的就是这份）。
- 复制的节点保留原 plan id（Go 的克隆也保留），运行时统计按语句分开收集，不冲突。
- PointGet / BatchPointGet 的 `Handle`、`IndexValues` 与 TableScan 的 `Ranges` 同样处理（试点报告 §5 风险 4）。
- `probe_parents` 指向祖先：路径复制后，副本中的 `probe_parents` 仍指向缓存版本的祖先。它们只用于按 plan id 读运行时统计（EXPLAIN ANALYZE 的实际探测次数），id 相同，结果不变。VS1-24 决定的表示若不是按 id 引用，在 IndexJoin 进入第 2 阶段时复核。

每次命中的代价对比：

| | Go 实例级 | Go 会话级 | Rust |
|---|---|---|---|
| 放入 | 深拷贝整棵计划 | 不复制 | 不复制 |
| 命中 | `sync.Map` 查找 + **深拷贝整棵计划和全部表达式** + 原地重建 | 原地重建 | 分片查找 + 一次引用计数加一 + 只浅拷贝改写路径（相同参数时为 0） |
| 并发 | 无锁读；每次命中写 `lastUsed` | 会话私有 | 读锁持有几十纳秒；`lastUsed` 节流写（§3.4） |

### 3.3 放入：`PlanCacheValue` 与可缓存集合

```rust
// go: PlanCacheValue
pub struct PlanCacheValue {
    pub plan: PlanCachePlan,             // 物理计划；DML 进第 3 阶段时为 Insert/Update/Delete 外壳
    pub output_columns: Arc<NameSlice>,  // 只读
    pub param_types: Box<[FieldType]>,   // NewPlanCacheValue 中逐个 Clone
    pub stmt_hints: Arc<StmtHints>,      // 只读
    mem_bytes: i64,                      // 放入时算一次（§3.5）
}
```

- `PlanCacheValue::seal(plan, names, param_types, hints) -> Option<Self>`：先检查 `instance_cacheable`，不满足返回 `None`（对应 Go 的 `CloneForPlanCache` 返回 `false`，静默不放入）；再算 `mem_bytes`。
- `instance_cacheable`：`PhysicalPlan` trait 的方法，默认 `false`；§1.1 列出的 27 种算子覆盖为 `true` 并递归检查子节点、`probe_parents` 以及读取器内部的计划链（与 Go 克隆递归的范围相同）。新增算子时默认不可缓存，与 Go 一致。
- **不只看算子种类**：Go 生成的克隆函数在某些字段非空时也返回 `false`，`instance_cacheable` 必须逐条覆盖（`planner/core/plan_clone_generated.go`）：

  | 算子 | 字段非空即不可缓存 | Go 行 |
  |---|---|---|
  | `PhysicalTableScan` | `SampleInfo`（TABLESAMPLE）、`runtimeFilterList`、`AnnIndexExtra`（向量索引） | L57、L62、L65 |
  | `PhysicalIndexScan` | `GenExprs`（表达式索引） | L85 |
  | `PhysicalHashJoin` | `runtimeFilterList` | L188 |
  | `PhysicalTableReader` | `TableScanAndPartitionInfos`（分区信息） | L224 |
  | `Update` | `PartitionedTable`、`FKChecks`、`FKCascades` | L481、L484、L487 |
  | `Delete` | `FKChecks`、`FKCascades` | L505、L508 |
  | `Insert` | `FKChecks`、`FKCascades` | L529、L532 |

  另外，各克隆函数中"子计划或附属字段克隆失败则返回 `false`"的分支（如 `clonePhysicalPlansForPlanCache` 对 `probeParents`）按同样的递归范围覆盖。测试：每一行一个用例，Go 侧确认不缓存（第二次 EXECUTE `last_plan_from_cache = 0`、无 warning），Rust 相同；Go 文件升级时用脚本比对生成文件中 `return nil, false` 的条件列表与 Rust 的实现。
- 放入的就是本语句正在执行的同一个 `Arc` 树，不复制。放入之后本语句同样只读这棵树（§3.1 第 3 条保证）。

### 3.4 存储与并发

```rust
pub struct InstancePlanCache {
    shards: Box<[CachePadded<RwLock<HashMap<PlanCacheKey, SmallVec<[Arc<Entry>; 1]>>>>]>,
    tot_cost: AtomicI64,     // 字节（§3.5）
    tot_plan: AtomicI64,
    soft_limit: AtomicI64,
    hard_limit: AtomicI64,
    in_evict: AtomicBool,
    evict_lock: Mutex<()>,   // 同一时刻只有一个淘汰
}
struct Entry { value: Arc<PlanCacheValue>, last_used: AtomicU64 /* 单调时钟纳秒 */ }
```

- **分片**：分片数取 2 的幂，默认 `max(64, 4 × 线程池大小)`，按 key 的哈希选分片。哈希函数带进程启动时随机生成的种子（`foldhash` / `ahash` 的随机状态），避免用户构造的语句文本把条目集中到一个分片或一个桶。`parking_lot::RwLock`，锁内只做哈希查找、类型匹配和 `Arc::clone`，不分配、不求值、不跨 `.await`（`M-BLOCK`）。
- **同一 key 的多个计划**：`SmallVec` 中按放入顺序倒序保存（新的在前），查找取第一个 `checkTypesCompatibility4PC` 为真的，与 Go 的链表顺序相同。
- **Get**：命中时更新 `last_used`（正在淘汰时不更新，同 Go L92）。为避免每次命中都写同一条缓存行，只在新值比旧值大 1ms 以上时才写（决定 7）。
- **Put**：Go 的四个失败条件照做（正在淘汰、超过硬限额、已有兼容计划、并发冲突）。"已有兼容计划"的检查和插入在同一把写锁下完成，所以不存在 Go 的 CAS 失败；结果上是 Go 允许的情况之一。拿到写锁后**再查一次 `in_evict`**（同 Go L118 的第二次检查），淘汰已开始就放弃插入。
- **Evict**（后台任务，§3.8）：第一遍按分片逐个持读锁收集 `last_used`，用 L1 移植的 `calc_eviction_threshold` 算阈值；第二遍按分片逐个持写锁，把 `last_used ≤ 阈值` 的条目**移出到一个 `Vec`** 并减 `tot_cost` / `tot_plan`，删掉空 key，**释放写锁之后再 drop 这个 `Vec`**。计划树可能很大，最后一个引用的释放（递归 drop 整棵树）绝不在分片锁内发生。一次只锁一个分片，命中路径最多等一个分片的扫描（不含释放）。
- **释放**：条目被删除时只是缓存放掉一个引用。正在执行它的语句仍持有 `Arc`，语句结束时最后一个引用释放，内存确定性归还 jemalloc；不需要 GC，也不会出现悬空引用。`tot_cost` 在删除时立即减少（与 Go 相同：Go 删除节点后计划对象也要等 GC 才真正释放）。

**cache key 的表示**（Rust 的做法，等价于 Go 的字符串比较）：

```rust
pub struct PlanCacheKey {
    stmt_text: Arc<str>,           // PlanCacheStmt 中的 StmtText，PREPARE 时一次性建好
    env: SmallVec<[u8; 128]>,      // §2.1 其余各项按 Go 的顺序编码，字符串带长度前缀
    hash: u64,                     // hash(stmt_text) 在 PREPARE 时算好，再与 env 的哈希合并
}
```

- 语句文本可能很长，Go 每次 EXECUTE 都把它复制进新字符串再哈希。Rust 把文本的哈希留在 `PlanCacheStmt` 里，每次只编码几十字节的 `env`，写进会话复用的缓冲区；比较时先比 `Arc` 指针，不同再比内容。
- **"命中路径不分配"的前提**：(1) 会话的 key 缓冲区已在之前的 EXECUTE 中增长到足够大（第一次 EXECUTE 或 `env` 变长时会扩容一次）；(2) 不算参数类型数组：`parseParamTypes` 每次构造的 `FieldType` 列表放在会话复用的 `SmallVec` 中，参数个数超过内联容量时扩容一次后复用；(3) 只指查找本身（建 key、选分片、匹配、取 `Arc`）。之后的路径复制按设计要为被改写的节点分配（参数相同时为 0），执行器构建的分配不在此列。基准中用计数分配器只断言 (3) 的范围。
- 字符串字段带长度前缀，所以不会出现 Go 把用户名、主机名、库名直接拼接造成的串接歧义（`'a127.0.0.'@'%'` 与 `'a'@'127.0.0.%'` 得到同一个 key，在 v8.5.8 上复现，#308）。决定 9，台账 R16。
- **`delete_all`**（`ADMIN FLUSH INSTANCE PLAN_CACHE`，决定 8）：Go 实例级缓存没有这个操作。按分片逐个持写锁，把全部条目移出到 `Vec`、减 `tot_cost` / `tot_plan`，释放写锁后再 drop（与 Evict 相同的释放规则）；与 Evict 共用 `evict_lock`，不与淘汰并发。正在执行的语句仍持有自己的 `Arc`，不受影响。flush 之后的第一次 EXECUTE 未命中、重新优化并放入。
  - **flush 代数**（Lucky 对 #309 的意见）：共用 `evict_lock` 挡不住"flush 之前开始优化、flush 之后才 Put"的语句把旧计划放回去。缓存带一个 `flush_gen: AtomicU64`，`delete_all` 在清空前加一；语句在开始优化（`generateNewPlan` 之前，`NewPlanCacheKey` 之后）时记下当前值，Put 时在分片写锁下比较，变了就静默放弃（与 Go 其他 Put 失败一样不发 warning）。Evict 不加代数（淘汰后重新放入是正常的）。
- 时区偏移照 Go 用当前时刻计算（含夏令时切换的效果）。

### 3.5 内存记账

- **记什么**（决定 4）：条目实际持有的字节 = 计划树中各节点的结构体大小与堆上容量（`Vec::capacity × size_of`），加上节点引用的表达式、schema、`FieldType`、参数类型、输出列名、`StmtHints`。用 derive 宏 `#[derive(HeapSize)]` 为计划和表达式类型生成，放入时遍历一次。
  - 同一条目内被多处引用的 `Arc` 只计一次（遍历时按指针去重）；
  - N13 的长期共享元数据（`TableInfo`、`IndexInfo`、`ColumnInfo` 等）由 InfoSchema 拥有，不计入；被旧条目钉住的旧版本元数据也不计（Go 同样不计）；
  - 口径与 A1 §3.2 的 `Chunk.MemoryUsage` 一致（容量之和 + 结构体大小）。
- **限额语义与 Go 相同**：硬限额 = `tidb_instance_plan_cache_max_size`，`本条目 + 总量 > 硬限额` 时拒绝放入；软限额 = 硬限额 ×（1 − 保留比例），淘汰到软限额以下；阈值算法不变。
- **单条计划的大小上限**（`tidb_plan_cache_max_plan_size`，warning `plan is too large(decided by the variable @@tidb_plan_cache_max_plan_size)`）**仍按 Go 的估算公式**判断，因为它决定用户能看到的 warning。
- 与 A1 的关系：缓存不挂在语句 tracker 下（Go 也不记，A1 待定 7 的建议是只观测）；它的内存包含在实例级 jemalloc 统计中，参与 `tidb_server_memory_limit` 的判断，与 Go 堆中包含它相同。
- 数值与 Go 不同，按 A1 §3.5 不做数值对照：同样的 `max_size` 下 Rust 通常能放更多计划（不复制元数据、表达式共享）。

### 3.6 代价缓存与 EXPLAIN（§2.3 第 4 行）

Go 中每次命中都得到私有克隆，克隆带着放入时的代价值；之后任何代价计算（EXPLAIN 补算、`true_card_cost` 重算、慢日志和语句摘要生成二进制计划时的代价计算）都只写这个克隆，缓存中的原件永远保持放入时的代价值。未命中的语句在原计划 `p` 上执行，写入也不会到达缓存（缓存中是放入前的克隆）。

不能按"是否来自缓存"（`FoundInPlanCache`）来区分：放入缓存的那条语句本身就在已封存的树上执行；慢日志、语句摘要的二进制计划（默认打开）对普通语句也会算代价；快速路径的 PointGet 正是在那里第一次写代价缓存。Rust 规则：

1. `PlanCacheValue::seal` 时为树中每个节点置 `sealed` 位（`BasePhysicalPlan` 中一个只在发布前写一次的 `AtomicBool`，类别 (a)）；`clone_node` 产生的副本 `sealed = false`。
2. **封存的节点上照常计算代价，但不写回**：读代价缓存照旧（已初始化且不要求重算时直接返回放入时的值）；需要计算或重算时算出结果直接返回给调用方，不写入节点的 memo。这正是 Go 的实际语义（写入只落在之后被丢弃的克隆上），不需要复制整棵树。
3. 未封存的节点（语句独占的树、路径复制产生的副本）照旧写 memo，`FindBestTask` 期间的共享写（N1 §3 第 9 条）不受影响。
4. 代价：封存节点上同一语句内重复算代价时不能复用，会重算（只在 EXPLAIN、慢日志 / 语句摘要编码时发生，量小）。
5. 测试：两个代价因子不同的会话交替 `EXPLAIN FORMAT='verbose' EXECUTE` 同一缓存计划，`estCost` 各自与 Go 相同；放入缓存的语句开启慢日志二进制计划后，缓存中节点的 memo 不变；快速路径 PointGet 命中后的慢日志计划与 Go 相同。

### 3.7 表达式求值的缓存（N1 §7 第 2 条、试点报告 §5 风险 3）

- Go 的 `builtinFuncCache` 按 `CtxID` 缓存 LIKE 模式、正则、JSON schema 等，用 mutex 保护；Go 实例级缓存每次命中都克隆表达式，克隆从空缓存开始（Rust 现有移植 `crates/expression/src/builtin.rs` 的 `BuiltinFuncCache::clone` 也是如此）。所以 Go 中各会话从不共享这份缓存，每次命中都重新编译一次。
- 多个会话共享同一个 `Arc<Expr>` 时，如果仍把 `BuiltinFuncCache` 放在函数节点里，会争用同一把锁，并因 `CtxID` 不同互相冲掉缓存。
- **决定：按执行持有**（N4 §3.5）：编译出的模式放在每次执行的 `RpnState` 中（每个签名一个 `OnceCell`），逐行驱动和规划期求值（如重建 range 时求参数）用临时的状态。`RpnState` 不跨语句、不跨会话，不需要 `CtxID` 键。效果与 Go 实例级缓存相同（每次执行编译一次）。
- 要求：LIKE、REGEXP 族、`JSON_SCHEMA_VALID` 移植时（目前 `BuiltinFuncCache` 已定义但还没有签名使用）直接用 `RpnState`，不在节点上放缓存；`BuiltinFuncCache` 类型在第一个使用者移植前删除或改为 `RpnState` 的槽。在此之前，含这些函数的计划不会出现（函数未移植），实例级缓存可以先开。
- 可选优化（不在第 1 阶段）：模式参数是严格常量（`ConstStrict`，不含参数标记和推迟求值）时，编译结果只取决于节点的不可变字段，可以用节点上的 `OnceLock` 跨会话共享（类别 (a)，只缓存成功结果，错误照 Go 每次返回）。是否做由性能数据决定。
- `CorrelatedColumn` 只出现在 Apply 中，Apply 不可缓存；N4 已改为关联槽。

### 3.8 后台任务与系统变量

- 缓存由 `Domain` 持有（`Arc<dyn InstancePlanCache>`，接口在 `tidb-sessionctx`，实现在 `tidb-planner`，启动时注入，对应 Go `domain.NewInstancePlanCache` 的 init 注入，按 `layers.toml` 核对方向）。
- 淘汰任务：周期照 Go 固定 30 秒（`domain.go:3483`），不可配置、没有变量（决定 6），在运行时上运行，每处理一个分片让出一次（N3 §3.7）。记日志 `instance plan eviction` 与 metrics 同 Go。
- `ADMIN FLUSH INSTANCE PLAN_CACHE`（决定 8）：Go 的校验与 warning 照做，之后调用 `delete_all`（§3.4）。会话级的部分（`LastUpdateTime4PC`、`SetExpiredTimeStamp4PC`）照 Go 保留，因为没有会话级缓存，它们没有可观察的效果。
- metrics 任务：每 15 秒更新条数与内存 gauge（同 Go）。
- 限额：`SET GLOBAL` 的钩子（N8）立即更新 `soft_limit` / `hard_limit`（决定 5；Go 最多晚 15 秒）。
- `tidb_enable_instance_plan_cache` 存在一个进程级 `AtomicBool`（Go 的 `variable.EnableInstancePlanCache`），每次 EXECUTE 读一次。

### 3.9 与读时间戳、MDL、schema 版本的关系

- **计划中没有时间戳**。读时间戳在执行时取（`GetStmtReadTS`）；`AdviseOptimizeWithPlanAndThenWarmUp`（`executor/compiler.go:142`）只读计划形状决定是否用 MaxTS 等优化；stale read 的 `SnapshotTSEvaluator` 在会话的 `PlanCacheStmt` 中。`AS OF TIMESTAMP` 的字面量在语句文本里，进 key（Go 测试 `TestInstancePlanCacheStaleRead`）。
- **MDL 在查找之前**：`planCachePreprocess` 第 3 步逐表加 MDL 并更新 revision（D05.4 §3a 已移植），之后才算 key。MDL 出错时 revision 置为 `MaxUint64`，key 不会命中。
- **schema 版本**：key 含 PREPARE 时（或最近一次重新 preprocess 时）的版本和逐表 revision；RC 与 `FOR UPDATE` 读另含最新版本。DDL 之后旧条目不再被命中，留在缓存中直到淘汰；它们钉住的旧 `TableInfo` 也随之保留（Go 中旧指针同样保留）。
- **事务**：key 含 `InTxn`、autocommit 与脏表；事务内写过的表生成带 UnionScan 的计划，与无脏表的计划不同 key（Go 测试 `TestInstancePlanCacheInTxn`）。

## 3a. 方案比较

| 方案 | 做法 | 结论 |
|---|---|---|
| a. 照 Go：放入和命中都深拷贝 | 实现 `CloneForPlanCache` 的 Rust 版 | 浪费 Rust 的不可变共享；每次命中复制整棵树和全部表达式。**否决** |
| b. 不可变 `Arc` 树 + 路径复制（本 spec） | §3.2 | N1 已确认的机制；执行器构建逻辑保持 L1 |
| b′. 只换分片内部结构：分片改为 `ArcSwap<HashMap>`（读用 `arc-swap` 的 guard，不写共享计数器）或 seqlock 式读，`Entry` 中的计划用借用而不是 `Arc::clone` 交给本语句（语句结束前 guard 保证不释放），`last_used` 按决定 7 节流 | 读路径不写任何共享状态 | 写路径（Put、淘汰）要复制分片的表，分片小时代价可控；不改计划表示，也不改执行器。§8 风险 1 的第一后备 |
| c. 不复制节点，按 plan id 把本次的 range 放进旁表，执行器构建时优先读旁表 | 命中时只一次 `Arc::clone` | 每个读取器的执行器构建都要改成先查旁表，偏离 L1；与 N1 §3.5 不符。作为热点争用的后备，见 §8 风险 1 |
| d. 每个工作线程一份缓存副本 | 线程本地查找 | 内存乘以线程数，淘汰复杂。只作为 §8 风险 1 的后备 |

## 4. 与 Go 的差异（全部登记，无一可被确定性的 SQL 观察到，除非另注）

1. 不复制计划（内部）。
2. 内存数值（决定 4）：同一 `max_size` 下条目数不同；淘汰时刻不同（Go 的淘汰本来就由 30 秒周期和时间戳决定，不确定）。metrics 中的内存数值不同（A1 §3.5）。
3. 限额立即生效（决定 5）。
4. `last_used` 1ms 内的写节流（决定 7）：同一毫秒内被访问的条目在淘汰阈值上并列。
5. key 无串接歧义（决定 9）：仅在用户名 / 主机名 / 库名恰好能拼接成相同字符串时，Go 共享而 Rust 不共享（`last_plan_from_cache` 不同）。Go 缺陷的修正，#308，台账 R16，写进 `docs/compatibility.md`。
6. 默认值与会话级缓存不存在带来的差异：见决定 1、2、3，属**特性例外**，用户可见，台账 R14，写进 `docs/compatibility.md`。
7. `ADMIN FLUSH INSTANCE PLAN_CACHE` 清空实例级缓存（决定 8）：Go v8.5.8 不清空（flush 后仍 `last_plan_from_cache = 1`，#307 有复现）。按宪章第一条 2（2026-11-21 确认）作为 Go 缺陷修正，台账 R15，写进 `docs/compatibility.md`。

## 5. 验收

### 5.1 行为对照

- **govec `plancache_instance`**：Go 端用 `tidb_enable_instance_plan_cache = ON`（或测试上下文 `PlanCacheKeyEnableInstancePlanCache`）生成。每个用例是一串语句（多会话），每条记录：结果、`SHOW WARNINGS`、`@@last_plan_from_cache`、`@@last_plan_from_binding`、EXPLAIN（去掉 estRows 与代价）。覆盖：
  - key 的每一项各翻转一次（§2.1 的 10 组，约 25 个字段），翻转后未命中、恢复后命中；
  - 参数类型兼容矩阵（`checkTypesCompatibility4PC` 的各分支，含 DECIMAL 精度、unsigned、VARCHAR/VAR_STRING、charset、collation）；
  - 同一 key 多计划时的选择顺序；
  - 重建失败（不安全 range、`convertConstant2Datum` 转换后不等）的 warning 与退回；
  - 不可缓存原因（`TestInstancePlanCacheReason` 的各条）与"实例级不可克隆"的静默不缓存；
  - schema 变更、事务与脏表、stale read、不同用户、不同 collation / charset（Go `casetest/instanceplancache/others_test.go` 的 `Vars`、`Reason`、`StaleRead`、`InTxn`、`SchemaChange`、`PrivilegeChanges`、`DifferentCollation`、`DifferentCharset`、`DifferentUsers`、`Plan`、`WithDualTable`；`Binding`、`Partitioning` 随第 3、2 阶段）；
  - 跨会话命中：会话 A 执行两次后，会话 B 第一次 EXECUTE 即 `last_plan_from_cache = 1`。
- **单测移植**：`plan_cache_instance_test.go` 的 `Basic`、`WithMatchOpts`、`ConcurrentRead`、`ConcurrentWriteRead`（接口按 Rust 改写，断言不变，数值类断言按 §3.5 改为性质）；`calc_eviction_threshold` 按 Go 的输入输出逐例对照。
- **ab**：sysbench 预处理语句与 integrationtest 的 plan cache 子集，Go `tidb_enable_instance_plan_cache = ON` 对 tidb-rs，逐条比较结果、warning 与 `last_plan_from_cache` 序列。`strict` 模式下 VS1 形态 100% 一致。
- **随机对照**：移植 Go `main_test.go` `TestInstancePlanCache` 的做法：同一组随机参数，缓存开 / 关的结果必须相同。

### 5.2 并发

- 移植 `concurrency_test.go`（`Sysbench`、`Comp`）与 `concurrency_tpcc_test.go` 中 VS1 形态能跑的部分：N 个会话并发 EXECUTE（含 Put、命中、重建、淘汰同时进行），结果与单线程一致。
- 缓存不可改：并发命中与重建前后，对缓存中每个条目计算结构哈希，必须不变；`make_mut_plan` 复制计数：点查命中 = 1，相同参数 = 0，range 语句 = 根到 reader 路径上的节点数。
- flush 与 Put 并发（确定性测试）：会话 A 记下 flush 代数后在优化与 Put 之间暂停（测试钩子），会话 B 执行 `ADMIN FLUSH INSTANCE PLAN_CACHE`，放开 A：A 的 Put 被放弃，缓存为空，A 与 B 下一次 EXECUTE 都是 `last_plan_from_cache = 0`；无 flush 时同样的步骤 A 的 Put 成功。
- 淘汰与命中并发：被淘汰的条目在仍有语句使用时结果正确，最后一个引用释放后内存归还（用计数分配器断言）。
- 静态断言见 §3.1 第 5 条；`tidb-executor` 的 `disallowed_methods` lint 生效（构造一个违规样例确认会报错）。

### 5.3 内存

- `HeapSize` 精度：在独立线程中构建条目，与 jemalloc `thread.allocated` 的增量比较，误差 ≤ 10%（去重与元数据排除规则另有单测）。
- 硬限额：总量 + 新条目 > 硬限额时 Put 失败；软限额：Evict 后总量 < 软限额（条目大小相近时）；保留比例为 0 / 1 的边界；`max_size` 调小后下一次淘汰生效。
- 无泄漏：大量不同语句填满、淘汰、清空后，jemalloc 已分配量回到基线 ±1%；`tot_cost` 与各条目 `mem_bytes` 之和始终相等。

### 5.4 性能（负责人的门槛）

- **sysbench read_only**（预处理语句；VS1 能执行的子集，含 point_select、simple_ranges、order_ranges，sum / distinct 在聚合可用后加入），1 / 8 / 32 / 64 线程，QPS **≥ 1.0 × Go v8.5.8（Go 默认配置，即会话级 plan cache 打开）**，同时报告对 Go 实例级缓存打开时的比值。环境与次数按 perf gate（#263）。
- 每次 EXECUTE 的规划耗时（命中时）p50 / p99 ≤ Go 会话级缓存命中时的耗时（`timing.rs` 分阶段计时）。
- 热点争用：64 线程跑单一语句时，命中路径中原子操作与锁的耗时占比（perf 采样）≤ 5%，否则启用 §8 风险 1 的后备。

## 6. 负责人决定（2026-11-23，#305）

11 项由负责人在 #305 决定（Lucky 评审 #304 时同意全部建议）。1–5、7、9、11 按原建议；6、8、10 与原建议不同。下面每项写决定、理由与去处；原建议的备选方案不再列出。

1. **`tidb_enable_instance_plan_cache` 默认 ON**（按建议，特性例外）。Go 默认 OFF（实验特性），因为 Go 默认有会话级缓存；tidb-rs 没有会话级缓存，保持 OFF 就意味着默认完全没有 plan cache。默认 ON 让用户仍然得到"预处理语句命中缓存"，与 Go 的默认体验一致。
   - 可见影响：`SELECT @@global.tidb_enable_instance_plan_cache` 显示 ON；默认就有跨会话命中（新会话的第一次 EXECUTE 可能就是 `last_plan_from_cache = 1`）。混合集群中这个 GLOBAL 值持久化在 `mysql.global_variables`，以引导集群的节点写入的值为准（N8 中核对加载时是否调用 `SetGlobal` 钩子；§8 风险 6）。
   - 去处：台账 R14，`docs/compatibility.md` Plan cache 一节。
2. **`tidb_enable_instance_plan_cache = OFF` 时完全不缓存**（按建议）。`last_plan_from_cache` 恒为 0，不发 warning（Go 关掉时也不发）。写进 `docs/compatibility.md`（与第 1 项同一行，台账 R14）。
3. **会话级变量照常接受、校验、保存，不起作用，不发 warning**（按建议）。`tidb_enable_prepared_plan_cache` 照 Go 作为"本会话是否用缓存"的开关（Go 实例级模式也看它）；`tidb_session_plan_cache_size`、`tidb_prepared_plan_cache_size`、`tidb_prepared_plan_cache_memory_guard_ratio` 在 Go 实例级模式下同样静默不起作用。不把 `tidb_session_plan_cache_size` 映射成实例级的条数上限。
4. **`tidb_instance_plan_cache_max_size` 按条目实际持有的字节计量**（按建议，§3.5）。与 A1"按实际分配记账、数值不对照"一致，限额真正约束内存；要为计划和表达式类型 derive `HeapSize`，同一设置下条目数与 Go 不同。`tidb_plan_cache_max_plan_size` 仍按 Go 公式（warning 相同）。
5. **限额在 `SET GLOBAL` 后立即生效**（按建议）。Go 每 15 秒从变量刷新一次，这个延迟不是设计意图，也不是确定性行为。
6. **淘汰检查周期照 Go 固定 30 秒，不可配置**（负责人 2026-11-23 确认）。不加变量。注意 30 秒是淘汰**检查周期**，不是条目的 TTL：每次检查只在总量超过软限额时淘汰最久未用的条目，Go 和 tidb-rs 都**没有按条目的 TTL**。
   - "Put 因硬限额失败时提前唤醒淘汰"仍不做；若 §5.3 的测试或线上显示缓存长时间冻结，再按数据考虑。
7. **`last_used` 1ms 内不重复写**（按建议），避免 64 线程命中同一计划时每次都写同一条缓存行。1ms 内的访问顺序在淘汰阈值上并列，不可观察。
8. **`ADMIN FLUSH INSTANCE PLAN_CACHE` 真正清空实例级缓存**（与原建议不同）。Go v8.5.8 中它只清本会话的会话级缓存并记一个实例级缓存从不读的时间戳（`executor/simple.go` `executeAdminFlushPlanCache`），打开实例级缓存时等于什么也不做（v8.5.8 上复现：flush 后本会话和新会话的 EXECUTE 仍 `last_plan_from_cache = 1`）。负责人：这是管理语句，应该生效。
   - 按宪章第一条 2 的例外（2026-11-21 确认）作为 Go 缺陷修正：开 issue #307（`upstream-pitfall`，含 v8.5.8 复现），台账 R15，`docs/compatibility.md` Plan cache 一节。
   - 补充（2026-11-24 负责人决定，台账 R18）：执行该语句要求 `SUPER`，与其他 ADMIN 语句一致（Go 不检查，因为它清不掉实例级缓存）。权限检查随用户身份与权限的移植生效，在那之前任何用户都能执行。
   - 设计：`delete_all`（§3.4）；校验、warning 照 Go（§1.1、§3.8）。
9. **key 的字符串字段带长度前缀**（按建议）。Go 直接拼接用户名、主机名、库名、语句文本，不同账号可以得到相同的 key（v8.5.8 上复现：`'a127.0.0.'@'%'` 与 `'a'@'127.0.0.%'`，后者第一次 EXECUTE 就命中前者的计划；命中后仍会再检查权限，不是权限漏洞）。开 issue #308（`upstream-pitfall`），台账 R16，`docs/compatibility.md` Plan cache 一节。只在名字恰好能拼成相同字符串时可观察。
10. **缓存设计覆盖非预处理语句**（与原建议不同）。原建议本 spec 不做。负责人指出 Go 中非预处理语句也能用实例级缓存；核对 v8.5.8：`lookupPlanCache` 由两者共用，打开实例级缓存时非预处理语句（`tidb_enable_non_prepared_plan_cache = ON`，默认 OFF）的计划也放在实例级缓存中。
    - 范围见 §1.3：缓存本体、key、重建对两者相同；非预处理特有的常量参数化、可缓存判定、快速解析路径是之后的阶段。
    - 实现顺序：先预处理语句（第 1 阶段），非预处理语句随后接上（§7 第 4 阶段）。在那之前设为 ON 时照常接受、不起作用（D13.1 现状）。
11. **VS1-39 改为必做**（按建议）。VS1 plan §10-3 原定"read_only < 0.9× 才做"；负责人的性能门槛是"不慢于打开 plan cache 的 Go"，range 语句每次都完整优化，不太可能达到 1.0×。排在 VS1-32 的测量之后、默认值切换之前。

## 7. 分阶段

### 第 1 阶段：VS1 形态的预处理语句（VS1-39，约 1.1k 行含测试；原估 650 行未含 key 与淘汰的全部字段）

前置：VS1-20（预处理点查在新栈跑通，#300–#303）、VS1-28 第 1 部分（TableReader / TableScan 的物理计划）。

- key：`NewPlanCacheKey` 全部字段 L1。还没有的块按 Go 在同样条件下的值计算并标注：无绑定（bindinfo 未移植时 `MatchSQLBindingForPlanCache` 返回空，不是 `Blocked`，因为 Go 在没有绑定时就是这个结果）；统计版本取统计句柄的版本（只有 pseudo 时为 0，同 Go `getLatestVersionFromStatsTable` 的第 1 种情况）；表达式下推黑名单时间戳读已移植的值。
- `parseParamTypes`、`checkTypesCompatibility4PC`、`lookupPlanCache`、`adjustCachedPlan`、`generateNewPlan` 的 Put、`instancePlanCacheEnabled`：L1（去掉克隆）。
- 缓存本体、淘汰、记账、后台任务、三个变量（默认 ON 等决定 1–3）、`delete_all` 与 `ADMIN FLUSH INSTANCE PLAN_CACHE`（决定 8）、metrics、`FoundInPlanCache` / `last_plan_from_cache`。
- 重建（`rebuildRange` 中 VS1 用得到的分支）：
  - `PointGetPlan`：`HandleConstant`（int handle，`convertConstant2Datum`）、`IndexConstants`（唯一索引）、`AccessConditions` 非空的两条路径；
  - `PhysicalTableReader` → `PhysicalTableScan`：int handle（`BuildTableRange`），common handle 若 VS1 有则走 `DetachCondAndBuildRangeForIndex`；
  - 其余算子（Selection、Projection、Sort、TopN、Limit、StreamAgg、HashAgg、TableDual）走"递归子节点"分支。
- `instance_cacheable`：上面这些算子与 27 种中已移植的；其余默认 `false`。
- 代价缓存规则（§3.6：封存节点不写回）与 `sealed` 位；执行器 lint（§3.1 第 3 条）。
- **完成条件**：§2.3 第 9a 行（唯一索引点查执行器不写回 `IndexValues`）与第 9b 行（`buildTableReader` 不对计划 `SetSchema`）已按该表改写，各有回归测试：缓存中的计划在执行后结构哈希不变，且结果与 Go 相同。
- `instance_cacheable` 覆盖 §3.3 字段表中第 1 阶段算子的行（TableScan、TableReader）。
- 其余分支（IndexScan、IndexReader、IndexLookUp、BatchPointGet、IndexJoin、IndexMerge、DML、分区）是 `blocked-by:<块>` 的边界桩：遇到时**不缓存**（`instance_cacheable` 为 `false`），而不是报错，与 Go 不可克隆时的行为相同，结果仍正确。这一点在 ab 中显示为 `last_plan_from_cache` 与 Go 不同，按"块未到位"登记，不算 bug。

#### 第 1 阶段实现记录（2026-11-23，VS1-39）

已完成（代码位置见各条）：

- 缓存本体 `crates/planner/src/core/plan_cache_instance.rs`：分片 `RwLock` 哈希表（分片数 `max(64, 4 × CPU)` 的 2 的幂，按 key 哈希的第 32 位起选分片），同 key 多计划新的在前，Put 的四个失败条件、写锁下复查 `in_evict` 与 flush 代数，Evict 两遍扫描 + L1 移植的 `calcEvictionThreshold`，被移出的条目在释放全部锁之后才 drop；`delete_all` 与 Evict 共用 `evict_mutex`。
- key `PlanCacheKey`（`plan_cache_utils.rs`）：语句文本单独保存、哈希按文本缓存；其余字段按 Go 顺序编码，字符串带长度前缀（决定 9），整数为定长小端（key 只比较、不显示）；key 缓冲在 `PlanCacheStmt` 中复用。`parseParamTypes`、`checkTypesCompatibility4PC`、`NewPlanCacheValue`（含 `instance_cacheable` 检查与封存）、`MemoryUsage`（实际字节）。
- 命中与放入（`plan_cache.rs`）：`lookupPlanCache` / `adjustCachedPlan` / `generateNewPlan` 的 Put，`FoundInPlanCache`、命中 / 未命中 metrics、`StmtHints` 恢复；重建 `plan_cache_rebuild.rs`（PointGet 的三条路径、TableReader → TableScan 的 int handle 与 common handle，其余算子递归子节点；相同参数不写、不复制）。
- `instance_cacheable`：PointGet、TableReader、TableScan、Selection、Projection、Sort、TopN、Limit、TableDual（§3.3 字段表中第 1 阶段两行的字段 Rust 都不携带，只看子节点）；NominalSort 与其余默认 `false`。
- `sealed` 位与代价缓存（§3.6）：`BasePhysicalPlan.sealed`，`clone_node` 的副本不带；封存节点算出的代价直接返回、不写 memo。
- domain（`crates/domain/src/plan_cache.rs`）：持有缓存、30 秒淘汰与 15 秒 metrics 两个后台任务（tokio，淘汰放在 blocking 池）；限额在 `SET GLOBAL` 钩子中立即生效（决定 5，经 `tidb_sessionctx::context` 的登记表）。
- 默认 ON（决定 1）：`TIDB_RS_DEFAULT_OVERRIDES`（生成的 sysvar 表保留 Go 的值，govec 回放按 Go 值比较）。
- `ADMIN FLUSH INSTANCE PLAN_CACHE`（决定 8）：`executor/simple.rs` 的 `executeAdminFlushPlanCache`；解析器还没有 ADMIN 的 AST（D02.2），路由器按语法分支 `AdminStmt#30` 识别，在任何路由模式下都交给新会话执行。
- 执行器 lint（§3.1 第 3 条）：`crates/executor/clippy.toml` 禁止 `make_mut_plan` 与 `map_children`（违规样例已确认报错）。`Arc::get_mut` 不能按类型禁止，没有加（对缓存中的计划它总是返回 `None`）。
- §2.3 第 9a 行：点查执行器已经持有自己的 `idx_vals`（从计划复制）；回归测试 `unique_index_point_get_does_not_write_the_cached_index_values`（会话级，整数与字符串参数各执行两次，缓存计划结构哈希不变）。第 9b 行：`buildTableReader` 尚未移植（D12.1，VS1-34），先移植它的这一段为 `table_reader_schema`（读、不写），测试 `empty_reader_schema_is_read_from_the_table_plan_not_written`；VS1-34 用它构建执行器。
- PoC 回答 `@@last_plan_from_cache` / `@@last_plan_from_binding`：读路由器的"上一条语句"状态（VS1 plan §4.5），ab 用它比较。

与本 spec 的差异（实现时的选择，请评审）：

1. `HeapSize` 不是 derive 宏，而是 `plan_cache_heap_size.rs` 中按算子手写的遍历（只覆盖第 1 阶段可缓存的算子；其余算子 `instance_cacheable` 为 `false`，到不了这里）。精度测试用计数分配器比较条目最后一个引用释放时归还的字节：点查两种形状都是 0 误差（2335 / 2718 字节）。新增可缓存算子时要同时扩展这个遍历（§8 风险 4 的缓解改为评审检查项）。
2. key 中的用户名、主机名为空：新会话还没有 `SessionVars.User`（会话只接受一个账号）。账号只有一个时与 Go 不可区分；会话携带用户身份时按决定 9 写入。
3. 统计信息版本恒为 0（没有统计信息句柄，同 Go 只有 pseudo 时）；脏表为空（`TblInfo2UnionScan` 是写路径）；绑定为空（D13.2）。
4. `SET GLOBAL` 还到不了新会话（全局值是编译时默认值，D06.5 / VS1-17），所以 `tidb_enable_instance_plan_cache` 与两个限额在 tidb-rs 上暂时改不了；钩子已接好。
5. `IsSafeToReusePointGetExecutor`（会话私有的点查执行器复用，§1.1 最后一行）不在本阶段；每次 EXECUTE 仍新建点查执行器。（更新：VS1-20b 移植了这个函数，自动提交的预处理点查照 Go 走点查短路径，MaxTS 时照 §1.1 最后一行复用会话自己预处理语句里的点查执行器，D05.3 spec §10。）
6. §3.1 第 2 条（把 `check_model_mutability.py` 扩展到计划与表达式类型）未做；新增的内部可变字段都加了 `// interior:` 注释。
7. §5.4 的 64 线程争用测量、命中路径计数分配器断言未做（见下面的数字）。

验证：

- 单测：`plan_cache_instance_tests.rs`（Go `plan_cache_instance_test.go` 的 Basic、WithMatchOpts、ConcurrentRead、ConcurrentWriteRead 照搬断言；淘汰 + flush 与命中并发；`calcEvictionThreshold` 逐例；flush 代数的确定性测试；参数类型匹配顺序；key 长度前缀；封存节点不写代价；路径复制只复制被改写的路径）；会话级：两个会话共享、跨会话第一次 EXECUTE 命中、相同参数复制 0 个节点、不同参数复制 1 个、执行后结构哈希不变、flush 后重新优化、会话关闭 `tidb_enable_prepared_plan_cache` 后不查缓存；路由器：`ADMIN FLUSH` 的三种范围与 warning。
- govec：`plancache` 领域新增 op `types_compat`（`checkTypesCompatibility4PC`，780 条，arm64 本地生成，amd64 取 tracker CI 产物）。其余函数的豁免理由已更新。
- ab（本地 playground，Go v8.5.8 打开实例级缓存，`strict`，2 张表 × 1 万行、4 个 region）：`point_select / ps` 136 条中 128 条由新栈执行且与 Go 一致，0 不一致，8 条为超出 INT 范围的参数（Go 是 TableDual，`blocked-by:D10.1`，与 #303 相同）；`--plan-cache`（每条 EXECUTE 之后读 `@@last_plan_from_cache`）：`point_select` 136/136 与 Go 相同（双方 128 条为 1）；范围查询由 PoC 回答，Go 为 1、tidb-rs 为 0（新栈还不执行它们，不是本项的差异）。
- 性能（sysbench `oltp_point_select`，1 张表 1 万行，每轮 20 秒，`fallback`）：对 Go 实例级缓存 ON：1 线程 0.98×、8 线程 0.95× / 0.93×（两轮）；对 Go 默认配置（会话级缓存）：0.95× / 0.91×。之前（无缓存，#303）为 0.93× / 0.89×。每条点查约 506 次分配（jemalloc `nrequests`，10 万条与 1 条两次运行之差），之前约 566，Go 约 200。全部 75.5 万条语句由新栈执行，plan cache 命中 755405、未命中 1。

### 第 2 阶段：索引与分区（随 VS1-28 第 2 部分、D10.x、D12.x）

`buildRangeForIndexScan`、IndexReader / IndexLookUp（经链写，N1 §3 第 9 条）、BatchPointGet、`PrunePartitions` 改为执行器局部（§2.3 第 2 行）、IndexJoin 的可变 range（`clone_node` 时深拷贝 `mutableIndexJoinRange`）、IndexMerge。

### 第 3 阶段：写路径与其他

Insert / Update / Delete 的 `SelectPlan`（及点更新）、绑定（`FoundInBinding`、`ignore plan cache by binding`）、真实统计信息下的失效、`EXPLAIN FOR CONNECTION`。

### 第 4 阶段：非预处理语句（决定 10；第 1 阶段之后，可与第 2、3 阶段并行）

缓存本体与命中、放入、重建复用第 1–3 阶段的实现，本阶段只做非预处理语句特有的入口：`getPlanFromNonPreparedPlanCache`（`planner/optimize.go`）的条件检查、`NonPreparedPlanCacheableWithCtx` 及其原因文本、常量参数化 `ParameterizeAST` / `GetParamSQLFromAST` / `ParseParameterizedSQL`（`plan_cache_param.go`）、会话的参数化语句缓存、`isNonPrepared` 分支（`SetCacheType(SessionNonPrepared)`、warning `skip non-prepared plan-cache: …`、non-prepared metrics）。快速解析路径（避免每条语句完整解析再参数化）随性能数据决定，是本阶段之后的优化。验收：govec 与 ab 在 `tidb_enable_non_prepared_plan_cache = ON` 时对照 `last_plan_from_cache` 与 warning；能被第 1 阶段算子覆盖的语句与预处理语句同样命中。

## 8. 风险

1. **热点计划的缓存行争用**。所有会话执行同一语句时，分片读锁、根节点的引用计数、`last_used` 都落在少数几条缓存行上。Go 在这里只有原子读（以及每次命中写 `lastUsed`）。缓解：读锁只在取 `Arc` 的瞬间持有；`last_used` 节流（决定 7）；路径复制在参数相同时为 0。定量地看：每次命中有 4 次对共享缓存行的原子读改写（读锁获取、释放，根 `Arc` 的加一、减一），Go 只有 1 次写（`lastUsed`）。64 线程跑同一语句时，这 4 次 RMW 很可能是命中路径的主要开销。若 §5.4 的争用指标不达标，后备依次是：方案 b′（只换分片内部结构，读路径不写共享状态，§3a）；方案 c（旁表）；方案 d（线程本地副本）。采用时修订本 spec。
2. **路径复制的规模**。range 语句每次命中都要复制根到 reader 的路径，通常是 root task 中的大部分节点（浅拷贝）。节点结构体较大时（如带多个 `Vec` 的 reader），浅拷贝的分配可能成为热点。§5.2 的复制计数和 §5.4 的耗时会暴露。
3. **执行期写计划的位置可能有遗漏**。§2.3 基于 v8.5.8 执行器的逐处检查；D12 各块移植时，Rust 的所有权与 lint 保证遗漏的写入不会污染缓存（只会多一次复制或编译失败），但代价缓存一类的内部可变性要靠 §3.1 第 2 条的 grep 检查。
4. **`HeapSize` 的覆盖**。新增计划或表达式字段时忘记 derive 会少计。缓解：derive 是必需的 trait 约束（`PhysicalPlan: HeapSize`），编译期强制；§5.3 的精度测试。
5. **N4 尚未确认**。§3.7 依赖 N4 §3.5 的 `RpnState`。若 N4 改变方案，`BuiltinFuncCache` 的去处要在本 spec 中重新定；在含 LIKE / REGEXP 的计划能被缓存之前必须定下来。
6. **默认值的混合集群影响**（决定 1）：GLOBAL 变量的持久值由先引导集群的一方决定，tidb-rs 节点加入 Go 引导的集群时会读到 OFF。

## 9. 评审检查项

- [ ] §2.1 的 key 字段与 Go 逐项对照，顺序一致。
- [ ] `instance_cacheable` 的算子集合与 `plan_clone_generated.go` 的 27 种一致，新增算子默认 `false`。
- [ ] 命中路径不分配（基准中用计数分配器断言），读锁内无求值、无 `.await`。
- [ ] 执行器 crate 的 `disallowed_methods` lint 已加；计划与表达式类型的内部可变性都有类别注释。
- [ ] 代价缓存的 `sealed` 规则有测试。
- [x] 待定 1–11 已由负责人逐项决定（2026-11-23，#305），结果写回本 spec（§6）、`docs/compatibility.md` 与偏差台账（R14–R16）。
