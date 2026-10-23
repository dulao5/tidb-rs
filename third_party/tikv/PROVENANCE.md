# Vendored TiKV query crates

The crates in this directory are copies of TiKV's TiDB-query components. They are not
submodules. tidb-rs builds them on stable Rust.

## Source

- Repository: https://github.com/tikv/tikv
- Branch: `rust-tidb/stable-rpn-fn`. This is a local branch and has not been pushed upstream.
- Commit: `abf6df2f45d5a7b9c7b3c5f02aa12fcdfd21faca`
- Base: TiKV master `78aedc1c8` (#19765)

The branch adds these commits on top of master:

| Commit | Change |
|---|---|
| `c3d4301a3` | Remove specialization from `#[rpn_fn]` codegen and from aggregate dispatch. |
| `63ca6e13c` | Build the query crates without nightly features. libtest benches move behind a `nightly` cargo feature. |
| `e2b1e88e5` | Make TiKV-only dependencies optional:<br>• error_code `raft`<br>• tidb_query_common `coprocessor`<br>• tidb_query_datatype `coprocessor` |
| `abf6df2f4` | tidb_query_datatype declares chrono's `clock` feature. |

TiKV turns on `nightly`, `raft` and `coprocessor` through its `[workspace.dependencies]`, so its
build behaves exactly as before. tidb-rs leaves all three features off.

## Crates

Each crate below is copied unmodified from `components/<crate>` at the commit above.

| Crate | Purpose |
|---|---|
| `codec` | Byte and number codecs |
| `collections` | fx `HashMap` / `HashSet` |
| `error_code` | Error codes |
| `tidb_query_common` | Query errors, execution stats, key-range helpers |
| `tidb_query_codegen` | `#[rpn_fn]` and `AggrFunction` proc macros |
| `tidb_query_datatype` | Field types, datum, row v2, chunk, and table key codecs; MySQL types |
| `tidb_query_expr` | RPN expression builder and evaluator, scalar functions |
| `tidb_query_aggr` | Aggregate functions |
| `panic_hook`, `profiler`, `tipb_helper` | Dev-dependencies of the crates above (test helpers) |

## Local modifications

None. Every crate directory is byte-identical to the upstream tree at the commit above. Make any
change to these crates on the TiKV branch, then re-sync.

Everything that is specific to tidb-rs lives outside this directory:

- **Root `Cargo.toml`.** It defines the `[workspace.dependencies]` entries that the vendored
  manifests inherit with `{ workspace = true }`. External crates use the same specs as TiKV's root
  manifest. It also applies TiKV's `[patch.crates-io]` for `protobuf` / `protobuf-codegen` (the
  PingCAP rust-protobuf v2.8 fork) and `raft` / `raft-proto` (raft-rs master).
- **`Cargo.lock`.** It pins the same git revisions as TiKV's lock:
  - tipb `5f9928e9`
  - kvproto `623e58e6`
  - raft-rs `ad13f3d9`

  It also pins `grpcio 0.10.4`, as TiKV does. tipb accepts `grpcio = "0.*"`, but `grpcio-sys` 0.13
  does not build here.
- **`third_party/tikv-shims/`.** These are stand-ins for TiKV-internal crates that are too
  entangled with TiKV to vendor:

  | Shim | What it contains |
  |---|---|
  | `tikv_util` | A small subset of TiKV's crate. These files are copied verbatim from `components/tikv_util/src` and refreshed by the sync script: `buffer_vec.rs`, `codec/{bytes,number}.rs`, `log.rs`, `macros.rs`. `codec/mod.rs` is upstream minus `stream_event`. `lib.rs` (`escape`, `time`, `deadline`) is hand-written. Its unit tests are disabled, because they need TiKV dev-dependencies and nightly. |
  | `tikv_alloc` | An empty crate. The global allocator is chosen by the binary. |
  | `log_wrappers` | Hex `Value` and `hex_encode_upper` only. There is no redaction switch. |
  | `crypto` | `rand::rand_bytes` / `rand_u64` on `rand::rngs::OsRng`. TiKV uses OpenSSL RAND here. |
  | `file_system` | `calc_crc32_bytes` only. |
  | `api_version` | `KvFormat::parse_keyspace` and `ApiV1` only. Needed only for dependency resolution and the `coprocessor` features. |
  | `tracker` | An empty placeholder. Cargo resolves optional dependencies, so the package must exist. |

## Not vendored: `tidb_query_executors`

On the branch the crate no longer needs nightly; `63ca6e13c` removed its last `#![feature]`s. It is
still left out here, because it is coupled to TiKV beyond what shims can cover:

- It depends on `txn_types`, the real `api_version` (which pulls in `engine_traits`), `kvproto`
  and `fail`.
- It uses much more of `tikv_util`: `Deadline`, the quota limiter, memory tracing and metrics.
- It implements the async `BatchExecutor` trait over TiKV's `Storage`.

Reusing its operators needs the executor-trait split described in the query-core spike report.

## Tests

On stable 1.99.1 in tidb-rs:

| Crate | Tests passing |
|---|---|
| codec | 44 (+1 ignored) |
| error_code | 1 |
| tidb_query_common | 2 |
| tidb_query_codegen | 18 |
| tidb_query_datatype | 274 |
| tidb_query_expr | 402 |
| tidb_query_aggr | 40 |

On TiKV's nightly the same commit passes more tests. The differences are libtest benches, which
`cargo test` runs once as tests, plus tests of feature-gated code:

| Crate | Nightly | Stable (tidb-rs) | Why the stable count is lower |
|---|---|---|---|
| codec | 92 | 44 | 47 benches + 1 test of a bench-only helper in `byte::benches` |
| tidb_query_common | 11 | 2 | 9 `storage` tests, gated by `coprocessor` |
| tidb_query_datatype | 300 | 274 | 25 benches + 1 `check_table_ranges` test |
| tidb_query_expr | 408 | 402 | 6 benches |

## Re-syncing

```sh
tools/sync-tikv-query.sh ~/src/pingcap/tidb-all-source-code/tikv <commit>
```

The script:

1. Replaces each crate directory with `git archive <commit> components/<crate>`. It does not touch
   the TiKV working tree.
2. Refreshes the verbatim `tikv_util` files in the shim.
3. Updates the commit line above.

After running it:

- Update the branch table and test counts by hand.
- Run `cargo test --workspace`.
- If TiKV's lock moved, re-pin tipb / kvproto / raft-rs with `cargo update -p <crate> --precise <rev>`.
