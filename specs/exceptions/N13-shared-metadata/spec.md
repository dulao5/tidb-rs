# N13：长期共享的元数据：`Arc` 共享与写时复制（基础设计 spec）

状态：**spec 待确认**（2026-11-15，#229 第一个 PR；§8 `FieldType` 的共享为批次 A，同日，待确认；§9 元数据的内存布局为批次 E 第 1 步，同日，待确认）。类型：实现例外（基础设计）+ 映射规则 `M-SHARE`。
所属块：D06.1（元数据模型）、D06.3（InfoSchema）、D06.4（table 抽象）；§8 另涉及 D02.3（`FieldType`）、D08.1 / D08.2（表达式）、D01.7（chunk）、D12.0（执行器）；约束今后所有持有元数据的块（统计信息、plan cache、bindinfo、权限缓存移植时同样适用）。
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
3. **共享的元数据内部不用内部可变性**：不放 `Mutex`、`RwLock`、`RefCell`、`Cell`、原子类型、`OnceCell` 一类可以经 `&T` 修改的字段。否则规则 2 的保证失效，#148 这类 bug 又回来了。需要缓存的派生值放在持有者一侧（如表对象），或在构造时算好。CI 检查 `scripts/check_model_mutability.py`（脚本自身的测试 `scripts/test_check_model_mutability.py` 在同一步运行）：`crates/model/src`（生成和手写的类型）和 `FieldType` 所在的 `crates/parser/src/types/field_type.rs`（`FieldType` 以 `Arc<FieldType>` 共享，§8）中出现 `Cell`、`RefCell`、`OnceCell`、`Mutex`、`RwLock`、`OnceLock`、`LazyLock`、原子类型、`thread_local!`、`static mut` 等即失败；注释和 `use` 不算，只初始化一次的 `static X: LazyLock<T>`（Go 的包级变量，不是共享类型的字段）不算。确有必要的例外在同一行写 `// N13-allow: <理由>`，按例外评审。现有两处，都是全局量，不是 `FieldType` 的字段：`TiDBStrictIntegerDisplayWidth`（Go 的包级配置开关）、`intern_name` 的名字集合（§8.2）。
4. **指针只向下**：父对象持有子对象的 `Arc`（`TableInfo` → `ColumnInfo`、表对象 → `TableInfo`）。子对象要找父对象时，按 ID 查，或由调用方作为参数传入；**不保存指向父对象的 `Arc` 或 `Weak`**。这样不会出现引用环，旧版本释放后整棵子树一起释放；也不会因为 `Weak` 的存在让 `Arc::get_mut` 失败（N1 §3.4 对计划树的同一要求）。
5. **`GoCopy` 与 `Clone`**：
   - `GoCopy`（生成）是 Go 的 `nt := *t`：`Arc` 字段克隆 `Arc`，与 Go 共享同样的对象；
   - 手写的 `Clone` 是 Go 的 `Clone` 方法：Go 深拷贝的地方 Rust 也深拷贝，写成 `Arc::new(T::clone(&x))`；
   - 代码中对 `Arc` 字段的 `x.clone()` 是**克隆指针**，不是 Go 的 `Clone`。需要 Go `Clone` 的地方写成 `T::clone(&x)`，避免看错。由 lint 固化（宪章第二条 5）：`tidb-model`、`tidb-infoschema` 以及批次 A 改动的 crate（`tidb-parser`、`tidb-chunk`、`tidb-expression`、`tidb-expr-pushdown`、`tidb-planner`、`tidb-executor`、`tidb-systable`）在 `lib.rs` 开启 `#![warn(clippy::clone_on_ref_ptr)]`，CI 的 clippy 以 `-D warnings` 运行：克隆 `Arc` 必须写 `Arc::clone(&x)`，于是裸的 `.clone()` 只能是值的复制。lint 只作用于库代码；测试代码已一并改写，但不受检查。
6. **Go 的写操作逐处分类**（§4）：Go 通过共享指针写入、别的持有者能看到的位置，每处归为两类之一：
   - **"Go 应该先复制"**：写入只为本处使用，别的持有者看到是 bug 或无害。Rust 用写时复制，行为更安全，不模拟 Go 的可见性。
   - **"依赖共享可见性"**：正确性依赖别的持有者看到这次写入。Rust 不能用写时复制照搬，要改设计（生成新的 InfoSchema 版本、显式传参等），在所属块的 plan 中写明。

