# Go-generated vectors (v8.5.8), domain "pointget" (block D12.1 part 1, VS1-12)

`<target>/pointget.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/pointget.jsonl.gz` (branch `govec/pointget-vs12`, tracker#89) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261116), 3600 records |
| `arm64/pointget.jsonl.gz` sha256 | `9999deb758cff8743fdfbac45e63cb8eb25fee88a8a296f9374b3ab7ce3e5d81` (generated locally) |
| `amd64/pointget.jsonl.gz` sha256 | `ca0c647735ff676d640561240202b228437aa9d817cc43d564687e40e0b8d308` (GOARCH=amd64, GOAMD64=v1; tracker#89 CI artifact, run 43651873205) |

The replay (`tests/govec_pointget.rs`) also reads the tablecodec domain's vectors
(`crates/tablecodec/tests/vectors`) for the record keys of a point get.

The two targets hold the same records.
