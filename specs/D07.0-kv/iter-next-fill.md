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
| H 混合（Lucky，#123） | 实现方提供 C 的 `advance` + `fill`；trait 提供默认方法 `fn next(&mut self) -> Next<'_>`（即 D：`advance()` 为 true 返回 `Ready`，否则返回 `Fetch(self.fill())`） | `it.next().await?`（不变） | 1 次虚调用 + 一个分支 | 1 次分配 |

C、D、H 都保持 trait 可以做成 `dyn`。

## 3. 测量

`crates/kv/benches/iter.rs`：1024 行的内存批，经 `&mut dyn` 调用，读 key 和 value；C、D 每 256 行"取"一次（future 立即完成，只计分配和调用开销，不计 RPC）。Linux arm64（10 核虚拟机），criterion 中位数：

| 方案 | 1024 行 | 每行 |
|---|---|---|
| A 异步 `next`（现状） | 10.28 µs | 10.0 ns |
| 同步循环（下限） | 1.21 µs | 1.2 ns |
| C `advance` + `fill` | 2.35 µs | 2.3 ns |
| D 枚举 future | 3.37 µs | 3.3 ns |
| H 混合（第二次运行：A 9.7、C 2.6、D 3.1） | 3.08 µs | 3.0 ns |

（tasks.md T11 中 arm64 Mac 上 A 为每行约 11.2 ns，与此一致。）

## 4. 对使用方的影响

TiDB v8.5.8 中 `kv.Iterator` 的读路径使用方（`Iter` / `IterReverse` 调用处，不含测试）：

| 使用方 | 场景 | 行数量级 | 每行省下的约 7 ns 是否重要 |
|---|---|---|---|
| coprocessor（`store/copr`） | 表扫描、索引扫描、聚合 | — | **不经过 `kv.Iterator`**：结果按 chunk 返回（`kv.Response.Next`，每个响应一次 await），三种方案都不影响 |
| snapshot 扫描（`store/driver`，D07.1 的 TiKV scanner） | meta（`structure` 的 hash / list）、DDL 历史 job、`information_schema` 的部分表 | 几十到几万行 | 很小：每批一次 RPC（毫秒级）远大于每行 ns 级的差别 |
| memdb（`executor/mem_reader.go`） | 有未提交写入的事务读；tidb-rs 只读，暂不需要 | — | 目前不涉及 |
| `UnionIter`（`store/driver/txn/union_iter.go`） | memdb 与 snapshot 的合并迭代器；读路径中唯一的组合迭代器，tidb-rs 支持写事务后需要 | 同 snapshot | 不在于每行开销，而在于能否组合，见 §5 |
| `executor/sample.go` | `TABLESAMPLE REGIONS()` | 每 region 若干行 | 很小 |
| `table` / `statistics` / `ddl` | 记录迭代、统计信息读取、DDL 回填 | 统计信息可达百万行 | 中等：百万行时 A 比 D 多约 7 ms，与读取本身相比仍小 |

也就是说，主要读路径（coprocessor）不受影响；每行开销只在 snapshot / memdb 逐行迭代时出现，而这些地方要么行数少，要么每批的 RPC 远大于这部分开销。

## 5. 比较

**选择取决于组合迭代器怎样实现，而不是每行开销**：§4 的使用方都测不出 A、C、D、H 之间几 ns 的差别。

关键是 `UnionIter`。Go 的 `UnionIter.updateCur`（`union_iter.go:66`）是一个循环，一次外层 `Next` 可能多次调用 `dirtyNext()` 和 `snapshotNext()`（跳过已删除的脏数据，key 相等时两边都前进）。在 D 下，外层 `next()` 调用 `self.snapshot.next()`，如果返回 `Fetch(f)`，循环的剩余部分要在 `f` 完成后才能继续；而 `f` 可变借用了 `self.snapshot`，继续执行又需要 `&mut self`，一个装箱的 `async move { f.await?; self.update_cur().await }` 同时持有不了两者（Lucky，#123）。只用 D 的 trait，就只能每次都走装箱的慢路径，失去 D 的好处；要避免这一点，需要事先知道内层这一步会不会取批，也就是 C 的 `advance()`。

