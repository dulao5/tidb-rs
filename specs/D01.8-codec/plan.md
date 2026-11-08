# D01.8 编码：计划与交接

## 1. 状态

- 实现完成：tidb-rs `block/D01.8-codec`（crate `tidb-codec`），tracker `govec/codec-D01.8`（govec `codec` 领域）。
- 剩余：tasks T9–T11。

## 2. 复现

- 回放：`cargo test -p tidb-codec`（另：`GOVEC_CODEC_FILES=a.jsonl.gz,b.jsonl.gz` 回放 tracker CI 产物中的其他种子）。
- 生成：tracker `govec/` 下 `go run . codec -o ...`（需 `CC=/usr/bin/clang GOFLAGS=$(scripts/govec-fixes.sh)`），`-seed N` 换种子。
- 基准：`cargo bench -p tidb-codec`；Go 侧 `go test ./domains -run '^$' -bench Codec`。

## 3. 注意事项

- govec 的 `decoder` 只生成 TiDB 中会出现的 flag 与列类型组合（`codecDecoderFits`）：把 int64 追加进 FLOAT 列之类是 chunk 的误用，Go 与 Rust 的 chunk 在这种情况下行为不同（D01.7 范畴）。
- 哈希类比较写入的字节序列；`{"panic": true}` 子结果要求 Rust 在同一步 panic。
