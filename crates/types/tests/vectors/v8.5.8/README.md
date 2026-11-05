# Go-generated vectors (v8.5.8)

`decimal.jsonl` is copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/decimal.jsonl` |
| tracker commit | `e9da32292f9b5f2f51316dc256ea717b12f68911` (`e9da322`) |
| sha256 | `abca8bf5323bd5b0fde8398bc652f8bef13188afaaba325025d32275a19b42ac` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20260827, generated on darwin/arm64), 31,620 records (27,641 base, 1,795 `*_alias`, 2,184 `*_then_*` chained) |

Record format and replay rules: tracker `govec/README.md` (errors are compared by code and
the unfilled message template; aliasing ops are replayed per `M-ALIAS` by copying the aliased
input first; chained ops pass the first step's object, not a re-parsed string).
If a domain file is stored as `.jsonl.gz` in the tracker, copy the `.gz` file and record its
sha256. Replayed by
`crates/types/tests/govec_decimal.rs`. To update, copy the file from a newer tracker commit
and update the commit and sha256 above.
