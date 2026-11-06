# N5a：根节点内存中的列格式（Go `util/chunk` 移植 vs TiKV 列类型）（基础设计 spec，草案）

状态：草案（2026-11-04）。类型：实现例外（基础设计）。计划定稿：P1 之前（D01.7 开工前）。
所属块：D01.7（Chunk 与列存）、D01.10（落盘容器，只涉及接口）、D07.3（distsql 结果解码）；约束 D12 全部执行器。
相关：N4（向量驱动读写列）、N3+A1（`TrackedAlloc`、`M-MEM`、`M-POOL`、共享响应缓冲的记账）、N5（值表示与零拷贝，P2）、A6（读取路径）、ADR 0007 第 1 条。

位置均为 v8.5.8 `pkg/` 下的 `文件:行`（TiKV 位置注明 `tikv/components/…`）；统计命令见文末脚注。

## 1. 要解决的问题

根节点的执行器、表达式向量求值、落盘容器、coprocessor 结果解码都直接操作同一种列格式。它在 P1 的 D01.7 就要落地，之后几乎所有执行器代码都依赖它的 API。可选的有两种：照 Go `util/chunk` 移植，或者用 TiKV `tidb_query_datatype` 的列类型（L3）。另外要定：coprocessor 的 chunk 编码响应怎么做到不比 Go 多复制，N4 怎么读写列，A1 的记账怎么挂到列缓冲上。

### 1.1 v8.5.8 中的事实

| 事实 | 位置 | 对 Rust 的影响 |
|---|---|---|
| `Chunk{sel []int, columns []*Column, numVirtualRows, capacity, requiredRows, inCompleteChunk}`；`NumRows()` 有 `sel` 时返回 `len(sel)` | `util/chunk/chunk.go:35-54`、`:384-390` | 选择向量是 chunk 的一部分，N4 的向量驱动要处理它 |
| `Column{length, nullBitmap, offsets []int64, data, elemBuf, avoidReusing}`；空位图 1 表示非空 | `util/chunk/column.go:73-81` | 布局简单，适合 L1 |
| 定长宽度：FLOAT 4；整数类、DOUBLE、YEAR、DURATION 8；DATE/DATETIME/TIMESTAMP 为 `sizeof(types.Time)` = 8（`Time{coreTime}`、`CoreTime uint64`）；DECIMAL 为 `MyDecimalStructSize` = 40，即 `MyDecimal` 结构体的内存映像（3 个 int8 + bool + `[9]int32`）；其余变长 | `util/chunk/codec.go:174-188`、`column.go:390`、`types/time.go:224-226`、`types/core_time.go:25`、`types/mydecimal.go:233-246` | 线上格式就是 Go 结构体的内存映像。Rust 的 `MyDecimal`（D01.2）必须 `#[repr(C)]` 且字段顺序相同 |
| `Int64s()`、`Decimals()` 等用 `unsafe` 把 `data` 直接重解释成类型切片，不检查对齐 | `column.go:584-595`、`:627-631` | Rust 中对未对齐的 `&[u8]` 做同样的事是 UB |
| `Row{c *Chunk, idx int}`；executor 非测试代码中 63 个文件 363 处 `chunk.Row`、94 处 `GetRow(`；至少 22 个结构体字段跨 `Next` 持有 `chunk.Row` 或 `[]chunk.Row`（hash join v1、index join、stream agg、sort、window 函数等） | `util/chunk/row.go:29-32`；脚注 2 | 行访问 API 要保留；跨调用持有的行需要一种不借用的句柄 |
| API 规模：`Column` 75 个方法，`Chunk` 54 个，`Row` 26 个；`util/chunk` 非测试 6,125 行；全仓 321 个非测试文件引用该包，executor 中 139 个 | 脚注 1 | 换成 TiKV 类型意味着改写这 321 个文件的调用方式 |
| 列可以别名：`MakeRef` 让两个槽指向同一个 `*Column`；`SwapColumns` 交换列指针；投影的 `ColumnSwapHelper` 把输入列直接换到输出，重复列用 `MakeRef` | `chunk.go:226-228`、`:289-292`、`chunk_util.go:268-282` | Rust 中列需要共享所有权（`Arc`）或别名表 |
| coprocessor 结果零拷贝：kvproto `coprocessor.Response.Data` 与 tipb `Chunk.RowsData` 都是 `SharedBytes`，反序列化时只引用原缓冲；`Codec.decodeColumn` 让 `nullBitmap`、`offsets`、`data` 直接切自该缓冲（三下标切片限制容量），并设 `avoidReusing` | kvproto `coprocessor.pb.go:236`、tipb `select.pb.go:275`、`sharedbytes.go:33-36`；`util/chunk/codec.go:109-148`、`:160-168` | Go 从 gRPC 缓冲到列一次都不复制。这是"不多复制"的基线 |
| `selectResult.readFromChunk`：剩余行数 > 需求 × 0.8 时 `ReuseIntermChk` 直接交换列（并**原地改写**响应缓冲中的 offsets 减去 `offsets[0]`）；否则 `Decoder.Decode` 把数据 `append` 复制进目标 chunk | `distsql/select_result.go:561-595`、`util/chunk/codec.go:292-307`、`:309-353` | 小块拼接时 Go 本来就复制；Rust 的 offsets 调整不能写共享缓冲 |
| 复用池 `chunk.Allocator`（`alloc.go` 344 行、`pool.go` 125 行），由 `tidb_enable_reuse_chunk`（默认 ON）控制 | `util/chunk/alloc.go:33-34`、`sessionctx/variable/tidb_vars.go:1616`、`session.go:1833`、`:1852` | 已由 A1 定为删除（`M-POOL`），变量接受并在非默认值时返回 warning |
| `Chunk.MemoryUsage()` 按容量估算 | `chunk.go:182-185` | A1 已定：改为读实际容量（L2） |

