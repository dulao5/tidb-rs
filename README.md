# tidb-rs

Experimental Rust implementation of the TiDB SQL layer, starting as a read-only node that runs
alongside Go TiDB in the same cluster. See `docs/adr/0001-scope-and-baseline.md`.

```
crates/server       tidb-server binary: MySQL protocol on 127.0.0.1:4001, wired to a stub
                    session handler until the SQL layer lands (`tidb-server --help`)
crates/mysql-protocol  MySQL wire-protocol server layer (see crates/mysql-protocol/README.md)
crates/parser       SQL parser ported from TiDB's parser.y (see crates/parser/README.md)
crates/query-smoke  smoke tests for the vendored TiKV query crates
crates/kv           snapshot reads over tikv-client (our fork with coprocessor support): TSO,
                    get/batch_get/scan, coprocessor requests, TiDB-like retry budgets
crates/meta         TiDB schema meta (DBInfo/TableInfo JSON) read at a snapshot; InfoSchema and
                    the per-statement schema cache (docs/adr/0003-schema-loading.md)
crates/distsql      record key ranges, tipb DAG building (scan/partial agg/limit), response and
                    row decoding; cluster tests vs Go TiDB (TIDB_RS_PD, TIDB_RS_TIDB_PORT)
third_party/tikv    TiKV's tidb_query_* crates, vendored unmodified (see PROVENANCE.md)
third_party/tikv-shims  stand-ins for the TiKV-internal crates they depend on
tools/parser-port   converters that regenerate the parser's grammar/keyword/test inputs from a
                    TiDB checkout (regen.sh)
tools/sync-tikv-query.sh  re-vendors third_party/tikv from a TiKV commit
tools/protocol-smoke  protocol smoke/perf clients: libmysqlclient (ps_smoke.c) and a sysbench lua
                    script (prepared `SELECT ?` and text `SELECT 1`)
```

Building needs a C++ compiler, CMake, protoc (>= 3.1) and OpenSSL (grpcio-sys and openssl-sys are
pulled in by the vendored crates via tipb).

Checks (first-party crates only for fmt/clippy; vendored code keeps TiKV's style):

```
cargo fmt --check $(tools/first-party.sh)
cargo clippy --no-deps --all-targets $(tools/first-party.sh) -- -D warnings
cargo test --workspace
```
