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

| | |
|---|---|
| tracker commit | `c70bca6` (branch `govec/time-D01.3`) |
| `amd64/time.jsonl.gz` sha256 | `19abff457405d74cf7747d857dee0c1cd67693900ef817e59f70aa7af211f098` (CI linux/amd64, GOAMD64=v1) |
| `arm64/time.jsonl.gz` sha256 | `7047d0331fdd6921111327b494d97205ee253241a6532bef1204d17dc8ed6887` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261107); the targets differ in 3 records (`parse_time_float64` of NaN, +Inf, 1e19) |

## vector (block D01.6)

`<target>/vector.jsonl.gz` are copied unmodified from tidb-rs-tracker
`vectors/v8.5.8/<target>/vector.jsonl.gz` (branch `govec/vector-D01.6`): 91,762 records of
`pkg/types` vector.go and vector_functions.go plus the jsoniter parser. Inputs include NaN
bit patterns (QNaN with payloads, SNaN, negative SNaN, all ones), so the replay checks
which NaN each distance returns under gc's operand order on each target. The targets
differ in 1,851 `distance` records (M-FMA rounding, and the x86 / ARM NaN rules).
Replayed by `tests/govec_vector.rs`.

| | |
|---|---|
| tracker commit | `bbb75e6` (branch `govec/vector-D01.6`) |
| `amd64/vector.jsonl.gz` sha256 | `dd598d395cb3a25c2e3bd891092c4cafcec9bbf6b83db0686445c1dd77ff04ba` (CI linux/amd64, GOAMD64=v1) |
| `arm64/vector.jsonl.gz` sha256 | `2efb8e86bc9e41d3ce75c54766a31a0310ed2e635ca45ab63fc081d4f901a6bb` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 31337) |

## collate (block D01.9)

`<target>/collate.jsonl.gz` are copied unmodified from tidb-rs-tracker
`vectors/v8.5.8/<target>/collate.jsonl.gz` (branch `govec/collate-D01.9`): 46,736 records of
`pkg/util/collate`, `pkg/parser/charset/charset.go` and `pkg/util/stringutil`, including
SHA-256 digests of `Key`, `KeyWithoutTrimRightSpace` and `Compare` over all code points for 8
collations. The two targets have the same records. Replayed by `tests/govec_collate.rs`.

| | |
|---|---|
| tracker commit | `82396f9` (branch `govec/collate-D01.9`) |
| `amd64/collate.jsonl.gz` sha256 | `abe9f09d0ad86e9138b193659733d3211032e708ecc43120d51b9362a3d12194` (CI linux/amd64, GOAMD64=v1) |
| `arm64/collate.jsonl.gz` sha256 | `7f708d29d166268e0f74695f3fe2eb10614588ad68c5de3d1060f70f0a1aa22d` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261109) |
