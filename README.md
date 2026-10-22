# tidb-rs

Experimental Rust implementation of the TiDB SQL layer, starting as a read-only node that runs
alongside Go TiDB in the same cluster. See `docs/adr/0001-scope-and-baseline.md`.

```
crates/server       MySQL-protocol server binary (placeholder)
crates/parser       SQL parser ported from TiDB's parser.y (see crates/parser/README.md)
tools/parser-port   converters that regenerate the parser's grammar/keyword/test inputs from a
                    TiDB checkout (regen.sh)
```

Checks: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`.
