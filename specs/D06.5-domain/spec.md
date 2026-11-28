# D06.5 schema 同步与 domain：spec

块 D06.5（ROADMAP P2 L4）：`pkg/domain`。crate `tidb-domain`（N0 rank 12，本 PR 建立，去掉 `planned`）。

状态：**第 1 部分**（VS1-17：schema 加载），由 PoC domain 的同步循环驱动。**第 3 部分**（§8，#521）：sysvar 缓存接入新栈，新会话的全局变量取集群 `mysql.global_variables` 中的值。第 2 部分（VS1-17b：schema validator / checker、reload 循环、lease）等 N11 的 lease 规则确认后再做（§4）。domain 的其余部分（统计、权限、绑定、TTL、infosync、server ID 等后台任务）不在 VS1 范围内。

## 1. 范围

第 1 部分移植 `domain.go` 中 schema 加载用到的 16 个函数：

- `loadInfoSchema`、`tryLoadSchemaDiffs`、`fetchAllSchemasWithTables`、`splitForConcurrentFetch`、`fetchSchemasWithTables`、`fetchPolicies`、`fetchResourceGroups`、`shouldUseV2`、`canSkipSchemaCheckerDDL`；
- `Reload`（时间戳由调用方给出，见 §2.2）、`InfoSchema`、`InfoCache`、`Store`、`GetSnapshotInfoSchema`、`GetSnapshotMeta`；
- `getTimestampForSchemaVersionWithNonEmptyDiff` 是边界桩（§3）。

另外按 Go 移植了 `NewDomain`（只建 store 和 `InfoCache`，L2）、`getFlashbackStartTSFromErrorMsg`、`mathutil.Divide2Batches`。

`InfoCache`（`pkg/infoschema/cache.go`）属于 D06.3 第 2 部分（任务 T8），见 D06.3 spec §10。

## 2. 设计

### 2.1 `Domain`

只保留 schema 加载要读的字段：store、`InfoCache`、reload 锁 `m`。

- Go 把 `do` 本身作为 builder 的 `autoid.Requirement`（只用 `Store()`）；Rust 用一个持有同一个 store 的 `StoreRequirement`。
- `m` 是 `tokio::sync::Mutex`：Go 的 `Reload` 在锁内做全部读取，Rust 的读取是 `async`，锁要跨 `await` 持有（M-LOCK 允许的情形：锁只保护"同一时间只有一个 reload"）。
- `InfoCache` 的容量取 `tidb_schema_version_cache_limit`（默认 16），与 Go 的 `NewDomain` 相同。

### 2.2 由 PoC domain 驱动（VS1 plan §4.1）

在 D06.5 整块完成之前，etcd 注册、同步循环、MDL、lease 仍由 PoC 的 `poc-domain` 负责。只有新栈打开时（`--new-stack` 不是 `off`）才建立这个 domain；`off`（默认的生产模式）不建立，PoC 的同步循环与之前完全相同（测试 `off_mode_creates_no_ported_domain`、`ported_domain_only_with_the_new_stack_on`）。PoC 每次 reload 取一个 TSO，先加载 PoC 自己的 schema，再在**同一个时间戳**上调用 `Domain::reload_at(ts)`。所以两边读到的是同一个 schema 版本（都取 `GetSchemaVersionWithNonEmptyDiff`），新的一边用 diff 增量加载。

- `reload_at` 是 Go `Reload` 在 `store.CurrentVersion` 之后的部分；`reload` 是完整的 `Reload`（自己取 TSO），留给 domain 自己的循环（第 2 部分）。
- Go 的 `Reload` 在加载后更新 etcd 中的 self version、全量加载时重置 validator、续 lease。这三件事目前由 PoC 做，`reload_at` 把它们需要的信息（`hitCache`、旧版本、`RelatedSchemaChange`、时间戳）返回给调用方。
- 新栈的会话从 `Domain::info_schema()` 取最新版本。路由器在每个命令开始时取一次（`NewStack::refresh_info_schema`），第 0 步比较的就是这一版，语句全程使用它（VS1 plan §4.1："比较一次，之后固定"）。版本不等时语句走 PoC。
- 新的 domain 加载失败只记日志：新栈停在旧版本上，版本不等，语句走 PoC，直到下一次 reload 成功。

