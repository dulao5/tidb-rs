# D01.11 tablecodec / rowcodec：块 spec

状态：草案（2026-11-08）。阶段：P1，A 线。基线：Go TiDB v8.5.8（`8b857efa2`）。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[mappings.md](../../docs/porting/mappings.md)、methodology Q3（2026-11-08 按负责人决定 #39 修订：编码格式从 Go 移植（L1），对 TiKV 的实现做双向逐字节对照）、[N0 crate 分层](../exceptions/N0-crate-layering/spec.md)。本块建立 crate `tidb-tablecodec`（rank 6，依赖 `tidb-base`、`tidb-parser`、`tidb-types`、`tidb-model`、`tidb-chunk`、`tidb-codec`、`tidb-kv`）。

## 1. Go 范围

`pkg/tablecodec`（tablecodec.go、rowindexcodec）和 `pkg/util/rowcodec`（common.go、row.go、decoder.go、encoder.go）：

- 表、记录、索引、meta 的 key 布局，key 的编码与切分（含 API V2 前缀、分区、全局索引、临时索引 key）；
- 行格式 v1（`colID, value` 交替）和 v2（rowcodec，大小两种布局、NULL 列、checksum）的解码，解码到 Datum map、chunk、字节；
- 索引值的各个版本（旧 collation、version 0 / 1、restored data、common handle、分区 ID）的解码，`DecodeIndexKV`、`DecodeHandleInIndexValue`；
- 行 checksum（`RowData.Checksum`）。

## 2. 读路径划分

tidb-rs 是只读计算节点，写路径由 Go TiDB 承担。在 tracker 的函数级引用图 `work/go_refs.tsv` 上，从所有非 X 的块出发求可达集，再逐个判断（tracker `scripts/blocks/rules.py`）。D01.11 由 169 个函数变为 106 个，63 个归 X：

| 类别 | 内容 |
|---|---|
| 写路径 | 行编码（`EncodeRow`、`EncodeOldRow`、rowcodec `Encoder` 及其排序器、`row.toBytes` / `init*`），索引值编码（`GenIndexValue*`、`EncodeHandleInUniqueIndexValue`、`encodeCommonHandle`、`encodePartitionID`、`TryGetCommonPkColumnRestoredIds`），临时索引值编解码（DDL backfill / merge），`IsUntouchedIndexKValue`、`IndexKVIsUnique`、`TempIndexKey2IndexKey` |
| 只有 DDL、unistore、TiCDC 用到 | `CutCommonHandle`、`DecodeIndexID`、`EncodeMetaKeyPrefix`、`TruncateToRowKeyLen`、`IsRowKey`、`row.ColumnIsNull`、`row.CalculateRawChecksum` |
| v8.5.8 中没有调用方 | `CutIndexKey`、`CutIndexPrefix`、`CutRowKeyPrefix`、`EncodeMetaKey`、`MetaPrefix`、`UnflattenDatums`、`RowData.Encode` |

key 的编码保留：构造扫描范围、点查 key 都需要它，TiKV 对照也需要。Go 的行编码器在 govec 中仍用来生成输入（Rust 只解码）。

## 3. 设计

- **crate 布局**：`tablecodec.rs`（pkg/tablecodec）、`rowindexcodec.rs`、`rowcodec/{common,row,decoder}.rs`。
- **借用**：解码结果尽量借用输入（`cut_row_new` 返回 `Vec<&[u8]>`，`decode_index_kv` 返回 `Vec<Cow<[u8]>>`，只有 restored data 需要新建时才分配），对应 Go 中对输入切片的子切片。
- **D07.0 的后续**：`tidb-kv` 中 `NewCommonHandle`（`codec.CutOne`）、`CommonHandle.Data`（`codec.DecodeOne`）、`IntHandle.Encoded` 改用 `tidb-codec`，去掉了 `glue:`。kv 的 govec 记录现在包含每个 handle 的 `String` / `Data`，3 个豁免已删除，D07.0 converge 104/111（剩 `ResourceGroupTagBuilder` 的 7 个）。
- **`init`**：Go 的包初始化把 `kv.DecodeTableIDFunc` 设为 `DecodeTableID`；Rust 中 `init()` 返回这个函数，由调用方设置（没有包初始化）。

## 4. 已知差异

1. `init` 返回 `kv.DecodeTableIDFunc` 的函数，不设置全局钩子。
2. `[]byte` 的 nil 与空：空切片按 Go 的非 nil 空切片处理（KV 的值不会是 nil）。
3. `DecodeHandleInIndexValue` / `DecodeIndexHandle` 返回 `Option<Handle>`；Go 的 `PartitionHandle{nil}`（只有分区 ID、没有 handle）为 `None`。
4. `IndexValueSegments` 的字段是 `Option<&[u8]>`：Go 的 nil 与空在这里有区别。
5. `ChunkDecoder` 的 location 为 nil 时，handle 列按 UTC 处理（Go 只在 TIMESTAMP handle 列非零时 panic）。
6. `appendDatumForChecksum`：被 recover 的 panic 文字中 "interface conversion" 之后是 Rust 的文字。
7. restored `_bin` 的填充空格分配失败时 panic，而不是 Go 的致命 OOM（见 §5 第 1 条）。

