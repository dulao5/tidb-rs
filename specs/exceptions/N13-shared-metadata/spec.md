# N13：长期共享的元数据：`Arc` 共享与写时复制（基础设计 spec）

状态：**spec 待确认**（2026-11-15，#229 第一个 PR）。类型：实现例外（基础设计）+ 映射规则 `M-SHARE`。
所属块：D06.1（元数据模型）、D06.3（InfoSchema）、D06.4（table 抽象）；约束今后所有持有元数据的块（统计信息、plan cache、bindinfo、权限缓存移植时同样适用）。
相关：N1（计划树、表达式树：语句级的 `Arc` 与写时复制，`M-PLAN` / `M-EXPR` / `M-FT`）、A3（InfoSchema v2 的缓存）、[D06.1 spec](../../D06.1-model/spec.md) §3.4、§3.5、[D06.4 spec](../../D06.4-table/spec.md) §2。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/`。

## 1. 要解决的问题

Go 用指针共享长期存在的元数据：`InfoSchema` 中的 `*model.TableInfo`、表对象（`table.Table`）中的 `*model.ColumnInfo` / `*model.IndexInfo`、多个 schema 版本之间没有变化的表。Rust 的元数据模型（D06.1）此前把 Go 的 `[]*T` 按值持有，"共享"一律是复制（D06.1 §3.5 原文）。后果：

1. **内存**：`TableFromMeta` 为每列、每个索引复制一份 `ColumnInfo` / `IndexInfo`。10,000 张表 × 30 列 × 3 个索引时，这些副本是 151.4 MiB，比表对象本身（38.5 MiB）大四倍（§5）。
2. **与 Go 的结构对应**：Go 的表对象、分区、缓存都指向同一份列定义；按值复制以后，"同一份定义"在 Rust 中变成了多份可能不一致的值。

另一方面，Go 的共享指针允许任何持有者原地修改，修改对所有持有者可见。#148 是典型：Go 的规划器通过 `RetType: &col.FieldType`（`planner/core/logical_plan_builder.go:5510`）拿到 memory table 列的 `FieldType` 指针，之后的 `SetDecimal` / `SetFlag` 直接改了进程全局的 infoschema 定义，影响此后所有会话的查询（§4 第 1–3 行）。

本 spec 定一条规则：**共享就只读，要改先复制**。用 `Arc<T>` 共享，只能经 `Arc::make_mut`（写时复制）修改。共享的值只能拿到 `&T`，所以 #148 这一类"通过共享指针改了别人的数据"在 Rust 中编译不通过。

## 2. Go 中的事实

| 事实 | 位置 | 对 Rust 的影响 |
|---|---|---|
| 表对象的列、索引指向 `TableInfo` 中的同一个对象：`table.ToColumn(colInfo)`、`NewIndex(t.physicalTableID, tblInfo, idxInfo)` | `table/tables/tables.go:134`、`:183`、`:267` | 表对象与 `TableInfo` 共享 `ColumnInfo` / `IndexInfo` |
| 分区表的每个分区与父表共享同一份列列表 `tbl.Columns` | `table/tables/partition.go:184` | D06.4 第 2 部分：分区共享列列表，不各自复制 |
| InfoSchema v1 从旧版本构造新版本时浅拷贝 map，没有变化的 `table.Table` / `*TableInfo` 在版本之间是同一个指针；v2 的多版本 `Data` 每张表只存一次 | `infoschema/builder.go:981`–`:1003`、`infoschema/infoschema_v2.go` | 没有变化的表在版本之间共享同一个 `Arc<TableInfo>` |
| 应用 schema diff 时，被改动的表**从 meta 重新解码**（`applyTableUpdate` → `applyCreateTable` → `m.GetTable`），不在旧对象上原地改，也不调用 `TableInfo.Clone` | `infoschema/builder.go:801` | 被改动的表在新版本中的列全是新值，与旧版本不共享（Go 也一样）；旧版本不受影响 |
| `TableInfo.Clone` 是深拷贝：逐个 `Columns[i].Clone()`、`Indices[i].Clone()` | `meta/model/table.go:234`–`:267` | Rust 手写的 `Clone`（Go `Clone`）同样深拷贝；Go 的 `nt := *t` 对应 `GoCopy`，共享元素 |
| v2 的表对象缓存（`tableCache`，SIEVE，键是表 ID + schema 版本）不随 `GCOldVersion` 清理；`Data.remove` 删除的是删除那一版的键，不是创建那一版的。旧版本的表对象靠容量淘汰离开缓存，容量由 `tidb_schema_cache_size` 决定 | `infoschema/infoschema_v2.go:221`、`:274`–`:284`；`infoschema/builder.go:1193`–`:1194` | 旧版本的 `TableInfo` 在缓存淘汰之前一直存活，上限由缓存容量决定，不随 DDL 次数增长（§5 泄漏测试） |

## 3. 规则（`M-SHARE`）

1. **哪些用 `Arc`**：Go 中被**长期存在的结构**共享的 `*T`（InfoSchema、表对象、分区、各种缓存），Rust 中是 `Arc<T>`；Go 的 `[]*T` 是 `Vec<Arc<T>>`。只在单条语句或单行内使用的值（计划中间结果、执行器状态、行数据）继续按值持有，或按 N1 处理。是否改成 `Arc` 以"有没有长期持有者共享它"为准，不是凡 `*T` 都改。
2. **只经写时复制修改**：共享的值只能拿到 `&T`。要改时用 `Arc::make_mut`（只有一个持有者时原地改，否则先复制再改，别的持有者看到的仍是旧值）。D06.1 的模型类型用 `tidb_model::make_mut`：复制用 `GoCopy`（Go 的结构体赋值），不用 `Clone`。原因是 D06.1 中一些类型的 `Clone` 是 Go `Clone` 方法的移植，会把 nil 切片变成空切片（如 `IndexInfo.Clone` 的 `Columns`），用它做写时复制会改变 JSON（`null` 变 `[]`）。
3. **共享的元数据内部不用内部可变性**：不放 `Mutex`、`RwLock`、`RefCell`、`Cell`、原子类型、`OnceCell` 一类可以经 `&T` 修改的字段。否则规则 2 的保证失效，#148 这类 bug 又回来了。需要缓存的派生值放在持有者一侧（如表对象），或在构造时算好。
4. **指针只向下**：父对象持有子对象的 `Arc`（`TableInfo` → `ColumnInfo`、表对象 → `TableInfo`）。子对象要找父对象时，按 ID 查，或由调用方作为参数传入；**不保存指向父对象的 `Arc` 或 `Weak`**。这样不会出现引用环，旧版本释放后整棵子树一起释放；也不会因为 `Weak` 的存在让 `Arc::get_mut` 失败（N1 §3.4 对计划树的同一要求）。
5. **`GoCopy` 与 `Clone`**：
   - `GoCopy`（生成）是 Go 的 `nt := *t`：`Arc` 字段克隆 `Arc`，与 Go 共享同样的对象；
   - 手写的 `Clone` 是 Go 的 `Clone` 方法：Go 深拷贝的地方 Rust 也深拷贝，写成 `Arc::new(T::clone(&x))`；
   - 代码中对 `Arc` 字段的 `x.clone()` 是**克隆指针**，不是 Go 的 `Clone`。需要 Go `Clone` 的地方写成 `T::clone(&x)`，避免看错。评审时逐处看。
6. **Go 的写操作逐处分类**（§4）：Go 通过共享指针写入、别的持有者能看到的位置，每处归为两类之一：
   - **"Go 应该先复制"**：写入只为本处使用，别的持有者看到是 bug 或无害。Rust 用写时复制，行为更安全，不模拟 Go 的可见性。
   - **"依赖共享可见性"**：正确性依赖别的持有者看到这次写入。Rust 不能用写时复制照搬，要改设计（生成新的 InfoSchema 版本、显式传参等），在所属块的 plan 中写明。

映射规则表（[mappings.md](../../../docs/porting/mappings.md)）中登记为 `M-SHARE`，标注写 `[M-SHARE]`。

## 4. Go 通过共享指针写入的位置（v8.5.8 审计）

按 #229 评审（Lucky）要求列出。范围：写入对象可以从 InfoSchema 或其他长期结构到达，别的持有者能看到这次写入。写入前已经新解码或 `Clone` 过的位置不列（`infoschema/builder.go:828`–`:906` 的字符集规范化作用于新解码的 `TableInfo`；`executor/show.go:696`、`planner/core/preprocess.go:2148`、`table/tables/partition.go:1664` 先 `Clone`；DDL 提交路径先复制）。

| # | 位置 | 写了什么 | 别的持有者 | 分类 | Rust |
|---|---|---|---|---|---|
| 1 | `planner/core/logical_plan_builder.go:5510`（`buildMemTable`） | 不写，但计划列的 `RetType: &col.FieldType` 直接指向 memory table 的列定义（普通表在 `:5346` 是 `col.FieldType.Clone()`） | 进程全局的 memory table `TableInfo`（`infoschema/infoschema.go` 的 `init` 中构造一次），所有 InfoSchema 版本、所有会话 | Go 应该先复制（#148）。这是别名；第 2、3 行是落在它上面的写 | 计划列的类型按值或 `Arc` 共享（批次 A），写时复制 |
| 2 | `expression/builtin_time.go:2632` | `bf.args[1].GetType().SetDecimal(MaxFsp)`；字符串列经 `WrapWithCastAsString` 不加 cast 原样传入时，写的是列本身的 `FieldType` | 与第 1 行相同的别名 | Go 应该先复制 | 写时复制 |
| 3 | `planner/core/rule_eliminate_projection.go:201` | `foldedExpr.GetType().SetFlag(...)`；表达式是裸列时写回同一个值 | 同上 | Go 应该先复制（同值写入，只是数据竞争） | 值相同时不写，避免无谓的复制 |
| 4 | `table/tables/partition.go:164`、`:165`（defer）、`:178`–`:182` | `newPartitionedTable` 把每个分区的索引列表临时换进 `tblInfo.Indices`，传给 `initTableCommonWithIndices`，结束后换回 | 输入是新解码的 `TableInfo` 时无害；输入是 infoschema 中的对象时（第 5 行）是真的竞争 | Go 应该先复制 | D06.4 第 2 部分：索引列表作为参数传入，不写进元数据 |
| 5 | `executor/importer/table_import.go:159` | 对 infoschema 中的 `e.Table.Meta()` 调用 `TableFromMeta`，经第 4 行原地改 `Indices` | InfoSchema 和并发的会话：分区重组期间读者可能短暂看到错误的 `Indices` | Go 应该先复制 | 写路径，tidb-rs 不移植；规则同第 4 行 |
| 6 | `infoschema/infoschema.go:638`（`addReferredForeignKeys`） | `referredFK.Cols = fk.RefCols`，原地改 `*ReferredFKInfo` | 旧的 InfoSchema 版本：`referredForeignKeyMap` 在 `infoschema/builder.go:987` 只是 `maps.Clone`，列表元素共享 | Go 应该先复制（同文件 `deleteReferredForeignKeys` 就是新建） | 新建一个 `ReferredFKInfo`（批次 C） |
| 7 | `infoschema/infoschema_v2.go:1734`–`:1736` | `SchemaByID` 取出存着的 `*DBInfo`，原地改 `Charset`、`Collate` | v2 的 `Data` 被所有版本共享，旧版本的 `schemaItem` 指向同一个 `DBInfo`：stale read、进行中的事务会看到新的字符集 | Go 应该先复制（v1 经 `getSchemaAndCopyIfNecessary`，`infoschema/builder.go:686`、`:997` 是对的） | 写时复制，新版本存新的 `Arc<DBInfo>`（批次 C） |
| 8 | `infoschema/infoschema_v2.go:1753`–`:1754`，以及经 `:1738` / `:1756` 到达的 `:269` | 原地改 `PlacementPolicyRef`；随后 `addDB` 在同一个共享的 `DBInfo` 上置 `Deprecated.Tables = nil` | 同第 7 行 | Go 应该先复制 | 同第 7 行 |
| 9 | `infoschema/builder.go:872`（从 `infoschema/infoschema_v2.go:1660` `applyTableUpdateV2` 到达） | 在 v2 `SchemaByID` 返回的共享 `DBInfo` 上 `Deprecated.Tables = append(...)`（v1 传的是 `Copy()`，`infoschema/builder.go:1002`） | 所有 v2 版本 | Go 应该先复制（v2 的读者不用 `Deprecated.Tables`，实际无害，但列表会一直增长） | 写时复制（批次 C） |
| 10 | `infoschema/builder.go:981`–`:1003`；v2 的 btree | 不写。v1 浅拷贝 map，没有变化的表在版本之间是同一个指针；v2 每张表只存一次 | 所有版本 | **依赖共享可见性**：版本之间的廉价复制、v2 按（ID，版本）的表缓存都以"没变的表是同一个对象"为前提 | 没有变化的表克隆 `Arc`，结果相同；任何修改都必须生成新版本，**不能**在共享的版本上原地改（规则 2 已保证） |

另有只建立别名、未发现写入的位置，交给批次 A（`Arc<FieldType>`）逐处核对：`planner/core/planbuilder.go:2051`、`:2112`、`:2131`，`planner/core/expression_rewriter.go:2605`，`planner/core/find_best_task.go:2716`，`planner/core/pb_to_plan.go:154`；根源是 `expression/column.go:631`–`:638` 的 `Column.Clone` 只复制结构体，`RetType` 指针仍共享（N1 §1 已登记）。

结论：读路径上没有需要"共享可见性"的写入；第 10 行要求的"改动生成新版本"由写时复制天然满足。statistics/handle、ttl、privilege 中没有发现对元数据的写入。

## 5. 验收

本 PR（#229 批次 B）的测试，数字为 arm64 macOS 上的实测：

- **内存**（`crates/infoschema/tests/arc_sharing_mem.rs`，jemalloc `stats.allocated`；10,000 张表 × 30 列 × 3 个索引，列交替为 BIGINT 和 VARCHAR(64)，索引 1–2 列）：

  | 结构 | tidb-rs | Go v8.5.8（`HeapAlloc`） |
  |---|--:|--:|
  | `TableInfo`（模型） | 156.2 MiB（546 B/列） | 117.6 MiB（411 B/列） |
  | 表对象（`TableFromMeta`，共享列和索引） | 38.5 MiB（134 B/列） | 30.1 MiB（105 B/列） |
  | 本 PR 之前表对象额外复制的列和索引 | 151.4 MiB（529 B/列），已去掉 | — |
  | InfoSchema v2 全量加载（名字 / ID 项、缓存的表对象；模型移入，不计） | 47.0 MiB | 未测 |

  表对象每列 134 B 是 `table::Column` 包装（`Arc<ColumnInfo>` 加生成列、默认值表达式的槽位，40 B，放在 `Arc` 中）与几个列列表的项，不含 `ColumnInfo`（416 B）。测试断言：表对象的每一列、每个索引与 `TableInfo` 中的是同一个 `Arc`（`Arc::ptr_eq`），表对象每列的增量小于一个 `ColumnInfo` 结构体。Go 的数字由 tracker `govec/cmd/memshape`（同样的形状）测得。模型本身比 Go 大约 33%，主要是 `FieldType` 的 `String` 字符集 / 排序规则等，属批次 A。
- **共享与写时复制**（`crates/infoschema/tests/arc_sharing.rs`、`crates/model/src/gocopy.rs` 的单元测试）：
  - 表对象与 InfoSchema 中的 `TableInfo` 共享列和索引；
  - 修改一列的 diff（`ActionModifyColumn`）之后：没有改动的表在新旧版本中是同一个 `Arc<TableInfo>`；被改动的表从 meta 重新解码，它的列全是新值（与 Go 相同，§2），旧版本中的值不变；新旧两版的表对象都与各自的 `TableInfo` 共享；
  - `GoCopy` 之后 `make_mut` 改一列：只有这一列是新的 `Arc`，其余列与原值 `Arc::ptr_eq`，原值看不到修改；`make_mut` 的复制保留 nil（不是 Go `Clone`）；`TableInfo::clone`（Go `Clone`）不共享任何列和索引。
- **泄漏**（`arc_sharing_ddl_rounds_free_old_versions`，默认 2,000 轮，约 2 秒；`ARC_SHARING_ROUNDS=<n>` 改轮数）：每轮 create table → modify column → drop table，各自是一次 `ApplyDiff` 并 `Build` 出新版本，每 100 轮按最新版本 `GCOldVersion`。用 `Weak` 跟踪每个版本的 `TableInfo` 以及它的每个 `ColumnInfo`、`IndexInfo`：
  - `tidb_schema_cache_size` = 256 KiB 时，GC 之后仍存活的是缓存中的 58 个 `TableInfo`（464 个列、116 个索引），2,000 轮和 10,000 轮相同，不随轮数增长；
  - 淘汰整个缓存之后，4,000 个 `TableInfo`、32,000 个 `ColumnInfo`、8,000 个 `IndexInfo` 全部释放（`Weak::upgrade` 失败）；
  - 未被 DDL 触及的表仍可读，表对象仍共享。
- govec `model` 领域（62,826 条）、`infoschema`、`table` 领域回放通过：JSON 编解码与 Go 逐字节相同。

## 6. 范围与后续

本 PR：规则本身；D06.1 的 `TableInfo.Columns`、`TableInfo.Indices` 改为 `Option<Vec<Arc<_>>>`，`DBInfo.Deprecated.Tables` 改为 `Option<Vec<Arc<TableInfo>>>`（生成器 tracker `govec/cmd/rsgen` 的 `sharedFields`）；D06.4 的表对象复用列和索引的 `Arc`；InfoSchema 全量加载直接持有 `Deprecated.Tables` 中的 `Arc<TableInfo>`，特殊库（memory / metrics / performance schema）保留的表列表与表对象共享同一批 `TableInfo`（Go `addSpecialDB` 也是同一批指针）；`DBInfo.Copy` 与 Go 一样共享表信息（原来是 L2，复制）。

`TableInfo` / `PartitionInfo` 中其余的 Go 指针字段，本 PR 不改：

- `Constraints []*ConstraintInfo`：表对象只在写路径加载约束（D06.4 §4 第 4 条），没有长期持有者共享；
- `ForeignKeys []*FKInfo`：InfoSchema 的外键索引存的是新建的 `ReferredFKInfo`，不指向 `FKInfo`（第 6 行另行处理，批次 C）；
- `PartitionInfo.Definitions` 等在 Go 中按值（`[]PartitionDefinition`），保持按值；
- `Partition *PartitionInfo`、`TTLInfo` 等单个指针：随 `TableInfo` 一起共享，没有单独的长期持有者。

后续批次（#229 审计表）：

- **A**：`Arc<FieldType>`（审计第 2、4、6、9 项；本 spec §4 第 1–3 行及别名位置），字符集 / 排序规则改 `&'static str`。必须保持规则 3：共享元数据内部没有内部可变性。
- **C**：InfoSchema 的表项和特殊库（审计第 1、8 项）；§4 第 6–9 行；`DBInfo` 本身在 InfoSchema 中的共享（`Arc<DBInfo>`，第 7–9 行的写时复制）。
- **D**：`Expr::Column(Arc<Column>)`（审计第 5 项，N1 §7 第 1 条）与 coprocessor 请求（审计第 7 项）。
- **D06.4 第 2 部分**：分区与父表共享同一份列列表（`Arc<[ColumnRef]>`，Go `partition.go:184`），分区的索引列表作为参数传入（§4 第 4 行）。

待定：被 DDL 改动的表重新解码后，没有变化的列能否复用旧版本的 `Arc`（按列 ID 比较相等）。Go 不这样做；收益只在宽表频繁 DDL 且多个版本同时存活时。需要时另开 issue，先测。

## 7. 评审检查项

- 新的长期结构持有元数据：是否用 `Arc`，是否只经 `make_mut` 修改；
- 共享的元数据类型中是否出现内部可变性（规则 3）；
- 是否有子对象保存父对象的 `Arc` / `Weak`（规则 4）；
- `Arc` 字段上的 `.clone()` 是否其实想要 Go 的 `Clone`（规则 5）；
- 新移植的 Go 代码中通过共享指针写入的位置，是否按 §4 的两类登记在块 plan 中。
