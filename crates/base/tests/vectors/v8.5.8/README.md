# Go-generated vectors (v8.5.8), domain "base" (block D00.2)

`<target>/base.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/base.jsonl.gz` (branch `govec/base-D00.2`) |
| tracker commit | `6087306` |
| `arm64/base.jsonl.gz` sha256 | `11cf613f72680fbcd7dcf4a966df5dcda4da62b44d8a05f4f0138c4edaebc656` (GOARCH=arm64) |
| `amd64/base.jsonl.gz` | **not yet copied**: taken from the tracker PR's CI artifact (`govec-vectors-ubuntu-latest`), see `specs/D00.2-base/tasks.md` T9. Until then `cargo test -p tidb-base` fails on x86_64. |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261107), 7,998 records: 26 full tables, 1,795 `sprintf`, 30 `sprint`, 660 `new_err(f)`, 1,043 `terror`/`dbterror` `NewStd`, 395 prototypes, 150 `to_sql_error`, 2,756 unicode lookups, ...; 32 Go panics (`FORMAT` on too-short input) |

The domain has no floating-point arithmetic that gc could fuse (mapping rule `M-FMA`), so
both targets have the same records; the per-target layout only follows the tracker's.

Record format and replay rules: tracker `govec/README.md` ("base"). Replayed by
`crates/base/tests/govec_base.rs`. To update, copy the `.gz` files from a newer tracker commit
and update the commit and sha256 above.
