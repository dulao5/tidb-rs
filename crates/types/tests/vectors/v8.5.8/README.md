# Go-generated vectors (v8.5.8)

`decimal.jsonl.gz` is copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/decimal.jsonl.gz` (branch `govec/decimal`) |
| tracker commit | `6366acda33e951b1bdefdf7f6012d1c410867889` (`6366acd`) |
| sha256 | `f9b27cc409e14fc1ce114ad625a6d90e9a8713e4382d8d291f092a6d041aed75` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20260827, generated on darwin/arm64), 33,149 records: 27,641 base, 1,795 `*_alias`, 2,184 `*_then_*` chained, 1,529 `dirty_*` (dirty receiver) |

Vector files are committed gzip-compressed (`.jsonl.gz`, written by govec itself; the
plain JSONL of this domain is 6.7 MB, the `.gz` 0.49 MB) and read in the test with
`flate2`. Look at one with `gzip -dc decimal.jsonl.gz | head`.

Record format and replay rules: tracker `govec/README.md` (errors are compared by code and
the unfilled message template; aliasing ops are replayed per `M-ALIAS` by copying the aliased
input first; chained ops pass the first step's object, not a re-parsed string; `dirty_*`
ops write into the object `from_string(args[0])` left behind, without resetting it).
Replayed by `crates/types/tests/govec_decimal.rs`. To update, copy the `.gz` file from a
newer tracker commit and update the commit and sha256 above.