映射规则表（[mappings.md](../../../docs/porting/mappings.md)）中登记为 `M-SHARE`，标注写 `[M-SHARE]`。

## 4. Go 通过共享指针写入的位置（v8.5.8 审计）

按 #229 评审（Lucky）要求列出。范围：写入对象可以从 InfoSchema 或其他长期结构到达，别的持有者能看到这次写入。写入前已经新解码或 `Clone` 过的位置不列（`infoschema/builder.go:828`–`:906` 的字符集规范化作用于新解码的 `TableInfo`；`executor/show.go:696`、`planner/core/preprocess.go:2148`、`table/tables/partition.go:1664` 先 `Clone`；DDL 提交路径先复制）。

| # | 位置 | 写了什么 | 别的持有者 | 分类 | Rust |
|---|---|---|---|---|---|
| 1 | `planner/core/logical_plan_builder.go:5510`（`buildMemTable`） | 不写，但计划列的 `RetType: &col.FieldType` 直接指向 memory table 的列定义（普通表在 `:5346` 是 `col.FieldType.Clone()`） | 进程全局的 memory table `TableInfo`（`infoschema/infoschema.go` 的 `init` 中构造一次），所有 InfoSchema 版本、所有会话 | Go 应该先复制（#148）。这是别名；第 2、3 行是落在它上面的写 | 计划列的类型是 `Arc<FieldType>`，写时复制（§8）；memory table 的计划构建尚未移植，移植时按普通表复制一次（`:5346`），不指向 infoschema 中的列定义 |
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

- **A**（§8）：`FieldType` 的共享（审计第 4、6、9 项；第 2 项已在 #217 改为借用）。
- **C**：InfoSchema 的表项和特殊库（审计第 1、8 项）；§4 第 6–9 行；`DBInfo` 本身在 InfoSchema 中的共享（`Arc<DBInfo>`，第 7–9 行的写时复制）。
- **D**：`Expr::Column(Arc<Column>)`（审计第 5 项，N1 §7 第 1 条）与 coprocessor 请求（审计第 7 项）。
- **D06.4 第 2 部分**：分区与父表共享同一份列列表（`Arc<[ColumnRef]>`，Go `partition.go:184`），分区的索引列表作为参数传入（§4 第 4 行）。

待定：被 DDL 改动的表重新解码后，没有变化的列能否复用旧版本的 `Arc`（按列 ID 比较相等）。Go 不这样做；收益只在宽表频繁 DDL 且多个版本同时存活时。需要时另开 issue，先测。

## 7. 评审检查项

- 新的长期结构持有元数据：是否用 `Arc`，是否只经 `make_mut` 修改；
- 共享的元数据类型中是否出现内部可变性（规则 3；`crates/model/src` 由 CI 检查）；
- 是否有子对象保存父对象的 `Arc` / `Weak`（规则 4）；
- `Arc` 字段上的 `.clone()` 是否其实想要 Go 的 `Clone`（规则 5；开启 `clone_on_ref_ptr` 的 crate 由 clippy 检查）；
- 新移植的 Go 代码中通过共享指针写入的位置，是否按 §4 的两类登记在块 plan 中。
- 长期共享的元数据结构加字段或改字段类型：`ArcInner` 是否仍在原来的 jemalloc 大小级别（§9.2，`crates/model/tests/layout.rs` 守住）；很少设置的可为 nil 的字段是否经生成器的 `boxedFields` 装箱。

## 8. `FieldType` 的共享（#229 批次 A）

审计第 4、6、9 项：Go 共享 `*types.FieldType` 的地方，Rust 按值复制。第 2 项（`DecodeRawRowData`）已在 #217 改为借用。

### 8.1 Go 中的事实

