# Go-generated vectors (v8.5.8), domain "kv" (block D07.0)

`<target>/kv.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/kv.jsonl.gz` (branch `govec/kv-D07.0`, PR #22) |
| tracker commit | `d83413e` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 707), 3,128 records |
| `arm64/kv.jsonl.gz` sha256 | `3d2c3cdb6739f1104c1283fb08f005bee932f5df2c4b60f863d55b55d2796201` (GOARCH=arm64, generated locally) |
| `amd64/kv.jsonl.gz` sha256 | `11a0e92c365c973202f5204f0c1870b17673a106adf3b837227318e660000270` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |

There is no floating-point arithmetic in the domain: both targets hold the same records; the
per-target layout follows the tracker's. Records with `err.name == "panic"` must panic in Rust.