PoC 侧的 store 是 `poc-domain` 中的 `PocStorage`：基于 `poc-kv` 的 `kv.Storage`（点查、批量点查、正向迭代器用缓冲的 scan 实现，足够读 meta hash；schema 加载不发 coprocessor 请求，`GetClient` 返回的 client 对每个请求返回错误，不 panic）。原因是移植的 store driver 还没有 `kv.Storage` 和迭代器（D07.1 第 2 部分）。这是 glue，D07.1 完成后去掉。

### 2.3 只有 infoschema v2

D06.3 只移植 v2（D06.3 spec §1.2）。所以：

- `enableV2` 恒为真（`tidb_schema_cache_size = 0` 是不限容量的 v2，D06.3 spec §5.4）；
- 不会出现 v1/v2 切换：`shouldUseV2` 照 Go 移植，返回的 `isV1V2Switch` 恒为假，`InfoCache.Upsert` 分支不会走到；
- `fetchSchemasWithTables` 总是走 v2 的读取（名字 + must-load 的表），内存库（`IsSpecialDB`）照 Go 读全部表。

### 2.4 并发读取

Go 用 errgroup 按 `splitForConcurrentFetch` 的分组并发读取（`fetchSchemaConcurrency = 1`，库多于 1000 个时 8 组）。Rust 把各组作为 future 一起 poll（`try_join_all`），结果按分组顺序拼接，与 Go 填充原切片的顺序相同。

### 2.5 不移植的选项

- `schemaFilter`（跨 keyspace 加载，next-gen）：tidb-rs 不支持 keyspace（N11 §1 范围外），恒为 nil。
- `domainutil.RepairInfo`（Go TiDB 启动参数 repair mode）：tidb-rs 不提供，恒为关闭。
- metrics（`LoadSchemaDuration*`、`LoadSchemaCounter`）和 failpoint 不移植。

## 3. 边界桩

| 桩 | 类别 | 说明 |
|---|---|---|
| `getTimestampForSchemaVersionWithNonEmptyDiff` | 临时，`blocked-by:D07.1` | Go 用 `store/helper` 的 `GetMvccByEncodedKeyWithTS`（TiKV 的 MVCC 调试接口）读 schema diff 的提交时间戳。没有它时 Go 对非 TiKV store 返回 "cannot get store from domain"，调用方每次加载都记一条 warning 并用 0（`pkg/domain/domain.go` 第 308–311 行；接 TiKV 的 Go 不会走到这里，所以 Go 实际上从不打这条）。桩返回 `blocked(...)`（`blocked-by:D07.1`），调用方同样用 0；但因为 sync 循环每秒加载一次，每次都记 warning 会刷屏（#484），所以对 `blocked(...)` 每个 domain 只在第一次加载（启动时）记一条 WARN `schema version commit ts is not available, logged once`（字段 `error`、`version`、`reason`），其他错误仍照 Go 每次记 `failed to get schema version`（字段 `error`、`version`）。结果是 `InfoCache` 中的时间戳为 0，`GetBySnapshotTS` 找不到版本，`GetSnapshotInfoSchema` 退回加载历史版本（结果正确，只是多读一次）。新栈目前不处理快照读（`tidb_snapshot` / stale read 走 PoC），不受影响 |
| `InfoCache.gcOldVersion`（D06.3） | 临时，`blocked-by:D06.2` | 见 D06.3 spec §10 |

## 4. 第 2 部分：schema validator、reload 循环、lease（未做）

VS1-17 原计划还包括 schema validator。N11（集群成员身份、schema 同步与 lease）目前是**调研草案**，lease 用于读语句准入（D3）、lease 过期后的处理（D4）、默认值（D5）、升级检查（D7）都还没有确认。按要求，本部分不建立在未确认的例外决定之上：

- `schema_validator.go`（12 个函数）、`schema_checker.go`（4 个）、`loadSchemaInLoop`、`mustReload`、`GetSchemaLease`、`IsLeaseExpired` 留给第 2 部分（tidb-rs `specs/VS1-vertical-slice/functions.tsv` 中的 VS1-17b）。
- 在此之前，lease 和语句准入仍由 PoC 的 `poc-domain`（`schema_validator.rs`、`registry.rs`）负责。新栈只在两边版本相等时运行，而 PoC 在准入时检查 lease，所以新栈的语句同样受 PoC lease 的约束。
- Go 的 validator 逻辑（`Update` / `Check` / `enqueue` / `isRelatedTablesChanged`）本身不依赖 N11 的决定（N11 改的是**在哪里**检查 lease），第 2 部分可以先按 L1 移植这部分，N11 确认后再接入准入。
- N11 §4.3 的升级检查（D06.3 spec §6 的钩子 `set_refuse_unknown_actions`）保持关闭，等 N11 D7。

