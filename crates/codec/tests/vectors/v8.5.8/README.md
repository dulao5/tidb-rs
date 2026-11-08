# govec vectors of block D01.8 (`codec`)

Copies of tidb-rs-tracker `vectors/v8.5.8/<target>/codec.jsonl.gz` (domain `codec`, seed
20261109, 44,758 records). arm64 was generated locally; amd64 is the tracker CI artifact. The
two targets hold identical records (no floating-point arithmetic in pkg/util/codec).
Replayed by `tests/govec_codec.rs`; set `GOVEC_CODEC_FILES` (comma-separated) to replay the extra-seed artifacts
of tracker CI as well.