| 位置 | Go | 共享还是复制 |
|---|---|---|
| `meta/model/column.go`（`ColumnInfo`） | 内嵌 `types.FieldType`，按值 | — |
| `planner/core/logical_plan_builder.go:5346`、`expression/expression.go:1082`（`ColumnInfos2ColumnsAndNames`） | 普通表建计划列：`RetType: col.FieldType.Clone()` | 每列复制一次 |
| `planner/core/logical_plan_builder.go:5510` | memory table：`RetType: &col.FieldType` | 别名（§4 第 1 行，#148） |
| `expression/column.go:631`（`Column.Clone`）、`expression/schema.go`（`Schema.Clone` 调 `col.Clone()`） | `newCol := *col` | 共享 `RetType` |
| `expression/scalar_function.go:347`（`ScalarFunction.Clone`）、`expression/builtin.go:426`（`cloneFrom`） | `RetType: sf.RetType`、`b.tp = from.tp`；`RetType` 与签名的 `tp` 是同一个指针 | 共享 |
| `expression/constant.go:190`（`Constant.Clone`） | `con.RetType = c.RetType.Clone()` | 复制 |
| `executor/internal/exec/executor.go:144`–`:154`、`:289`–`:295` | `retFieldTypes[i] = cols[i].RetType`；`NewBaseExecutorV2` 把同一个切片交给 `newExecutorChunkAllocator` | 共享（类型和切片） |
| `util/chunk/alloc.go:28`（`Allocator.Alloc`）、`expression/distsql_builtin.go:1197` | 参数是 `[]*types.FieldType`；`RetType: tps[offset]` | 共享 |
| `table/tables/tables.go:1113`、`executor/insert_common.go:179` | `colTps[col.ID] = &col.FieldType` | 借用 |
| `FieldType.charset` / `collate` | Go `string`：赋值共享字节，名字大多来自常量 | 不分配 |

### 8.2 Rust 表示

| 位置 | 之前 | 现在 | 理由 |
|---|---|---|---|
| `FieldType.charset` / `collate`（D02.3） | `String` | `&'static str`，`tidb_parser::types::intern_name` | 复制 `FieldType` 不再分配；字符集 / 排序规则表中的名字直接返回静态字符串，其他名字（schema JSON 中的未知名字、大小写不同的写法）在进程级集合中复制一次、不释放。SQL 路径在名字进入 `FieldType` 之前都校验过（`Unknown character set` / `Unknown collation`），集合只随不同的名字增长。不用 `Arc<str>`：每次复制要原子加减；不用 `Cow<'static, str>`：24 B，JSON 解码出的名字仍要分配。JSON 编解码不变（govec 逐字节对照通过）。全局集合不在共享元数据内部，不违反规则 3（标 `N13-allow`）。各入口（`crates/sessionctx/tests/interned_names.rs`，`interned_name_count` 计数）：parser 的 `CharsetName` / `CollationName`（CAST、`COLLATE`、`CONVERT … USING`、introducer）校验并转小写，未知名字报错；`SET NAMES … COLLATE 'x'` 的排序规则在 Go 中也是原样的 `StringName`，经 `collation_connection` 的校验转成规范名；`character_set_*` / `collation_*` 系统变量的校验返回规范名，未知名字报错——这些入口都不增加集合。schema JSON 解码保留存储的写法（TableInfoVersion3 之前的大写名字、未知名字），与 Go 相同，否则 JSON 不能逐字节往返，所以**不**在解码时规范化：每个不同的名字复制一次，重复解码不再增加（随后 `ConvertCharsetCollateToLowerCaseIfNeed` 对旧版本表转小写，得到的是表中的名字）。`FieldType` 128 B → 112 B（Go 120 B） |
| `ColumnInfo.field_type`（D06.1） | `FieldType` | 不变，按值 | Go 是值字段。普通表建计划时 Go 也复制一次；借用者（`DecodeRawRowData`、systable、distsql）用 `&FieldType`。改成 `Arc` 要改生成器（tracker `govec/cmd/rsgen`），每列多 16 B 的 `Arc` 头，在 memory table 计划移植之前没有收益（§8.4） |
| `expression::Column.ret_type`（D08.1） | `FieldType` | `Arc<FieldType>` | `Column.Clone`、`Schema.Clone` 克隆 `Arc`，与 Go 共享同一个类型；改用 `Column::ret_type_mut`（`Arc::make_mut`）。`Column::default()` 的类型是一个共享的默认值，不分配 |
| `BuiltinFunc.tp`，即 `ScalarFunction` 的 `RetType`（D08.1 / D08.2） | `FieldType` | `Arc<FieldType>` | `cloneFrom` 共享；`ScalarFunction::ret_type_mut` 和函数类型推导中的写入都是 `Arc::make_mut(&mut bf.tp)`。构建时引用计数为 1，原地写，与 Go 逐行对应；`ScalarFunction::new` 接收 `impl Into<Arc<FieldType>>`，CAST 直接传签名的 `Arc`（Go 的同一个指针） |
| `Constant.ret_type` | `FieldType` | 不变，按值 | Go `Clone` 复制 |
| 执行器 `retFieldTypes`（D12.0） | `Vec<FieldType>`，meta 和分配器各复制一份 | `Arc<[Arc<FieldType>]>`，meta 和分配器共享同一个切片，元素是 schema 列的 `ret_type` | 每个执行器 0 次复制。`Executor::ret_field_types`、`exec::ret_types` 返回 `&[Arc<FieldType>]`；`Executor::new_chunk_with_capacity`、`chunk::Allocator::alloc` 接收 `&[Arc<FieldType>]`（Go 的 `[]*types.FieldType`） |
| chunk 构造函数（D01.7） | `&[FieldType]` | `&[F]`，`F: Borrow<FieldType>` | `&[FieldType]` 和 `&[Arc<FieldType>]` 都能传，不复制 |
| `HandleCols::get_fields_types`（D09） | `Vec<FieldType>` | `Vec<Arc<FieldType>>` | Go 返回列的 `RetType` 指针 |
| systable `replace.rs`、`sysvar_cache.rs`；distsql `DagPlan`（审计第 9 项） | `Arc::new(field_type.clone())`、`Vec<FieldType>` | `HashMap<i64, &FieldType>`；`DagPlan<'a>.output_types: Vec<Cow<'a, FieldType>>` | Go `&col.FieldType`：借用，聚合函数的结果类型才是新值 |

