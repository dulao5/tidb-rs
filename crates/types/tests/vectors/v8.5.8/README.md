# Go-generated vectors (v8.5.8)

`<target>/decimal.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository, one
file per Go code-generation target:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/decimal.jsonl.gz` (branch `govec/decimal`) |
| tracker commit | `508781891123b2863af288e31a0bfedccb36b9a6` (`5087818`) |
| `amd64/decimal.jsonl.gz` sha256 | `ba9ddc7bf91bbc87c8b00c127b77277d5388be84929e6f0b78811c1a4ac26f20` (GOARCH=amd64, GOAMD64=v1) |
| `arm64/decimal.jsonl.gz` sha256 | `fa187122cf550d3d076adb3faec0134346abdac3ab4a103264ed7029dbc191d2` (GOARCH=arm64) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20260827), 33,831 records: 27,641 base, 1,795 `*_alias`, 2,866 `*_then_*` chained (incl. 682 with the `div` producer), 1,529 `dirty_*` (dirty receiver); 309 Go panics |

**Why per target**: Go gc fuses the two word sums of `MyDecimal.ToFloat64`
(mydecimal.go:1118, 1125) into FMA on arm64 and not on amd64 (tracker
`data/go_fma_sites.tsv`, mapping rule `M-FMA`). For `DecimalDiv` results
(`resultFrac < digitsFrac`) the result can differ in the last digit:
`DecimalDiv(50.77, 32, 4)` then `ToFloat64` is 1.586563 on arm64 and 1.586562 on amd64.
The two files differ in exactly 44 such `div_then_to_float64` records. The test replays
the file of the Go target matching the Rust target arch (`x86_64` -> `amd64`,
`aarch64` -> `arm64`); `MyDecimal::to_float64` uses `f64::mul_add` on aarch64 and a
separate multiply and add on x86_64.

Vector files are committed gzip-compressed (`.jsonl.gz`, written by govec itself) and read
in the test with `flate2`. Look at one with `gzip -dc arm64/decimal.jsonl.gz | head`.

Record format and replay rules: tracker `govec/README.md` (errors are compared by code and
the unfilled message template; aliasing ops are replayed per `M-ALIAS` by copying the aliased
input first; chained ops pass the first step's object, not a re-parsed string; `dirty_*`
ops write into the object `from_string(args[0])` left behind, without resetting it).
Replayed by `crates/types/tests/govec_decimal.rs`. To update, copy both `.gz` files from a
newer tracker commit and update the commit and sha256 above.

## time (block D01.3)

`<target>/time.jsonl.gz` are copied unmodified from tidb-rs-tracker
`vectors/v8.5.8/<target>/time.jsonl.gz` (branch `govec/time-D01.3`): 101,814 records of
`pkg/types` time.go, core_time.go and fsp.go plus the Go `time` package behaviour they
use. The TZif files of the zones used are inside the file (`zone` records), so the replay
does not depend on the machine's tz database. The two targets differ only where
`ParseTimeFromFloat64` converts NaN / ±Inf / out-of-range floats (mapping rule `M-F2I`).
Replayed by `tests/govec_time.rs`.
