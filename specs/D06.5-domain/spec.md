# D06.5 schema 同步与 domain：spec

块 D06.5（ROADMAP P2 L4）：`pkg/domain`。crate `tidb-domain`（N0 rank 12，本 PR 建立，去掉 `planned`）。

状态：**第 1 部分**（VS1-17：schema 加载），由 PoC domain 的同步循环驱动。第 2 部分（VS1-17b：schema validator / checker、reload 循环、lease）等 N11 的 lease 规则确认后再做（§4）。domain 的其余部分（统计、权限、绑定、TTL、infosync、server ID 等后台任务）不在 VS1 范围内。

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
