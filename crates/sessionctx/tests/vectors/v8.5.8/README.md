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