## 5. 测试

- `crates/domain/tests/load_info_schema.rs`：内存 store（按提交时间戳返回 meta 状态，meta 键照 Go 的布局写出）。
  - `full_load_then_diff_load`：第一次全量加载；同版本命中缓存；CREATE TABLE 的 diff 增量加载（`RelatedSchemaChange` 的表 ID 与 action）；旧版本仍可按版本取到且看不到新表；空 diff 的版本不推进。
  - `large_gap_is_a_full_reload`：版本差达到 `LoadSchemaDiffVersionGapThreshold` 时全量加载。
- `crates/domain/tests/schema_ts_warning.rs`（单独的测试二进制，`tracing` 按调用点缓存是否有订阅者，与同一进程里其他线程的测试会互相干扰）：`blocked_schema_ts_lookup_warns_once` 走 21 次加载（启动时全量、缓存命中、diff 增量、快照加载），整行比较捕获到的 WARN：只有第一次加载的一行。两个测试文件共用 `tests/common/mod.rs` 的内存 store。
- 单元测试：`Divide2Batches`、`getFlashbackStartTSFromErrorMsg`、`canSkipSchemaCheckerDDL`。
- 不另做 govec：加载的每一步（全量加载、成批应用 diff、出错退回全量加载）已由 D06.3 的 govec `infoschema` 领域按 domain 的流程覆盖（D06.3 spec §7）；本部分新增的只是版本选择（缓存命中 / 增量 / 全量），上面的测试逐条覆盖。
- 端到端：路由器测试 `real_prepare_is_blocked_and_restored` 在真实的新会话上用含 `sbtest1` 的 InfoSchema 执行 `PREPARE`，预处理找到表后停在下一个桩（§6）。

## 6. 验收结果（VS1-17 第 1 部分）

- 新会话的 InfoSchema 来自 `tidb-domain` 的加载器（全量加载 + diff 增量），不再是空的替身。
- 预处理的表查找可用：`PREPARE s FROM 'SELECT c FROM sbtest1 WHERE id=?'` 在新栈上通过表查找，随后通过表的元数据锁（`tryLockMDLAndUpdateSchemaIfNecessary`，照 Go 移植，D05.4 spec §3a），停在规划器的 `buildSelect`（`blocked-by:D09.1`，计划构建器）。新栈会话的元数据锁登记尚未由本 domain 上报给集群（Go 的 MDL 检查循环属于本块的后续部分；桩 `report_metadata_locks`，#252）。
- 规划器的表查找是同步的、不读 TiKV，会话在规划前先加载语句用到的表（D06.3 spec §11.2）。

## 7. converge

`scripts/converge.py --block D06.5 --part specs/VS1-vertical-slice/functions.tsv --item VS1-17 crates/domain/src/domain.rs`：`matched 15/16`，桩 1 个（§3），没有多余、没有重复。

## 8. 第 3 部分：sysvar 缓存的接入（#521）

### 8.1 Go 的行为和 tidb-rs 的结构

Go v8.5.8：

- domain 持有 sysvar 缓存（`sysvar_cache.go`），分 session map 和 global map。`rebuildSysVarCache`（第 104 行）执行 `SELECT variable_name, variable_value FROM mysql.global_variables`（`fetchTableValues`，第 85 行），对每个注册的变量取表里的值，没有行的取默认值；不跳过初始化的变量放进 session map，有 GLOBAL 作用域的放进 global map，并执行它的 SetGlobal 钩子（第 141 行）。
- `LoadSysVarCacheLoop`（`domain.go` 第 2034 行）在启动时 rebuild 一次（失败则 bootstrap 返回错误，`session.go` 第 3723 行，服务器不启动），之后在 etcd 键 `/tidb/sysvars` 的 watch 事件（任何实例 `SET GLOBAL` 后都会 PUT 这个键）和 30 秒定时器上 rebuild；watch 通道关闭时重建 watch。
- 新会话在第一条语句时复制 session map（`loadCommonGlobalVariablesIfNeeded`，`session.go` 第 4144 行）；读全局值走 `session.GetGlobalSysVar`（第 1296 行）：读 global map，没有项时回表读，再按类型校验（`ValidateFromType`），校验失败用默认值。
- `SET x = DEFAULT`（会话作用域）取全局值：`SetExecutor.getVarValue`（`executor/set.go` 第 304 行）在 sysVar 为 nil 时调用 `GetGlobalSystemVar`。