规则 2、3 仍然成立：共享的 `FieldType` 只能拿到 `&FieldType`，所有写入经 `Arc::make_mut`，没有内部可变性。`M-FT` 的语义不变：默认写时复制，Go 的别名不模拟；`Arc` 的指针身份**不**用来让写入对别的持有者可见（N1 §3.3、D09.7 试点报告修订建议 3 的方案 (b) 仍然否决）。改变的只是载体：N1 §3.3 原写"`Column.ret_type` 保持按值保存"，本节改为 `Arc<FieldType>` 加写时复制（需项目负责人确认，宪章第三条 5）。

### 8.3 测试

- `crates/expression/tests/expr_unit.rs` `clones_share_ret_type_and_writes_copy`：`Column.Clone`、`Schema.Clone`、`ScalarFunction.Clone` 之后是同一个 `Arc`；`ret_type_mut` 之后只有写入方是新的，原值不变。
- `crates/executor/src/internal/exec/executor.rs` `ret_field_types_share_the_schema_types`：meta 和分配器是同一个切片，元素与 schema 列的 `ret_type` `Arc::ptr_eq`（`BuildNewBaseExecutorV2` 同样）。
- `crates/sessionctx/tests/interned_names.rs` `entry_points_do_not_grow_the_intern_set`：parser、系统变量、schema JSON 三类入口，大小写变体和未知名字不增加名字集合（JSON 中的变体每个不同的名字只复制一次）。
- `scripts/test_check_model_mutability.py`：结构体字段中的内部可变性类型（失败）、单行 `static`（未标记时失败）、`// N13-allow:`（通过）、块注释中的提及（通过）、仓库中的检查范围（通过）。
- `crates/model/src/gocopy.rs` `move_column_info_copies_only_changed_indices`（Lucky 对 #230 的评审第 3 条）：`MoveColumnInfo` 只对偏移量变化的索引调用 `make_mut`，其余索引在共享的 `TableInfo` 之间仍 `Arc::ptr_eq`。
- govec：`model`、`infoschema`、`table`、`expr`、`funcs`、`cast`、`chunk`、`tablecodec` 等全部通过，`FieldType` 的 JSON 逐字节不变。

### 8.4 测量（arm64 macOS）