### 1.2 TiKV 列类型的情况

| 事实 | 位置（`tikv/components/tidb_query_datatype/src/codec/`） | 影响 |
|---|---|---|
| `VectorValue` 按求值类型分 10 种：`ChunkedVecSized<Int/Real/Decimal/DateTime/Duration>`、`ChunkedVecBytes`、`Json`、`Enum`、`Set`、`VectorFloat32` | `data_type/vector.rs:15-26` | 按求值类型而不是字段类型组织；没有"行"抽象，没有 `sel` |
| `ChunkedVecBytes{data: Vec<u8>, bitmap: BitVec, length, var_offset: Vec<usize>}`；`ChunkedVecSized{data: Vec<T>, bitmap}` | `data_type/chunked_vec_bytes.rs:7-12`、`chunked_vec_sized.rs:18-22` | 全是拥有型 `Vec`，不能借用外部缓冲 |
| `LazyBatchColumn{Raw(BufferVec), Decoded(VectorValue)}`，Raw 用于行格式 datum | `batch/lazy_column.rs:28-31` | 针对 TiKV 扫描行数据，不针对 chunk 响应 |
| TiKV 只**编码** chunk（`ChunkColumnEncoder`）；解码只有 `decode_for_test`，逐段 `to_vec()` 复制 | `chunk/column.rs:1021-1049`、`:1052` 起 | ADR 0007 第 1 条所说"chunk wire format 作为 L3"在解码方向没有可用代码 |
| TiKV `read_decimal_from_chunk` 直接 memcpy 40 字节到 TiKV `Decimal`；`read_time_from_chunk` 读一个 u64 | `mysql/decimal.rs:2292-2301`、`mysql/time/mod.rs:2970-2973` | 佐证线上格式是 Go 结构体映像；Decimal/Time 的值语义仍按 ADR 0007 第 2 条从 Go 移植 |
| tidb-rs 生成的 tipb 中 `rows_data` 是 `Vec<u8>`（rust-protobuf 2.8，`take_rows_data`），kvproto 同理 | `tidb-rs/target/.../tipb-*/out/protos/select.rs:1263` | 用生成代码反序列化 = gRPC 缓冲→`Response.data` 一次复制，→`rows_data` 再一次复制，比 Go 多两次 |

