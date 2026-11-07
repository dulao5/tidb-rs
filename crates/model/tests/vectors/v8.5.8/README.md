# Go-generated vectors (v8.5.8), domain "model" (block D06.1)

`<target>/model.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/model.jsonl.gz` (branch `govec/model-D06.1`, PR #13) |
| tracker commit | `8d2f5b8` |
| `arm64/model.jsonl.gz` sha256 | `2873394b1396206b2acd3161b49bf30813763e32daf62b97ba26757112a58e05` (GOARCH=arm64, generated locally) |
| `amd64/model.jsonl.gz` sha256 | `99d5079e3277ed889d53d169976a0a244475f16245d0dfdc9561722e6bd0693a` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261125), 9,053 records: 3,918 `roundtrip` (866 meta exported from a v8.5.8 cluster, 1,500 random values, 1,500 mutations, 68 time values), 300 `into`, 3,800+ method calls, 640 Go errors |

Both targets have the same records (the CI artifact differs from the local arm64 file only in
the header); the per-target layout only follows the tracker's.

Record format and replay rules: tracker `govec/README.md` ("model") and the header of
`govec/domains/model.go`. Replayed by `crates/model/tests/govec_model.rs`. To update, copy the
`.gz` files from a newer tracker commit and update the commit and sha256 above.
