# Go-generated vectors (v8.5.8), domain "kv" (block D07.0)

`<target>/kv.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/kv.jsonl.gz` (regenerated on branch `govec/tablecodec-D01.11`, PR #25: handle String/Data records) |
| tracker commit | `b2feaba` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 707), 3,128 records |
| `arm64/kv.jsonl.gz` sha256 | `33d70f30d7c405bb700439c9377fbe71f54942e82550a33b0e275efd9986bbef` (GOARCH=arm64, generated locally) |
| `amd64/kv.jsonl.gz` sha256 | `123b64f45fa3997fa714cc92829917dd877235fbc6ead657b1d6c5173f40a92d` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |

There is no floating-point arithmetic in the domain: both targets hold the same records; the
per-target layout follows the tracker's. Records with `err.name == "panic"` must panic in Rust.