tidb-rs（只读节点，所有 `--new-stack` 模式）：

| Go | tidb-rs |
|---|---|
| `sysVarCache`、`rebuildSysVarCache`、`fetchTableValues` 的解码 | 已移植的 `tidb_systable::sysvar_cache`（D06.7） |
| `LoadSysVarCacheLoop` | 已移植的 `tidb_systable::session_io::load_sys_var_cache_loop`，由新模块 `poc_domain::sysvar_loader::SysVarSync` 在单独的线程上运行 |
| `fetchTableValues` 的 SQL | 在新的 TSO 上扫描 `mysql.global_variables` 的 record 键（`poc-kv`，与权限加载共用读 TableInfo 和扫描的函数），再由 `decode_global_variables` 解码 |
| etcd watch | `sysvar_loader` 中的一个 tokio 任务：每个 watch 事件唤醒循环（`Watch`）；已建立的 watch 结束时报告 `WatchClosed`（循环据此 rebuild，D05.2 spec §6.2），然后自己重新 watch；连不上 etcd 时按 1、2、4… 最多 30 秒的间隔重试（Go 的 etcd 客户端自己重试），期间靠 30 秒定时器 |
| 会话的 `GlobalVarAccessor`（`session` 自身） | `poc_session::router::newstack::ClusterGlobals`，读 `SysVarCache`；替换原来回答编译默认值的 `DefaultGlobals` |
| `domain.GetDomain(s).GetSessionCache()` | `GlobalVarAccessor::session_cache`（新增的带默认实现的方法，glue）：Rust 的会话不持有 domain，由读缓存的 accessor 交出 session map；没有缓存的 accessor（单元测试用的）仍由 tidb-session 的替身按 `get_global_sys_var` 构造 |

加载器在**所有模式**下建立，包括 `off`（默认的生产模式；维护者决定，§8.5）：`off` 模式下原型自己也有按会话变量做的判定，PREPARE 的 plan cache 警告（D13.1 spec 已知差异 13）用集群的全局值；#503 的 SET 校验合并后同样走原型路径。`off` 模式下缓存建立之前（读不到表），原型不拒绝语句，照旧用编译默认值（`ClusterGlobals::compiled_defaults`：从空表 rebuild 出的缓存），进程内只记一次 WARN `the global variables of the cluster are not loaded yet (mysql.global_variables); the prototype uses the compiled defaults until they are`。原型会话在第一次 PREPARE 时取一次值（与 Go 每个会话加载一次相同），之后不再变；取到的是编译默认值时，缓存建立后的下一次 PREPARE 重新取集群的值（#539 评审）。

### 8.2 新会话和 `GetGlobalSysVar`

- **加载**：路由器在会话打开时调用 `loadCommonGlobalVariablesIfNeeded`（D05.3 spec §11.2），session map 来自缓存。所以 Go TiDB 上 `SET GLOBAL sql_mode = 'ANSI_QUOTES'` 之后新开的 tidb-rs 连接，新会话的 `sql_mode` 是 `ANSI_QUOTES`，第 0 步的 sql_mode 守卫（#509）对它生效；`tidb_skip_isolation_level_check`、`tidb_enable_noop_functions` 等同样取集群的值。
- **`GetGlobalSysVar`**（`ClusterGlobals::get_global_sys_var`，L2）：未知变量报 `ErrUnknownSystemVar`；缓存已建立时取 global map 的值，经 `ValidateFromType` 校验，失败用默认值，与 Go 相同。差异：
  - Go 的 `GetGlobalVar` 在缓存为空时同步 rebuild，失败或没有项时回表读。新会话的打开路径不能阻塞地读 TiKV，所以缓存从未建立时返回错误（§8.3），缓存已建立而没有项时（只有没有 GLOBAL 作用域的变量，建立后的缓存含所有全局变量）直接用默认值，与 Go 回表读不到行时相同。
  - 类型校验在 accessor 自己的一个 `SessionVars` 上做（accessor 在会话的 `SessionVars` 里面，拿不到调用它的会话）。校验产生的 warning 因此不进调用方的语句上下文；表中的值通常合法，不产生 warning。
