# Go-generated vectors (v8.5.8), domain "planutil" (block D10.10, VS1-19 part)

`<target>/planutil.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/planutil.jsonl.gz` (branch `vs1/planner-base`) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261115), 2862 records |
| `arm64/planutil.jsonl.gz` sha256 | `3ab79cf026ceb4f51f111a463395231ccfc803a14bf3a5667594bd41453cbe50` (generated locally) |
| `amd64/planutil.jsonl.gz` sha256 | (tracker CI artifact, GOARCH=amd64, GOAMD64=v1) |

The functions the domain covers only add, multiply and divide floats (no multiply-add for gc to
fuse), and the two targets hold the same records (tracker `TestTargetsDiffer`).
