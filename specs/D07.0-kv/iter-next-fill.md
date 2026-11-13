# D07.0 T11：`Iterator::next` 的形状（对比与建议）

对应 `tasks.md` T11 的后半部分和 #53。前半部分（迭代器生命周期与 key 参数分开）已在 #120 完成。本页只做对比，由 Maya、Lucky 决定。

## 1. 问题

`tidb_kv::kv::Iterator::next(&mut self) -> KvFuture<'_, ()>` 每次返回一个装箱 future：每行一次堆分配和一次虚调用。Go 的 `Iterator.Next()` 在内存中的批里只是一次普通方法调用，只有批用完时才发 RPC 取下一批（client-go `Scanner`，`scanBatchSize = 256`）。

## 2. 候选

| | 形状 | 调用方写法 | 批内每行 | 取下一批 |
|---|---|---|---|---|
| A（现状） | `fn next(&mut self) -> KvFuture<'_, ()>` | `it.next().await?` | 1 次分配 + 1 次虚调用 | 同左 |
| C 拆分 | `fn advance(&mut self) -> bool`（同步，批用完时返回 false）+ `fn fill(&mut self) -> KvFuture<'_, ()>` | `if !it.advance() { it.fill().await? }` | 1 次虚调用 | 1 次分配 |
| D 枚举 future | `fn next(&mut self) -> Next<'_>`，`enum Next<'a> { Ready, Fetch(KvFuture<'a, ()>) }` 实现 `Future` | `it.next().await?`（不变） | 1 次虚调用 + 一个分支 | 1 次分配 |

C 和 D 都保持 trait 可以做成 `dyn`。

## 3. 测量

`crates/kv/benches/iter.rs`：1024 行的内存批，经 `&mut dyn` 调用，读 key 和 value；C、D 每 256 行"取"一次（future 立即完成，只计分配和调用开销，不计 RPC）。Linux arm64（10 核虚拟机），criterion 中位数：

| 方案 | 1024 行 | 每行 |
|---|---|---|
| A 异步 `next`（现状） | 10.28 µs | 10.0 ns |
| 同步循环（下限） | 1.21 µs | 1.2 ns |
| C `advance` + `fill` | 2.35 µs | 2.3 ns |
| D 枚举 future | 3.37 µs | 3.3 ns |

（tasks.md T11 中 arm64 Mac 上 A 为每行约 11.2 ns，与此一致。）

## 4. 对使用方的影响

TiDB v8.5.8 中 `kv.Iterator` 的读路径使用方（`Iter` / `IterReverse` 调用处，不含测试）：

| 使用方 | 场景 | 行数量级 | 每行省下的约 7 ns 是否重要 |
|---|---|---|---|
| coprocessor（`store/copr`） | 表扫描、索引扫描、聚合 | — | **不经过 `kv.Iterator`**：结果按 chunk 返回（`kv.Response.Next`，每个响应一次 await），三种方案都不影响 |
| snapshot 扫描（`store/driver`，D07.1 的 TiKV scanner） | meta（`structure` 的 hash / list）、DDL 历史 job、`information_schema` 的部分表 | 几十到几万行 | 很小：每批一次 RPC（毫秒级）远大于每行 ns 级的差别 |
| memdb / `UnionIter`（`executor/mem_reader.go`） | 有未提交写入的事务读；tidb-rs 只读，暂不需要 | — | 目前不涉及 |
| `executor/sample.go` | `TABLESAMPLE REGIONS()` | 每 region 若干行 | 很小 |
| `table` / `statistics` / `ddl` | 记录迭代、统计信息读取、DDL 回填 | 统计信息可达百万行 | 中等：百万行时 A 比 D 多约 7 ms，与读取本身相比仍小 |

也就是说，主要读路径（coprocessor）不受影响；每行开销只在 snapshot / memdb 逐行迭代时出现，而这些地方要么行数少，要么每批的 RPC 远大于这部分开销。

## 5. 比较

| | A 现状 | C 拆分 | D 枚举 future |
|---|---|---|---|
| 每行开销 | 10.0 ns | 2.3 ns | 3.3 ns |
| 调用方改动 | 无 | 所有循环改成两步；漏写 `fill` 会静默地提前结束迭代 | 无（`it.next().await?` 不变） |
| 与 Go 代码形状的一致性（移植规则） | 一致 | 不一致：Go 只有 `Next()` | 一致 |
| 实现方改动 | 无 | 每个实现拆成两个方法 | 每个实现把 `Box::pin(async {..})` 换成 `Next::Ready` / `Next::Fetch(..)`，组合迭代器（`UnionIter` 等）直接转发内部的 `Next` |
| 风险 | 无 | 中：调用方容易写错 | 低：`Next` 只是一个两分支的 future |

## 6. 建议

**D（枚举 future），在 D07.1 移植 TiKV scanner 和 memdb 之前改。** 理由：

1. 调用方写法不变，和 Go 的 `Next()` 一一对应，符合逐函数移植的规则；C 的两步写法在每个调用处都偏离 Go，而且漏写 `fill` 不会报错。
2. 每行开销从 10 ns 降到 3.3 ns，与 C 只差约 1 ns/行，在 §4 的使用方中都不可测。
3. 现在改成本最低：目前的实现只有 `EmptyIterator`、测试 mock 和 PoC 示例；等 D07.1 写完 TiKV scanner、memdb、`UnionIter` 再改，就要改动更多实现。

如果评审认为每行开销本来就不重要（§4），也可以保持 A 不动，在 T11 中记为"已评估，不改"。D 的改动量很小，所以仍推荐 D。

## 7. 附带：`key()` / `value()` 在迭代器失效时的约定

已写进 `Iterator` trait 的文档（与本页同一分支）：失效时 `key()` 返回空 key、`value()` 返回空切片，不能 panic。Go 的调用方（如 `ReverseHashIterator.Next`）在 `Next` 之后不检查 `Valid()` 就读 `Key()`，依赖 client-go `Scanner.Key` 返回 nil。