## 2. 方案比较

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| a. 移植 Go `util/chunk`（L1，载体按映射规则改） | `Chunk`/`Column`/`Row`/`Codec`/`Decoder`/`List`/`RowContainer` 逐函数移植；缓冲改为可借用外部字节的类型 | 321 个调用文件的代码形状不变；`Row`、`sel`、`SwapColumns`、`MemoryUsage`、落盘格式都按 Go；线上格式与内存格式相同，零拷贝解码自然成立 | 要自己处理对齐、别名和跨调用持有行这三件 Go 不用管的事 |
| b. 用 TiKV `VectorValue` / `LazyBatchColumnVec`（L3） | 根节点执行器改写为按 `VectorValue` 操作 | 成熟、`Vec<T>` 天然对齐 | 没有行抽象和 `sel`，executor 中 363 处 `chunk.Row` 的写法要全部改，L1 对照失效；chunk 响应要先解码复制进 `Vec`，做不到零拷贝；Enum/Set/Bytes 的内存形式与 Go 不同，落盘格式、`MemoryUsage`、EXPLAIN ANALYZE 的内存归属都要另设计；与 ADR 0007 第 2 条（值语义按 Go）冲突的类型（`Decimal`、`Time`）在列里也是 TiKV 的 |
| c. 两套并存 | 读取层用 TiKV，算子用 Go 格式 | — | 边界处必然复制一次，达不到"不多复制"。**否决** |

**建议：方案 a。** 下面 3.2–3.6 是 Rust 化时必须定下来的几处载体。

## 3. 设计

### 3.1 按 L1 保留的部分

- `Chunk`、`Column`、`Row`、`Iterator4Chunk`、`List`、`RowPtr`、`Codec`、`Decoder`、`MutRow`、`chunk_util` 的全部函数，名字、分支、`sel` 语义、`requiredRows`/`IsFull`、`0.8` 复用阈值（`select_result.go:581-586`）都照搬。
- 定长宽度表（`getFixedLen`）、空位图约定（1 = 非空）、变长 `offsets` 为 `i64`、Enum/Set 的"值 + 名字"变长编码、JSON 的 `TypeCode + Value`。
- `RowContainer`、`chunk_in_disk`、`row_in_disk` 的落盘格式第一步也按 Go（N3+A1 3.2 已定）。

### 3.2 缓冲：拥有或共享

```rust
pub struct Column {
    length: usize,
    null_bitmap: ColBuf,
    offsets: ColBuf,          // 变长列：(length+1) 个 i64，小端
    data: ColBuf,
    // elemBuf、avoidReusing 不再需要：前者是 Go 避免分配的暂存，后者是给复用池看的（M-POOL）
}
pub enum ColBuf {
    Owned(Vec<u8, TrackedAlloc>),          // 本算子分配，按实际容量记账（M-MEM）
    Shared(RespBytes),                     // Arc<响应缓冲> + 区间；只读
}
```

- **debug 断言**：`Column` 的所有写入口在 debug 构建下断言 `Arc::strong_count == 1`，大于 1 时 panic 而不是复制。写时复制和 Go 的指针别名在"通过一个别名写、从另一个读"时结果不同，而且是静默的；本 spec 没有审计全部 `MakeRef` 调用点，这条断言让漏掉的别名写入在测试里暴露出来。需要复制的地方显式调用 `make_mut_cow()`。
- 写入 `Shared` 时先复制成 `Owned`（写时复制）。Go 用三下标切片把容量限制为长度，`append` 时同样会重新分配并复制，所以复制时机与 Go 相同，不是新增复制。
- 列在 chunk 中存为 `Arc<Column>`：`MakeRef` = `Arc::clone`，`SwapColumns` = 交换 `Vec<Arc<Column>>`，写入走 `Arc::make_mut`。`Reset` 对共享的列直接换成一个新的空列，不复制。
- `Chunk: Send`（N3 的 async 任务会在线程间移动它）；`RespBytes: Send + Sync`。

### 3.3 对齐

响应缓冲中，各列的 `offsets` 和定长数据相对缓冲起点的位置由前面的列决定（变长列、奇数行的 FLOAT、`nullCount>0` 时的位图都会打乱 8 字节对齐），缓冲本身在 protobuf 消息中的起点也不固定。所以 Go 那种"直接当 `[]int64` 用"在 Rust 中不能照做。

建议：**类型视图使用 1 字节对齐的小端包装类型**（如 `zerocopy::little_endian::{I64, U64, F64, F32}`，以及 40 字节、1 字节对齐的 `MyDecimalBytes`）。`Column::int64s()` 返回 `&[I64Le]`，读取时按值取出；x86-64 和 aarch64 上这就是一条普通的非对齐加载，向量化不受影响。`Owned` 列的结果写入用 `int64s_mut() -> &mut [i64]`（`Owned` 缓冲由分配器保证对齐）。这样共享缓冲不需要为对齐再复制。备选见待定 2。

