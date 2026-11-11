# Go-generated vectors (v8.5.8), domain "write_systable" (block D06.7)

`<target>/write_systable.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/write_systable.jsonl.gz` (branch `govec/sessionvars-D05.2-c`, tracker #34) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 5067), 168 records: `tableinfo` 2, `replace` 97 (93 through Go's sysSessionPool session, row format 1; 4 on a normal session, row format 2), `alloc` 29, `next_step` 42 |
| `arm64/write_systable.jsonl.gz` sha256 | `58730e73087890cd13cd2dbcdbb5817fbed37e5c3297cdf49abf4eb66ed8cbea` (GOARCH=arm64, generated locally) |
| `amd64/write_systable.jsonl.gz` sha256 | `222e84b71fa000bff7e58acabed3171c31bf42db83f0a1d694c3adb14ec84503` (tracker CI artifact, run 42947092988) |

The oracle is Go's own REPLACE executor on a bootstrapped mockstore: each `replace` record
holds what the statement can read and the transaction's memory buffer (key, value, key
flags) before the commit. Both targets hold the same records. Replayed by
`tests/govec_systable.rs`; set `GOVEC_SYSTABLE_FILES` (comma-separated) to also replay
other seeds.
