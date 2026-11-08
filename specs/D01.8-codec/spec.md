# D01.8 编码：`pkg/util/codec`——块 spec

状态：已实现（tidb-rs `crates/codec`，crate `tidb-codec`，N0 rank 4；tracker govec `codec` 领域）。

## 1. Go 范围

tracker `data/func_blocks.tsv` 中 block = D01.8 的 85 个函数，全部在 `pkg/util/codec`（v8.5.8）：

| 文件 | 函数 | Rust | 内容 |
|---|--:|---|---|
| `number.go` | 18 | `number.rs` | 定长 memcomparable 整数、varint、comparable varint |
| `bytes.go` | 13 | `bytes.rs` | memcomparable 字节串（8 字节分组 + marker）、compact bytes、desc 形式 |
| `float.go` | 6 | `float.rs` | float64 转可比较 uint64 |
| `decimal.go` | 3 | `decimal.rs` | precision、frac 两个字节加 `MyDecimal.WriteBin` |
| `codec.go` | 45 | `codec.rs` | Datum 层、解码进 chunk、chunk 行哈希与 key 序列化、`HashCode`/`Hash64` |

`encoding/binary` 的 varint 四个函数在 `gobinary.rs`（`go-std`）。converge：85/85（L1 82，L2 3）。

## 2. 移植方式（已定：L1，TiKV 作为测试 oracle）

负责人在 tidb-rs#39 确认：`pkg/util/codec` 整体按 **L1** 从 Go 移植；vendored TiKV 编码器（`codec`、`tidb_query_datatype::codec::datum`）只作为测试中的第二个 oracle，**双向**逐字节对照（`tests/tikv_oracle.rs`），不是实现依赖。methodology Q3、ADR 0007、ROADMAP 已随本块更新。理由：API 以 Go 的类型为单位（Datum、MyDecimal、Time）；`codec.go` 一半以上（chunk 行哈希、解码进 chunk、collation key）在 TiKV 中没有对应物；底层原语逐行移植最容易保持 Go 的错误文字和边界行为。

`planner/cascades/base.Hasher`：`Hash64` 使用 parser 的 `IHasher`（D02.3 已有，`types.Datum.Hash64` 的参数类型），不在本块新增 trait；`Cache`/`SetCache` 只是复用 hasher 的临时缓冲（见 §4 第 1 条）。planner 块需要完整的 `base.Hasher` 时，在 rank 更低的 crate 中扩展 `IHasher`。

## 3. 设计要点

- 追加式 API：Go 的 `func F(b []byte, ...) []byte` 对应 `fn f(b: &mut Vec<u8>, ...)`；`reallocBytes` / `preRealloc` 的预留量照 Go（只影响分配次数）。
- 解码器返回借用输入的剩余部分。Go 在错误时返回有意义的值的地方（`DecodeOne` 的 remain 和 datum、`DecodeRange` 的已解码值、`DecodeDecimal` 的 remain/precision/frac、`EncodeKey` 的 b）按 M-ERRVAL 一并返回。
- `encode` 的 `err` 与 Go 一样是整个循环共用的一个变量（见 §5 第 3 条）。
- 错误：`errors.New` / `errors.Errorf` 用 `tidb_base::errors`，消息逐字一致（govec 逐字比较）。
- `hash.Hash64` 映射为 `std::hash::Hasher`（`Write` 不会失败；`Sum64` 即 `finish`），`io.Writer` 映射为 `std::io::Write`。`unsafe.Slice` 取 uint64/float64 的内存映像，对应 `to_ne_bytes`。
- `time.Location`：`Arc<Location>`；Go 的 `loc != time.UTC` 是与单例的指针比较，保留为 `Arc::ptr_eq(loc, &utc())`。
- `EncodeHashChunkRowIdx` 返回 `Cow<[u8]>`：Go 的切片别名 chunk 内存时借用，计算出的字节才拥有。
- `ConvertByCollation` 调用 D01.9 的 `Collator::key`。

## 4. 偏离（L2）

1. `Hash64`：`IHasher` 没有 `Cache`/`SetCache`，编码在新的 Vec 中生成后 `HashBytes`。写入 hasher 的字节与 Go 相同（govec `hash64`）。
2. `init`：Rust 没有包初始化，程序启动时调用 `tidb_codec::init()` 设置 `types::Hash64ForDatum`（幂等）。D00.1 / 服务端启动时接线。
3. `fastReverseBytes`：Go 在 amd64 上按机器字取反（unsafe 切片转换），Rust 按字节取反，由编译器向量化，结果相同。
4. `HashGroupKey` 遇到不支持的 eval type 时 Go 返回 nil 的 buf；Rust 保持 buf 不变（调用方随错误丢弃）。
5. `DecodeDecimal` 的 `errorInDecodeDecimal` failpoint 不移植（tidb-rs 还没有 failpoint）。

## 5. 上游行为（`upstream-pitfall` 候选）

