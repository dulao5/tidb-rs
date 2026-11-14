# Go-generated vectors (v8.5.8), domain "meta" (block D06.2 part 1)

`<target>/meta.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/meta.jsonl.gz` (branch `govec/meta-D06.2`) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261111), 1854 records |
| `arm64/meta.jsonl.gz` sha256 | `62ab55f7aa30f2bd4580af03d02406ab28430387147da4c28f1bfdacfa9db46b` (generated locally) |
| `amd64/meta.jsonl.gz` sha256 | `fef541d1b42c83114bc187de7065b58b2ba77e93ddcb8aa2f0f9fa8c03fe4b71` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |

The two targets hold the same records.

Records with `err.name == "panic"` must panic in Rust with the same message.

## Domain "infoschema" (block D06.3 part 1)

`<target>/infoschema.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/infoschema.jsonl.gz` (branch `block/D06.3-infoschema-1b`, tracker#73) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261114), 220 records |
| `arm64/infoschema.jsonl.gz` sha256 | `83a5aaf332da6d2d2ad4f994d7f2d570d37e056c6f6d9ee34bc1b79db3320a98` (generated locally) |
| `amd64/infoschema.jsonl.gz` sha256 | `0544438a4f6f00564c8883a098842ab12ecf66fc7b7cdc1b9d52cb4e429fce5f` (GOARCH=amd64, GOAMD64=v1; tracker#73 CI artifact, run 43536418561) |

The two targets hold the same records.

The replay (`tests/govec_infoschema.rs`) treats Go's nil and empty slices as equal.
