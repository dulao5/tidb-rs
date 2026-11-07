# Go-generated vectors (v8.5.8), domain "base" (block D00.2)

`<target>/base.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/base.jsonl.gz` (branch `govec/base-D00.2`) |
| tracker commit | `08ba252` |
| `arm64/base.jsonl.gz` sha256 | `65d3c482f1a804effea752c8bfd1c233e2c56be97e6378f546b633ffd7b07410` (GOARCH=arm64, generated locally) |
| `amd64/base.jsonl.gz` sha256 | `b0b2ea1cfc1ef6b2076d75e394ec7caf0a6d4584f0c110b239f75926ba3578d9` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261107), 28,047 records: 26 full tables, 1,795 `sprintf` on every template, 20,049 random `sprintf_rand` (flags/width/precision/`*`/verbs, non-UTF-8 strings and bytes, float32, halfway floats, huge widths), 30 `sprint`, 660 `new_err(f)`, 1,043 `terror`/`dbterror` `NewStd`, 395 prototypes, 150 `to_sql_error`, 2,756 unicode lookups, ...; 32 Go panics (`FORMAT` on too-short input). Go strings that are not valid UTF-8 are encoded `{"hex": ...}` (tracker `vec.GoString`) |

The domain has no floating-point arithmetic that gc could fuse (mapping rule `M-FMA`), so
both targets have the same records; the per-target layout only follows the tracker's.

Record format and replay rules: tracker `govec/README.md` ("base"). Replayed by
`crates/base/tests/govec_base.rs`. To update, copy the `.gz` files from a newer tracker commit
and update the commit and sha256 above.
