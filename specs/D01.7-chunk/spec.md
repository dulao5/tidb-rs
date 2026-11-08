# D01.7 Chunk 与列存：块 spec

状态：草案（2026-11-08）。阶段：P1，B 线（ROADMAP §4 L1）。基线：Go TiDB v8.5.8（`8b857efa2`）。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[mappings.md](../../docs/porting/mappings.md)、[N5a 列格式](../exceptions/N5a-chunk/spec.md)（决定 D23–D26）、[N3+A1](../exceptions/N3-A1-concurrency-memory/spec.md)（`M-MEM`、`M-POOL`）、[N0 crate 分层](../exceptions/N0-crate-layering/spec.md)。本块建立新 crate `tidb-chunk`（rank 3，依赖 `tidb-base`、`tidb-parser`、`tidb-types`、`tidb-memory`）。

## 1. Go 范围

函数清单来自 tracker `data/func_blocks.tsv` 中 block = D01.7 的行。本块把复用池的内部实现（`pool.go` 全部，`alloc.go` 的 `allocator`、`columnList`、`poolColumnAllocator`、`checkColumnType`，`chunk.go` 的 `Chunk.resetForReuse`）按 A1 的 `M-POOL` 改为 X（tracker `scripts/blocks/rules.py`），D01.7 由 349 个函数变为 330 个（含 `mutrow.go` 的 22 个）。

| Go 文件 | 函数数 | Rust 文件 | 状态 |
|---|--:|---|---|
| `column.go` | 84 | `column.rs` | 80 个已移植；`AppendEnum`、`AppendSet`、`GetEnum`、`GetSet` 等 D01.1 |
| `chunk.go` | 64 | `chunk.rs` | 60 个已移植；`AppendEnum`、`AppendSet`、`AppendDatum`、`ToString` 等 D01.1 |
| `row.go` | 26 | `row.rs` | 19 个已移植；`GetEnum`、`GetSet`、`GetDatumRow`、`GetDatumRowWithBuffer`、`GetDatum`、`DatumWithBuffer`、`ToString` 等 D01.1 |
| `codec.go` | 20 | `codec.rs` | 全部 |
| `chunk_util.go` | 14 | `chunk_util.rs` | 9 个已移植；`diskFileReaderWriter` 的 5 个方法等 D01.10（落盘文件、checksum 与加密层） |
| `iterator.go` | 52 | `iterator.rs` | 43 个已移植；`iterator4RowContainer` 的 9 个等 D01.10 |
| `list.go` | 15 | `list.rs` | 全部 |
| `compare.go` | 19 | `compare.rs` | 16 个已移植；`Compare`、`Chunk.LowerBound`、`Chunk.UpperBound` 等 D01.1 |
| `alloc.go` | 14 | `alloc.rs` | 全部（不复用，见 3.6） |
| `mutrow.go` | 22 | — | 全部等 D01.1：建立在 `any` / `types.Datum` 值上 |

收敛检查：`matched 276/330`，`missing 32` 是上表中"等 D01.1 / D01.10"的函数，另有未建文件的 `mutrow.go`（22 个）。govec 的 `chunkExempt` 逐个登记了原因。

**块外依赖**

| 来源 | 内容 | Rust | 标注 |
|---|---|---|---|
| Go runtime `growslice` / `roundupsize` | 切片增长的容量与 size class 取整（Go 1.25 的 68 个 class） | `goslice.rs` | `go-std:` |
| `types.CompareString`（D01.1） | `collate.GetCollator(c).Compare(x, y)` 一行 | `compare.rs` 中直接调用 D01.9 的 collator | 注释说明 |
| `types.BinaryLiteral.Compare`（D01.1） | 去掉前导零字节后比较 | `compare.rs` 的 `binary_literal_compare` | `glue:`，D01.1 合并后替换 |
| `disjointset.Set`（D00.3） | `mergeInputIdxToOutputIdxes` 中的别名分组 | 直接计算"同一别名组中最小的下标" | L2 |
| `cmp.Compare` | NaN 小于一切数、NaN 等于 NaN | `compare.rs` 的 `go_cmp` | `go-std:` |