### 3.4 零拷贝解码路径（与 N5、A6 的边界）

Go 从 gRPC 缓冲到列 0 次复制。Rust 要做到不多于 Go，需要：

1. gRPC 层收到的消息缓冲转成 `Bytes`（一次接收复制，与 Go 相同，不可避免）。
2. `coprocessor.Response.data`、`SelectResponse.chunks[i].rows_data` 要以 `Bytes` 切片交出。**先试 prost**：client-rust 已经用 tonic + prost 生成 kvproto，prost 可以把 `bytes` 字段生成为 `bytes::Bytes`（`Config::bytes`），从 `Bytes` 解码时是零拷贝切片；tipb 改用 prost 生成并对 `rows_data` 开 `Bytes`，同时去掉 grpcio 依赖（N3 改为 tokio 后已无理由保留）。要验证的是 tonic 的 `DecodeBuf` 是否把接收缓冲零拷贝交给 prost。不成立时才退回下面的手写方案：只解析这两层消息的字段边界，把 `rows_data` 作为 `Bytes::slice` 返回。其余字段（`warnings`、`execution_summaries`、`output_counts` 等）仍用生成代码解析各自的子消息。对应 Go 的 `SharedBytes` 自定义类型，按 L2 登记。
3. `Codec::decode_column` 让三个缓冲都成为 `ColBuf::Shared`（对应 `codec.go:109-148`），0 复制。
4. `Decoder::reuse_interm_chk` 需要把 `offsets` 减去 `offsets[0]`（`codec.go:296-302`）。Go 原地改写响应缓冲；Rust 只在 `delta != 0` 时把该列的 `offsets` 复制成 `Owned` 再改（每行 8 字节，`data` 仍共享）。这是相对 Go 唯一新增的复制，登记为 L2，并在验收中计量。
5. `Decoder::decode`（拼接小块）照 Go 复制。

共享缓冲的记账按 A1 3.4：只记一次，记在 copr/reader 的 tracker 上，最后一个 `Arc` 释放时归零。

### 3.5 N4 如何读写列

```rust
pub struct ColView<'a> { col: &'a Column, sel: Option<&'a [usize]> }   // RPN 的 ColumnRef 节点产出
impl ColView<'_> {
    fn len(&self) -> usize;                       // = Go 的 input.NumRows()
    fn is_null(&self, i: usize) -> bool;          // 经 sel 映射到物理行
    fn i64(&self, i: usize) -> i64;               // 读 I64Le
    fn bytes(&self, i: usize) -> &[u8];           // 变长，借用（可能借自响应缓冲）
    fn decimal(&self, i: usize) -> MyDecimal;     // 40 字节按值复制
}
// 结果列：&mut Column（Owned），API 与 Go 相同：
//   resize_int64(n, is_null) / int64s_mut() / set_null(i, b) / merge_nulls(&other)
//   reserve_string(n) / append_bytes(b) / append_null() / finish_append_var()
```

- 输入列不复制（Go 的 `Column.VecEvalInt` 会 `CopyReconstruct`）；`sel` 在视图里处理，函数体看到的是逻辑行号。
- 结果列总是稠密、长度等于逻辑行数，与 Go 相同。`evalOneVec` 直接写进输出 chunk 的目标列（`chunk_executor.go:119`），若该列被别名共享则 `make_mut`。
- 中间结果列属于 N4 的 `RpnState`，跨 chunk 复用（这是普通的"保留容量"，不是 Go 那种对象池），用 `TrackedAlloc` 挂在算子 tracker 下。

### 3.6 跨调用持有的行

`Row<'a> { chk: &'a Chunk, idx: usize }` 是 `Copy` 的借用，覆盖绝大多数用法。对 1.1 中跨 `Next` 存进结构体字段的 22 处，提供 `RowHandle { chk: Arc<Chunk>, idx: u32 }`：持有行的算子把对应 chunk 放进 `Arc`（Go 在这些地方本来就为它们单独分配或 `CopyConstruct` chunk）。这是唯一的形状变化，按 L2 登记为新映射规则 `M-ROW`。

