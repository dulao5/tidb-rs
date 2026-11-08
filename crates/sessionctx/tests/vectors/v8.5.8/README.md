# Go-generated vectors (v8.5.8), domain "sysvar" (block D05.1)

`<target>/sysvar.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/sysvar.jsonl.gz` (main after tracker #26) |
| tracker commit | `0e3dd47` (tracker #26) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 5051), 67521 records (incl. 1,710 `sys/whitespace`) |
| `arm64/sysvar.jsonl.gz` sha256 | `bf01de69b35dd96f31b0c9bb243ed9d05375cb7e184a86c6ea3e58a2f17a1eb0` (GOARCH=arm64, generated locally, B3) |
| `amd64/sysvar.jsonl.gz` sha256 | `a1ce4806581dd43ff6b57f93ea089db99c655356afe68513d1946f8061317e7e` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact, B3) |

There is no floating-point arithmetic that gc fuses here: both targets hold the same records
(the amd64 file comes from the tracker CI artifact). The inputs never depend on the host
(the defaults of `version_compile_os` / `_machine` are replaced by a fixed value). Records with
`err.name == "panic"` must panic in Rust. `version_compile_os` / `version_compile_machine`
are recorded as `<GOOS>` / `<GOARCH>`: the replay checks the Rust registry's values against
its own target.

## Domain "sessionvars" (block D05.2)

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/sessionvars.jsonl.gz` (branch `govec/sessionvars-D05.2-b3`, tracker #33) |
| generator | `govec` (seed 5052, with upstream fix pingcap/tidb#65884: header `"fixes"`; errors recorded as `errors.Cause`, the client's view), 43,623 records: `fresh` 1, `set` 31,017 (12 `src: "fix pingcap/tidb#65884"`, 68 `sys/slow_log_rules`), `get` 945, `set_global` 9,482, `set_global_hook` 2,098 (26 `sys/cloud_storage_uri`, 34 `sys/slow_log_rules`), `init` 80; both targets hold the same records |
| `arm64/sessionvars.jsonl.gz` sha256 | `5bac1150e3eaad2618b460d892c85921d3c338ba19d7b34f4b5f325b1bdd611b` (generated locally, B3) |
| `amd64/sessionvars.jsonl.gz` sha256 | `27523fcad476dd6aa49658f0064abfc6c84535dc707af35db77f61d17c598874` (tracker CI artifact, B3) |

## Domain "stmtctx" (block D00.1 part 1)

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/stmtctx.jsonl.gz` (branch `govec/stmtctx-D00.1`, tracker #30) |
| generator | `govec` (seed 20261109), 1,142 records: `script` 612, `sqlwarn_marshal` 144, `sqlwarn_unmarshal` 125, `push_down` 257, `resolve` 4; both targets hold the same records (no floating point) |
| `arm64/stmtctx.jsonl.gz` sha256 | `ac631f28a2f43fa941e3ba197cd7925eb5fa4063b0220855263bc9bbb3fce0c2` (generated locally) |
| `amd64/stmtctx.jsonl.gz` sha256 | `6c5a0d081ea867faf8490f7ff35936dbc048de0e1a6eff0d62e7c38f379944a7` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |
