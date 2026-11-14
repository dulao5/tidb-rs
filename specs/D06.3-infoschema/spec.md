# D06.3 InfoSchema 构建与缓存：spec

块 D06.3（ROADMAP P2 L3）：`pkg/infoschema` 的 `builder.go`、`builder_misc.go`、`bundle_builder.go`、`infoschema.go`、`infoschema_v2.go`、`cache.go`、`sieve.go`、`internal/sizer.go`，以及 `pkg/infoschema/context`。crate `tidb-infoschema`（N0 rank 9，与 D06.2、D06.4、D06.6 同一个 crate），模块在 crate 根下：`builder`、`builder_misc`、`bundle_builder`、`infoschema`、`infoschema_v2`、`sieve`、`context`、`error`。

状态：第 1 部分，分两个 PR：**1a**（本 spec、例外 A3 的设计、`Data` 与 `InfoSchemaV2` 的读取、SIEVE、边界桩，converge 120/270）和 **1b**（builder、`ApplyDiff`、govec 回放，converge 184/270）。§5.6 的四项 2026-11-14 已确认（协调人 Maya：1–3 是例外 A3 内的工程选择，4 来自负责人"v1 是技术债"的决定）；§6 的 N11 钩子保持默认关闭，等 N11 D7。

## 1. 范围

### 1.1 函数划分

2026-11-14 tracker 划分（`scripts/blocks/rules.py`）：295 → 270 个函数。

- **归 X 的 25 个**：
  - `internal/testkit.go` 的 24 个：在 mockstore 上建库建表的测试辅助函数（"测试工具"）；
  - `updateAutoIDForExchangePartition`：exchange partition 的 diff 被应用时，**每个** Go TiDB 都在新事务里把两张表的 auto ID 写成两者的最大值（"DDL 与元数据写入由 Go TiDB 承担"）。写入的值只取决于 meta，集群里任一 Go TiDB 都会写，tidb-rs 不写不影响结果。
- **第 1 部分**（PR 1a + 1b）：v2 的构建与读取全部路径，converge 见 §8。
- **永久豁免（v1，§1.2）**：42 个。
- **永久豁免（A3，§5）**：`internal/sizer.go` 的 `Sizeof`、`sizeOf`（按 Go 内存布局反射计算大小，由 Rust 的大小估算取代）。
- **第 2 部分**（plan.md）：`cache.go`（`InfoCache`，16 个）、`SessionTables` / `SessionExtendedInfoSchema`（19 个）、以接口为参数的辅助函数（`AllSchemaNames`、`FindTableByTblOrPartID`、`GetSequenceByName`、`SchemaByTable`、`TableIsSequence`、`TableIsView`）、`init`（注册 information_schema，表定义属于 D06.6）。

### 1.2 只移植 infoschema v2（负责人决定，2026-11-14）

Go 在 `tidb_schema_cache_size > 0`（默认 512 MiB）时用 v2，为 0 时用 v1。负责人决定：只有 v2 的缓存有意义，v1 是技术债，**不移植**，作为永久豁免（理由："infoschema v1 is not ported (owner decision ...)"，tracker govec `infoschema` 领域的豁免表）：

- `builder.go`：`Builder.applyTableUpdate`、`applyDropSchema`、`applyRecoverSchema`、`applyDropTable`、`applyModifySchemaCharsetAndCollate`、`applyModifySchemaDefaultPlacement`、`copySortedTables`、`copySortedTablesBucket`、`deleteReferredForeignKeys`、`getSchemaAndCopyIfNecessary`、`sortAllTablesByID`，`tableBucketIdx`；
- `bundle_builder.go`：`updateInfoSchemaBundles`、`completeUpdateTables`；
- `infoschema.go`：`infoSchema` 的 25 个方法（`schemaMap`、`schemaID2Name`、`sortedTablesBuckets`、`referredForeignKeyMap` 及读取它们的方法）、`sortedTables.searchTable`、`MockInfoSchema`、`MockInfoSchemaWithSchemaVer`（构造 v1 的测试替身）。

v2 也调用 v1 结构的地方，在 v2 下这些结构恒为空，调用不产生效果，照此省略并在代码中注明：`applyDropTableV2` 里的 `b.infoSchema.deleteReferredForeignKeys`、`applyCreateTable` 对 v1 bucket 的排序和 `dbInfo.Deprecated.Tables` 的追加（v1 的 `TableByID` 在 v2 下永远找不到）。`infoSchema.PolicyByID` 读的是 misc 的 policy map，v2 用到，已移植。

`Builder.enableV2` 恒为真，`NewBuilder` 不再有 `useV2` 参数；`IsV2` 恒为真；`InitWithOldInfoSchema` 中"v1/v2 不能混用"的检查不会触发。`tidb_schema_cache_size = 0` 的行为见 §5.4。

## 2. 依赖与边界桩

- `tidb-base`（错误、gofmt、config）、`tidb-parser`（`CIStr`、charset 常量）、`tidb-model`（D06.1；本 PR 新增 `ReferredFKInfo`、`TableNameInfo` 两个无方法的类型，`GoCopy` 改为公开）、`tidb-kv`（`Storage`、`Snapshot`）、`tidb-sessionctx`（`SchemaCacheSize`、`MetaOnlyInfoSchema` 替身 trait）、D06.2 的 `meta::Mutator`。
- `parking_lot`（M-LOCK）、`futures`（`BoxFuture`、`Shared`）、`tokio`（只用 `time::sleep`：flashback 重试的 200 ms 等待）、`log`。

| 边界桩 | 类别 | 替代 |
|---|---|---|
| `crate::table`：`table.Table` 只有 `Meta()`、`Allocators()`；`tables.TableFromMeta` 只保存 table info | 临时，`blocked-by:D06.4` | D06.4 的 table 抽象（列、索引、分区表、缓存表、状态检查） |
| `crate::autoid`：`Requirement`、`Allocator`、`Allocators`；`NewAllocatorsFromTblInfo`、`NewAllocator` 不创建 allocator | 临时，`blocked-by:D06.2`（第 2 部分 `pkg/meta/autoid`） | `IsMemSchemaID` 和 schema ID 常量没有出边，按宪章 4.5 提前移植（L1，计入 D06.2） |
| `crate::placement`：`Bundle` 只记录覆盖的物理 ID 和 policy；`NewTableBundle`、`NewPartitionBundle` 只按 Go 的规则决定有没有 bundle 和覆盖哪些 ID | 永久（X：placement 规则由 DDL owner 和 PD 处理） | 读路径上只有 `distsql.VerifyTxnScope` 在 local txn scope 时读 bundle，tidb-rs 没有 local TSO |
| `applyRecoverSchema` 的 `ReadTableFromMeta`（BR 恢复库时用 `meta.ListSimpleTables` 列表） | 临时，`blocked-by:D06.2` 第 2 部分 | 返回明确错误；domain 会退回全量加载 |
| 内存库（information_schema、metrics_schema、performance_schema） | 临时，`blocked-by:D06.6` | `RegisterVirtualTable` 已移植，D06.6 注册后 `initVirtualTables` 照 Go 建表 |

## 3. 设计（映射）

- **异步**：会读 TiKV 的方法是 `async`：`TableByName`、`TableByID`（缓存未命中时 `loadTableInfo`）、`SchemaTableInfos`、`FindTableByPartitionID`、`TableExists`、`TableInfoBy*`，以及 builder 的 `ApplyDiff`、`InitWithDBInfos`、`Build`。`ApplyDiff` 递归（`applyAffectedOpts`），返回 `BoxFuture`。
- **context**：Go 的 `context.Context` 中被读取的只有 refill 选项（`WithRefillOption`），对应 `infoschema_v2::Ctx`（M-GOCTX）；没有取消（tidb-kv 的 Context 也没有）。`SchemaTableInfos` 的 flashback 重试因此只等待，不监听取消。
- **共享对象**：`*model.DBInfo`、`*model.TableInfo`、`*model.ReferredFKInfo`、`*PolicyInfo` 等是 `Arc`；`table.Table` 是 `Arc<dyn Table>`。
- **`infoschemaV2`**：Go 嵌入 `*infoSchema`（v2 只用其中的 `infoSchemaMisc`）和 `*Data`。Rust 的 `InfoSchemaV2 { misc: Arc<InfoSchemaMisc>, data: Arc<Data>, r, factory, ts }`；builder 持有自己的 `InfoSchemaV2`，写 misc 时 `Arc::make_mut`（新建的 builder 独占，不复制）。`Build` 消耗 builder，返回 `InfoSchemaV2`。`CloneAndUpdateTS` 与 Go 一样共享 misc 和 Data。
- **misc 的锁**：Go 用 mutex 保护 policy / resource group map。只有拥有它的 builder 在 `Build` 前写入，之后不可变，Rust 不加锁（L2）。
- **`loadTableInfo` 的 singleflight**：全局的 `key → Shared<BoxFuture>` 表，键与 Go 相同（`dbID-tblID-schemaVersion`），完成后由等待者移除。
- **错误**：`error.go` 的 46 个错误变量全部移植。`errors.Wrap(err, "Build table ... schema failed")` 按 M-ERR 成为文字为 `<msg>: <cause>` 的普通错误。
- **`MetaOnlyInfoSchema`**：Go 的 `infoschema/context` 接口在 sessionctx 之下就被用到，tidb-sessionctx 中已有它的替身 trait（`expropt::MetaOnlyInfoSchema`，只有 `SchemaMetaVersion`）。`InfoSchemaV2` 实现它；完整的接口随第 2 部分的 `InfoSchema` trait。

