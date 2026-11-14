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
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2` with fix `tidb-rs#195`, the GC deviation; go1.25.12, seed 20261114), 220 records |
| `arm64/infoschema.jsonl.gz` sha256 | `ddc27c6200f7db0416b2695cbe50895b3eee0748a029cf26f0526d7acedbf8b0` (generated locally) |
| `amd64/infoschema.jsonl.gz` sha256 | `879f1769cf733a84c248085e03ee1c731bb175412c3eeb57587739bb0d13773b` (GOARCH=amd64, GOAMD64=v1; tracker#73 CI artifact, run 43543149312) |

The two targets hold the same records.

The replay (`tests/govec_infoschema.rs`) treats Go's nil and empty slices as equal.

## Domain "table" (block D06.4 part 1)

`<target>/table.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/table.jsonl.gz` (branch `block/D06.4-table`) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261115), 8365 records |
| `arm64/table.jsonl.gz` sha256 | `eba9cadf9bc4a2783dd3c161a8571e5f1c90e5b676dc6956ac7a6bec0dd47e8d` (generated locally) |
| `amd64/table.jsonl.gz` sha256 | `ba26837de1bdce03cff4851ad70d3f4a06be156bc7db2fb596d59fba9a864ee2` (GOARCH=amd64, GOAMD64=v1; tracker#77 CI artifact, run 43572016142) |

The two targets hold the same records.

The replay (`tests/govec_table.rs`) compares parser errors (`[parser:1064]`) by code only (D06.4 spec §4).
