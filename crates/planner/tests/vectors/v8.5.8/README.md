# Go-generated vectors (v8.5.8), domain "planutil" (block D10.10, VS1-19 part)

`<target>/planutil.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/planutil.jsonl.gz` (branch `vs1/planner-base`) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261115), 2862 records |
| `arm64/planutil.jsonl.gz` sha256 | `3ab79cf026ceb4f51f111a463395231ccfc803a14bf3a5667594bd41453cbe50` (generated locally) |
| `amd64/planutil.jsonl.gz` sha256 | `e731c8b46e642b2851768bd3c37590b5f911d0f118b741d7ad7ee4bba392af44` (tracker CI artifact, run 43575124447, GOARCH=amd64, GOAMD64=v1) |

The functions the domain covers only add, multiply and divide floats (no multiply-add for gc to
fuse), and the two targets hold the same records (tracker `TestTargetsDiffer`).

## Domain "preprocess" (block D09.0, VS1-10 part)

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/preprocess.jsonl.gz` (branch `vs1/planner-optimize`) |
| generator | `govec` (seed 20261116), 101 records |
| `arm64/preprocess.jsonl.gz` sha256 | `3e494a8ea0542a63cb1590014e28b7076322f4281a6d5e06b1a97a3fdae44d81` (generated locally) |
| `amd64/preprocess.jsonl.gz` sha256 | `cff9afbef176b219d7c744dbfe5de24f10b791e8503da4cc541d09f919131af9` (the arm64 records with the amd64 header; the first amd64 artifact, tracker run 43576655239, matched arm64 record for record; tracker CI's amd64 check verifies it) |

`tests/govec_preprocess.rs` counts the records whose Rust result is a boundary stub instead
of failing on them, and pins the count per block (`D06.3` base table 11, `D02.2` TABLESAMPLE 4,
`D09.5` shared WITH clause 3); a record Go parses must parse in Rust.
