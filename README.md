# tidb-rs

Experimental Rust implementation of the TiDB SQL layer, starting as a read-only node that runs
alongside Go TiDB in the same cluster. See `docs/adr/0001-scope-and-baseline.md`.

```
crates/server   MySQL-protocol server binary (placeholder)
crates/parser   SQL parser ported from TiDB's parser.y (placeholder)
```

Checks: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`.
