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

负载与 Go 基准相同（`benches/chunk.rs` 逐个对应 chunk_test.go、codec_test.go、chunk_util_test.go、list_test.go 中的 Benchmark）。

| 基准 | Go | Rust | Rust/Go |
|---|--:|--:|--:|
| AppendInt（1000 次 AppendInt64） | 3.48 µs | 1.98 µs | 0.57 |
| AppendString（1000 次 "abcd"） | 3.76 µs | 3.57 µs | 0.95 |
| AppendRow（1000 行 × 4 列） | 16.4 µs | 21.3 µs | **1.30** |
| Access（8192 次 GetRow().GetInt64） | 4.05 µs | 4.56 µs | **1.12** |
| EncodeChunk（4 个空 BIGINT 列，cap 1024） | 2,145 ns | 168 ns | 0.08 |
| Decode | 177 ns | 121 ns | 0.68 |
| DecodeToChunk（4 × 1024 BIGINT） | 13.9 ns | 33.9 ns | **2.44** |
| DecodeToChunkWithVariableType | 37.9 ns | 60.2 ns | **1.59** |
| CopySelectedJoinRowsDirect | 21.2 µs | 21.1 µs | 1.00 |
| ListGetRow | 1.76 ns | 2.94 ns | **1.67** |
| ChunkMemoryUsage | 1.37 ns | 2.72 ns | **1.99** |

慢于 Go 的几项及原因：
- **DecodeToChunk 系列**：每个共享缓冲是一个 `Bytes`，切片时引用计数加一、替换旧缓冲时减一（原子操作）；Go 只是切片头赋值。4 列共约 12 次原子操作。这是零拷贝共享所有权的代价（N5a 3.2），绝对值是每个响应几十纳秒。
- **AppendRow / Access / ListGetRow**：每个单元多几次分支（共享 / owned 的 match、`Option` 的列与行、`Arc` 唯一性检查）。
- **ChunkMemoryUsage**：每列从三个 `GoSlice` 和 `ElemBuf` 取容量。

N5a §5 的性能指标是"不低于 Go"，以上 6 项尚未达到，记入 tasks.md T9，作为 D12 算子接线前的优化项（例如：列缓冲去掉 `Store` 的 match，owned 时直接持有 `Vec`；`Row` 直接持有 `&Chunk` 和列切片）。

## 4. 验证命令

```
cargo test -p tidb-chunk                                    # 回放 + 单元测试
cargo test --profile release-checked -p tidb-chunk
python3 scripts/converge.py --block D01.7 --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv crates/chunk/src/*.rs
cargo bench -p tidb-chunk --bench chunk
# Go 侧（tracker govec 模块下）：
go test github.com/pingcap/tidb/pkg/util/chunk -run XXX -bench 'BenchmarkAppendInt$|...' -count 5
```
