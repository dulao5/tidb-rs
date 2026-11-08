# D01.11 tablecodec / rowcodec — plan and handoff

Branches (both rebased on main after D01.8 merged): tidb-rs `block/D01.11-tablecodec`
(worktree projects/tidb-rs-d0111), tracker `govec/tablecodec-D01.11` (worktree
projects/tracker-d0111, draft PR tracker #25). No tidb-rs PR opened yet.

## Done

- crate `tidb-tablecodec` (rank 6, `planned` removed in layers.toml): `tablecodec.rs`,
  `rowcodec/{common,row,decoder}.rs`, `rowindexcodec.rs`. converge D01.11: 106/106.
- Read-path scoping (tracker rules.py, func_blocks/block_edges regenerated): 169 → 106 functions;
  X = row/index value encoders, rowcodec Encoder + sorters, temp-index value codec,
  IsUntouchedIndexKValue, IndexKVIsUnique, TempIndexKey2IndexKey, DDL/unistore/TiCDC-only
  helpers, no-caller functions (list and reasons in rules.py comment).
- D07.0 follow-up in tidb-kv: `new_common_handle` (codec.CutOne), `CommonHandle.data`
  (codec.DecodeOne), `IntHandle.encoded` via tidb-codec (glue removed); kv govec domain records
  String/Data for every handle; tracker kv exemptions removed; D07.0 converge 104/111 (7
  ResourceGroupTagBuilder exemptions remain).
- govec `tablecodec` domain (tracker): 12,883 records, ops keys / key_decode / encode_value /
  unflatten / row / checksum / index / index_raw / truncate / verify_ranges; minelits literals
  (testdata/tablecodec_literals.tsv, 37 test functions) registered in minedGoTests; CI generates
  seeds 1–8 (artifact). arm64 vectors committed; amd64 pending from tracker CI
  (`targetsPending["tablecodec"]`), kv amd64 vectors also need regenerating from CI.
- Rust replay `tests/govec_tablecodec.rs`: 0 mismatches (debug + release-checked), local seeds
  1–8 replay clean (`GOVEC_TABLECODEC_FILES`, comma-separated).
- Adversarial: 8 breaks each caught (index version 1034, compact bytes 2643, null search 549,
  bit byte size 44, API V2 prefix 26, _bin padding 13, prefix truncation runes 5, hash data flag 4).
- TiKV oracle `tests/tikv_oracle.rs`: keys both ways; 904 v2 rows parsed by TiKV RowSlice to the
  same bytes/NULLs/checksum; 512 rows re-encoded by TiKV's encoder to Go's bytes; 2310 v2→v1
  datum conversions value-equal (1009 byte-different by design: varint vs comparable ints,
  durations); 300 v1 rows cut identically. Known: 39 rows TiKV panics on (checksum section not
  5/9 bytes; Go ignores trailing bytes).
- `tests/decode_mutation.rs`: 220k mutated decodes, all Ok/Err/catchable panic.
- Go benchmark side: tracker `govec/domains/tablecodec_bench_test.go` (inputs printed by
  `GOVEC_PRINT_BENCH=1 go test -run TestTablecodecBenchInputs -v ./domains/`).

## Remaining (next steps, in order)

1. `benches/tablecodec.rs` (currently a stub `fn main`): criterion with the same inputs (hex
   printed by the Go test above), jemalloc like crates/codec/benches; run Go
   (`go test -run x -bench BenchmarkTC -count 5 ./domains/`) and Rust; table in spec §7.
2. amd64 vectors: download the tracker CI artifact (govec-vectors-ubuntu-latest) for
   tablecodec + kv, commit to tracker, empty targetsPending, copy into tidb-rs
   (crates/tablecodec/tests/vectors/v8.5.8/amd64/, crates/kv/tests/vectors/v8.5.8/amd64/),
   update the vectors README sha256s (kv README exists; add one for tablecodec).
3. CI: add `-p tidb-tablecodec` to the three test lines of `.github/workflows/ci.yml`.
4. Port Go unit-test assertions govec cannot express (if any) as Rust unit tests; list in spec §5.
5. specs/D01.11-tablecodec/{spec,tasks}.md (scope §1, relevance §2, design §3, deviations §4,
   upstream §5, acceptance §6, perf §7); ROADMAP D01.11 line; D07.0 spec note (glue removed).
6. Run CI command set (fmt on touched crates, clippy first-party, tests, release-checked,
   check_layers, converge D01.11 + D07.0), open tidb-rs PR (base main), fill tracker #25 body,
   mark both ready.

## Deviations so far (for spec §4)

- `init` returns kv.DecodeTableIDFunc's function (no package initialisers).
- `[]byte` nil vs empty: an empty slice is Go's non-nil empty (KV values are never nil).
- `DecodeHandleInIndexValue` / `DecodeIndexHandle` return `Option<Handle>`; Go's
  `PartitionHandle{nil}` (partition ID without handle) is `None`.
- ChunkDecoder with a nil location uses UTC for handle columns (Go would panic only for a
  non-zero TIMESTAMP handle column).
- appendDatumForChecksum: the recovered panic text after "interface conversion" is Rust's.
- Restored `_bin` padding allocation failure panics instead of Go's fatal out-of-memory.
- `IndexValueSegments` fields are `Option<&[u8]>` (Go nil vs empty matters).

## Upstream-pitfall candidates (evidence in the final report)

1. Corrupted restored `_bin` padding count in an index value → Go builds a string of up to
   2^63 spaces (fatal OOM; a few GiB really allocated). decodeRestoredValuesV5.
2. ChunkDecoder.tryAppendHandleColumn / commit-ts column append int64/uint64 whatever the column
   type → out-of-bounds write into a narrower chunk column (memory corruption), found with
   `-gcflags=all=-d=checkptr`; reachable only with a schema/handle-type mismatch.
3. DecodeRowWithMapNew decodes columns in map order: which column error/panic is reported
   varies between runs.
4. TiKV RowSlice::from_bytes asserts the checksum section is 5|9 bytes (panic); Go ignores
   trailing bytes.
5. DecodeValuesBytesToStrings returns (nil, nil) when ToString fails (wrong error variable).
6. DecodeMetaKey compares the flag as a byte (uint64 truncated).
