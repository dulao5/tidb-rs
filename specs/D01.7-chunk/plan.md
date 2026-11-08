# D01.7 Chunk：计划与记录

## 1. 交接状态

- 分支：tidb-rs `block/D01.7-chunk`（worktree `projects/tidb-rs-d017`），tracker `govec/chunk-D01.7`（`projects/tracker-d017`，PR #19）。
- 完成：`crates/chunk`（`tidb-chunk`）全部可移植函数（276/330），govec `chunk` 领域与回放，单元测试，基准，CI 加入 `tidb-chunk`。
- 待做：等 D01.1 合并后补上 Enum/Set/Datum 相关的 32 个函数和 `mutrow.go`（见 tasks.md T8）；D01.10 补落盘文件与 `RowContainer` 迭代器。

## 2. 实现顺序（已完成）

1. `goslice.rs`：Go 切片语义（len/cap、`growslice`、size class、共享缓冲写时复制、`capb()` 读到容量）。
2. `column.rs` → `chunk.rs` → `row.rs` → `codec.rs` → `chunk_util.rs` → `iterator.rs` → `list.rs` → `compare.rs` → `alloc.rs`。
3. govec `chunk` 领域：脚本式 op，生成器边构造边执行；dump 用 reflect 读出四个切片和容量。
4. 回放测试 `tests/govec_chunk.rs`，第一次运行 126 条不一致，原因全部是 Go 切片表达式可以读到 len 之外（§3.1 of spec）；改为 `capb()` 后全部一致。
5. 性能：append 的快路径（小定长元素不调用 memcpy）、解码用游标只在共享缓冲处切 `Bytes`、列改用 `triomphe::Arc`。

## 3. 性能（Apple M 系列，同一台机器；Go 5 次中位数，Rust criterion）

负载与 Go 基准相同（`benches/chunk.rs` 逐个对应 chunk_test.go、codec_test.go、chunk_util_test.go、list_test.go 中的 Benchmark）。"修正前"是第一版 PR 的数字。

| 基准 | Go | Rust 修正前 | Rust 现在 | 现在/Go |
|---|--:|--:|--:|--:|
| AppendInt（1000 次 AppendInt64） | 3.54 µs | 1.98 µs | 2.40 µs | 0.68 |
| AppendString（1000 次 "abcd"） | 3.99 µs | 3.57 µs | 2.79 µs | 0.70 |
| AppendRow（1000 行 × 4 列） | 16.2 µs | 21.3 µs | 15.9 µs | 0.98 |
| Access（8192 次 GetRow().GetInt64） | 4.38 µs | 4.56 µs | 2.59 µs | 0.59 |
| EncodeChunk | 2,142 ns | 168 ns | 169 ns | 0.08 |
| Decode | 181 ns | 121 ns | 123 ns | 0.68 |
| DecodeToChunk（4 × 1024 BIGINT，全 NULL） | 13.8 ns | (33.9 ns)¹ | 39.3 ns | **2.84** |
| DecodeToChunkWithVariableType | 38.3 ns | 60.2 ns | 58.9 ns | **1.54** |
| CopySelectedJoinRowsDirect | 19.8 µs | 21.1 µs | 16.5 µs | 0.83 |
| ListGetRow（每次 GetRow） | 0.76 ns | (2.94 ns)² | 0.51 ns | 0.67 |
| ChunkMemoryUsage | 1.53 ns（结果不用：2.03 ns）³ | 2.72 ns | 2.47 ns | **1.62**（1.22）³ |

1. 第一版的 Rust 基准往列里追加的是非 NULL 值（null count 为 0，走 `setAllNotNull`），Go 的基准是零值位图、零值数据，即每列 1024 个 NULL（null count > 0，位图直接切自缓冲）。现在照 Go 构造，每列两个共享缓冲。
2. 第一版用 `(i + 1) % len` 取下一个指针，除法在循环依赖链上；Go 是顺序遍历 `ptrs[i]`。现在一次迭代顺序走完 10000 个指针，按每次 GetRow 报告。
3. Go 自己的基准丢弃 `MemoryUsage()` 的结果，内联后字段读取是死代码；括号里是把结果累加到包级变量的 Go 版本（同机 5 次中位数 2.03 ns）。

修正（commit `66c6592`、`e1a650c`）：
- `GoSlice` 缓存元素 0 的地址，读和容量内的写不再对 owned/shared 做 match，也不再重复检查底层 `Vec` 的边界；1/4/8 字节的追加不调用 memcpy；空位图置位走 `or_at`。
- 解码时共享缓冲原地设置（`set_shared`）；`setAllNotNull` 在容量内一次填满（与 Go 分段追加的结果相同，容量内不会增长）。
- `col_mut` 的唯一性检查内联，复制共享列的慢路径不内联。

**没有追平的两项及原因**：
- **DecodeToChunk 系列**：剩下的差距是共享缓冲的所有权。每个解码出来的缓冲（位图、offsets、数据）是一个 `Bytes` 切片：建立时通过 `Bytes` 的 vtable 克隆并对响应缓冲的引用计数加一，替换旧缓冲时再通过 vtable 释放、减一；Go 只赋值一个切片头。实测：同一基准换成 `Bytes::from_static`（没有原子操作）是 30.1 ns，所以 10 次引用计数操作约 8 ns；再去掉缓冲替换本身（只保留读头、`setAllNotNull` 等）是 19 ns。也就是说，即使把引用计数批量化（一次解码只做一次原子加减），也只能降到 30 ns 左右；要追平需要不经 `Bytes` 的表示（例如按 N5a 3.4 让列借用响应缓冲、带生命周期），这改变 `Chunk` 的 API，不在本块范围。绝对值是每个 1024 行的响应多 25–20 ns（每行约 0.02 ns）；N5a §5 中"sysbench / TPC-H 解码 CPU 占比不高于 Go"这一条端到端指标在 D07.3 接上真实响应后测量。
- **ChunkMemoryUsage**：与结果被使用的 Go 版本相比慢 0.44 ns（4 列）。代码已是每列 5 次加载、无分支以外的开销；剩下的是跨 crate 的函数调用和 `Column` 中四个容量字段分布在不同的缓存行。内联反而更慢（3.8 ns），没有采用。

N5a §5 的非对齐读取：Access 的热循环中 `get_int64` 是一条普通的 64 位加载（`ldr x8, [x16, x8]`，aarch64，已核对汇编）；x86-64 在本机无法测量，CI 在 x86-64 上运行同样的代码。

## 4. 验证命令

```
cargo test -p tidb-chunk                                    # 回放 + 单元测试
cargo test --profile release-checked -p tidb-chunk
python3 scripts/converge.py --block D01.7 --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv crates/chunk/src/*.rs
cargo bench -p tidb-chunk --bench chunk
# Go 侧（tracker govec 模块下）：
go test github.com/pingcap/tidb/pkg/util/chunk -run XXX -bench 'BenchmarkAppendInt$|...' -count 5
```
