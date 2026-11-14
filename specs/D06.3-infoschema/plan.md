# D06.3：plan

## 第 1 部分（PR 1a + PR 1b）

PR 1a：spec、tracker 划分、`tidb-model` 的类型、边界桩、`sieve`、`context`、`error`、`infoschema`（misc）、`infoschema_v2` 中 `Data` 与 `InfoSchemaV2` 的读取、`builder` 中与延迟加载共用的函数（`tableFromMeta`、两个 charset 转换、`tableIDIsValid`）；单元测试。converge 120/270。

PR 1b（叠在 1a 上）：`builder` 的其余部分、`builder_misc`、`bundle_builder`、`infoschema_v2` 中 builder 的 v2 方法与 `applyXxx` 分发函数、`InfoSchemaV2` 的 `MetaOnlyInfoSchema` 实现、govec 回放与向量。converge 184/270。

合计：

- tracker：`rules.py` 把 `internal/testkit.go`（测试辅助，24 个）和 `updateAutoIDForExchangePartition`（meta 写入）划到 X，重新生成 `func_blocks.tsv`、`block_edges.tsv`（295 → 270 个函数）。
- crate `tidb-infoschema`：
  - `infoschema_v2`：`Data`（7 个多版本索引、表缓存、specials、`recentMinTS`；GC、全量加载前的 reset、外键引用）、`InfoSchemaV2` 的全部读取方法（含 `loadTableInfo` 的 singleflight）、builder 的 v2 方法和 `applyXxx` 分发函数；
  - `builder`、`builder_misc`、`bundle_builder`：`ApplyDiff` 的全部分支、全量加载、`InitWithOldInfoSchema`、`Build`、虚拟表注册、placement policy / resource group / 临时表、bundle 的增量与全量更新；
  - `infoschema`：`infoSchemaMisc`、`PolicyByID`、`HasAutoIncrementColumn`、`getTableInfo(List)`；
  - `sieve`（A3 §5.2）、`context`（特殊属性过滤器、`HasSpecialAttributes`、`DBInfoAsInfoSchema`）、`error`（46 个错误变量）；
  - 边界桩：`table`（D06.4）、`autoid`（D06.2 第 2 部分；`IsMemSchemaID` 与常量按 L1）、`placement`（X）。
- `tidb-model`：新增 `ReferredFKInfo`、`TableNameInfo`；`GoCopy` 改为 `pub`（builder 保留内存库的 `Deprecated.Tables` 时用）。
- govec `infoschema` 领域与 Rust 回放 `tests/govec_infoschema.rs`（spec §7）；`sieve_test.go` 移植为单元测试。
- converge D06.3 `184/270`。

### 偏离登记（spec §4）与 Go 的 map 顺序（M-MAPORDER）

逻辑与顺序无关的地方：`createSchemaTablesForDB` 遍历 `TableName2ID`（按键插入索引；同一库内表名按 `L` 唯一，生成器保证这一点）、`updateInfoSchemaBundlesV2` 遍历 `updateTables`、`AllPlacementBundles` / `AllPlacementPolicies` / `AllResourceGroups`（调用方自己排序）。Go 的结果依赖顺序的地方：`PolicyByID` / `ResourceGroupByID` 在 ID 重复时返回哪一个、`SchemaByID` 对内存库的 `Range`、`allSchemas` 中内存库的顺序：Rust 取确定的顺序（map 的键序），向量不含这些情形。

## 第 2 部分

1. `cache.go`：`InfoCache`（版本缓存、`GetBySnapshotTS`、空版本、`Upsert`、`gcOldVersion`）。`gcOldVersion` 需要 D06.2 第 2 部分的 `GetOldestSchemaVersion`（经 `store/helper` 读 MVCC）；`Insert` 中的 `go h.gcOldVersion()` 按 M-GO 放到运行时上。与 D06.5 的 domain 一起接入（替换 PoC 的 `tidb-meta::SchemaCache`）。
2. `InfoSchema` trait（`interface.go` 与 `context.MetaOnlyInfoSchema` 的全部方法），`SessionTables`、`SessionExtendedInfoSchema`，以接口为参数的辅助函数（`AllSchemaNames`、`FindTableByTblOrPartID`、`GetSequenceByName`、`SchemaByTable`、`TableIsSequence`、`TableIsView`），sessionctx 的 `MetaOnlyInfoSchema` 替身补全。
3. `init`：information_schema 的注册（表定义是 D06.6 的 `tableNameToColumns`）。
4. 边界桩去掉时：D06.4 的 table 抽象（`table_from_meta` 的状态检查、缓存表的 `factory`）、D06.2 第 2 部分的 allocator 与 `ListSimpleTables`（`applyRecoverSchema` 的 `ReadTableFromMeta`）。
5. 混合集群测试：v8.5.8 playground 上用 Go 执行 DDL，tidb-rs 增量应用 diff，与 Go 的 `information_schema` 对照（随 D06.5）。
6. N11 §4.3 的表级检查（随 D06.5 与 D06.1 的严格解码）。