### 3.7 记账（`TrackedAlloc` 挂在哪里）

- `Owned` 缓冲用 `Vec<u8, TrackedAlloc>`。chunk 内各列共用一个"账户"句柄，账户指向当前所属算子的 tracker 叶子。
- `swap_columns`、把 chunk 交给父算子时，改挂账户（一次原子减、一次原子加），对应 Go 在 `SwapColumns` 后 `Consume(delta)` 的惯例（A1 3.3 第 4 条）。`Vec` 的分配器参数本身不能替换，所以改挂发生在账户层，不在 `Vec` 层。
- `Chunk::memory_usage()` 返回各列实际容量之和（A1 3.2）。
- 无复用池：`chunk.Allocator`、`pool.go`、`TryNewCacheChunk` 的池化分支删除（`M-POOL`）；`tidb_enable_reuse_chunk` 按 A1 待定 3 处理（接受，设为非默认值时返回 warning）。

## 4. 对 L1 移植的影响（示例）

Go（`util/chunk/codec.go:109-148`，节选）：

```go
if nullCount > 0 {
	numNullBitmapBytes := (col.length + 7) / 8
	col.nullBitmap = buffer[:numNullBitmapBytes:numNullBitmapBytes]
	buffer = buffer[numNullBitmapBytes:]
} else {
	c.setAllNotNull(col)
}
...
col.data = buffer[:numDataBytes:numDataBytes]
col.avoidReusing = true
return buffer[numDataBytes:]
```

Rust：

```rust
// go: pkg/util/chunk/codec.go (*Codec).decodeColumn L2 [M-POOL] (SharedBytes → ColBuf::Shared)
if null_count > 0 {
    let n = (col.length + 7) / 8;
    col.null_bitmap = ColBuf::Shared(buffer.slice(..n));   // 不复制
    buffer = buffer.slice(n..);
} else {
    self.set_all_not_null(&mut col);
}
...
col.data = ColBuf::Shared(buffer.slice(..num_data_bytes as usize));
// avoidReusing 不需要：没有复用池
buffer.slice(num_data_bytes as usize..)
```

执行器中的行访问（Go `executor/join/joiner.go` 一类代码）不变：`row.GetInt64(i)` → `row.get_int64(i)`；跨调用持有的 `[]chunk.Row` → `Vec<RowHandle>`。

## 5. 验收（指标均为初拟）

| 项目 | 指标 |
|---|---|
| 必须通过的 Go 测试 | `util/chunk` 下行为类测试随 D01.7/D01.10 移植并通过；断言字节数的（`TestChunkMemoryUsage`、`TestListMemoryUsage` 等）按 A1 的"数值不对照"规则改写 |
| 线上格式 | govec 生成：Go `Codec.Encode` 产生的字节，Rust `decode` 后逐行逐列与 Go `Row.GetDatum` 一致；Rust `encode` 输出与 Go 逐字节相同；覆盖全部字段类型、全 NULL、无 NULL、0 行、1024 行 |
| 复制次数 | 用计数分配器测：TPC-H SF10 Q1、Q6 全表扫描，从 gRPC 缓冲到根节点 `Selection` 输入的复制字节数 ≤ Go 同路径（Go 为接收复制 + 小块拼接复制）+ `ReuseIntermChk` 的 offsets 复制；后者占 `rows_data` 的比例单独报告 |
| 记账 | 共享缓冲只记一次；语句结束后 tracker 为 0；TPC-H 每条查询 copr reader 节点的 `MaxConsumed` ≥ 实际收到的 `rows_data` 字节数 |
| 性能 | `util/chunk` 的 Go 基准（`BenchmarkAppend*`、`BenchmarkAccess`、`BenchmarkDecode`、`BenchmarkDecodeToChunk*`、`BenchmarkEncodeChunk` 等）对应项不低于 Go；sysbench `oltp_read_only` 与 TPC-H 22 条的解码 CPU 占比不高于 Go |
| 非对齐读取 | `ColView::i64()` 等非对齐读取在 aarch64 和 x86-64 上各跑一次 `BenchmarkAccess` 对照，确认生成的是普通加载 |
| 静态 | `Chunk: Send`；`ColView` 不能比输入 chunk 活得长（编译期保证）；`RowHandle: Send + Sync` |

