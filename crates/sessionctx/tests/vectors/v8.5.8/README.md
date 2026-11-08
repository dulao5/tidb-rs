# Go-generated vectors (v8.5.8), domain "sysvar" (block D05.1)

`<target>/sysvar.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/sysvar.jsonl.gz` (branch `govec/sysvar-D05.1`) |
| tracker commit | `3739ab9` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 5051), 65811 records |
| `arm64/sysvar.jsonl.gz` sha256 | `c4551f48cf647e9d51976854e5c197dae59e9bd50df3ba706bab19c68254904f` (GOARCH=arm64, generated locally) |

There is no floating-point arithmetic that gc fuses here; both targets are expected to hold the
same records (the amd64 file comes from the tracker CI artifact). Records with
`err.name == "panic"` must panic in Rust. `version_compile_os` / `version_compile_machine`
are recorded as `<GOOS>` / `<GOARCH>`: the replay checks the Rust registry's values against
its own target.
