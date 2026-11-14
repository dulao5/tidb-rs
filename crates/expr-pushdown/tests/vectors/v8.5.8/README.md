# Go-generated vectors (v8.5.8), domain "pushdown" (block D08.3 part 1)

`<target>/pushdown.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/pushdown.jsonl.gz` (branch `block/D08.3-pushdown`, PR #71) |
| tracker commit | `e0f94fa` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261114), 7,259 records: `expr` 5,497, `split` 700, `ft` 352, `ftpb` 300, `enabled` 400, `ctx` 10 |
| `arm64/pushdown.jsonl.gz` sha256 | `4aabf93effa13cd8882a0ee09c95c595a3392661c44a8b87f430801368f44623` (GOARCH=arm64, generated locally) |
| `amd64/pushdown.jsonl.gz` sha256 | `a455d1db771fadba062306fcf4b5f2893422859b67e83c7dc6033d822935eadd` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact, run 43533066966) |

There is no floating-point arithmetic in the domain: both targets hold the same records.

Records give a recipe that Go builds with `NewFunctionBase` (so Go picks the real signatures
of every function family, ported or not); the output carries the built tree (function name,
Go signature type, result type, collation, pb code, CAST's `inUnion`), which the replay
rebuilds with descriptor-only signatures. `tipb` outputs are compared as marshalled bytes.
Spec `specs/D08.3-pushdown` §5.
