# Go-generated vectors (v8.5.8), domain "sysvar" (block D05.1)

`<target>/sysvar.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/sysvar.jsonl.gz` (main after tracker #26) |
| tracker commit | `0e3dd47` (tracker #26) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 5051), 67521 records (incl. 1,710 `sys/whitespace`) |
| `arm64/sysvar.jsonl.gz` sha256 | `a94a24fdce53b35fc4bbfc064d680fc24d831b560f6e393e56b03deedf2f71e7` (GOARCH=arm64, generated locally, go-fix pingcap/tidb#62892, tracker #38) |
| `amd64/sysvar.jsonl.gz` sha256 | `06fa9a4c1d925b2fea4222f4597c37e6aaf4be1c80f112dbfd28b539fe2a1f77` (tracker CI artifact, run 42956793646, go-fix pingcap/tidb#62892) |

There is no floating-point arithmetic that gc fuses here: both targets hold the same records
(the amd64 file comes from the tracker CI artifact). The inputs never depend on the host
(the defaults of `version_compile_os` / `_machine` are replaced by a fixed value). Records with
`err.name == "panic"` must panic in Rust. `version_compile_os` / `version_compile_machine`
are recorded as `<GOOS>` / `<GOARCH>`: the replay checks the Rust registry's values against
its own target.

## Domain "sessionvars" (block D05.2)

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/sessionvars.jsonl.gz` (branch `govec/sessionvars-D05.2-d`, tracker #43) |
| generator | `govec` (seed 5052, with upstream fix pingcap/tidb#65884: header `"fixes"`; errors recorded as `errors.Cause`, the client's view; the accessor holds an in-memory mysql.tidb), 67,583 records: after the 1,208 `tidb_table` (`sys/tidb_table`, PR C: the tidb_gc_* hooks), the 1,095 PR D records (`methods`, `uservars`, `paramlist`, `svmisc`); both targets hold the same records |
| `arm64/sessionvars.jsonl.gz` sha256 | `d8d16f2ce7e948cb33b46cea9f7955a777e5b6fad9f2090599414fe184ed668a` (GOARCH=arm64, generated locally, D05.2 PR D, tracker #43) |
| `amd64/sessionvars.jsonl.gz` sha256 | `4597f686997d4259a7994d0f4a950c4d4a511d167aba20e319439f7043a4736c` (tracker CI artifact, run 42981268960, D05.2 PR D) |

## Domain "stmtctx" (block D00.1 part 1)

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/stmtctx.jsonl.gz` (branch `govec/stmtctx-D00.1`, tracker #30; exec details appended on branch `govec/ctx-D00.1-execdetails`, tracker #41) |
| generator | `govec` (seed 20261109), 1,345 records: `script` 815 (203 exec-details scripts appended after the first 1,142 records), `sqlwarn_marshal` 144, `sqlwarn_unmarshal` 125, `push_down` 257, `resolve` 4; both targets hold the same records (no floating point) |
| `arm64/stmtctx.jsonl.gz` sha256 | `8b03b7e0b5326d1a92cc0fc8b07ed4fdb464d0f4191c25b1380ec09684241977` (generated locally) |
| `amd64/stmtctx.jsonl.gz` sha256 | `bcb777c5897cdc32208f66281d3dcb87d90ea820a6af49827b3ecde43c93936c` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |

## Domains "ranger" and "exprctx" (block D00.1 part 2a)

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/{ranger,exprctx}.jsonl.gz` (branch `govec/ctx-D00.1-2`, tracker #32, commit `92205cf`) |
| generator | `govec` ranger (seed 20261110): 3,839 records; exprctx (seed 20261111): 2,242 records. Every record runs with new collations enabled; exprctx `load` restores `errors.RedactLogEnabled` (#62). Both targets hold the same records |
| `arm64/ranger.jsonl.gz` sha256 | `9ddec0b9ba9e1681f52bc8ba899113050176cc8bc70748202af310a1deccc7d8` (generated locally) |
| `amd64/ranger.jsonl.gz` sha256 | `591a0af500291e203cd334031361dc9e25c5706d0bb3dc355820e157be9a39e6` (tracker CI artifact) |
| `arm64/exprctx.jsonl.gz` sha256 | `9614dfa62107e9fec7a0eae6b716287dd013c54f6d3f3a31a018e83d6cb40453` (GOARCH=arm64, generated locally, go-fix pingcap/tidb#62892, tracker #38) |
| `amd64/exprctx.jsonl.gz` sha256 | `0fcf31e5022fd90a03ece81f0e2579ab9ab7caf86907fd092279ae7a8fd6142e` (tracker CI artifact, run 42956793646, go-fix pingcap/tidb#62892) |

Extra seeds (tracker CI artifact `govec-extra-seeds-*`): replay with `GOVEC_RANGER_FILES` / `GOVEC_EXPRCTX_FILES` (comma-separated).

## Domain "ctx2b" (block D00.1 part 2b)

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/ctx2b.jsonl.gz` (branch `govec/ctx-D00.1-2b`, tracker #36) |
| generator | `govec` ctx2b (seed 20261109): 731 records (`plancounter`, `basicctx`, `statsload`, `tracker`, `expropt`, `debugtrace`). Floats travel as strings; every record runs with new collations enabled and restores `errors.RedactLogEnabled` (#62). Both targets hold the same records |
| `arm64/ctx2b.jsonl.gz` sha256 | `34206de17bc234f79d75a3aa5b4c30d356756a1b99de90a3112b2f2cc46277ed` (generated locally) |
| `amd64/ctx2b.jsonl.gz` sha256 | `fd61b32cfa0999820fc8993e149ab427e4912abe26e2e86a12283d2732cc0c60` (tracker CI artifact) |
| replay | `tests/govec_ctx2b.rs`; `GOVEC_CTX2B_FILES` replays the extra-seed artifacts |
