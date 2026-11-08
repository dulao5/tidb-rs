# Go-generated vectors (v8.5.8), domain "sysvar" (block D05.1)

`<target>/sysvar.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker`, `vectors/v8.5.8/<target>/sysvar.jsonl.gz` (branch `govec/sysvar-D05.1`) |
| tracker commit | `63da069` |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 5051), 65811 records |
| `arm64/sysvar.jsonl.gz` sha256 | `0363b82242ab5a3e1edd7a46c381430e3ac32875762256cd8a07babcc7163c1c` (GOARCH=arm64, generated locally) |
| `amd64/sysvar.jsonl.gz` sha256 | `cd2f3d5d330e17f049dad355b10e7f428af03fffc19a32d778495591e806eb18` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |

There is no floating-point arithmetic that gc fuses here: both targets hold the same records
(the amd64 file comes from the tracker CI artifact). The inputs never depend on the host
(the defaults of `version_compile_os` / `_machine` are replaced by a fixed value). Records with
`err.name == "panic"` must panic in Rust. `version_compile_os` / `version_compile_machine`
are recorded as `<GOOS>` / `<GOARCH>`: the replay checks the Rust registry's values against
its own target.