1. **float -0.0 的编码，TiDB 与 TiKV 不同**（TiKV oracle 发现）：Go `encodeFloatToCmpUint64` 用 `f >= 0` 判断，-0 为真，所以 `EncodeFloat(-0.0)` = `EncodeFloat(+0.0)` = `05 80 00 00 00 00 00 00 00`（datum 形式，flag 5）。TiKV `NumberCodec::encode_f64` 看符号位，-0 编码为 `05 7f ff ff ff ff ff ff ff`，排在 +0 之前。两边都能解码对方的字节：TiKV 的 -0 在 TiDB 中解码为 -0，TiDB 的 -0 在 TiKV 中解码为 +0。tidb-rs 按 Go 实现，oracle 中登记为已知差异（`NEG_ZERO`，必须仍然不同）。
2. **`DecodeDecimal` 遇到损坏的字节会 panic**：`FromBin` 报告的 binSize 大于剩余长度时，`b = b[binSize:]` 越界（govec `dec` 领域有 `slice bounds out of range [22:7]` 等记录）。`DecodeOne`/`Decoder.DecodeOne` 都经过它，读存储或协处理器返回的损坏数据时，Go 是 panic 而不是错误；Rust 照样 panic（govec 要求）。
3. **`encode` 的错误可能被覆盖**：decimal 编码出错后，同一次调用中后面的 BIT/BinaryLiteral（`ToInt` 的结果赋给同一个 `err`）、时间或另一个 decimal 会覆盖这个错误，`EncodeKey` 最后返回 nil error 和不完整的 decimal 编码。govec 有记录覆盖（反证：提前返回 decimal 错误，回放报 1 条不一致）。

## 6. 验收结果

- converge：`matched 85/85`。
- govec `codec` 领域（tracker `govec/codec-D01.8`）：44,758 条，debug 和 release-checked 回放 0 条不一致；本地种子 1–12 生成后回放都一致，tracker CI 额外生成种子 1–8。Go 测试字面量登记进 `minelits`（29 个测试函数、256 行）。chunk 哈希类比较写入的字节序列而不是哈希值，覆盖 NULL、新 collation 开和关。
- TiKV oracle（`tests/tikv_oracle.rs`，双向）：整数、varint、浮点、字节串（含 desc、compact）的编码相同，TiKV 的字节在 tidb-rs 中解码为同值、tidb-rs 的字节在 TiKV 中解码为同值；datum 层（int、uint、float、bytes、decimal、duration、datetime、json、vector、null）的 key/value 编码相同、双向解码一致；3,000 个 TiKV 按 `prec_and_frac` 写出的 decimal 在 tidb-rs 中解码为同值和同 precision/frac，重新编码后与 TiKV 字节相同。唯一差异是 §5 第 1 条。
- 解码变异（`tests/decode_mutation.rs`）：20,000 条截断、改 flag、随机改字节、长度/marker 改成极值的输入，经过全部解码器（含 `Decoder` 解码进各类型的列），结果都是错误或可捕获的 panic，没有 abort 或卡死，单次分配最大约 12 KB。
- 反证（各改坏一处，回放的不一致条数）：分组 marker 305；desc 取反范围 38；decimal frac 上限 61；collation key 223；哈希中 -0 折叠 104；浮点改按符号位（TiKV 的规则）100；decimal 错误提前返回 1。

## 7. 性能

同一台 Mac（arm64），Go 取 5 次中位数（`govec/domains/codec_bench_test.go`），Rust 用 criterion（`benches/codec.rs`），工作负载相同。x86 两边都还没测（待 CI 或 Linux 机器）。

| 基准 | Go | Rust |
|---|--:|--:|
| DecodeWithSize（100 个 int） | 1,366 ns | 1,073 ns |
| DecodeWithOutSize | 2,514 ns | 1,247 ns |
| EncodeIntWithSize | 5.6 ns | **8.0 ns** |
| DecodeDecimal | 19.7 ns | 8.8 ns |
| DecodeOneToChunk | 11.5 ns | 9.8 ns |
| EncodeKey（5 列混合） | 116 ns | 103 ns |
| DecodeOne（5 列混合） | 36 ns | **66 ns** |
| HashChunkRow（1024 行 × 3 列） | 97.6 µs | 97.8 µs |
| HashChunkSelected（1024 行 × 3 列，FNV-64） | 86.4 µs | 85.7 µs |
| SerializeKeys（1024 行 × 3 列） | 75.6 µs | **88.2 µs** |

慢于 Go 的三项见 tasks T9：`EncodeIntWithSize` 中 Go 的 `make([]byte, 0, 8)` 不逃逸、在栈上，Rust 的 `Vec::with_capacity(8)` 在堆上；`DecodeOne` 中 Datum 拥有自己的字节（compact bytes 复制一份，Go 的 datum 别名输入），这是 D01.1 中 `b` 表示方式（`Cow`/`Bytes`）的待定问题；`SerializeKeys` 的差距原因待查（变长列的 collation key 与 decimal 哈希 key 的分配）。