## 5. 上游行为（upstream-pitfall 候选）

1. **索引值中 restored `_bin` 的填充数被损坏时，分配巨大的字符串**：`decodeRestoredValuesV5` 按填充数构造空格串，最多 2^63 个，Go 直接致命 OOM；几 GiB 的数值会真的分配。govec 中不生成这类输入，变异测试把填充数限制在可分配范围内。
2. **`ChunkDecoder.tryAppendHandleColumn` 和 commit-ts 列不看列类型，直接追加 int64 / uint64**：列更窄时越界写入 chunk（内存损坏），用 `-gcflags=all=-d=checkptr` 发现。只有 schema 与 handle 类型不一致时可达。
3. **`DecodeRowWithMapNew` 按 map 的顺序解码各列**：先报哪一列的错误、是错误还是 panic，每次运行可能不同。govec 把这类失败记为 `{"fail":true}`，Rust 只要求同样失败。
4. **TiKV 的 `RowSlice::from_bytes` 断言 checksum 部分正好 5 或 9 字节**：39 条 Go 能接受的畸形行会让 TiKV panic（Go 忽略多余的字节）。
5. **`DecodeValuesBytesToStrings` 在 `ToString` 失败时返回 `(nil, nil)`**：返回了错误的错误变量。
6. `DecodeMetaKey` 把 flag 当作 byte 比较（uint64 被截断），影响很小。

## 6. 验收

- converge：D01.11 `matched 106/106`；D07.0 `104/111`（7 个 `ResourceGroupTagBuilder` 豁免）。
- govec `tablecodec` 领域：12,883 条，arm64 本地生成、amd64 取自 tracker CI，两个平台记录相同；回放在 debug 和 release-checked 下都是 0 条不一致；种子 1 到 8 本地生成并回放都通过（tracker CI 也生成种子 1 到 8）。
- Go 测试：tablecodec_test.go、rowcodec_test.go、rowindexcodec_test.go 中 38 个测试函数的字面量由 `cmd/minelits` 抽取（`testdata/tablecodec_literals.tsv`，`minedGoTests["tablecodec"]`）。govec 表达不了的断言只有 `TestError`（包内私有错误转成 SQL 错误后码不变），移植为 `tablecodec.rs` 中的单元测试。`TestBenchDaily` 只是基准。
- TiKV 双向对照（`tests/tikv_oracle.rs`，vendored `tidb_query_datatype` 与 `codec`）：
  - key：两个方向字节相同；
  - 行 v2：904 行 Go 的行由 TiKV 的 `RowSlice` 解析，列字节、NULL 集合、checksum 都相同；512 行由 TiKV 的编码器重新编码，与 Go 的字节相同；
  - v2→v1 datum 转换：2,310 次值相同，其中 1,009 次字节按设计不同（Go 用 varint / varuint、duration 按整数；TiKV 用可比较整数、duration flag）；
  - 行 v1：300 行切分结果相同；
  - TiKV 的索引值 / restored data 解码在 `tidb_query_executors` 中，没有 vendored，索引值只与 Go 对照。
- 解码变异测试（`tests/decode_mutation.rs`）：22 万次，全部是 Ok、错误或可捕获的 panic。
- 反证：8 处故意弄坏，回放都能发现（plan.md）。

## 7. 性能

同一台 arm64 Mac，Go 取 `-count 5` 的中位数，Rust 用 criterion；输入相同（tracker `govec/domains/tablecodec_bench_test.go`，`benches/tablecodec.rs`）。

| 基准 | Go | Rust |
|---|--:|--:|
| EncodeRowKeyWithHandle | 23.6 ns | 23.7 ns |
| DecodeRowKey | 1.99 ns | 1.31 ns |
| DecodeHandleInIndexValue（int） | 6.15 ns | 1.25 ns |
| DecodeHandleInIndexValue（common） | 35.7 ns | **46.8 ns** |
| DecodeToChunk（3 列，每次 Reset） | 31.7 ns | **37.0 ns** |
| DecodeToChunk（10 列，每 1024 行 Reset） | 131.4 ns | 102.8 ns |
| DecodeRowToDatumMap（10 列） | 489.6 ns | 389.9 ns |
| CutRowNew（v1，10 列） | 155.4 ns | 90.8 ns |
| DecodeIndexKV（restored，general_ci） | 137.4 ns | **164.1 ns** |
| RowData.Checksum（3 列） | 31.7 ns | 31.5 ns |

热路径（10 列解码到 chunk、解码到 map、v1 切分）快于 Go。慢于 Go 的 3 项登记为 T9。
