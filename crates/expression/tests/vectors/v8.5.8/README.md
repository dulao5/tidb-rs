# Go-generated vectors (v8.5.8), domain "expr" (block D08.1 part 1)

`<target>/expr.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/expr.jsonl.gz` (branch `govec/expr-D08.1`) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261112), 5,766 records: `col` 690, `colmisc` 300, `const` 2,500, `corr` 460, `evalbool` 300, `new` 60, `schema` 600, `tbl` 856 |
| `arm64/expr.jsonl.gz` sha256 | `4a659120157edd2c2fd3ed9fd2003dce64dc50f179dbb07a2d7c8a386c06c884` (GOARCH=arm64, generated locally) |
| `amd64/expr.jsonl.gz` sha256 | `f8b61974355d4ae6c50d116008a8112dec1e03210561f739eba62b585b80e959` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |

Column and constant values reuse the chunk domain's column specs and the datum domain's datum
specs. Three `const` records differ between the targets (M-F2I: a NaN float datum with a
hybrid `RetType` reaches `int64(NaN)`); the Rust conversion follows the target, as in the
datum domain. `tbl` records are the 856 TableInfo texts exported from a v8.5.8 cluster (model domain)
without virtual generated columns. A `{"panic": true}` output must panic in Rust too.

Pinned deviation (spec §4): a correlated column's value lives in the evaluation state, not in
the node, so `CorrelatedColumn.MemoryUsage` counts the zero Datum; the `corr` records carry
`dmem` (the value's `MemUsage`) and the replay compares `mem - zero + dmem` with Go's `mem`.
