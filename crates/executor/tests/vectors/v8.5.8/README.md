# Go-generated vectors (v8.5.8), domain "pointget" (block D12.1 part 1, VS1-12)

`<target>/pointget.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/pointget.jsonl.gz` (branch `govec/pointget-vs12`, tracker#89) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261116), 5850 records |
| `arm64/pointget.jsonl.gz` sha256 | `4ddd4ce0251af460706e04cb7099c2b0e13477826630166be664f59e28d92f93` (generated locally) |
| `amd64/pointget.jsonl.gz` sha256 | `417cc44ff0ff649059f0ecd282fb3de1ca686095421e351b927c90d9a84948fb` (GOARCH=amd64, GOAMD64=v1; tracker#89 CI artifact, run 43655916945) |

The replay (`tests/govec_pointget.rs`) also reads the tablecodec domain's vectors
(`crates/tablecodec/tests/vectors`) for the record keys of a point get.

The two targets hold the same records.
