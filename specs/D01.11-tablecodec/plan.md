# D01.11 tablecodec / rowcodec — plan and handoff

Branches: tidb-rs `block/D01.11-tablecodec` (stacked on `block/D01.8-codec`, tidb-rs #38),
tracker `govec/tablecodec-D01.11` (stacked on `govec/codec-D01.8`, tracker #23). Rebase onto
main after #38 / #23 merge.

## Status

- [x] crate `tidb-tablecodec` (rank 6): `tablecodec.rs`, `rowcodec/{common,row,decoder}.rs`,
      `rowindexcodec.rs` — read path only (relevance analysis in spec §2).
- [x] D07.0 follow-up in tidb-kv: `NewCommonHandle` (codec.CutOne), `CommonHandle.Data`
      (codec.DecodeOne), `IntHandle.Encoded` through tidb-codec (glue removed).
- [ ] tracker: rules.py X entries (spec §2), regenerate func_blocks.tsv / block_edges.tsv;
      kv domain: drop the 3 D01.8 exemptions, record CommonHandle String for every handle.
- [ ] govec `tablecodec` domain (minelits + random rows/keys/index KVs + edge cases).
- [ ] Rust replay (tests/govec_tablecodec.rs), TiKV oracle (tests/tikv_oracle.rs), decode
      mutation test, benches vs Go.
- [ ] spec.md / tasks.md, CI (tidb-tablecodec in the test jobs), converge, PRs (draft).
