# tidb-rs

Experimental Rust implementation of the TiDB SQL layer, starting as a read-only node that runs
alongside Go TiDB in the same cluster. See `docs/adr/0001-scope-and-baseline.md`.

```
crates/server       MySQL-protocol server binary (placeholder)
crates/parser       SQL parser ported from TiDB's parser.y (see crates/parser/README.md)
crates/query-smoke  smoke tests for the vendored TiKV query crates
third_party/tikv    TiKV's tidb_query_* crates, vendored unmodified (see PROVENANCE.md)
third_party/tikv-shims  stand-ins for the TiKV-internal crates they depend on
tools/parser-port   converters that regenerate the parser's grammar/keyword/test inputs from a
                    TiDB checkout (regen.sh)
tools/sync-tikv-query.sh  re-vendors third_party/tikv from a TiKV commit
```

Building needs a C++ compiler, CMake, protoc (>= 3.1) and OpenSSL (grpcio-sys and openssl-sys are
pulled in by the vendored crates via tipb).

Checks (first-party crates only for fmt/clippy; vendored code keeps TiKV's style):

```
cargo fmt --check $(tools/first-party.sh)
cargo clippy --no-deps --all-targets $(tools/first-party.sh) -- -D warnings
cargo test --workspace
```