| | A 现状 | C 拆分 | D 枚举 future | H 混合 |
|---|---|---|---|---|
| 每行开销 | 10.0 ns | 2.3 ns | 3.3 ns | 3.0 ns |
| 调用方写法 | `it.next().await?` | 两步循环，漏写 `fill` 会静默地提前结束 | `it.next().await?` | `it.next().await?` |
| 与 Go `Next()` 的一致性 | 一致 | 不一致 | 一致 | 一致 |
| 组合迭代器（`UnionIter`） | 可以（每步装箱） | 可以，在内层 `advance` 上同步组合 | **不能直接组合**（见上） | 可以：组合迭代器实现 `advance` / `fill`，在内层 `advance` 上同步组合 |
| 风险 | 无 | 中：调用方容易写错 | 高：组合迭代器写不出来 | 低 |

## 6. 建议

**H（混合），在 D07.1 移植 TiKV scanner 和 memdb 之前改。**

```rust
pub trait Iterator: Send {
    fn valid(&self) -> bool;
    fn key(&self) -> &Key;
    fn value(&self) -> &[u8];
    /// 实现方：在已缓存的批内前进；返回 false 表示需要 `fill`。
    fn advance(&mut self) -> bool;
    /// 实现方：取下一批，并定位到 `next` 本应到达的位置。
    fn fill(&mut self) -> KvFuture<'_, ()>;
    /// 调用方（Go 的 `Next`）：`it.next().await?`。
    fn next(&mut self) -> Next<'_> {
        if self.advance() { Next::Ready } else { Next::Fetch(self.fill()) }
    }
    fn close(&mut self);
}
```

理由：

1. 调用方写法不变（`it.next().await?`），与 Go 的 `Next()` 一一对应，也不会漏掉 `fill`。
2. 组合迭代器在内层的 `advance` 上同步组合：`UnionIter::advance` 用内层的 `advance()` 跑 `updateCur` 的循环，一旦内层需要取批就返回 false；它的 `fill` 先填充那个内层迭代器，再把循环跑完。单一来源的迭代器（TiKV scanner、memdb）自然对应：`advance` 在缓存的批内移动，`fill` 是 RPC。
3. 每行开销与 D 相同（3.0 ns），比现状少约 7 ns；`next` 是 trait 提供的默认方法，trait 仍可以做成 `dyn`。
4. 现在改成本最低：目前的实现只有 `EmptyIterator`、测试 mock 和 PoC 示例。

**实现方的约定**（写进 trait 文档）：

- **只有 `fill` 会出错，`advance` 不会**。这与 Go 一致：`Next` 的错误只来自 scanner 的 RPC 和内层迭代器的 `Next`。
- **`fill` 完成后，迭代器的位置必须与 `next` 本应到达的位置完全相同**：取到下一批就定位到它的第一条，没有更多数据就 `valid() = false`。
- **最后一批**：单一来源的迭代器在批用完时并不知道后面还有没有数据，所以 `advance` 返回 false，由 `fill` 发现数据已结束并把迭代器置为失效。这只在最后多一次装箱 future，可以接受。内存迭代器（memdb、内存批）知道自己已到末尾时，`advance` 可以直接返回 true 并让 `valid()` 为 false，不经过 `fill`。

如果评审认为每行开销本来就不重要（§4），也可以保持 A 不动，在 T11 中记为"已评估，不改"。但 A 下组合迭代器每步都要装箱，H 的改动量又很小，所以仍推荐 H。

## 7. 附带：`key()` / `value()` 在迭代器失效时的约定

已写进 `Iterator` trait 的文档（与本页同一分支）：失效时 `key()` 返回空 key、`value()` 返回空切片，不能 panic。Go 的调用方（如 `ReverseHashIterator.Next`）在 `Next` 之后不检查 `Valid()` 就读 `Key()`，依赖 client-go `Scanner.Key` 返回 nil。
