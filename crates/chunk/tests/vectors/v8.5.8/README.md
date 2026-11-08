# Go-generated vectors (v8.5.8), domain "chunk" (block D01.7)

`<target>/chunk.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/chunk.jsonl.gz` (branch `govec/chunk-D01.7-datum`, PR #21) |
| tracker commit | `0ba147a` |
| `arm64/chunk.jsonl.gz` sha256 | `82b76f4dc8af384d70534742335c0339f531f4c3717dab0db613bb6659ac4b53` (GOARCH=arm64, generated locally) |
| `amd64/chunk.jsonl.gz` sha256 | `aaa74c4b191b028557060794be252e105bd59121d32a7d87d2e30ce820fa4d73` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261201), 2,276 records: 2,225 `script` (1,600 random scripts of about 100 steps on average, 20 scenarios of the Go tests of pkg/util/chunk; after block D01.1, 600 Datum/MutRow scripts and 5 scenarios of mutrow_test.go / chunk_test.go) and 51 `type_width` |

Both targets have the same records (the CI artifact differs from the local arm64 file only in
the header: no floating-point arithmetic); the per-target layout only follows the tracker's.

A dump in a record compares the chunk's state including each column's four slices with Go's
capacities (`nullBitmap`, `offsets`, `data`, `elemBuf`) and the bytes under NULL cells, and
`Codec.Encode`'s output (spec D01.7 §3.1, §3.2). A Go panic in a step (`{"panic": msg}`) must
be a Rust panic; the message is not compared.

Steps that would make Go read or write past a slice's capacity (`Row.ToString` reading a BIT cell
with `GetInt64`, `MutRow.SetValue` storing a Time into a 4-byte cell) are recorded as
`{"past_cap": true}` instead of being run; the replay applies the same check (spec §4 items 4–5).