## 2. 列格式（N5a 决定 D23–D26）

按 N5a 方案 a：Go `util/chunk` 的 L1 移植，载体如下。

- **列缓冲** `GoSlice<E>`：owned（`allocator_api2::Vec<u8, ColAlloc>`）或共享（`bytes::Bytes`，响应缓冲的零拷贝切片），写入时复制。`offsets` 是 `[]int64` 的小端字节。
- **列** `Arc<Column>`（`triomphe::Arc`）：`MakeRef` 共享，`SwapColumns` 交换向量，写入走 `col_mut`，debug 构建下引用计数大于 1 时 panic（N5a 3.2）。用 triomphe 而不是 std 的 `Arc`：std 的 `get_mut` / `make_mut` 要对 weak 计数做一次 compare-exchange，每次写列都要付出；triomphe 没有 weak，唯一性检查是一次 load（AppendRow 基准从 25.5µs 降到 21.3µs）。
- **类型视图**（D24）：`int64s()`、`float64s()`、`decimals()` 等返回 1 字节对齐的视图（`zerocopy::little_endian` 类型、`[u8; 40]`），`*_mut()` 返回可写视图，供 N4 的结果列使用。
- **零拷贝解码**（N5a 3.4）：`Codec::decode` 让三个缓冲都切自输入 `Bytes`；`Decoder::reuse_interm_chk` 在 `offsets[0] != 0` 时复制该列的 offsets 再调整（Go 原地改写共享缓冲），这是相对 Go 唯一新增的复制。
- **`Row<'a>`**：`Copy` 的借用，零值 `Row{}`（迭代器的结束标记）没有 chunk。跨 `Next` 持有的行（`M-ROW`）留给第一个需要它的算子块。

## 3. 设计决定

### 3.1 Go 切片的长度与容量

chunk 的行为有两处直接取决于 Go 切片的容量：

1. `Column.resize` 在容量够时原地 reslice，`ResizeX(n, true)` 露出原来的字节；容量不够时分配全零的新缓冲。
2. `Chunk.MemoryUsage` 是各缓冲容量之和。

而 NULL 单元下面的字节会进入 `Codec.Encode` 的输出。为了编码逐字节一致，`GoSlice` 保存 Go 的 len 和 cap，每次增长用 `growslice_cap`（`nextslicecap` 加 size class 取整）计算新容量，分配恰好 `cap * E` 字节并初始化为零（Go 的分配器同样清零）。被 reslice 截掉的字节留在缓冲中，reslice 回来时重新出现。实际分配的字节数因此就是 Go 的容量，`TrackedAlloc` 记的也是它；`MemoryUsage` 按 Go 的公式计算（`unsafe.Sizeof(Column{})` = 112），结果与 Go 相同（A1 原定"数值不对照"，这里做到了对照）。

Go 的切片表达式 `s[a:b]` 只要求 `b <= cap(s)`，chunk 代码依赖这一点（例如 `castSliceHeader` 把长度设成 `c.length`，不管 `len(c.data)`；列类型混用时 `appendCellByCell` 会读到 len 之外）。`GoSlice::capb()` 返回整个容量供这类读取；索引表达式 `s[i]` 仍检查长度。Go 读到容量之外是未定义行为，Rust 在那里 panic。

### 3.2 `elemBuf`

Go 的定长列追加 NULL 时写入 `elemBuf` 的内容，也就是上一次 `AppendInt64` 等写进去的值；`resize` 只改长度、保留内容；`CopyConstruct` 用 `append(dst[:0], src...)` 复制（容量按 growslice 取整）。这些都会进入编码输出，所以 `ElemBuf` 按 Go 保留长度、容量和内容，nil（变长列）是 `None`。

### 3.3 别名与 `Reset`

Go 的 `Chunk.Reset` 对每个槽位调用 `col.reset()`，同一个列被两个槽位引用时重置两次（幂等）。Rust 对每个列只重置一次：先把别名槽位摘下，使本槽位成为唯一持有者，原地重置后再挂回，别名关系保持不变（L2）。被另一个 chunk 共享的列仍会写时复制；govec 不生成这种写入，因为 Go 会通过别名改到另一个 chunk。

