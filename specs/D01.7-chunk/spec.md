# D01.7 Chunk 与列存：块 spec

状态：草案（2026-11-08）。阶段：P1，B 线（ROADMAP §4 L1）。基线：Go TiDB v8.5.8（`8b857efa2`）。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[mappings.md](../../docs/porting/mappings.md)、[N5a 列格式](../exceptions/N5a-chunk/spec.md)（决定 D23–D26）、[N3+A1](../exceptions/N3-A1-concurrency-memory/spec.md)（`M-MEM`、`M-POOL`）、[N0 crate 分层](../exceptions/N0-crate-layering/spec.md)。本块建立新 crate `tidb-chunk`（rank 3，依赖 `tidb-base`、`tidb-parser`、`tidb-types`、`tidb-memory`）。

## 1. Go 范围

函数清单来自 tracker `data/func_blocks.tsv` 中 block = D01.7 的行。本块把复用池的内部实现（`pool.go` 全部，`alloc.go` 的 `allocator`、`columnList`、`poolColumnAllocator`、`checkColumnType`，`chunk.go` 的 `Chunk.resetForReuse`）按 A1 的 `M-POOL` 改为 X（tracker `scripts/blocks/rules.py`），D01.7 由 349 个函数变为 330 个（含 `mutrow.go` 的 22 个）。

| Go 文件 | 函数数 | Rust 文件 | 状态 |
|---|--:|---|---|
| `column.go` | 84 | `column.rs` | 全部 |
| `chunk.go` | 64 | `chunk.rs` | 全部 |
| `row.go` | 26 | `row.rs` | 全部 |
| `codec.go` | 20 | `codec.rs` | 全部 |
| `chunk_util.go` | 14 | `chunk_util.rs` | 9 个已移植；`diskFileReaderWriter` 的 5 个方法等 D01.10（落盘文件、checksum 与加密层） |
| `iterator.go` | 52 | `iterator.rs` | 43 个已移植；`iterator4RowContainer` 的 9 个等 D01.10 |
| `list.go` | 15 | `list.rs` | 全部 |
| `compare.go` | 19 | `compare.rs` | 全部 |
| `alloc.go` | 14 | `alloc.rs` | 全部（不复用，见 3.6） |
| `mutrow.go` | 22 | `mutrow.rs` | 全部（D01.1 合并后的后续 PR） |

收敛检查：`matched 316/330`，`missing 14` 是上表中"等 D01.10"的函数。govec 的 `chunkExempt` 逐个登记了原因。

**块外依赖**

| 来源 | 内容 | Rust | 标注 |
|---|---|---|---|
| Go runtime `growslice` / `roundupsize` | 切片增长的容量与 size class 取整（Go 1.25 的 68 个 class） | `goslice.rs` | `go-std:` |
| `types.CompareString`（D01.1） | `collate.GetCollator(c).Compare(x, y)` 一行 | `cmpStringWithCollationInfo` 直接调用 D01.9 的 collator；`Compare` 调用 D01.1 的 `compare_string` | 注释说明 |
| `types.BinaryLiteral.Compare`（D01.1） | 去掉前导零字节后比较 | `compare.rs` 的 `binary_literal_compare`（借用字节；D01.1 的方法要一个拥有所有权的 `BinaryLiteral`，比较函数不能每次分配） | `glue:` |
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

字段类型参数（N13 §8，#229 批次 A）：Go 的 `[]*types.FieldType`。`Allocator::alloc` 接收 `&[Arc<FieldType>]`（调用方是执行器，传它共享的返回类型）；`New`、`NewChunkWithCapacity`、`NewChunkFromPoolWithCapacity`、`NewEmptyChunk` 对 `F: Borrow<FieldType>` 泛型，`&[FieldType]` 和 `&[Arc<FieldType>]` 都可以传，不复制类型。

### 3.7 其他按 Go 保留的细节

- `ColumnSwapHelper.SwapColumns` 在 Go 中按 map 顺序遍历；Rust 按输入下标顺序（`M-MAPORDER`），任何顺序得到的列共享关系都相同。
- `multiIterator` 的 `curIter` 在 `Begin` 之前是 nil，Go 在这时调用 `Current` 会 panic；Rust 用 `Option` 照搬（向量中有这条记录）。
- `Iterator4Slice` 的 cache line 填充用 `#[repr(align(64))]`。
- `AppendCellFromRawData` 的 `unsafe.Pointer` 改为 `&[u8]` 加偏移（L2）。

## 4. 已知差异