## 4. 偏离

1. **`applyModifySchema*V2` 的共享 `DBInfo`**（L2）：Go 原地修改 `SchemaByID` 返回的 `*DBInfo`（字符集或 placement），这个指针被较早版本的 schema item 共享，所以**较早的 InfoSchema 版本也看到新值**（见 §9 第 1 条）。Rust 的 `DBInfo` 是不可变的 `Arc`：复制并修改，再把共享旧 `Arc` 的所有 item 换成新的（`Data::replace_shared_db_info`）。此后的查询与 Go 一致（govec 中 5 条记录覆盖了这种情况：去掉替换就不一致）。之前已经取出的 `Arc` 不变，Go 中那是一次数据竞争。
2. **`SchemaByID` 返回 nil 时的 panic**：`applyRecoverSchemaV2` 把 `GetDatabase` 返回的 nil 传给 `addDB`、`applyModifySchema*V2` 直接写 nil 的字段，Go 都会 nil 解引用；Rust 以相同文字 panic（同 D06.2 的正则越界）。`FindTableByPartitionID` 对非分区表的 `GetPartitionInfo()` 解引用同样处理。
3. **metrics**：`TableByNameHitDuration` / `MissDuration`、`DDLResetTempIndexWrite`、sieve 的 metrics hook（`metrics.go` 属于 D06.6）没有移植；sieve 先装空 hook。
4. **failpoint**：`repairFetchCreateTable`、`mockLoadTableInfoError`、`skipGet` 不移植；`tracing.StartRegion` 不移植。
5. **`DBInfoAsInfoSchema.SchemaTableInfos`** 对 nil 的 `Deprecated.Tables` 返回空切片（Go 的 nil 与空切片对调用方相同）。
6. **N11 升级检查的钩子**（§6）：默认关闭，关闭时与 Go 相同。
7. **GC 保留截止版本上的读者需要的 item**（有意偏离，[#195](https://github.com/dulao5/tidb-rs/issues/195)，2026-11-14 Maya + Lucky 决定）：`gcCollectTableItem`、`gcCollectReferredForeignKeyItem` 每张表保留截止版本及以下最新的 item，只删更旧的（Go 见 §9 第 7 条）。govec 向量由只改了这两个函数的 v8.5.8 生成（tracker `govec/fixes/pkg/infoschema/infoschema_v2.go.fixed`，fix `tidb-rs#195`）。

用户可见的差异有 §5.4（`tidb_schema_cache_size = 0`）和第 7 条（GC），都已写入 `docs/compatibility.md`。

## 5. 例外 A3：infoschema v2 的缓存（设计，待确认）

A3（candidates.md）："按 Rust 的内存布局和淘汰策略重做"，约束是命中与淘汰的可观测行为（容量变量等）与 Go 一致。A3 还没有独立 spec，按负责人要求写在这里。

### 5.1 数据结构

Go 的 `Data` 是一组**多版本索引**：每个 item 带 schema version，删除写 tomb，查询取 `version ≤ 本版本` 的最新 item；所有版本的 InfoSchema 共享同一个 `Data`。每个索引是写时复制的 B-tree（`btree.BTreeG`，degree 16），挂在 `atomic.Pointer` 上，写入方 `Clone` + `ReplaceOrInsert` + CAS。

Rust：

| Go | Rust |
|---|---|
| `byName`、`byID`、`schemaMap`、`schemaID2Name`、`referredForeignKeys`、`pid2tid`、`tableInfoResident`：`atomic.Pointer[btree.BTreeG[T]]` | 每个一个 `parking_lot::RwLock<BTreeMap<键, item>>`，就地插入 / 删除 |
| `compareXxx` 比较函数 | map 的键（元组），排序与 Go 相同：Go 字符串比较是字节序，Rust `String` 的 `Ord` 相同 |
| `btreeSet`（Clone + ReplaceOrInsert + CAS 重试） | 写锁内 `insert`（相同键替换） |
| `specials`（`sync.Map`） | 按名字排序的 `BTreeMap`（M-MAPORDER：Go 的 `Range` 顺序不定） |
| `tableCache`：`Sieve[tableCacheKey, table.Table]` | `Sieve<TableCacheKey, Arc<dyn Table>>`（§5.2） |
| `recentMinTS`：`atomic.Uint64` | `AtomicU64` |

为什么就地修改与 Go 的写时复制给出相同结果：

- **只有一个写者**。builder 在 domain 的 reload 锁下运行（Go 相同，Go 的 CAS 只是防御）；GC（`GCOldVersion`）由 `InfoCache.Insert` 触发，Go 中也与 builder 并发，双方都只删除"同一张表已有更新版本"的旧 item。
- **读者只看自己版本及以下的 item**。builder 为版本 v+1 写入的 item 对版本 ≤ v 的读者不可见，与 Go 读到旧树的结果相同。
- **reset 前先收集**：`resetXxxBeforeFullLoad` 在 Go 中遍历的是写入前的旧树（写入去了新副本）。Rust 先在读锁下收集要打 tomb 的 item，再写入，得到同样的 item；Go 每批 1000 个的分批不影响结果。
- 读者在一次查询内持有读锁，不跨 `await`（M-LOCK）。

不采用的方案：每个写入复制整个 map 再 `ArcSwap`（全量加载时每张表一次 O(n) 复制）；持久化 B-tree crate（新依赖，换来的只有无锁读）。读锁的开销在 schema 查询的路径上可以忽略；若压测显示争用，再在本 spec 内改为分片或持久化结构，接口不变。

### 5.2 淘汰：保留 SIEVE

表缓存的淘汰算法照 Go 保留 SIEVE（`sieve.go` 的业务逻辑按 L1 移植：同样的 hand 移动、`Set` 每次最多淘汰 10 个、`SetCapacityAndWaitEvict` 的循环、`Remove` 对 hand 的调整，包括 `Purge` 之后 hand 仍指向已删除元素的行为）。布局改为 slab 下标链表（代替 `container/list`），整个 cache 一把 `parking_lot::Mutex`（Go 也是一把 mutex，`Get` 要写 `visited`）。没有理由换算法：SIEVE 对 schema 访问的"少量热表 + 大量一次性访问"模式正合适，换成别的算法反而让命中行为与 Go 不同。

### 5.3 大小

Go 的 `entry.Size()` 用反射按 **Go 的内存布局**遍历 `table.Table` 的对象图（`internal.Sizeof`）。Rust 的对象图不同，同一张表的字节数必然不同。Rust 的估算：

- entry 自身的 `size_of`，加上键和值的堆大小（`SieveSize` trait）；
- 表的大小 = `TableInfo` 结构体 + 名字、注释 + 每列 / 索引 / 外键 / 分区定义的结构体与名字（`table::table_info_mem_size`）。D06.4 换上真正的 table 对象后，按它的字段补充。

可观测的结果：`tidb_schema_cache_size` 的含义（字节上限）不变；同样的容量下能缓存的表数与 Go 不同（预计更多：Rust 的表对象更紧凑）。这不影响查询结果，只影响 TiKV 读取次数。govec 只在容量为 1（任何大小下都只留最后一个）和容量足够大时比对命中情况（`TableIsCached`）。

### 5.4 容量变量

- `tidb_schema_cache_size` 的取值、校验、warning、持久化都在 D05.1 / D06.7（`parseSchemaCacheSize`、`ChangeSchemaCacheSize` 钩子），与 Go 相同。
- `NewBuilder` 与 `Data.SetCacheCapacity` 把它作为表缓存的字节上限，与 Go 相同。
- **`tidb_schema_cache_size = 0`**：Go 切换到 v1（所有表常驻内存，不淘汰）。tidb-rs 没有 v1，用**不限容量的 v2** 代替：所有加载过的表常驻，查询结果与 Go 相同（v1 / v2 本来就给出相同结果）。差别在内部：Go 的 v1 在全量加载时读入所有表，tidb-rs 仍只读 must-load 的表，其余按需读入后不再淘汰；`IsV2` 恒为真；Go 的 v1/v2 切换（`isV1V2Switch`、`InfoCache.Upsert`）在 tidb-rs 不会发生。`Data.CacheCapacity()` 此时返回 `u64::MAX`（Go 返回 0；只有测试读它）。
- Go 在容量为 0 的 SIEVE 上实际只留一个 entry；tidb-rs 不会出现这个状态。

### 5.5 仍按 L1 的部分

版本可见性（`search`、`tableByNameHelper`、`SchemaByName`/`SchemaByID` 的遍历、`IterateAllTableItems`、`SchemaSimpleTableInfos` 的去重——包括它们按整个 `CIStr`（含 `O`）而不是 `L` 比较的地方）、tomb 规则、`GCOldVersion` 的选择规则与每次 1024 个的上限、`resetBeforeFullLoad` 的 tomb、外键引用的增删与排序、`ListTablesWithSpecialAttribute` 的分组（包括 §9 第 3 条的行为）、`keepAlive`、`loadTableInfo` 的 singleflight 键和重试、refill 规则（`TableByID` 默认不回填、`TableByName` 默认回填）、`EvictTable`。

### 5.6 决定（2026-11-14 已确认，全部按提议）

1. §5.1 的数据结构（锁 + `BTreeMap`，不用写时复制）。
2. §5.2 保留 SIEVE。
3. §5.3 的大小估算方式（Rust 布局，不追求与 Go 字节数一致）。
4. §5.4 `tidb_schema_cache_size = 0` 为不限容量（用户可见：内存不受该变量限制；已写入 `docs/compatibility.md` 的 "Deliberate deviations"）。

## 6. N11 升级检查的钩子（待 N11 D7 决定）

N11 §4.3（调研草案）建议：遇到 v8.5.8 不认识的 `SchemaDiff.Type` 时节点级拒绝服务。`Builder::set_refuse_unknown_actions(true)` 后，`ApplyDiff` 的 default 分支对 `model.ActionMap` 中没有的类型返回 1105 错误（文字按 N11 D7 的建议："tidb-rs v8.5.8 cannot read metadata written by a newer TiDB: unknown schema diff type %d"），domain（D06.5）据此停止准入并报警。默认关闭，即 Go 的行为。表级检查（`TableInfo.Version > 5`、JSON 中不认识的字段）需要 D06.1 的严格解码模式，随 D06.5 实现。

## 7. 测试

- **govec `infoschema` 领域**（tracker `govec/domains/infoschema.go`）：220 个随机场景，每个场景在一个 `Data` 上依次做全量加载（照 `domain.fetchAllSchemasWithTables` 的 v2 路径）、成批应用 schema diff（照 `tryLoadSchemaDiffs`，出错时照 domain 退回全量加载）、全量重载、GC、容量为 1、查询较早的版本。diff 覆盖 `ApplyDiff` 的所有分支：建删库、建删表（含删除中的中间状态）、truncate、跨库 rename、修改表（各种 action，含 v8.5.8 不认识的类型 200）、分区的 truncate / drop / reorganize、exchange partition（public 与非 public 两种）、修改库字符集与 placement、placement policy 与 resource group 的增改删、recover table / schema、create tables、flashback、refresh meta、过期的 diff（库或表已不存在）。每个 InfoSchema 查询全部读取 API（约 25 组，含表的 Go JSON、外键引用、特殊属性过滤、缓存状态、`EvictTable`）。meta 状态由 Go 的 `meta.Mutator` 写出；store 按 commit ts 返回状态（`loadTableInfo` 在 InfoSchema 的 ts 读）。内存库（D06.6）在生成时去掉（清空 `drivers`）。
- Rust 回放 `tests/govec_infoschema.rs`（PR 1b）：全部一致；种子 1–8 的 1,760 条也一致。反证：把 `TableByName` 的默认回填改成 false、GC 的 `<` 改成 `<=`、`ListTablesWithSpecialAttribute` 的去重条件改掉、去掉 §4 第 1 条的替换，分别有 157、16、147、5 条记录不一致。
- `sieve_test.go` 的 6 个测试移植为单元测试（`sieve::tests`）；`infoschema_v2_test.go` 中只用 `Data` 的 `TestReferredForeignKeys`、`TestGCOldFKVersion` 移植到 `infoschema_v2::tests`（PR 1a）；`builder_test.go` 的 `TestGetKeptAllocators`（PR 1b）。
- A3 §5.1 的并发论证（PR 1b）：`readers_at_a_fixed_version_while_the_builder_writes` 固定版本 v 的全部查询结果（与缓存无关的部分），4 个线程反复查询 v，同时一个线程在同一个 `Data` 上反复应用该场景之后的 diff、更晚版本的全量重载，并以 v 为截止版本 GC；v 的结果始终不变。反证：截止版本改为 v + 1000 时立即不一致。
- `loadTableInfo` 的 singleflight：等待者全部取消时删除该键（`load_table_sf_entry_removed_when_waiters_cancelled`），之后的查询重新加载，不会加入一个已取消的 future。
- 生成器在同一个库内做只改大小写的 rename（`t` → `T`），覆盖 §9 第 4 条：62 个输出中出现 Go 的重复项，Rust 一致；改成按 `L` 去重时 30 条记录不一致。
- 混合集群（v8.5.8 playground 上用 Go 执行 DDL、tidb-rs 增量应用 diff）随 D06.5 的 domain 接入做。

## 8. converge

D06.3：270 个函数（295 个中 25 个归 X）。PR 1a 之后 `matched 120/270`，PR 1b 之后 `matched 184/270`，没有多余、没有重复。第 1 部分完成后其余 86 个：

| 类别 | 个数 | 理由（tracker 豁免表） |
|---|--:|---|
| v1，永久 | 42 | §1.2 负责人决定 |
| A3，永久 | 2 | `Sizeof`、`sizeOf`：§5.3 |
| 第 2 部分 | 42 | `cache.go` 16、`SessionTables` / `SessionExtendedInfoSchema` 19、以接口为参数的辅助函数 6、`init` 1（plan.md） |

`IsMemSchemaID`（`pkg/meta/autoid`）提前移植，计入 D06.2。

## 9. 可能的上游 bug（不提交 issue，记录在此）

1. **ALTER DATABASE 改写较早版本的 InfoSchema**：`applyModifySchemaCharsetAndCollateV2` / `applyModifySchemaDefaultPlacementV2` 原地修改 `SchemaByID` 返回的共享 `*DBInfo`，较早 schema 版本（stale read、`tidb_snapshot`、正在执行的语句）随之看到新的字符集或 placement，且是无锁写（数据竞争）。tidb-rs 照 Go 的可见结果实现（§4 第 1 条）。
2. **nil 解引用**：`applyRecoverSchemaV2` 不检查 `GetDatabase` 返回的 nil；`applyModifySchema*V2` 不检查 `SchemaByID` 的结果（只有 builder 与 meta 不一致时触发，例如应用 diff 的批次中途失败后没有全量重载）；`FindTableByPartitionID` 对 `GetPartitionInfo()` 不判空。
3. **`ListTablesWithSpecialAttribute` 的分组**：`currDB` 在切换库时不更新，第二个及以后的库里每张表各成一组（结果是同一库的多个 `TableInfoResult`）。调用方只遍历，结果集合相同。
4. **按 `CIStr` 整体比较的去重**：`SchemaSimpleTableInfos`、`IterateAllTableItems`、`allSchemas` 用整个 `CIStr`（含原始大小写）判断"同一张表 / 同一个库的不同版本"，而索引按 `L` 排序。只改大小写的 rename 之后，同一张表的新旧版本会被当成两张表各输出一次（旧版本若不是 tomb）。
5. **`Purge` 后的 hand**：`Sieve.Purge` 不重置 `hand`，之后的 `evict` 从已删除的元素开始；若该 key 不在 map 中则 panic（"sieve: evicting non-existent element"）。v8.5.8 只有测试调用 `Purge` / `Close`。
6. **`Data.mu.versionTimestamps` 从不写入**，`getVersionByTS` 恒返回 (0, false)：死代码。
7. **GC 删掉截止版本上的读者需要的 item**（已确认，单独的 issue [#195](https://github.com/dulao5/tidb-rs/issues/195)）：`gcCollectTableItem`（`infoschema_v2.go:424`）、`gcCollectReferredForeignKeyItem`（`:452`）删除"比截止版本旧、且同一张表有更新 item"的 item，但更新的 item 可能在截止版本之上。例如表在 v3 建、v8 改，截止版本（`GetOldestSchemaVersion`）为 5 时 v3 被删，版本 5 的 InfoSchema 报表不存在。tidb-rs 作为有意偏离修正（§4 第 7 条）；单元测试 `gc_keeps_the_item_a_reader_at_the_cut_needs` 的注释记录了 Go 的行为。
