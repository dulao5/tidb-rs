# Go-generated vectors (v8.5.8), domain "kv" (block D07.0)

`<target>/kv.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/kv.jsonl.gz` (branch `govec/kv-D07.0`) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 707), 3,188 records |
| `arm64/kv.jsonl.gz` | GOARCH=arm64, generated locally |
| `amd64/kv.jsonl.gz` | GOARCH=amd64, GOAMD64=v1; tracker CI artifact |

There is no floating-point arithmetic in the domain: both targets hold the same records; the
per-target layout follows the tracker's. Records with `err.name == "panic"` must panic in Rust.