- **`SET x = DEFAULT`**：路由器的同步（`router/sync.rs`）原来取编译默认值，现在调用新会话的 `GetGlobalSystemVar`（集群的全局值）。读不到时该变量记为 diverged，语句走 PoC。同步的变量列表（PoC 接受的 16 个会话变量）不变：其他变量的会话 `SET` 由 PoC 拒绝，它们的全局值已经由加载取到集群的值（#474 R-6 的另一半）。

### 8.3 读不到全局值时：走 PoC（fail safe）

- **从未建立**（启动时读不到 TiKV、表不存在、解码失败）：节点照常启动（Go 不启动），只在第一次失败时记一条 WARN（`loading the global variables from mysql.global_variables failed; retrying, ...`，字段 `error`），循环在之后的每个事件（etcd、30 秒定时）上重试，重试失败不再记；第一次建立成功时记一条 INFO `global variables loaded from mysql.global_variables`。缓存建立之前路由器不加载会话的全局值（Go 的加载会阻塞在缓存的同步 rebuild 上，只读节点不让会话阻塞地读 TiKV），会话的语句走 PoC（第 0 步条件 `global variables not loaded`，D05.3 spec §11.2），PREPARE 同样走 PoC（第 0 步只有这一条对 PREPARE 适用），PoC 那份预处理语句的 plan cache 判定用编译默认值（不用未加载的会话变量）。路由器在每个命令开始时检查缓存，建立后立即加载（相当于 Go 第一条语句时的 `rebuildSysVarCacheIfNeeded`），之后正常路由（#539 评审）。指标 `tidbrs_sysvar_cache_loaded`（0/1）显示缓存是否已建立。
- **加载时 SetSession 钩子拒绝了表中的某个值**：会话的变量不是集群的值，这个会话之后一直走 PoC（Go 的加载每个会话只做一次）。路由器对每个不同的错误在进程内记一次 WARN `VS1 router: a value of mysql.global_variables is rejected by its hook, sessions stay on the PoC (logged once)`，字段 `variables` 列出被拒绝的变量（在一个编译默认值的会话上逐个设置会话 map 的值找出），`error` 是 Go 的错误。
- **之后某次 rebuild 失败**：与 Go 相同，保留上次成功的值，日志 WARN（`LoadSysVarCacheLoop failed`，已移植的循环每次失败都记，与 Go 相同），不设时效上限。
- **没有加载器**（工厂没有传入缓存，只在测试中出现）：工厂给新会话一个从未建立的缓存，结果同"从未建立"；`off` 模式用编译默认值。
- **`off` 模式**：不拒绝，用编译默认值并记一次 WARN（§8.1）。
- 新会话里出现 `blocked` 或其他错误的规则不变（VS1 plan §4.3）。

### 8.4 与 Go 的差异

1. **rebuild 不执行 SetGlobal 钩子**（`SysVarCache::rebuild` 的钩子会话为 `None`）。Go 在每个实例的每次 rebuild 中执行它们，让其他实例的 `SET GLOBAL` 在本实例生效。其中 7 个钩子调用 Go 的 domain、DDL、executor 在启动时注册的回调（`domain_sysvars.go` 第 36–52 行、`ddl.go` 第 806 行、`executor/select.go` 第 142 行）；tidb-rs 没有注册它们，调用未注册的回调会 panic；其中 `SwitchMDL` 写 meta、`SetExternalTimestamp` 写 PD，只读节点不能做。所以本部分只建两个 map：新会话的值和 `GetGlobalSysVar` 是集群的，钩子设置的进程级状态（如 `max_prepared_stmt_count`、`tidb_enable_instance_plan_cache`）仍是编译默认值，与合并前相同。跟进 issue #537 处理回调的注册和钩子的执行。
2. 启动时第一次 rebuild 失败不终止节点（§8.3）。
3. 会话路径不触发 rebuild（§8.2），由加载器的循环负责。
4. `metrics.LoadSysVarCacheCounter`（`tidb_domain_load_sysvarcache_total`）不维护。
5. 重建串行由单一循环线程保证：Go 在 `rebuildLock` 下取表，防止较早的读取较晚完成；这里只有循环线程 rebuild（SET GLOBAL 的写路径尚未接入），`rebuild` 仍在锁内调用读取。
6. 类型校验在 accessor 自己的 `SessionVars` 上做，warning 不进调用方的语句上下文（§8.2）。
7. **PoC 一侧仍用编译默认值**：PoC 的 `SET x = DEFAULT` 存编译默认值，新会话存集群的全局值。集群改过全局值、语句又回退到 PoC 时，两边存的值不同；PoC 执行时的结果按编译默认值（与合并前相同）。
8. etcd 客户端不用 TLS（与权限加载器相同；Go 沿用集群的安全配置）。
9. `Rebuilding sysvar cache from etcd watch event.`（DEBUG）照 Go 在 watch 事件和定时器两种情况下都记（`domain.go` 第 2084 行）。
10. 指标 `tidbrs_sysvar_cache_loaded` 是 tidb-rs 自己的，Go 没有。

