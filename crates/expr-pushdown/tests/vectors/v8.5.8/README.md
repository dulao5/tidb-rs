# Go-generated vectors (v8.5.8), domain "pushdown" (block D08.3)

`<target>/pushdown.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/pushdown.jsonl.gz` (part 1: branch `block/D08.3-pushdown`, PR #71; part 2: branch `block/D08.3-pbtoexpr`, PR #74) |
| tracker commit | `87c629f` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261114), 10,366 records: part 1 `expr` 5,497, `split` 700, `ft` 352, `ftpb` 300, `enabled` 400, `ctx` 10; part 2 (appended) `pbtps` 1, `pbexpr` 2,960, `pbraw` 146 |
| `arm64/pushdown.jsonl.gz` sha256 | `4ad7c613acbd90be9f486b608ab1ad50b5b7e70a4fa29bd3e90c56aa00aa48fb` (GOARCH=arm64, generated locally) |
| `amd64/pushdown.jsonl.gz` sha256 | `df4435018e1f4376a364e4ff00ec2399bb46c7b52e9b9ca7bed7b92e4da39e48` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact, run 43539529858) |

There is no floating-point arithmetic in the domain: both targets hold the same records.

Records give a recipe that Go builds with `NewFunctionBase` (so Go picks the real signatures
of every function family, ported or not); the output carries the built tree (function name,
Go signature type, result type, collation, pb code, CAST's `inUnion`), which the replay
rebuilds with descriptor-only signatures. `tipb` outputs are compared as marshalled bytes.
Spec `specs/D08.3-pushdown` §5.

Part 2 records (`pbexpr`, `pbraw`) carry the marshalled `tipb.Expr` and the expression Go's
`PBToExpr` built from it; signatures of families tidb-rs has not ported are boundary stubs, and
the replay checks the stub names the signature Go built (spec §6.3).

# Domain "aggdesc" (block D08.11 part B: aggregate descriptors to and from protobuf)

`<target>/aggdesc.jsonl.gz` are the same files as `crates/expression/tests/vectors/v8.5.8/<target>/aggdesc.jsonl.gz`
(source, tracker commit `9d637b2`, record counts, sha256 and how each target was generated are in
that README). This crate replays the part B records (`tests/govec_aggdesc_pb.rs`): `push` 387
(`CheckAggPushDown` / `CheckAggPushFlash`, with the push-down blacklist), `topb` 428
(`AggFuncToPBExpr` for TiKV, TiFlash, TiDB and an unspecified store), `mode2pb` 5, `pb2mode` 7,
`frompb` 143 (`PBExprToAggFuncDesc`, including edited messages: unknown type, no mode, mode 7,
no field type, a bad child), `tipb` 74. `tipb` outputs are compared as marshalled bytes.

| | |
|---|---|
| `amd64/aggdesc.jsonl.gz` sha256 | `d2178ddda504714fe9f3485d2da75056668eb08c8dacdbb78d1b454b00448bc9` (generated natively on x86_64) |
| `arm64/aggdesc.jsonl.gz` sha256 | `536bfe1a878a47c0eaeccedf3abe0f969c6746417d4729464e51001f9c7a0cc9` (generated under qemu-user) |
