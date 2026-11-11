# Go-generated vectors (v8.5.8), domain "tablecodec" (block D01.11)

`<target>/tablecodec.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/tablecodec.jsonl.gz` (branch `govec/tablecodec-D01.11`, PR #25) |
| tracker commit | `b2feaba` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261110), 12,883 records |
| `arm64/tablecodec.jsonl.gz` sha256 | `b1a1ae806561ff82526ebcc22526cd89b4e187b8e55848358195b20c96d7b69c` (GOARCH=arm64, generated locally, PR C: special float inputs / encoder ops) |
| `amd64/tablecodec.jsonl.gz` sha256 | `61c914f083e346e9cd3437b315a4f326181762dfceaffc3d2ec6e53e05bf6b46` (tracker CI artifact, PR C) |

No floating-point arithmetic in the domain: both targets hold the same records. Replayed by
`tests/govec_tablecodec.rs`; set `GOVEC_TABLECODEC_FILES` (comma-separated) to also replay the
extra-seed artifacts of tracker CI (seeds 1-8). Records with `{"fail":true}` are Go failures
whose error/panic depends on map iteration order (spec §5 item 3): Rust must fail too.
