# Go-generated vectors (v8.5.8), domain "chunk" (block D01.7)

`<target>/chunk.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/chunk.jsonl.gz` (branch `govec/chunk-D01.7`, PR #19) |
| tracker commit | `f2a2a51` |
| `arm64/chunk.jsonl.gz` sha256 | `315312388eb192d1e58cb652428fe57c0f2c4c006741348022ec92e5fce0abed` (GOARCH=arm64, generated locally) |
| `amd64/chunk.jsonl.gz` sha256 | `2dedeca66cd3f8cc1b086577514c7c469f94b7f63f2580388a82ac32a50f07ba` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261201), 1,671 records: 1,620 `script` (1,600 random scripts of about 100 steps on average, 20 scenarios of the Go tests of pkg/util/chunk) and 51 `type_width` |

Both targets have the same records (the CI artifact differs from the local arm64 file only in
the header: no floating-point arithmetic); the per-target layout only follows the tracker's.

A dump in a record compares the chunk's state including each column's four slices with Go's
capacities (`nullBitmap`, `offsets`, `data`, `elemBuf`) and the bytes under NULL cells, and
`Codec.Encode`'s output (spec D01.7 §3.1, §3.2). A Go panic in a step (`{"panic": msg}`) must
be a Rust panic; the message is not compared.
