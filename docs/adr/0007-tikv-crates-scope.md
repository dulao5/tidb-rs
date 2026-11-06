# ADR 0007: What tidb-rs takes from TiKV's query crates (supersedes ADR 0001 §4)

Status: proposed. Date: 2026-11-04. Decided by: project owner (pending).

## Context

ADR 0001 §4 said tidb-rs would reuse TiKV's `tidb_query_*` crates for expressions, types and
codecs, so that root-side and pushed-down evaluation share one implementation.

The porting constitution (2026-11-01) makes Go TiDB v8.5.8 the specification. Decision Q3 in
`docs/porting/methodology.md` §9 then split the question: encoding formats are reused from TiKV
(L3), value semantics are ported from Go. The A4 spec (JSON) found at least five behaviour
differences between TiKV's JSON and Go's (float text output, escaping, depth and key-length
checks, comparison, missing functions). Decimal, Time, CAST and collation comparison are likely
to differ in the same way. TiKV's functions are correct for TiKV's job, which is to agree with
TiDB on pushed-down expressions; they are not a specification of TiDB's root-side behaviour, and
the constitution forbids using them as one.

ADR 0001 §4 is therefore superseded. This ADR records which parts of the vendored crates
(`third_party/tikv`, see `PROVENANCE.md`) stay, where they may be used, and what is removed.

## Decision

1. **Kept as L3: byte formats only.** These parts may be used anywhere, because their contract
   is a byte format that Go TiDB, TiKV and the stored data already share:
   - `codec`: byte and number codecs;
   - `tidb_query_datatype::codec`: datum and memcomparable key encoding, table and index key
     codecs, row format v1/v2. Row-format decoding is used only at the coprocessor boundary
     (point get, and responses not in chunk format); coprocessor requests ask for `TypeChunk`
     responses, and nothing on the root side computes on the decoded TiKV types. (The chunk wire format is *not* taken from TiKV: TiKV only
     encodes chunks, and its decoder is test-only and copies. Decoding is ported from Go's
     `util/chunk/codec.go` as L1, see the N5a spec.);
   - collation weight tables (data only; comparison semantics are ported from Go);
   - the binary-JSON reading layer, as defined by the A4 spec.

   Each use carries `// go-l3: ADR-0007 <path>` and needs a byte-level comparison test against
   Go (vectors produced by the `govec` tool, `docs/porting/methodology.md` §5).

2. **Not used for value semantics.** Decimal arithmetic and rounding, Time and Duration,
   type conversion, comparison, collation comparison, `EvalContext` / warnings / `sql_mode`
   handling, all scalar function bodies and all aggregate function bodies are ported from Go
   (blocks D01.2, D01.3, D01.4, D01.9, D08.*, D12.4). The TiKV types for these may still appear
   at the coprocessor boundary while decoding, but are converted to the ported Go types before
   any root-side computation.

3. **`tidb_query_expr`: not used.** The N4 spec (expression evaluation) takes TiKV's RPN design
   as a reference but not its code: the evaluator and builder only work on TiKV's
   `VectorValue` / `LazyBatchColumnVec`, TiKV's `EvalContext` and `tipb::Expr`, and the
   `#[rpn_fn]` macro lacks lazy arguments, access to argument types and expressions, and the
   row-mode fallback on warnings that Go's control functions use. The vendored crate is removed
   together with the PoC `query-smoke` test. Function bodies are not reused in any case (point 2).

4. **`tidb_query_aggr`: removed** once D12.4 (aggregate functions) lands.

5. **PoC call sites** stay until the block that replaces them lands (constitution: PoC code is
   frozen, `glue: poc`). Current sites, with the replacing block:

   | Site | TiKV item | Kept / replaced by |
   |---|---|---|
   | `crates/distsql/src/{decode,ranges}.rs` | row v2, datum, table codecs | kept (L3, point 1) |
   | `crates/distsql/src/{decode,dag}.rs` | `expr::EvalContext`, `mysql::Decimal` | D01.2, D00.1 |
   | `crates/executor/src/agg.rs` | `mysql::Decimal`, `RoundMode` | D01.2, D12.4 |
   | `crates/executor/src/compare.rs` | `Collator` comparison | D01.9 |
   | `crates/query-smoke/tests/smoke.rs` | `RpnExpressionBuilder` (spike test only) | removed with the crate (point 3) |

6. **Branch policy is unchanged.** Changes to the vendored crates are made on the TiKV branch
   `rust-tidb/stable-rpn-fn` and re-synced. Differences between TiKV and Go found while porting
   (the A4 §4 table is the first list) are reported upstream as TiKV issues, because they may
   also be pushdown bugs.

## Consequences

- Root-side and pushed-down evaluation no longer share an implementation. Agreement between
  them is checked the same way Go TiDB checks it today: by comparison tests, not by sharing code.
- The vendored crates shrink over time. A crate is removed from `third_party/tikv` when no
  non-test code uses it.
- ADR 0001 §1 (baseline: master `4cee7f2084`) is likewise superseded by the constitution
  (baseline: v8.5.8).