1. `Column.GetDecimal` / `Row.GetMyDecimal` 返回值而不是指向列内的指针；要写回用 `decimals_mut()`。`negative` 字节按 `!= 0` 读（与 gc 一致，Go 不检查；随机字节由 `set_raw` 覆盖，见评审）。
2. `Row.CopyConstruct` 返回新的一行 chunk（其第 0 行就是 Go 的结果）。
3. `ColumnSwapHelper` 的合并结果用 `OnceLock` 保存，对应 Go 的 `atomic.Pointer` 加 `CompareAndSwap(nil, …)`（第一个写入者生效）。
4. Go 读到切片容量之外的地方（未定义行为），Rust panic。govec 的 `view` 步骤在 length × 宽度超过 `cap(data)` 时记为 `past_cap`（不读 Go 当时读到的内存），回放要求 Rust panic。同样的情况还有：`Row.ToString` 用 `GetInt64` 读 BIT（变长列，EvalType 是 ETInt）的单元，`GetTime` 等读一个被 `SetDatum` 改成 FLOAT（4 字节）的 MutRow 单元——typed getter 只检查第一个字节的下标。govec 的 `to_string`、`row_string`、`mr_string`、`mr_get` 在会读出容量时记 `past_cap`、不调用（`chunkReadSize`）。
5. Go 写到容量之外（内存破坏）的调用不进入向量：`AppendDatum` 写一个比列宽的定长 kind（例如 Duration 写进 FLOAT 列，8 字节写进 4 字节的 `elemBuf`；实测会改坏相邻的 null 位图），`MutRow.SetValue` 把 Duration / `*MyDecimal` / Time 写进比它短的单元（`*(*T)(unsafe.Pointer(&col.data[0]))` 只检查下标 0）。Rust 在容量内写、超出容量时 panic。生成器不产生前一种；后一种由 `mr_set_value(s)` 先检查，记 `past_cap` 并跳过这一步（两边都不执行）。
6. `MutRow.ShallowCopyPartialRow`：Go 让 MutRow 的单元成为源列缓冲的子切片，之后两边的写入互相可见（`TestMutRowShallowCopyPartialRow` 后半段检查了这一点：源 chunk `Reset` 再追加后，MutRow 读到新值）。Rust 把单元复制到同样长度、同样容量（到源的容量为止，内容也复制）的新缓冲，之后的写入互不影响（L2）。Go 的调用方（join 的 `ShallowCopyPartialRow` 后紧接 `ToRow` 读）不依赖这种别名；govec 的生成器在浅复制之后不再写源 chunk，也不再通过这个 MutRow 写。复制的代价（每个单元一次分配，大小到源缓冲的容量为止）留给执行器接线时评估（tasks T14）。
7. `makeMutRowColumn` 对不认识的类型（`bool`、`HexLiteral`、`BitLiteral` 等）返回 nil `*Column` 并放进 chunk，第一次访问时 panic；Rust 的列不能为 nil，`MutRowFromValues` / `MutRowFromDatums` / `MutRowFromTypes` / `SetDatum` 当场 panic（L2，同样的输入 panic，只是更早）。
8. MutRow 的列在 Go 中共用数组：定长列的 `elemBuf` 与 `data` 是同一个数组，变长列的 `nullBitmap` 是 `data` 底层数组的最后一个字节。`MutRow` 为每列记一个标志，在数组被替换之前把写入同步过去（`sync_elem`、`set_null_bit`），因此 dump 中的 `elemBuf` 和 `GetInt64` 读到的末字节都与 Go 一致。
9. `Row.GetDatum` 等：Go 的 datum 引用 chunk 的字节（`hack.String`、指向列内的 `*MyDecimal`），只在 chunk 不变时有效；Rust 的 datum 拥有一份拷贝（L2，值相同）。
10. `MutRow` 拥有它的 chunk（Go 的 `MutRow` 是按值复制、共享 `*Chunk` 的 `Row`）；setter 取 `&mut self`。`SetRow` 不能传入自己的 `to_row()`（借用检查）；Go 中这样做会先清掉自己的单元再复制，没有调用方这样用。

**给 D07.3 的说明**（Lucky 评审）：本块的 `Codec` 按 TiDB 的编码器验证。TiKV 的编码器（`tidb_query_datatype` 的 chunk codec）可能有合法的不同：没有 NULL 时是否写位图（TiDB 的编码器在 null count 为 0 时不写，解码走 `setAllNotNull`），以及 NULL 单元下面填什么字节。D07.3 用 TiKV 实际响应做向量时（tasks T12）要核对这两点，解码必须接受 TiKV 的合法写法；`decode_mutation` 测试覆盖畸形输入（只会 panic，不会 abort 或挂起）。

## 5. 验收

| 项目 | 结果 |
|---|---|
| 收敛检查 | 316/330，其余 14 个等 D01.10，已登记（§1）；D01.1 后续 PR 之前是 276/330 |
| govec `chunk` 领域 | 2,276 条：1,671 条（1,600 个随机脚本，平均约 100 步；20 个 Go 测试场景；51 条 `type_width`），加上 D01.1 后续 PR 的 605 条（600 个 Datum/MutRow 随机脚本，5 个 Go 测试场景：`TestMutRow`、`TestIssue29947`、`TestMutRowShallowCopyPartialRow` 前半段、`TestGetDecimalDatum`、`TestToString`）。新脚本加在原有脚本之后，原有 1,671 条逐字节不变。debug 与 release-checked 下回放全部一致 |
| 反证 | D01.1 后续：`DatumWithBuffer` 的 decimal frac 不用 digitsFrac 39 条；`ToString` 的 float32 用 bitSize 64 共 20 条；`Compare` 的 MinNotNull 遇 NULL 返回 0 共 17 条；浅复制不复制长度之后的字节 2 条；不同步 MutRow 的 null 位 1 条；不同步 `elemBuf` 213 条。原有：去掉 size class 取整 1,313 条不一致；NULL 写零而不是 `elemBuf` 1,226 条；`appendMultiSameNullBitmap` 不清多余位 335 条；`Sizeof(Column)` 改为 120 共 1,615 条；`resize(n, true)` 清零 16 条；`mergeInputIdxToOutputIdxes` 不合并别名 16 条 |
| 其他种子 | 种子 1–24 各 2,276 条，回放全部一致（`GOVEC_CHUNK_FILES`，tracker CI 生成种子 1–8 作为产物） |
| 畸形输入 | `tests/decode_mutation.rs`：5,000 份截断、翻转字节、改写长度/null count 的编码，解码、读出每个单元、重新编码，只出现可捕获的 panic |
| 单元测试 | `tests/unit.rs`：分配器、`TrackedAlloc` 记账（释放后归零）、共享缓冲写时复制、`ReuseIntermChk` 不改输入、别名写入在 debug 下 panic、`Reset` 保持别名 |
| 性能 | 见 plan.md §3（与 Go 基准同负载对比） |
