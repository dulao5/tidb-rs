# D07.0 kv：任务

- [x] T1 PoC crate 改名为 `poc-*`（#36，N0 §5）。
- [x] T2 tracker：`pkg/kv` 读路径划分（35 个 X、7 个 DEF.MPP），`func_blocks.tsv`、`block_edges.tsv`。
- [x] T3 `gostd::runtime`（`growslice`、`roundupsize`）移到 `tidb-base`，`tidb-chunk` 改用它。
- [x] T4 `tidb-kv`（rank 5）：107 个函数。
- [x] T5 govec `kv` 领域：minelits 字面量、随机用例、mock 驱动的 `RunInNewTxn` / cache DB / `NextUntil` / `GetValue`。
- [x] T6 回放测试（debug、release-checked），种子 1 到 8，反证 8 项；`tests/unit.rs`（`TestError`）；CI 加入 `tidb-kv`。
- [ ] T7 D01.8 合并后：`NewCommonHandle`（`codec.CutOne`）、`CommonHandle.Data`（`codec.DecodeOne`），`codec.EncodeInt` 换成 D01.8 的实现；govec 去掉对应豁免，`handleOut` 记录 CommonHandle 的 `Data` / `String`。
- [ ] T8 `ResourceGroupTagBuilder`：需要 `parser.Digest`（D02.2 digester）、`resourcegrouptag`（D01.11 rowindexcodec）、`tikvrpc.Request`（D07.1）。随 D07.2 的 coprocessor 请求标签一起做，`Request.ResourceGroupTagger` 同时补上。
- [ ] T9 `Request` / `ClientSendOption` 中尚未移植类型的字段（spec §4 第 4 条），随 D07.1、D07.2 补上。
- [ ] T10 PoC 的 `poc-kv` 逐步换成本 crate 的接口：store driver（D07.1）实现 `Storage` / `Snapshot` / `Transaction`，PoC 的 distsql 和 session 改用它们。本块不改动 PoC 的接线。
