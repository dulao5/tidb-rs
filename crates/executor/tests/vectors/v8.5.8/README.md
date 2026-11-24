# Go-generated vectors (v8.5.8), domain "pointget" (block D12.1 part 1, VS1-12)

`<target>/pointget.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/pointget.jsonl.gz` (branch `govec/pointget-vs12`, tracker#89) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261116), 5850 records |
| `arm64/pointget.jsonl.gz` sha256 | `4ddd4ce0251af460706e04cb7099c2b0e13477826630166be664f59e28d92f93` (generated locally) |
| `amd64/pointget.jsonl.gz` sha256 | `417cc44ff0ff649059f0ecd282fb3de1ca686095421e351b927c90d9a84948fb` (GOARCH=amd64, GOAMD64=v1; tracker#89 CI artifact, run 43655916945) |

The replay (`tests/govec_pointget.rs`) also reads the tablecodec domain's vectors
(`crates/tablecodec/tests/vectors`) for the record keys of a point get.

The two targets hold the same records.

## Domain "sortexec" (block D12.5, the sort executors: the in-memory sort and top-n part VS1-36, and the parallel sort, #339)

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/sortexec.jsonl.gz` (branch `claude/issue-339`, generator commit `ffbe2ce`; the first 1164 records are those of tracker#124) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261124), 1301 records: `sort` 582, `topn` 582, `psort` 137 |
| `amd64/sortexec.jsonl.gz` sha256 | `6197aa52e0dd93d2134d0eac265d37987bf2c5217a6b5f89c5f343ba4ddd9f8f` (generated locally on x86_64, GOAMD64=v1) |
| `arm64/sortexec.jsonl.gz` sha256 | `c2d2bbeb69ee55ebcf8008a70a29c2ff8f942c6afb0517e366e89f4076b546a9` (the same generator built for linux/arm64, CGO_ENABLED=0, and run under qemu-user) |

The tracker CI artifacts could not be downloaded from the environment that made this change,
so both files were generated locally with `scripts/govec.sh sortexec`
(`GOVEC_RUN_arm64=/usr/bin/qemu-aarch64-static`), the maintainer's decision in #329; the tracker CI's
`govec.sh --check` regenerates each file on its own architecture and compares it byte for byte
(before this change, both files were also regenerated twice locally with `--check --all` and
came out the same, although `psort` runs several workers).
The two targets hold the same records. `tests/govec_sortexec.rs` replays the sort and top-n
executors (op `sort`: Go's sort in its unparallel test mode; op `psort`: the parallel sort with
the record's worker count) and compares every result cell, the error and the step it came
from. In a parallel sort the order of rows whose sort keys compare equal depends on the
workers' scheduling, so for `psort` both Go's output and the replay's put each run of such rows
in the order of their cells' encoding first (spec D12.5 §7.6).
