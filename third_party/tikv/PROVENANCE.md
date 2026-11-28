# Vendored TiKV query crates

The crates in this directory are copies of TiKV's TiDB-query components. They are not
submodules. tidb-rs builds them on stable Rust.

## Source

- Repository: https://github.com/dulao5/tikv, the project's fork of https://github.com/tikv/tikv
- Branch: `rust-tidb/v8.5.8-stable-rpn-fn`
- Commit: `3b39795daa91b8dadaffea680fa5a99d8a55038c`
- Base: TiKV `v8.5.8` (tag `v8.5.8`, `a1fdd6a59bb9c8a282c15677e1894501a52425cf`), per owner decision on #530 (2026-11-28)

The branch replays the stable-Rust build commits, the two panic fixes, the patched-binary release workflow, and the Stacked Borrows soundness fixes onto `v8.5.8`:

| Commit | Replayed from | Change |
|---|---|---|
| `6664709f7` | `c3d4301a3` | Remove specialization from `#[rpn_fn]` codegen and from aggregate dispatch. |
| `fa9ad9753` | `63ca6e13c` | Build the query crates without nightly features. Libtest benches move behind a `nightly` cargo feature. Adapted to `v8.5.8`: `tidb_query_datatype/src/lib.rs` had no `bool_to_result`, and `tidb_query_datatype/src/codec/mysql/time/mod.rs` keeps `v8.5.8`'s `chrono` APIs while removing `unlikely` and `Boolinator::ok_or`. |
| `6bc1bc5dc` | `e2b1e88e5` | Make TiKV-only dependencies optional:<br>• `error_code` `raft`<br>• `tidb_query_common` `coprocessor` (without master's `tracker` dependency)<br>• `tidb_query_datatype` `coprocessor` |
| `ae9d12f9a` | `abf6df2f4` | `tidb_query_datatype` declares `chrono`'s `clock` feature. |
| `449b5d2fe` | `c36f47e93` | Cherry-pick of upstream master `19249e66b` (TiKV PR `20206`): `Unicode0900::char_weight` guards with `>=` instead of `>`, plus upstream's regression test (#524). |
| `a87863eb7` | `7614b0cbb` | `IntIntMod::calc` uses `wrapping_rem`, so `i64::MIN % -1` gives 0 (#523). Not fixed upstream. |
| `8cc700315` | — | Add `.github/workflows/release-patched-tikv.yml` to build and publish patched `tikv-server` binaries for test clusters (#531). |
| `5461083e5` | `63dc697eb` | Cherry-pick of upstream master `63dc697eb` (TiKV PR `19939`): derive both pointers from a single `as_mut_ptr()` in `MemComparableByteCodec::try_decode_first_in_place` / `_desc`, plus Miri soundness test (#553). |
| `3b39795da` | `57f09a343` | Cherry-pick of upstream master `57f09a343` (TiKV PR `19945`): derive both pointers from a single `as_mut_ptr()` in `MemComparableByteCodec::encode_all_in_place`, plus Miri soundness test (#553). |

TiKV turns on `nightly`, `raft` and `coprocessor` through its `[workspace.dependencies]`, so its
build behaves as before. tidb-rs leaves all three features off.

Other branches in the fork (kept as reference only, **not** vendored here):

- `rust-tidb/stable-rpn-fn-78aedc1-fixes` (`7614b0cbb`) and `rust-tidb/stable-rpn-fn-pre-sync-abf6df2f4`
  (`abf6df2f4`, duplicated by `backup/rust-tidb-stable-rpn-fn-pre-rebase`) hold the prior master
  `78aedc1c8` fork snapshots.
- `rust-tidb/stable-rpn-fn` was rebased onto upstream master `2c38416a1` on 2026-11-28 and carries
  the #523 commit there as a master reference image (#530).

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

## Local patches (where the fork differs from TiKV v8.5.8)

In addition to the stable-Rust build commits above, the fork carries three local fixes on top of TiKV `v8.5.8` (two panic fixes and one Stacked Borrows memory-safety fix):

| Issue | File, function | Change | Why |
|---|---|---|---|
| #523 | `tidb_query_expr/src/impl_arithmetic.rs`, `IntIntMod::calc` | `lhs % rhs` becomes `lhs.wrapping_rem(*rhs)` | `i64::MIN % -1` panicked ("attempt to calculate the remainder with overflow"). Go TiDB v8.5.8 (`a % b` in `builtinArithmeticModIntSignedSignedSig`) and MySQL give 0. |
| #524 | `tidb_query_datatype/src/codec/collation/collator/utf8mb4_uca/data_0900.rs`, `Unicode0900::char_weight` | the guard `r > UNICODE_CI_TABLE.len()` becomes `r >= …` | U+2CEA1, the first code point past the 0x2CEA1-entry table, indexed past its end ("index out of bounds: the len is 183969"). This is the same off-by-one that upstream Go fixed after v8.5.8 (`pingcap/tidb#67149`, ruling R3 in `docs/testing/rulings.toml`, #294). With `>=` TiKV gives the implicit weight that tidb-rs's collator gives. |
| #553 | `codec/src/byte.rs`, `MemComparableByteCodec::{encode_all_in_place, try_decode_first_in_place, try_decode_first_in_place_desc}` | derive both `src` and `dest` raw pointers from one `as_mut_ptr()` call (cherry-picked from upstream master `63dc697eb` / TiKV PR `19939` and `57f09a343` / TiKV PR `19945`) | Calling `as_ptr()` then `as_mut_ptr()` on the same slice invalidates the `SharedReadOnly` provenance tag under Stacked Borrows before `std::ptr::copy` reads through it (UB under Miri, no observable behavior change). |

The project owner, zhigang-du, decided on 2026-11-27 to fix #523 and #524 in the TiKV fork, open an issue
in this repository recording that upstream TiKV is affected, and leave reporting it to TiKV to the
owner. On 2026-11-28 the maintainer decided to carry the fixes as fork commits on the vendored base,
taking upstream's own fix for #524, and on 2026-11-28 (#553) to re-apply upstream master's Stacked
Borrows fixes in `codec/src/byte.rs` for memory safety.

Upstream status, from reading the source on 2026-11-28 (#523, #524, #553):

- TiKV v8.5.8 (tag `v8.5.8`, `a1fdd6a59`, now the vendored base here) and `release-8.5` (`3f4712b`) have all three bugs.
- Upstream master has #524's fix (`19249e66b`) and #553's fixes (`63dc697eb`, `57f09a343`), but not a fix for #523.

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
- **#553 reaches the `codec` crate** (a normal dependency of `tidb-meta` and `tidb_query_datatype`).
  No first-party crate currently calls the in-place memcomparable encode/decode methods directly,
  but fixing them removes library-level Stacked Borrows UB from safe public APIs in the vendored crate.
- **Patched cluster TiKV (#531).** Stock TiKV `v8.5.8` still panics on #523/#524; test clusters using the patched TiKV release built from this fork branch (`#531`) receive all three fixes.

A re-sync from a TiKV commit without the fixes would drop them.
`crates/query-smoke/tests/tikv_local_patches.rs` and the coprocessor evaluation oracle
(`crates/query-smoke/tests/copr_eval_oracle.rs`, `FORK_DIVERGENCES`) fail if #523 or #524 is
lost; `codec`'s `miri_soundness_encode_all_in_place` and `miri_soundness_try_decode_first_in_place`
tests verify #553 under Miri.

## Moving from master `78aedc1c8` to `v8.5.8` (#530)

Moving the vendored base from upstream master `78aedc1c8` to tag `v8.5.8` (`a1fdd6a59`)
changes `33` files (`+86 / -3745`) across `third_party/tikv` and the verbatim `tikv_util` shim
files. In `Cargo.lock`, `tidb_query_expr` switches from `uuid 1.7.0` to `uuid 0.8.2` (`uuid
1.28.0` stays for first-party crates) and drops `tracker`, `chrono-tz`, and `rand 0.8.8` from the
vendored query crates; `tipb` (`5f9928e9`), `kvproto` (`623e58e6`), `raft-rs` (`ad13f3d9`), and
`grpcio 0.10.4` are unchanged. `third_party/tikv-shims/tikv_util/src/log.rs` drops the
master-only `info_or_error!` and `warn_or_debug!` macros.

Where behavior changed vs `78aedc1c8`:

- **Production paths (`poc-executor`, `tidb-distsql`, `tidb-meta`) have no behavioral change.**
  - `crates/poc/executor/src/agg.rs` uses `tidb_query_datatype::codec::mysql::{Decimal, Res, RoundMode}` (`decimal.rs` changed only a clippy loop condition).
  - `crates/poc/executor/src/compare.rs` uses `tidb_query_datatype::{Collation, codec::collation::{Collator, collator::*}}`, unchanged once #524 is applied.
  - `crates/distsql/src/{decode,dag,ranges}.rs` uses `codec::table::{encode_row_key, decode_col_value, decode_row}`, `codec::row::v2::{RowSlice, V1CompatibleEncoder, CODEC_VERSION}`, `codec::datum::{Datum, encode_value, split_datum, decode}`, `mysql::Decimal`, and `expr::EvalContext`. None of those paths changed (`EvalConfig` drops only unused `max_keys_read` from TiKV PR `19518`). `Time::to_packed_u64` in `v8.5.8` lacks TiKV PR `19288` and `parse_from_float_string` lacks TiKV PR `19532`, but `tidb-distsql/src/decode.rs` only calls `from_packed_u64` and `tidb-distsql/src/dag.rs::origin_default_datum` returns `Error::Unsupported` for date/time columns before encoding.
  - `crates/meta/src/keys.rs` uses `codec::prelude::{BufferReader, MemComparableByteDecoder, MemComparableByteEncoder, NumberEncoder}`. At `v8.5.8` `codec/src/byte.rs` initially dropped master's Miri pointer-provenance fixes and a naive bench fix; `#553` re-applied the two pointer-provenance fixes (`5461083e5`, `3b39795da`), leaving only the naive bench fix absent.
- **Coprocessor evaluation oracle (`crates/query-smoke/tests/copr_eval_oracle.rs`).**
  - TiKV `v8.5.8` lacks TiKV PR `17935` (`ToInt for f64` using `round_ties_even()`) and TiKV PR `18009` (`round_real` / `round_with_frac_real` using `round_ties_even()` and an overflow guard). At `v8.5.8`, TiKV rounds half away from zero (`f64::round()`), whereas Go v8.5.8 uses `math.RoundToEven`: `RoundReal` now mismatches (6 rows, recorded under `#498`; `RoundWithFracReal` has the same `.round()` logic by code inspection) and signed `CastRealAsInt` now also joins the `#498` mismatches (`CastRealAsInt` value mismatches increase to 2522 rows).
  - Master-only functions and error codes absent in `v8.5.8` (`ExprType::SumInt` in `tidb_query_aggr/src/impl_sum_int.rs`, `FromUnixTime*`, `StrToDate*`, `UnixTimestamp*`, `TimestampDiff`, `UuidVersion`, `UuidTimestamp`, v1 `Uuid`, `ErrorCode::coprocessor::DEFAULT_NOT_FOUND`, and `coprocessor`-gated `Storage::scanned_bytes` / `RangesScanner::peek_*`) are neither in `tikv_pushable_sigs.tsv` nor used by `tidb-rs`.

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
  | `log_wrappers` | Hex `Value` and `hex_encode_upper` with a log-only redaction provider hook (`set_redact_provider`, wired to `tidb_redact_log` at server startup). `slog::Value` redacts `ON` as `?` and `MARKER` as `‹HEX›`, while `Display`/`Debug` and `hex_encode_upper` remain raw so client-visible error texts and SQL `HEX()` are unchanged. |
  | `crypto` | `rand::rand_bytes` / `rand_u64` on `rand::rngs::OsRng`. TiKV uses OpenSSL RAND here. |
  | `file_system` | `calc_crc32_bytes` only. |
  | `api_version` | `KvFormat::parse_keyspace` and `ApiV1` only. Needed only for dependency resolution and the `coprocessor` features. |
  | `tracker` | An empty placeholder. Kept as a workspace member; `tidb_query_common` on `v8.5.8` no longer depends on it. |

## Not vendored: `tidb_query_executors`

On the branch the crate no longer needs nightly; `fa9ad9753` (replayed from `63ca6e13c`) removed its last `#![feature]`s. It is
still left out here, because it is coupled to TiKV beyond what shims can cover:

- It depends on `txn_types`, the real `api_version` (which pulls in `engine_traits`), `kvproto`
  and `fail`.
- It uses much more of `tikv_util`: `Deadline`, the quota limiter, memory tracing and metrics.
- It implements the async `BatchExecutor` trait over TiKV's `Storage`.

Reusing its operators needs the executor-trait split described in the query-core spike report.

## Tests

On stable 1.99.1 in tidb-rs at `3b39795da`:

| Crate | Tests passing |
|---|---|
| codec | 44 (+1 ignored) |
| error_code | 1 |
| tidb_query_common | 2 |
| tidb_query_codegen | 18 (+6 ignored doctests) |
| tidb_query_datatype | 272 (+7 ignored doctests) |
| tidb_query_expr | 394 (+4 ignored doctests) |
| tidb_query_aggr | 35 |

Compared with the prior master `78aedc1c8` snapshot (`7614b0cbb`), `v8.5.8` + `#553` omits only
master-only tests (`3` date/time helper tests in `tidb_query_datatype`, `8` tests in
`tidb_query_expr` for `from_unixtime`/`timestampdiff`/`str_to_date`/`uuid` v1, and `5` `SumInt`
tests in `tidb_query_aggr`); the `2` Miri pointer-provenance tests in `codec`
(`miri_soundness_encode_all_in_place` and `miri_soundness_try_decode_first_in_place`) are back.

### Miri verification (`codec`, #553)

`tidb-rs` CI builds on stable Rust only (`ci.yml`), whereas Miri requires a nightly toolchain with
the `miri` component, so Miri is not added to `ci.yml`. Local verification on `nightly-2026-01-30`
(`miri 0.1.0`) passes under both Stacked Borrows and Tree Borrows:

```sh
# Stacked Borrows (both soundness regression tests)
cargo +nightly-2026-01-30 miri test --locked -p codec -- miri_soundness_
# Tree Borrows (both soundness regression tests)
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly-2026-01-30 miri test --locked -p codec -- miri_soundness_
```

On TiKV's nightly, the branch also enables libtest benches (`nightly` feature) and
`coprocessor`-gated tests (`tidb_query_common` storage tests, `tidb_query_datatype`
`check_table_ranges`); those nightly counts were not re-measured on the `v8.5.8` TiKV checkout.

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