内存（§5 的 `arc_sharing_mem`，10,000 张表 × 30 列 × 3 个索引）：

| 结构 | 之前 | 现在 | Go v8.5.8 |
|---|--:|--:|--:|
| `TableInfo`（模型） | 156.2 MiB（546 B/列） | 152.8 MiB（534 B/列） | 117.6 MiB（411 B/列） |
| `size_of::<ColumnInfo>()` | 416 B | 400 B | — |
| 表对象、InfoSchema v2 全量加载 | 38.5 / 47.0 MiB | 38.5 / 47.0 MiB | — |

这个形状中只有一半的列（VARCHAR）有字符集名字，BIGINT 列的名字为空，所以模型只少 3.4 MiB。剩下与 Go 的差距不在 `FieldType`（Rust 112 B，Go 120 B，都不再有堆分配）：`ColumnInfo` 的其他字段、`CIStr` 的两次分配（Go `NewCIStr` 对已是小写的名字共享同一个字符串）等，见 §9。（本节原写"每列 `Arc` 的 16 B 头"：16 B 本身只是每列 16 B；它要紧，只是因为 400 B 的 `ColumnInfo` 加上它是 416 B，越过了 jemalloc 384 B 的大小级别，落进 448 B 级别，每列多付 64 B，§9.1。）

分配次数（`crates/expression/tests/fieldtype_sharing.rs`，计数分配器；30 列的表，BIGINT 列 `binary` / `binary`，VARCHAR(64) 列 `utf8mb4` / `utf8mb4_bin`；次数 / 字节）：

| 步骤 | 之前 | 字符集 / 排序规则改 `&'static str` 之后 | 现在（加 `Arc<FieldType>`） |
|---|--:|--:|--:|
| `TableInfo2SchemaAndNames` | 575 / 38,134 | 455 / 35,794 | 486 / 30,402 |
| `Schema.Clone` | 121 / 10,340 | 61 / 9,410 | 61 / 6,290 |
| 30 次 `Column.Clone` | 91 / 9,620 | 31 / 8,690 | 31 / 5,570 |
| 执行器的返回类型 | 61 / 4,290 | 1 / 3,360 | 1 / 240 |

`TableInfo2SchemaAndNames` 与 Go 一样每列复制一次类型，多出的 30 次分配是每列一个 `Arc`，字节数下降。`Schema.Clone` / `Column.Clone` 剩下的分配是 `Arc<Column>` 和 `orig_name`，与 `FieldType` 无关。执行器每个算子原来复制两份返回类型（meta、分配器），现在只有一个切片。

### 8.5 范围与后续

本 PR：§8.2 全部；lint `clippy::clone_on_ref_ptr`（规则 5）和 CI 的内部可变性检查（规则 3）；`MoveColumnInfo` 的按需复制。

后续：

- `ColumnInfo.field_type` 改 `Arc<FieldType>`：等 memory table 的计划构建（§4 第 1 行）和更多长期持有者移植后再测，需改生成器；在那之前，借用者用 `&FieldType`，memory table 的计划列按普通表复制一次。
- `PBToExpr`（`tidb-expr-pushdown`，`distsql_builtin.rs`）的 `tps: &[FieldType]`：Go `RetType: tps[offset]` 共享，Rust 每个列引用复制一次；改为 `&[Arc<FieldType>]` 要连带调用方，另做。
- `tablecodec::rowcodec::ColInfo.ft` 由调用方从 `ColumnInfo` 复制一次（每个执行器 / 请求一次，Go 共享指针）；随 `ColumnInfo.field_type` 一起处理。
- 批次 B、C、D 见 §6。
- 元数据的内存布局（批次 E）见 §9。

## 9. 元数据的内存布局（#229 批次 E）

### 9.1 剩余差距的构成

批次 A（§8）之后，§5 的形状（10,000 张表 × 30 列 × 3 个索引）中模型是 152.8 MiB（534 B/列），Go 117.6 MiB（411 B/列）。逐个释放各组成部分测得（Rust jemalloc `stats.allocated`，Go `HeapAlloc`；各项之和与总差距相差 0.05 MiB）：

| 组成部分 | Rust B/列 | Go B/列 | 差距 MiB | 原因 |
|---|--:|--:|--:|---|
| `Arc<ColumnInfo>` 与列表 | 456.2 | 360.0 | **27.53** | `ColumnInfo` 400 B（Go 344 B）+ `Arc` 头 16 B = 416 B，落进 jemalloc 448 B 级别；Go 344 B 落进 352 B 级别 |
| 列名（`CIStr`） | 16.1 | 2.9 | 3.77 | Rust 两次分配；Go 在 O 与 L 相同时共享同一个字符串，且有 tiny 分配器 |
| `Arc<IndexInfo>` | 26.7 | 20.0 | 1.93 | 224 B + 16 B = 240 B，落进 256 B 级别；Go 192 B |
| 索引列名 | 2.1 | 0 | 0.61 | Rust 复制 `CIStr`；Go 共享底层字符串 |
| 索引名、表名 | 3.2 | 1.6 | 0.46 | O / L 不共享 |
| 索引列列表 | 10.5 | 9.6 | 0.27 | |
| 表结构体 | 18.8 | 16.8 | 0.58 | 512 B 级别；Go 480 B |

`ColumnInfo` 比 Go 大 56 B：两个 `AnyValue`（默认值、原始默认值）各 32 B，Go 的 `interface{}` 16 B（+32）；`Option<BTreeMap>` 32 B，Go 的 map 指针 8 B（+24）；`String` 24 B，Go 16 B（`CIStr` +16，另两个字符串 +16）；`FieldType` −8；Go 的对齐填充 −24。

### 9.2 大小级别与规则

**分配器**：tidb-server（`crates/poc/server/src/main.rs`）无条件以 jemalloc（`tikv-jemallocator`）为全局分配器，Linux 与 macOS 相同（A1 / P0 决定 D7），没有按平台的条件编译；基准程序和 §5 的内存测试用的也是它。jemalloc 把请求向上取整到大小级别：128 B 以内每 16 B 一级，之后每次翻倍分四级（…、256、320、384、448、512、640…）。级别与平台无关（64 位目标上 `size_of` 也相同）。一个长期存活的 `Arc<T>` 是一次 `ArcInner<T>` = 16 B 引用计数 + `T` 的分配，所以：

- 结构体变小只在 `ArcInner<T>` 跨进更低的级别时省内存。`ColumnInfo` 从 400 B 缩到 369 B 不省一个字节（385–448 B 都是 448 B 级别）；缩到 368 B 每列省 64 B。
- 反过来，在级别边界上加一个字段，每张表的每一列都多付一整级。
- 换分配器时这一推理要重做：glibc malloc 按 16 B 取整，省下的只是真正去掉的字节（本步每列 32 B，约 9.2 MiB；推算，未测：服务器不用 glibc malloc）；mimalloc 的级别与 jemalloc 相近，同样有级别效应。

**规则**（`M-SHARE` 的补充）：

1. 长期存活的共享元数据（`ColumnInfo`、`IndexInfo`、`TableInfo` 及今后同类结构）中**很少被设置**的可为 nil 的字段装箱：Go 的 map → `Option<Box<BTreeMap<_, _>>>`，`[]byte` → `Option<Box<[u8]>>`。`None` 是 Go 的 nil，`Some` 了一个空值是 Go 的非 nil 空值，JSON 与 Go 逐字节相同（nil 编码为 `null`，非 nil 空值为 `{}` / `""`）。装箱在生成器中按字段登记（tracker `govec/cmd/rsgen` 的 `boxedFields`，与 `sharedFields` 同一机制），不手改生成的文件。
2. 这些结构体的 `size_of` 由测试守住当前级别（`crates/model/tests/layout.rs`）：`size_of::<ColumnInfo>() ≤ 368`（`ArcInner` ≤ 384 B），`IndexInfo` + 16 ≤ 256，`TableInfo` + 16 ≤ 512。改动越过级别时测试失败；办法是把另一个很少设置的字段装箱，或者在 spec 中说明为什么值得多付一级。
3. 常用字段（每列都有值的默认值、名字、类型）不装箱：多一次分配和一次间接访问，省不下内存。

### 9.3 第 1 步（本 PR）

