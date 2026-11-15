# D11.1 统计结构（spec）

状态：第 1 部分（VS1-29：pseudo 统计与读取部分）评审中（2026-11-15）。块：D11.1（ROADMAP P3），crate `tidb-statistics`（N0 rank 10，与 D11.2 同一 crate；本 PR 建立）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（VS1-29）、§8 风险 7；[D10.6 spec](../D10.6-cardinality/spec.md)（读这些结构的基数估算）；[N13](../exceptions/N13-shared-metadata/spec.md)（长期共享元数据的规则，`M-SHARE`）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/`。

## 1. 范围

Go 范围：`pkg/statistics` 的结构与估算辅助（不含 `handle/`，那是 D11.2）。tracker 中 317 个函数。

### 1.1 第 1 部分（本 PR）

函数清单是 VS1 `functions.tsv` 中 VS1-29 的 D11.1 行（85 个），converge **85/85**。

| Go | Rust（`crates/statistics/src`） | 内容 |
|---|---|---|
| `statistics/table.go` | `table.rs` | `Table`、`HistColl`、`ColAndIdxExistenceMap`、`ExtendedStatsColl`、`PseudoTable`、`GenerateHistCollFromColumnInfo`、`ID2UniqueID`、`Copy` / `ShallowCopy`、`String`、`GetStatsInfo`、`GetStatsHealthy`、`IsOutdated`、`MemoryUsage` 等读取方法 |
| `statistics/column.go` | `column.rs` | `Column`、`ColumnStatsIsInvalid`、`EmptyColumn` 与读取方法 |
| `statistics/index.go` | `index.rs` | `Index`、`IndexStatsIsInvalid` 与读取方法 |
| `statistics/histogram.go` | `histogram.rs` | `Histogram` 结构、`NewHistogram`、`AppendBucket(WithNDV)`、`LocateBucket`、`LessRowCount`、`EqualRowCount`、`BetweenRowCount`、`GreaterRowCount`、`Copy`、`ToString` / `BucketToString` / `ValueToString`、`StatsLoadedStatus` |
| `statistics/scalar.go` | `scalar.rs` | `calcFraction`、`convertDatumToScalar`、`PreCalculateScalar` 等（`LessRowCount` 用到；`EnumRangeValues` 除外） |
| `statistics/cmsketch.go` | `cmsketch.rs` | 只有 `TopN` 的读取部分：`Num`、`TotalCount`、`Copy`、`MemoryUsage` |

清单外但一起移植的（没有出边的小函数，宪章第四条 5）：`LocateBucket`、`EqualRowCount`、`BucketCount`、`AbsRowCountDifference`、`GreaterRowCount`、`AppendBucket(WithNDV)`、scalar.go 的辅助函数、`StatsLoadedStatus` 的方法、`GetAnalyzeRowCount`、`MeetAutoAnalyzeMinCnt`、`IsAnalyzed`、内存用量的小结构。

### 1.2 第 2 部分与其他块（边界）

- **第 2 部分（`blocked-by:D11.1`）**：直方图构建（`builder.go`，ANALYZE 委托给 Go TiDB）、CMSketch、FMSketch、采样（`sample.go`、`row_sampler.go`）、TopN 的查询与构建（`QueryTopN`、`BetweenCount`、合并）、`OutOfRange` / `OutOfRangeRowCount`、`EnumRangeValues`、合并全局统计、protobuf 与存储编码、`debugtrace.go`、`analyze*.go`、`estimate.go`、扩展统计的构建。
- **D11.2**：统计缓存、加载（同步 / 异步）、淘汰（`DropEvicted`、`DropUnnecessaryData`）、`ColumnIsLoadNeeded` / `IndexIsLoadNeeded`、`SetAllIndexFullLoadForBootstrap`；`asyncload.AsyncLoadHistogramNeededItems.Insert` 是桩（§2 第 7 条）。
- `CMSketch`、`FMSketch` 在第 1 部分是**没有值的类型**（`enum CMSketch {}`）：第 1 部分不构建也不加载统计，所以任何统计都不会持有它们；`Option<Arc<CMSketch>>` 永远是 `None`，它们的方法在编译期就不可达（不是运行时 panic）。

## 2. 承载方式

1. `Column.Info` / `Index.Info` 是 `Option<Arc<ColumnInfo>>` / `Option<Arc<IndexInfo>>`，与 `TableInfo.Columns[i]` / `Indices[i]` 是同一个 `Arc`（Go 是同一个指针）；Go 的 `Copy` 对 `Info` 调 `Clone()`（深拷贝），Rust 写成 `Arc::new(ColumnInfo::clone(&info))`。
2. `Histogram.Tp` 是 `Arc<FieldType>`。Go 的 `PseudoTable` 传 `&col.FieldType`（指向列定义），Rust 的 `ColumnInfo` 按值持有类型（N13 §8.2），所以每个 pseudo 列复制一次 `FieldType`；字符串类型 Go 先 `Clone` 再改排序规则，Rust 用 `Arc::make_mut`（不改调用方的值）。
3. Go 嵌入 `Histogram`、`StatsLoadedStatus`（`Column` / `Index`）和 `HistColl`（`Table`）：前两者是字段，Go 经 `*Column` 调用的提升方法写成委托方法；`Table` 对 `HistColl` 实现 `Deref` / `DerefMut`（`M-SELF`）。
4. Go 的 map 遍历顺序是随机的；Rust 的 map 是 `BTreeMap`（按 id 升序）。逐处登记（`M-MAPORDER`）：`ForEach*Immutable`、`Get*Slice` 的顺序 Go 自己不确定，调用方都与顺序无关；`IndexStartWithColumn` / `ColumnByName` 在多个匹配时 Go 返回任意一个，Rust 返回 id 最小的那个（govec 只记录无歧义的情况）；`GetAnalyzeRowCount` Go 本来就排序。
5. `TopN`：Go 在第一次 `TotalCount()` 时经 `sync.Once` 把总数写进共享的 `TopN`（`cmsketch.go:612`–`:628`），这是通过共享指针的惰性写入。Rust 的 `TopN` 在构造时（`TopN::new`）算好总数，元素私有、只读，结构里没有内部可变性（§3）。
6. Go 的 `sctx planctx.PlanContext` 参数：这里只读会话变量，Rust 传 `&SessionVars`（Go 的 nil 是 `None`）。函数里的 optimizer debug trace（`debugtrace.*`）不移植：基数估算的入口在 trace 打开时返回 `blocked-by:D10.6`（D10.6 spec §2），所以这些函数走不到需要 trace 的状态。
7. `ColumnStatsIsInvalid` / `IndexStatsIsInvalid` 在统计没加载时把列 / 索引放进全局的异步加载集合（`asyncload.AsyncLoadHistogramNeededItems.Insert`）。tidb-rs 在 D11.2 之前没有统计句柄，也就没有可加载的东西：请求被丢弃（`asyncload.rs`，桩 `neededStatsMap.Insert`，`blocked-by:D11.2`）。它不影响发出请求的这条语句的估算（Go 也是异步加载，供之后的语句用）。
8. Go 用包级函数变量打破 import 环（`GetRowCountBy*`、`PrepareCols4MVIndex`）。前者 Rust 不需要（规划器直接调用 `tidb_planner::cardinality`，本 crate 不调用它们）；`PrepareCols4MVIndex` 由 planner/core 实现（D10.7），Rust 是 `OnceLock` 中的函数指针，没有注册时，`GenerateHistCollFromColumnInfo` 遇到 MV 索引返回 `blocked-by:D10.7`（§4 第 1 条）。
9. `MemoryUsage` 中的结构体大小用 Go 的 `unsafe.Sizeof`（64 位）：`EmptyHistogramSize` 112、`EmptyBucketSize` 24、`EmptyScalarSize` 24，统计缓存按 Go 的字节数记账。
10. 包级变量 `AutoAnalyzeMinCnt`（测试会改）、`RatioOfPseudoEstimate`（Go `atomic.Float64`）是 `static` 原子量，不是共享结构的字段（标 `N13-allow`）。
11. `M-FMA`：`Histogram.LessRowCountWithBktIdx` 的 `preCount + frac*(...)`（`histogram.go:555`）在 arm64 等目标上被 gc 融合。tracker 的登记表 `data/go_fma_sites.tsv` 本 PR 加入 `pkg/statistics` 与 `pkg/planner/cardinality` 两个包（第 1 部分涉及的位置见 D10.6 spec §2）。
12. `M-F2I`：`Index.QueryBytes` 的 `uint64(v)`、`GetScaledRealtimeAndModifyCnt` / `GetStatsHealthy` 的 `int64(...)` 用 `gostd::conv::f64_to_*`（`NotNullCount/NDV` 在 NDV 为 0 时是 NaN / Inf，amd64 与 arm64 结果不同，向量按目标分别比对）。

## 3. 共享规则与统计缓存的语义（N13，评审要求）

统计信息是长期共享的元数据，与 InfoSchema 同样按 [N13](../exceptions/N13-shared-metadata/spec.md) 设计，从第一天起遵守：

1. **读路径拿快照**：统计缓存（D11.2）中每张表是一个 `Arc<Table>`。语句开始规划时取一次（Go `GetPhysicalTableStats`），整个语句持有这个快照，只读。语句的 `HistColl`（`GenerateHistCollFromColumnInfo`）是新建的值，其中每列、每个索引的统计与快照共享同一个 `Arc<Column>` / `Arc<Index>`（不复制直方图）。`StatsInfo.HistColl` 是 `Option<Arc<HistColl>>`。
2. **更新整体替换**：后台更新（加载、增量计数、淘汰，D11.2）不在共享的 `Table` 上原地改，而是构建新的 `Table`（或对旧值 `copy()` / `shallow_copy()` 后修改），再整体替换缓存项（缓存本身的发布方式在 D11.2 定，按 `M-SNAP` 用 `ArcSwap` 或按表的替换）。正在规划的语句继续用旧快照，结果与 Go 相同：Go 的这些路径本来就先复制（下表）。
3. **共享结构里没有内部可变性**：`Table`、`HistColl`、`Column`、`Index`、`Histogram`、`TopN`、`ColAndIdxExistenceMap` 中没有 `Cell` / `Mutex` / 原子类型 / `OnceCell` 等字段；Go 的惰性缓存（`TopN` 的 `sync.Once`）改为构造时计算（§2 第 5 条）。CI 的 `scripts/check_model_mutability.py` 的检查范围本 PR 加入 `crates/statistics/src`。lib.rs 静态断言这些类型 `Send + Sync`。
4. **写时复制**：`HistColl` 的列 / 索引表是 `Arc<BTreeMap<_, Arc<Column>>>`。`shallow_copy()` 与 Go 的 `ShallowCopy` 一样共享两张表（克隆 `Arc`）；`set_col` / `del_col` 等写入经 `Arc::make_mut`，表被共享时先复制，写入不会出现在别的持有者那里。`CalcPreScalar` 改直方图时同样先 `make_mut`（独占时原地改，与 Go 相同）。Go 的结构体复制 `tbl := *statsTbl` 是派生的 `Clone`（共享 `Arc`），Go 的 `Copy` 方法是深拷贝 `copy()`。
5. **指针只向下**：统计不保存指向 `Table` / `HistColl` 的 `Arc` 或 `Weak`。
6. 例外：语句级的 `HistColl.MVIdx2Columns` 持有 `Arc<expression::Column>`，而 `expression::Column` 有缓存哈希的 `OnceLock`（D08.1，N1 的语句级表达式）。它只在语句内的 `HistColl` 中出现，不进统计缓存。

Go 通过共享指针写统计结构的位置（v8.5.8 审计，N13 §3 规则 6 的两类）：

| # | 位置 | 写了什么 | 分类 | Rust |
|---|---|---|---|---|
| 1 | `statistics/handle/cache/internal/lfu/lfu_cache.go:190`–`:191` | 淘汰时 `item.Value.(*statistics.Table).Copy()` 后 `DropEvicted()` | 已先复制 | 同（D11.2）；`DropEvicted` 只在副本上调用 |
| 2 | `statistics/handle/cache/statscache.go:206` | `oldTbl.ShallowCopy()` 后改计数 | 已先复制（只改标量字段） | `shallow_copy()` 后改字段 |
| 3 | `planner/core/logical_plan_builder.go:4810`–`:4812` | `OptObjectiveDeterminate` 下 `ShallowCopy()` 后改 `RealtimeCount` / `ModifyCount` | 已先复制 | 同（VS1-28 / D10.2 的 `getStatsTable`） |
| 4 | `planner/core/logical_plan_builder.go:4827`–`:4828` | `tbl := *statsTbl; tbl.Pseudo = true` | 已先复制（结构体复制） | `Clone` 后改 |
| 5 | `statistics/cmsketch.go:612`–`:628` | `TopN.calculateCount` 经 `sync.Once` 写 `totalCount`、`minCount` | Go 应该先算好（惰性缓存，`sync.Once` 保证了并发安全，但写在共享对象上） | 构造时计算，无写入 |
| 6 | `statistics/table.go:294`–`:301`、`:346`–`:355` | `SetCol` / `SetIdx` / `DelCol` / `DelIdx` 写 map（`ShallowCopy` 之后两份共享同一个 map） | Go 应该先复制（现有调用方在新建或已复制的表上调用） | `Arc::make_mut` |
| 7 | `statistics/table.go:407`–`:420` | `CalcPreScalar` 改桶的计数和 `Scalars` | 只用于 bootstrap 新建的表（`handle/bootstrap.go:712`），无共享 | `Arc::make_mut` |
| 8 | `statistics/table.go:920`–`:929` | `ReleaseAndPutToPool` 清空 map、把 FMSketch 放回池 | `M-POOL`：没有池；Rust 只替换本副本的表 | 替换为新的空表 |
| 9 | `statistics/index.go:165`–`:170` | `EvictAllStats`（只用于测试）原地改 | 测试用 | `&mut self` |
| 10 | `planner/cardinality/selectivity.go:970`–`:975` | `GetSelectivityByFilter` 临时把条件中列的 `Index` 改成 0 再改回 | Go 应该先复制（改的是共享的表达式列，D10.6 第 2 部分） | 未移植（桩） |

结论：读路径上没有依赖"共享可见性"的写入。缓存的 pseudo 表：Go 在没有统计时每条语句新建 `PseudoTable`（每列一个 `Column` 与空直方图），Rust 相同；是否按 `TableInfo` 版本缓存 pseudo 表留给 D11.2 测量后决定（Go 没有这样做，改变的只是分配次数）。

## 4. 与 Go 的差异（均已在代码中标注）

1. `GenerateHistCollFromColumnInfo` 返回 `Result`：MV 索引需要 planner/core 的 `PrepareCols4MVIndex`（D10.7），未注册时返回 `blocked-by:D10.7`。Go 不会失败。有 MV 索引的表因此回退 PoC。
2. `Index.QueryBytes` 返回 `Result`：有 TopN 时需要 `TopN.QueryTopN`（第 2 部分），返回 `blocked-by:D11.1`；Go 先算 murmur3 哈希给 CMSketch 用，第 1 部分没有 CMSketch，不算。
3. map 顺序（§2 第 4 条）。
4. `MemoryUsage` 用 Rust `Vec` 的容量；Go 的 `append` 扩容步长不同，逐个追加构建的直方图可能报告不同的容量。pseudo 直方图（容量 0）与按 `bucketSize` 预留后追加的直方图相同（govec 覆盖）。
5. `Histogram.Copy`：Go 的 `newHist := *hg` 共享 `Scalars` 切片，Rust 复制（它在 `PreCalculateScalar` 之后不再被写）。
6. debug trace 不移植（§2 第 6 条）；`ValueToString` 只移植 `BucketToString` 用到的形式（`vars` 与 `idxColumnTypes` 为 nil）。

## 5. 测试

- govec 领域 `cardinality`（tracker `govec/domains/cardinality.go`，D10.6 与 D11.1 共用），tidb-rs `crates/planner/tests/govec_cardinality.rs` 回放。本块相关的 op：`pseudo_table`（随机表的 `PseudoTable` 与所有读取方法、`String`、内存用量、健康度）、`hist_coll`、`hist`（7 种类型的直方图：`LessRowCount`、`EqualRowCount`、`LocateBucket`、`BetweenRowCount`、`GreaterRowCount`、`ToString`、`CalcPreScalar` 与 scalar 分数）、`col_stats`（列 / 索引统计与 `StatsLoadedStatus` 各状态、`ColumnStatsIsInvalid` / `IndexStatsIsInvalid`、`QueryBytes` 的 `M-F2I`）、`existence`、`table_ops`（`Copy`、`ShallowCopy`、`DelCol` / `SetCol`、`GetStatsInfo`、扩展统计、`GetStatsHealthy`）、`topn`。数字见 D10.6 spec §6。
- 单元测试：`histogram::tests::empty_histogram_reads`、`cmsketch::tests::top_n_total_is_computed_at_construction`；`govec_cardinality.rs` 的 `statistics_share_metadata_and_copy_on_write`（pseudo 统计与 `TableInfo` 共享列 / 索引的 `Arc`；`shallow_copy` 共享、`del_col` 后共享的表不变；`copy` 不共享；语句的 `HistColl` 与快照共享列统计）。
- Go 单测：`statistics` 包的测试大多依赖存储或直方图构建（第 2 部分）；`row_count_test.go` 的 `TestPseudoTable` 进入向量（D10.6）。