### 3.4 记账（N5a 3.7）

`ColAlloc` 是 `Global` 或 `Tracked(TrackedAlloc)`，列的每个 owned 缓冲用它分配。`chunk::new_in(fields, cap, max, alloc)` 建立记账的 chunk，`Renew`、`CopyConstruct` 等沿用源列的分配器。chunk 在算子之间移交时改挂账户（N5a 3.7 第 2 条）留给 D00.1 / D12 的算子接线，本块不做。

### 3.5 错误

`MakeRefTo`、`swapColumn`、`CopySelectedJoinRows*` 的 "selection vector is not nil" 是 `errors.New`，用 `tidb_base::errors::new`，消息相同。`List` 中 `memTracker.Consume` 在 Go 里超限会 panic，Rust 返回 `Err`（N3 3.4），所以 `List::append_row`、`add`、`alloc_chunk`、`reset`、`clear` 返回 `Result`。

### 3.6 复用池（M-POOL）

A1 删除复用池：`NewAllocator`、`NewSyncAllocator`、`NewReuseHookAllocator`、`NewEmptyAllocator` 都返回不复用的分配器（Go 的 `emptyAllocator` 行为），`InitChunkAllocSize` 忽略参数，`NewChunkFromPoolWithCapacity` 等同 `New(fields, cap, cap)`，`Chunk.Destroy` 直接丢弃。池的内部实现归 X。

### 3.7 其他按 Go 保留的细节

- `ColumnSwapHelper.SwapColumns` 在 Go 中按 map 顺序遍历；Rust 按输入下标顺序（`M-MAPORDER`），任何顺序得到的列共享关系都相同。
- `multiIterator` 的 `curIter` 在 `Begin` 之前是 nil，Go 在这时调用 `Current` 会 panic；Rust 用 `Option` 照搬（向量中有这条记录）。
- `Iterator4Slice` 的 cache line 填充用 `#[repr(align(64))]`。
- `AppendCellFromRawData` 的 `unsafe.Pointer` 改为 `&[u8]` 加偏移（L2）。

## 4. 已知差异

1. `Column.GetDecimal` / `Row.GetMyDecimal` 返回值而不是指向列内的指针；要写回用 `decimals_mut()`。`negative` 字节按 `!= 0` 读（与 gc 一致，Go 不检查；随机字节由 `set_raw` 覆盖，见评审）。
2. `Row.CopyConstruct` 返回新的一行 chunk（其第 0 行就是 Go 的结果）。
3. `ColumnSwapHelper` 的合并结果用 `OnceLock` 保存，对应 Go 的 `atomic.Pointer` 加 `CompareAndSwap(nil, …)`（第一个写入者生效）。
4. Go 读到切片容量之外的地方（未定义行为），Rust panic。

## 5. 验收

| 项目 | 结果 |
|---|---|
| 收敛检查 | 276/330，其余 54 个（32 个加 `mutrow.go` 的 22 个）已登记（§1） |
| govec `chunk` 领域 | 1,671 条（1,600 个随机脚本，平均约 100 步；20 个 Go 测试场景；51 条 `type_width`），debug 与 release-checked 下回放全部一致 |
| 反证 | 去掉 size class 取整 1,313 条不一致；NULL 写零而不是 `elemBuf` 1,226 条；`appendMultiSameNullBitmap` 不清多余位 335 条；`Sizeof(Column)` 改为 120 共 1,615 条；`resize(n, true)` 清零 16 条；`mergeInputIdxToOutputIdxes` 不合并别名 16 条 |
| 单元测试 | `tests/unit.rs`：分配器、`TrackedAlloc` 记账（释放后归零）、共享缓冲写时复制、`ReuseIntermChk` 不改输入、别名写入在 debug 下 panic、`Reset` 保持别名 |
| 性能 | 见 plan.md §3（与 Go 基准同负载对比） |
