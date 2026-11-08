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
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/sessionvars.jsonl.gz` (branch `govec/sessionvars-D05.2`) |
| generator | `govec` (seed 5052), 1 record so far (op `fresh`, T2) |
| `arm64/sessionvars.jsonl.gz` sha256 | `bf38030980bf09be2e39b337e58daa64f5162faaa52b3c08ce27158df7f47d83` (generated locally) |
| `amd64/sessionvars.jsonl.gz` sha256 | `4dfb4a7022f75ba6b46be57971820fc4a008f4419c74fad85b3b7f28ab0abd4b` (tracker CI artifact) |