`ColumnInfo` 的三个很少设置的字段装箱：`dependences`（只有被生成列引用的列才有）→ `Option<Box<BTreeMap<String, ()>>>`，`origin_default_value_bit`、`default_value_bit`（只有带默认值的 BIT 列才有）→ `Option<Box<[u8]>>`。三个字段从 80 B 降到 40 B，`size_of::<ColumnInfo>()` 从 400 B 降到 368 B（剩下 8 B 是小字段的对齐填充），`ArcInner<ColumnInfo>` 384 B，正好落进 384 B 级别。

生成器（`boxedFields`）只改字段类型和解码：解码时把值移出盒子，按原来的规则解码（map 与 encoding/json 一样合并进已有的 map，`[]byte` 整体替换；`null` 置为 nil），再放回盒子。编码与 `omitempty` 经 `Deref` 读取，与原来相同。手写代码只有 `SetOriginDefaultValue` / `SetDefaultValue` 的赋值（`Some(v.into())`）和 `GoCopy for Box<[u8]>`。

### 9.4 测试与测量（arm64 macOS，jemalloc）

- `crates/model/tests/layout.rs`：§9.2 规则 2 的三个 `size_of` 守卫；三个装箱字段的 nil、非 nil 空值、非空值各自的编码字节（`null` / `""` / `{}` / base64 / 排序的对象），以及解码到空的和已填充的 `ColumnInfo`（`null` 置 nil，`{}` 合并进已有的 map，字节替换）。
- govec `model` 领域新增 7 条 `into` 记录（`site/boxed/*`：nil、空值、非空值、JSON 数组形式的字节，分别解码到零值和已填充的 `ColumnInfo` 后编码），共 62,833 条，全部通过；`infoschema`、`table` 领域不变。
- 内存（§5 的 `arc_sharing_mem`）：

| 结构 | 批次 A 之后 | 本步 | Go v8.5.8 |
|---|--:|--:|--:|
| `TableInfo`（模型） | 152.8 MiB（534 B/列） | **134.4 MiB（470 B/列）** | 117.6 MiB（411 B/列） |
| `size_of::<ColumnInfo>()` / `ArcInner` 级别 | 400 B / 448 B | 368 B / 384 B | 344 B / 352 B |
| 表对象、InfoSchema v2 全量加载 | 38.5 / 47.0 MiB | 38.5 / 47.0 MiB | — |

  −18.4 MiB，与每列 64 B × 300,000 列（18.3 MiB）相符。数字为 arm64 macOS 上的实测（测试构建）。Linux arm64 上以 jemalloc、release 构建另测一次（评审 Lucky），结果相同：模型 134.4 MiB（470 B/列），表对象 38.5 MiB，InfoSchema v2 全量加载 46.9 MiB，`size_of::<ColumnInfo>()` 368 B。§9.2 的 `size_of` 守卫只在 64 位目标上运行（`#[cfg(target_pointer_width = "64")]`）。

### 9.5 后续

- **第 2 步（提议，未实现）**：`ColumnInfo` ≤ 304 B，`ArcInner` 进 320 B 级别，再省约 18 MiB（模型约 116 MiB，与 Go 相当）。做法：两个默认值改 `Option<Box<AnyValue>>`（`None` = nil，大多数列没有默认值；各 32 B → 8 B），`CIStr` 与 `comment` 改紧凑表示（如 `Box<str>`，或 O 与 L 相同时只存一份）。nil 只有一种表示：两个默认值字段设为私有，只经 `default_value()` / `set_default_value()`（原始默认值同样）访问，setter 把 `AnyValue::Nil` 映射为 `None`，于是 `Some(AnyValue::Nil)` 不会出现，这一点在一处保证（生成的 JSON 解码同样经 setter）。风险中等：`default_value` 的读写分布在 DDL、`GetDefaultValue`、执行器的默认值求值中，都要改；`CIStr` 属 D02.3，`name.o` / `name.l` 的直接字段访问遍及各 crate，改表示是大范围的 API 变化。需要先单独评估，另开 PR。
- **第 3 步**：`CIStr` 在 O 与 L 相同时共享（约 −2.9 MiB）；`IndexInfo` ≤ 176 B（进 192 B 级别，约 −1.9 MiB）。
- 每一步改变级别时同时收紧 `layout.rs` 中的守卫。
