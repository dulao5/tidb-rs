# D01.8 编码：`pkg/util/codec`——块 spec（草案，待评审决定 §2）

状态：草案。实现尚未开始；§2 的移植方式需要评审确认后再动手（见 plan.md）。

## 1. Go 范围

tracker `data/func_blocks.tsv` 中 block = D01.8 的 85 个函数，全部在 `pkg/util/codec`（v8.5.8，共约 2,300 行非测试代码）：

| 文件 | 函数 | 内容 |
|---|--:|---|
| `number.go` | 18 | 定长与变长整数：memcomparable 的 `EncodeInt`/`EncodeIntDesc`/`EncodeUint`…、`EncodeVarint`/`EncodeUvarint`、`EncodeComparableVarint`/`Uvarint` 及对应的 Decode |
| `bytes.go` | 13 | memcomparable 字节串（8 字节分组加填充标记）、compact bytes、desc 形式 |
| `float.go` | 6 | float64 转成可比较的 uint64 |
| `decimal.go` | 3 | `EncodeDecimal`/`DecodeDecimal`：精度、小数位加 `MyDecimal.ToBin` |
| `codec.go` | 45 | Datum 层：`EncodeKey`/`EncodeValue`（各类型的 flag 字节与编码）、`Decode`/`DecodeOne`/`CutOne`/`CutColumnID`/`DecodeRange`、`Decoder.DecodeOne`（解码进 chunk）、`EstimateValueSize`、collation key（`ConvertByCollation`）、hash join 用的 `HashChunkRow`/`HashChunkColumns`/`HashChunkSelected`/`EqualChunkRow`/`HashGroupKey`/`EncodeHashChunkRowIdx`/`SerializeKeys`、`Hash64`/`HashCode` |

依赖：D01.1 Datum 与 FieldType、D01.2 MyDecimal、D01.3 Time/Duration、D01.5 JSON、D01.6 Vector、D01.7 chunk、D01.9 collation、`parser/terror`。`Hash64` 用到 `planner/cascades/base.Hasher`（tracker 中 D01.8 → EXC.B 的 4 处引用），需要先确认它的归属：建议把这个接口（只有写入方法）随本块定义在 `tidb-codec` 中，由 planner 块引用。

## 2. 移植方式（需要决定）

决定 Q3（methodology §9）："编码格式（行格式、key 编码、chunk 格式）和 collation 数据表复用 TiKV 的 `tidb_query_datatype`（L3，需 ADR 和逐字节对照测试）；值的语义从 Go 移植"。ADR 0007（proposed）进一步规定，L3 只用于字节格式本身，并要求和 Go 做逐字节对照。

**建议：`pkg/util/codec` 整体按 L1 从 Go 移植；TiKV 的编码器只作为测试中的第二个 oracle（对照），不作为实现。** 理由：

1. **这个包的 API 是 Go 类型上的。** `EncodeKey`/`EncodeValue` 的输入是 Go 的 `types.Datum`（D01.1）、`MyDecimal`（D01.2）、`Time`（D01.3）。TiKV 的 `tidb_query_datatype::codec::datum` 输入的是 TiKV 自己的 `Datum`/`Decimal`/`Time`。复用就要先把 Go 类型转换成 TiKV 类型，这个转换本身就是一层要验证的代码。而 ADR 0007 第 2 条正是因为这些类型的语义不同，才要求根侧不用 TiKV 的类型。
2. **相当一部分没有 TiKV 对应物。** `codec.go` 的一半以上是 hash join、分组用的 chunk 行哈希（`HashChunkRow`、`EqualChunkRow`、`SerializeKeys` 等，直接读 D01.7 的列）、`Decoder.DecodeOne` 解码进 chunk、`EstimateValueSize`、collation key 转换。这些在 TiKV 中都没有，只能从 Go 移植。
3. **低层原语逐行移植的成本很低。** `number.go`、`bytes.go`、`float.go` 的 37 个函数加起来约 580 行，大多是几行的字节运算。逐行移植连同 Go 的错误文本（`insufficient bytes to decode value` 等）和 `append` 的容量行为一起保留，结果天然逐字节一致。改用 TiKV 的 `codec` crate，反而要为每个函数写适配层，去对齐 Go 的错误和切片语义。
4. **格式是共享的，实现不必共享。** Q3 要保证的是 tidb-rs、Go TiDB、TiKV 和已存数据之间的字节格式一致。这一点用两种对照来验证：govec 的 Go 向量（逐字节），加上 tidb-rs 测试中对重叠的类型调用 vendored 的 TiKV 编码器（`codec::number`、`codec::byte`、`tidb_query_datatype::codec::datum`），比较两者的输出。对照中发现的 TiDB 与 TiKV 不一致，按负责人的规则登记为 `upstream-pitfall`。

