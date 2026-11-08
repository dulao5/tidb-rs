# Go-generated vectors (v8.5.8), domain "sysvar" (block D05.1)

`<target>/sysvar.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/sysvar.jsonl.gz` (main after tracker #26) |
| tracker commit | `0e3dd47` (tracker #26) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 5051), 67521 records (incl. 1,710 `sys/whitespace`) |
| `arm64/sysvar.jsonl.gz` sha256 | `fe4876b3098b4e4c09eff75ba0544c47390631da5857cf11b3e92d74601265eb` (GOARCH=arm64, generated locally) |
| `amd64/sysvar.jsonl.gz` sha256 | `99e554a1f051765f6f4516331faf7a34c2db144bd450c47cad10f3deffe9e61a` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |

There is no floating-point arithmetic that gc fuses here: both targets hold the same records
(the amd64 file comes from the tracker CI artifact). The inputs never depend on the host
(the defaults of `version_compile_os` / `_machine` are replaced by a fixed value). Records with
`err.name == "panic"` must panic in Rust. `version_compile_os` / `version_compile_machine`
are recorded as `<GOOS>` / `<GOARCH>`: the replay checks the Rust registry's values against
its own target.

## Domain "sessionvars" (block D05.2)

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/sessionvars.jsonl.gz` (branch `govec/sessionvars-D05.2-b`, PR B) |
| generator | `govec` (seed 5052, with upstream fix pingcap/tidb#65884: header `"fixes"`; errors recorded as `errors.Cause`, the client's view), 43,498 records: `fresh` 1, `set` 30,951 (12 `src: "fix pingcap/tidb#65884"`), `get` 945, `set_global` 9,482, `set_global_hook` 2,039, `init` 80; both targets hold the same records |
| `arm64/sessionvars.jsonl.gz` sha256 | `271ef23bd2e142b91a820e581c96c678dc7b255ffb1dbac242840f747dc806c9` (generated locally) |
| `amd64/sessionvars.jsonl.gz` sha256 | `f4d990791c0dadc49776858238124bd961a2aeb6e2cee34e0c23e5a33d5db4f8` (tracker CI artifact) |

## Domain "stmtctx" (block D00.1 part 1)

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/stmtctx.jsonl.gz` (branch `govec/stmtctx-D00.1`, tracker #30) |
| generator | `govec` (seed 20261109), 1,142 records: `script` 612, `sqlwarn_marshal` 144, `sqlwarn_unmarshal` 125, `push_down` 257, `resolve` 4; both targets hold the same records (no floating point) |
| `arm64/stmtctx.jsonl.gz` sha256 | `ac631f28a2f43fa941e3ba197cd7925eb5fa4063b0220855263bc9bbb3fce0c2` (generated locally) |
| `amd64/stmtctx.jsonl.gz` sha256 | `6c5a0d081ea867faf8490f7ff35936dbc048de0e1a6eff0d62e7c38f379944a7` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |

## Domains "ranger" and "exprctx" (block D00.1 part 2a)

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/{ranger,exprctx}.jsonl.gz` (branch `govec/ctx-D00.1-2`, tracker #32, commit `92205cf`) |
| generator | `govec` ranger (seed 20261110): 3,839 records; exprctx (seed 20261111): 2,242 records. Every record runs with new collations enabled; exprctx `load` restores `errors.RedactLogEnabled` (#62). Both targets hold the same records |
| `arm64/ranger.jsonl.gz` sha256 | `9ddec0b9ba9e1681f52bc8ba899113050176cc8bc70748202af310a1deccc7d8` (generated locally) |
| `amd64/ranger.jsonl.gz` sha256 | `591a0af500291e203cd334031361dc9e25c5706d0bb3dc355820e157be9a39e6` (tracker CI artifact) |
| `arm64/exprctx.jsonl.gz` sha256 | `beaff05204db2cda88b8fc55f0042528121f8c535a9d479e01a9678d77fa72bc` (generated locally) |
| `amd64/exprctx.jsonl.gz` sha256 | `3c595d4f11593d73cdbc113c75fd31b6b67c3ea606d7c08743527cd5f96cd482` (tracker CI artifact) |

Extra seeds (tracker CI artifact `govec-extra-seeds-*`): replay with `GOVEC_RANGER_FILES` / `GOVEC_EXPRCTX_FILES` (comma-separated).
