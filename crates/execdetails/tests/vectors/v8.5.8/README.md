# Go-generated vectors (v8.5.8), domain "execdetails" (block D14.0)

`<target>/execdetails.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/execdetails.jsonl.gz` (branch `govec/execdetails-D14.0`) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261111), 2866 records |
| `arm64/execdetails.jsonl.gz` sha256 | `7ff9a4ca0162ebef8d76677ff1b5f0b00b54f54bb24942ec8f2a616c3d15551f` (generated locally) |
| `amd64/execdetails.jsonl.gz` sha256 | `9ef794b43d1b997ee49b882ddf0c2acc56fb84190c12dd6c4170ee378786ce1f` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |

The tdigest and Go's `math.Sin` / `math.Asin` fuse multiply-adds on arm64 (spec D14.0 §3.1), so the
two targets hold different records: 152 (tracker `targetDiffs`: tdigest 58, percentile 64, sync 18,
gomath 12). Records with `err.name == "panic"` must panic in Rust.
