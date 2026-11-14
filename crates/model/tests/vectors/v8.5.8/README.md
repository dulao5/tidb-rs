# Go-generated vectors (v8.5.8), domain "model" (block D06.1)

`<target>/model.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/model.jsonl.gz` (branch `rsgen/229-columninfo-size`, tidb-rs #229 batch E) |
| tracker commit | `e08db03` |
| `arm64/model.jsonl.gz` sha256 | `15e966da46fe61c475f6ec019c6778b0f0847243f57b7684d97010b1cae42655` (GOARCH=arm64, generated locally) |
| `amd64/model.jsonl.gz` sha256 | `537fbf26c44f171f73b7f1d95e0dc64fc9f8efe6617d8b347b67b3f3d2f04d90` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact, run 43620879687) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 1618), 62,833 records: 50,918 `roundtrip` (866 meta exported from a v8.5.8 cluster, 15,000 random values of 28 types, 15,000 object-level mutations, 20,000 `luckyMutate` mutations, 68 time values), 3,100 `into` (100 of them one per error / early-return site of encoding/json decode.go and of the JSON methods of terror.Error, atomic.Int64, json.RawMessage, and 7 for the fields boxed in Rust, spec N13 §9), 8,815 method calls (865 of them for job.go / reorg.go / ColumnNeedRestoredData); 8,474 Go errors; 9 records marked `known_deviation` |

Both targets have the same records (the CI artifact differs from the local arm64 file only in
the header); the per-target layout only follows the tracker's.

Records marked `"known_deviation": "nil-ptr-elem"` (a null element of a `[]*T`, spec D06.1
§4.1) must differ from Go, are counted apart, and must equal their `"ported"` output exactly
(Go's `Marshal` after the nil elements are replaced by `new(T)`); the test fails on any other
mismatch and on a marked record that no longer differs.

Size: about 13 MB per target (Part 1: 11 MB; before the review amplification: 1.6 MB), so each
regeneration adds about 26 MB to the history of both repositories. Acceptable for now. If other
domains grow the same way, the option is a small fixed set committed here and the amplified set
only generated and replayed in CI (that needs Go and the TiDB module in the tidb-rs CI, so it is
not done yet).

Record format and replay rules: tracker `govec/README.md` ("model") and the header of
`govec/domains/model.go`. Replayed by `crates/model/tests/govec_model.rs`. To update, copy the
`.gz` files from a newer tracker commit and update the commit and sha256 above.
