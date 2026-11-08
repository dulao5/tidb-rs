# D01.8 编码：计划与交接

## 1. 交接状态

- 分支：tidb-rs `block/D01.8-codec`（worktree `projects/tidb-rs-d018`，基于 main @41aa2ee）。tracker 分支尚未建立（计划用 `govec/codec-D01.8`，worktree `projects/tracker-d018`）。
- 已完成：块 spec 草案（spec.md），其中 §2 的移植方式待评审决定。
- 未开始：`crates/codec`、govec `codec` 领域、回放、TiKV 对照测试、基准。
- 原因：D01.7 的后续 PR（tidb-rs #37、tracker #21）先完成；本块在动手前需要先定 §2。

## 2. 实现顺序（§2 按建议定下之后）

1. `crates/codec`（`tidb-codec`）：`number.rs`、`float.rs`、`bytes.rs`，与 Go 文件一一对应。Go 测试中的表格先移植为单元测试，再接 govec。
2. `decimal.rs`（依赖 D01.2 的 `to_bin`/`from_bin`）。
3. `codec.rs` 的 Datum 层：`encode`、`EncodeKey`/`EncodeValue`、`EstimateValueSize`、`Decode`/`DecodeOne`/`CutOne`/`CutColumnID`/`DecodeRange`、`peek*`。
4. `codec.rs` 的 chunk 部分：`Decoder.DecodeOne`、`HashChunkRow`/`HashChunkColumns`/`HashChunkSelected`/`EqualChunkRow`/`HashGroupKey`/`EncodeHashChunkRowIdx`/`SerializeKeys`、`Hash64`/`HashCode`（`base.Hasher` 的归属见 spec §1）。
5. tracker govec `codec` 领域：minelits、边界值、随机 datum、损坏输入、hash 写入字节；两个平台；多种子。
6. TiKV 对照测试（`crates/codec/tests/tikv_oracle.rs`）。
7. 基准与 Go `bench_test.go` 对比。

## 3. 注意事项

- 生成器的 datum 编码沿用 datum2 的 `[kind, payload…]`；datum 输出沿用 `d2DatumOut`（D01.7 后续 PR 中已在 chunk 回放里复制了一份 Rust 侧解析，可以抽成 `tidb-types` 的测试辅助模块，避免第三份拷贝）。
- `HashChunkRow` 系列比较写入的字节序列（用一个记录写入的 `Hash64` 实现），不比较 FNV 结果，否则看不出是哪一段不同。
- `EncodeKey` 对字符串调用 collation key：D01.9 的全部 collation 都要进入向量。
