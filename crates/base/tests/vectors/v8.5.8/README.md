# Go-generated vectors (v8.5.8), domain "base" (block D00.2)

`<target>/base.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/base.jsonl.gz` (branch `govec/base-D00.2`) |
| tracker commit | `3408336` (branch `govec/cast-D08.8-2`; the struct/array records were appended there) |
| `arm64/base.jsonl.gz` sha256 | `d0d7c2212c676d1779af8eb26f259f944dafe0504c10f2c00826940c88aa946f` (GOARCH=arm64, generated locally) |
| `amd64/base.jsonl.gz` sha256 | `a342831992a6ea27257739fd8fd87a8be5a21e7ae70c853d3b8d21b21e4335d2` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261107), 29,547 records (1,500 `rand/sprintf_composite` appended for D08.8 part 2a: struct and array values printed by reflection): 26 full tables, 1,795 `sprintf` on every template, 20,049 random `sprintf_rand` (flags/width/precision/`*`/verbs, non-UTF-8 strings and bytes, float32, halfway floats, huge widths), 30 `sprint`, 660 `new_err(f)`, 1,043 `terror`/`dbterror` `NewStd`, 395 prototypes, 150 `to_sql_error`, 2,756 unicode lookups, ...; 32 Go panics (`FORMAT` on too-short input). Go strings that are not valid UTF-8 are encoded `{"hex": ...}` (tracker `vec.GoString`) |

The domain has no floating-point arithmetic that gc could fuse (mapping rule `M-FMA`), so
both targets have the same records; the per-target layout only follows the tracker's.

Record format and replay rules: tracker `govec/README.md` ("base"). Replayed by
`crates/base/tests/govec_base.rs`. To update, copy the `.gz` files from a newer tracker commit
and update the commit and sha256 above.
