# Vendored TiKV query crates

The crates in this directory are copies of TiKV's TiDB-query components. They are not
submodules. tidb-rs builds them on stable Rust.

## Source

- Repository: https://github.com/dulao5/tikv, the project's fork of https://github.com/tikv/tikv
- Branch: `rust-tidb/stable-rpn-fn-78aedc1-fixes`
- Commit: `7614b0cbbfb08a6b6ff04fb2efb08ae9d0d1f2b7`
- Base: TiKV master `78aedc1c8` (TiKV PR `19765`)

The branch adds these commits on top of master:

| Commit | Change |
|---|---|
| `c3d4301a3` | Remove specialization from `#[rpn_fn]` codegen and from aggregate dispatch. |
| `63ca6e13c` | Build the query crates without nightly features. libtest benches move behind a `nightly` cargo feature. |
| `e2b1e88e5` | Make TiKV-only dependencies optional:<br>• error_code `raft`<br>• tidb_query_common `coprocessor`<br>• tidb_query_datatype `coprocessor` |
| `abf6df2f4` | tidb_query_datatype declares chrono's `clock` feature. |
| `c36f47e93` | Cherry-pick of upstream master `19249e66b` (TiKV PR `20206`): `Unicode0900::char_weight` guards with `>=` instead of `>`, plus upstream's regression test (#524). |
| `7614b0cbb` | `IntIntMod::calc` uses `wrapping_rem`, so `i64::MIN % -1` gives 0 (#523). Not fixed upstream. |

TiKV turns on `nightly`, `raft` and `coprocessor` through its `[workspace.dependencies]`, so its
build behaves exactly as before. tidb-rs leaves all three features off.

Other branches in the fork:

- `rust-tidb/stable-rpn-fn-pre-sync-abf6df2f4` keeps `abf6df2f4`, the commit vendored here before
  the two fixes.
- `rust-tidb/stable-rpn-fn` was rebased onto upstream master `2c38416a1` on 2026-11-28 and carries
  the #523 commit there. It is **not** what is vendored here. Moving the vendored crates to that
  newer upstream brings in other upstream changes (short-circuit evaluation, Unicode 17 case
  tables), so it is a separate change.

## Crates

Each crate below is byte-identical to `components/<crate>` at the commit above. There are no
local edits: make any change to these crates on the TiKV branch, then re-sync.

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

## Fixes for two upstream TiKV panics

The last two commits of the branch fix two panics in upstream TiKV's coprocessor evaluation:

| Issue | File, function | Change | Why |
|---|---|---|---|
| #523 | `tidb_query_expr/src/impl_arithmetic.rs`, `IntIntMod::calc` | `lhs % rhs` becomes `lhs.wrapping_rem(*rhs)` | `i64::MIN % -1` panicked ("attempt to calculate the remainder with overflow"). Go TiDB v8.5.8 (`a % b` in `builtinArithmeticModIntSignedSignedSig`) and MySQL give 0. |
| #524 | `tidb_query_datatype/src/codec/collation/collator/utf8mb4_uca/data_0900.rs`, `Unicode0900::char_weight` | the guard `r > UNICODE_CI_TABLE.len()` becomes `r >= …` | U+2CEA1, the first code point past the 0x2CEA1-entry table, indexed past its end ("index out of bounds: the len is 183969"). This is the same off-by-one that upstream Go fixed after v8.5.8 (`pingcap/tidb#67149`, ruling R3 in `docs/testing/rulings.toml`, #294). With `>=` TiKV gives the implicit weight that tidb-rs's collator gives. |

The project owner, zhigang-du, decided on 2026-11-27 to fix both in the TiKV fork, open an issue
in this repository recording that upstream TiKV is affected, and leave reporting it to TiKV to the
owner. On 2026-11-28 the maintainer decided to carry the fixes as fork commits on the vendored base,
taking upstream's own fix for #524.

Upstream status, from reading the source on 2026-11-28 (#523, #524):

- TiKV v8.5.8 (tag `v8.5.8`, `a1fdd6a59`) and `release-8.5` (`3f4712b`) have both bugs.
- Upstream master has #524's fix (`19249e66b`), but not a fix for #523.

Where the fixes take effect:

- **#524 reaches the `tidb-server` binary.** `tidb_query_datatype` is a normal dependency of
  `poc-executor` (and of `tidb-distsql`, which uses only its datum and row codecs).
  `crates/poc/executor/src/compare.rs` compares and sorts strings with its collators (ORDER BY,
  GROUP BY, joins). So a PoC statement that compares or sorts U+2CEA1 under
  `utf8mb4_0900_ai_ci` gets the implicit weight instead of panicking. This is the result of
  tidb-rs's own collator and of upstream Go's fix (ruling R3); Go v8.5.8 itself returns
  error 1105 there (#294).
- **#523 is test-only.** `tidb_query_expr` is only a dev-dependency (ADR 0007 point 3, checked by
  `scripts/check_test_only_deps.py`), so the fix changes only the coprocessor evaluation oracle.
- **Neither fix reaches a cluster's TiKV.** A pushed-down expression is evaluated by the
  cluster's own TiKV, which still panics on these inputs at v8.5.8, as under Go TiDB.

A re-sync from a TiKV commit without the fixes would drop them.
`crates/query-smoke/tests/tikv_local_patches.rs` and the coprocessor evaluation oracle
(`crates/query-smoke/tests/copr_eval_oracle.rs`, `FORK_DIVERGENCES`) fail if either fix is
lost.

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
| tidb_query_datatype | 275 |
| tidb_query_expr | 402 |
| tidb_query_aggr | 40 |

On TiKV's nightly the same commit passes more tests. The differences are libtest benches, which
`cargo test` runs once as tests, plus tests of feature-gated code:

| Crate | Nightly | Stable (tidb-rs) | Why the stable count is lower |
|---|---|---|---|
| codec | 92 | 44 | 47 benches + 1 test of a bench-only helper in `byte::benches` |
| tidb_query_common | 11 | 2 | 9 `storage` tests, gated by `coprocessor` |
| tidb_query_datatype | 301 | 275 | 25 benches + 1 `check_table_ranges` test |
| tidb_query_expr | 408 | 402 | 6 benches |

The nightly counts were measured at `abf6df2f4`. `c36f47e93` adds one `tidb_query_datatype` test,
which is included above but was not re-run on nightly.

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