### 8.5 维护者已定（Maya 在 #521 上的答复，评论 7232448193）

- 钩子：本部分不执行，列为已知差异（§8.4 第 1 条），另开跟进 issue #537 注册回调（只读节点拒绝写）。
- 启动失败：不终止，缓存第一次建立之前会话走原型；每 30 秒重试；统一格式只记一次 WARN，成功时再记一次（§8.3）。
- 之后的 rebuild 失败：保留旧值并记 warning，不设时效（与 Go 相同）；只有"从未加载"算读不到；打开路径不阻塞是已知差异。
- （#539 评审，Lucky）缓存建立前打开的会话在之后的命令上重试加载，不再终生走 PoC；未加载时 PREPARE 也走 PoC。
- 所有模式都加载，包括 `off`；`off` 模式下从未加载时照旧用编译默认值、不拒绝，记一次日志（§8.1）。
- spec 位置：本节，并更新 D05.3 spec §11.1、D05.2 spec §6.2、D13.1 spec 已知差异 13。

没有需要负责人决定的点：这是按 Go 的机制接入，用户可见的结果向 Go 靠拢；读不到时走 PoC 是已有的路由规则。

### 8.6 测试

- `crates/systable/tests/sysvar_cache_unit.rs`：`rebuild_without_a_hook_session_builds_the_maps_only`（不执行钩子时两个 map 照表建立，进程级值不变）。
- `crates/poc/domain/src/sysvar_loader.rs`：`failed_first_load_is_retried_and_the_cache_stays_unbuilt_meanwhile`（第一次读取失败，缓存未建立，下一个事件建立）；`watch_events_and_the_timer_rebuild_and_drop_ends_the_loop`（watch 事件带来新值；失败的 rebuild 保留旧值；`WatchClosed` 也 rebuild；定时器自己 rebuild；丢弃句柄后线程结束）。
- 路由器（`crates/poc/session/src/router/tests.rs`）：`cluster_global_values_reach_a_new_session`（集群的 `sql_mode = 'ANSI_QUOTES'` 和两个 SET 校验开关进入新会话；守卫生效；`SET sql_mode = DEFAULT` 回到集群的值）；`unreadable_cluster_global_values_keep_the_session_on_the_poc_until_loaded`（缓存未建立时 strict 下也走 PoC、新栈不被调用；建立后同一个会话在下一个命令加载并走新栈）；`prepare_before_the_global_variables_are_loaded_stays_on_the_poc`（未加载时 PREPARE 只在 PoC，plan cache 判定用编译默认值、没有 "plan cache is disabled"；建立后两边都准备）；`cluster_globals_read_the_cache_as_go_reads_the_domain`（`GetGlobalSysVar` 的各分支）。
- 端到端（需要集群，本 PR 未做，由维护者跑）：playground 上在 Go 执行 `SET GLOBAL sql_mode = 'ANSI_QUOTES'` 等，再开新的 tidb-rs 连接，检查新会话的值、路由（`/debug/route`）和 SET 校验；etcd 通知 1 秒内、断开 etcd 时 30 秒内生效。

## 本文新增部分（§8）所用 ID

- D05.2：会话变量；D05.3：会话；D06.5：本块，domain；D06.7：系统表写入（`tidb-systable`）；D13.1：plan cache
- VS1 plan §4.3：新栈与原型之间的路由规则