## 6. 待定项（需要项目负责人决定）

1. **选方案 a（移植 Go Chunk）**，ADR 0007 第 1 条中的"chunk wire format L3"改为"chunk 编解码按 L1 从 Go 移植（`codec.go` 353 行）"，因为 TiKV 侧没有可用的解码代码。
2. **对齐的做法**：建议 1 字节对齐的小端视图类型（3.3）。备选：(a) 读到不对齐的列时复制一次使其对齐——简单，但对 TiKV 响应几乎每列都会触发，等于放弃零拷贝；(b) 只提供按行读取的非对齐访问器，向量函数体不拿切片——慢。
3. **零拷贝解码的实现路径**（3.4 第 2 条）：修订后建议 P1 中验证 prost 的 `Bytes` 路径（同时把 tipb 换成 prost 生成、去掉 grpcio）；不成立再在 A6 手写解析器。用第 5 节"复制次数"指标验收。
4. **`RowHandle` 的写法**（3.6）是否接受为全局映射规则 `M-ROW`。

## 7. 与其他 spec 的关系

| spec | 交互 |
|---|---|
| N4 | 向量驱动通过 `ColView` 读、通过 `&mut Column` 写（3.5）；中间列在 `RpnState` 中保留容量 |
| N3+A1 | `TrackedAlloc` 与账户改挂（3.7）；`M-POOL` 删除复用池；共享响应缓冲记在 reader 上（A1 3.4）；落盘格式第一步按 Go |
| N5 / A6 | N5a 只定列格式和解码路径的契约；`Datum`、紧凑值、批量请求等由 N5/A6 定。3.4 第 2 条的解析器建议在 A6 实现 |
| A4 | JSON 列存 `TypeCode + Value`，`JsonRef` 直接借用 `ColBuf`（含共享缓冲） |
| D01.2 | `MyDecimal` 必须是 `#[repr(C)]`、40 字节、字段顺序与 Go 相同，否则 chunk 线上格式不兼容 |
| ADR 0007 | 第 1 条按待定 1 修改 |

## 附：我不确定的地方

- "响应缓冲中各列几乎都不对齐"是按格式推算的，没有抓真实 TiKV 响应统计。若实测多数对齐，待定 2 的备选 (a) 也可以接受。
- 22 处跨调用持有 `chunk.Row` 的字段是按"字段类型为 `chunk.Row`/`[]chunk.Row`"的正则统计的，漏掉了 map、嵌套结构体等写法；每个算子块的 plan 中要逐个确认它们引用的 chunk 在持有期间不会被 `Reset`。
- `Arc<Column>` 的写时复制假设 Go 没有"通过一个别名写、从另一个别名读到新值"的用法。抽查的 `ColumnSwapHelper` 只读别名；全仓 `MakeRef`/`MakeRefTo` 的调用点没有逐个审计。
- 手写 protobuf 解析器之外，gRPC 客户端（client-rust/grpcio）能否把接收缓冲以 `Bytes` 交出、而不先复制进生成的消息结构，尚未查证。
- 账户改挂的细节（一个 chunk 一个账户还是每列一个；列被 `MakeRef` 到另一个算子的 chunk 时记在哪边）留给 D01.7 的块 plan，和 A1 一起定。

---

脚注（统计命令，在 `tidb-v858/pkg` 下）：

1. `ls util/chunk/*.go | grep -v _test | xargs wc -l`（合计 6,125；`codec.go` 353、`column.go` 873、`chunk.go` 735、`row.go` 284、`alloc.go` 344、`pool.go` 125）；`grep -c '^func (c \*Column)' util/chunk/column.go`（75）、`'^func (c \*Chunk)' chunk.go`（54）、`'^func (r Row)' row.go`（26）；`grep -rl '"github.com/pingcap/tidb/pkg/util/chunk"' . | grep -v _test.go | wc -l`（321），同法 `executor`（139）。
2. `F=$(grep -rl 'chunk\.Row\b' executor | grep -v _test.go)`：63 个文件，`cat $F | grep -o 'chunk\.Row\b' | wc -l` = 363；`GetRow(` 94；结构体字段：`grep -rnE '^\s+[a-zA-Z_]+\s+(\[\])?chunk\.Row\s*(//.*)?$' executor util | grep -v _test.go | wc -l` = 22。