另一种做法是"低层原语 L3（TiKV `codec` crate）、Datum 层 L1"。它的好处是低层与 TiKV 共用一份代码，代价是上面第 3 条的适配层，以及 `tidb-codec` 依赖 vendored TiKV crate（ADR 0007 打算逐步缩小这部分依赖）。

**这一节需要评审决定**：如果采用建议，它偏离了 Q3 的字面意思，应当在 ADR 0007 中补一条，说明 `pkg/util/codec` 按 L1 移植、TiKV 只作为测试 oracle；是否需要负责人确认（`block` issue），也请评审判断。

## 3. 设计要点（按 §2 的建议）

- 新 crate `tidb-codec`，在 `layers.toml` 中已规划（rank 见 layers.toml）。
- Go 的 `b []byte` 追加式 API（`func EncodeInt(b []byte, v int64) []byte`）对应 `fn encode_int(b: &mut Vec<u8>, v: i64)`。`reallocBytes` / `preRealloc` 的容量增长按 Go 保留：`EncodeKey` 的输出容量不可见，但 `preRealloc` 决定了分配次数，影响性能。
- 错误：`errors.New("insufficient bytes to decode value")` 这类错误用 `tidb_base::Error`，消息逐字一致；`terror` 类错误用对应原型（D00.2 / D01.1 已有）。
- collation：`ConvertByCollation` 调用 D01.9 的 `Collator::key`。
- hash：`hash.Hash64` 对应一个只有写入方法的 trait；`HashChunkRow` 写入的字节序列必须与 Go 相同（hash join 的落盘分区依赖它），向量直接比较写入的字节，而不是哈希值。
- `Hash64` / `HashCode` 中 Go 的 `logutil` warning 照搬到 `log`。

## 4. 验收（计划）

- converge：85/85，或登记过的豁免。
- govec `codec` 领域：Go 测试字面量登记进 `cmd/minelits`（`codec_test.go`、`bytes_test.go`、`decimal_test.go`、`collation_test.go`）；各类型的边界值（MinInt64、MaxUint64、±0、NaN、±Inf、空串、8 的倍数长度的字节串、超长 decimal、零日期、各种 fsp、JSON 各类型、向量、enum/set、各 collation 的 key）；随机 datum 序列的 `EncodeKey`/`EncodeValue`/`Decode`/`CutOne` 往返；截断与损坏的输入（错误消息逐字比较）；`HashChunkRow` 等写入的字节序列；desc 编码。每种输出都比较字节。
- TiKV 对照：int、uint、float、bytes、decimal、duration、time、json 的 key/value 编码，与 vendored TiKV 编码器逐字节比较。
- 两个平台都回放（amd64 向量取自 tracker CI）；生成器换多个种子都能生成，回放都通过。
- 反证：改坏 flag 字节、分组填充、desc 取反、decimal 精度、collation key 各一处，回放都要报错。
- 性能：与 Go 的 `bench_test.go`（`BenchmarkEncodeIntWithSize` 等）同负载对比，codec 是热路径，目标不慢于 Go。
