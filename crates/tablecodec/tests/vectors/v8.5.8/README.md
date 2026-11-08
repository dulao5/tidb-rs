# Go-generated vectors (v8.5.8), domain "tablecodec" (block D01.11)

`<target>/tablecodec.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/tablecodec.jsonl.gz` (branch `govec/tablecodec-D01.11`, PR #25) |
| tracker commit | `b2feaba` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261110), 12,883 records |
| `arm64/tablecodec.jsonl.gz` sha256 | `8424167babba7d3b28cc3ff28b5ce67f4b28d7ad02eeaf6cd45a03979183a583` (GOARCH=arm64, generated locally) |
| `amd64/tablecodec.jsonl.gz` sha256 | `7d87330aa20a9c0b1e019c896ea26ce2f7fe374bc68b28a6e60b0503868764c7` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |

No floating-point arithmetic in the domain: both targets hold the same records. Replayed by
`tests/govec_tablecodec.rs`; set `GOVEC_TABLECODEC_FILES` (comma-separated) to also replay the
extra-seed artifacts of tracker CI (seeds 1-8). Records with `{"fail":true}` are Go failures
whose error/panic depends on map iteration order (spec §5 item 3): Rust must fail too.
