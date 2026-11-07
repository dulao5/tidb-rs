# Go-generated vectors (v8.5.8), domain "model" (block D06.1)

`<target>/model.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/model.jsonl.gz` (branch `govec/model-D06.1`, PR #13) |
| tracker commit | `3647aea` |
| `arm64/model.jsonl.gz` sha256 | `1293a884cf89c80371e5ac61c8d0c7021e5cec9b8711c266b224f13ba4c0fa00` (GOARCH=arm64, generated locally) |
| `amd64/model.jsonl.gz` sha256 | `2cbb637ed66c93a86862f0c26b7ad9827ebec77a46c36f0b6525beccb457b663` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 1618), 63,366 records: 50,918 `roundtrip` (866 meta exported from a v8.5.8 cluster, 15,000 random values, 15,000 object-level mutations, 20,000 `luckyMutate` mutations, 68 time values), 3,061 `into` (61 of them one per error / early-return site of encoding/json decode.go), 9,387 method calls; 8,537 Go errors; 5 records marked `known_deviation` |

Both targets have the same records (the CI artifact differs from the local arm64 file only in
the header); the per-target layout only follows the tracker's.

Records marked `"known_deviation": "nil-ptr-elem"` (a null element of a `[]*T`, spec D06.1
§4.1) must differ from Go and are counted apart; the test fails on any other mismatch and on a
marked record that no longer differs.

Record format and replay rules: tracker `govec/README.md` ("model") and the header of
`govec/domains/model.go`. Replayed by `crates/model/tests/govec_model.rs`. To update, copy the
`.gz` files from a newer tracker commit and update the commit and sha256 above.
