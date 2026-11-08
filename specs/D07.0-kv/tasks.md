# D07.0 kv：任务

- [x] T1 PoC crate 改名为 `poc-*`（#36，N0 §5）。
- [x] T2 tracker：`pkg/kv` 读路径划分（40 个 X，含按评审归 X 的 cachedb.go；7 个 DEF.MPP），`func_blocks.tsv`、`block_edges.tsv`。
- [x] T3 `gostd::runtime`（`growslice`、`roundupsize`）移到 `tidb-base`，`tidb-chunk` 改用它。
- [x] T4 `tidb-kv`（rank 5）：102 个函数（cacheDB 按评审归 X）。
- [x] T5 govec `kv` 领域：minelits 字面量、随机用例、mock 驱动的 `RunInNewTxn` / `NextUntil` / `GetValue`。
- [x] T6 回放测试（debug、release-checked），种子 1 到 8，反证 8 项；`tests/unit.rs`（`TestError`）；CI 加入 `tidb-kv`。
- [ ] T7 D01.8 合并后：`NewCommonHandle`（`codec.CutOne`）、`CommonHandle.Data`（`codec.DecodeOne`），`codec.EncodeInt` 换成 D01.8 的实现；govec 去掉对应豁免，`handleOut` 记录 CommonHandle 的 `Data` / `String`。
- [ ] T8 `ResourceGroupTagBuilder`：需要 `parser.Digest`（D02.2 digester）、`resourcegrouptag`（D01.11 rowindexcodec）、`tikvrpc.Request`（D07.1）。随 D07.2 的 coprocessor 请求标签一起做，`Request.ResourceGroupTagger` 同时补上。
- [ ] T9 `Request` / `ClientSendOption` 中尚未移植类型的字段（spec §4 第 3 条），随 D07.1、D07.2 补上。
- [ ] T10 PoC 的 `poc-kv` 逐步换成本 crate 的接口：store driver（D07.1）实现 `Storage` / `Snapshot` / `Transaction`，PoC 的 distsql 和 session 改用它们。本块不改动 PoC 的接线。
- [ ] T11 **`Iterator::next` 的每行开销，在移植扫描的使用方（D07.1）之前定下来**。现在每次 `next` 返回一个装箱 future：一次分配加一次虚表调用。`benches/iter.rs`（arm64 Mac，1024 行的内存批）：异步 `next` 经 `dyn Iterator` 11.5 µs（每行约 11.2 ns），同步循环 1.5 µs（每行约 1.5 ns），每行多约 9.7 ns。memdb、内存中的批都要付这笔开销，而 Go 的 `Iterator.Next` 是普通方法调用。候选方案：把同步的 `next`（在当前批内移动）和异步的 `fill`（取下一批，每批一次分配）分开。按评审意见（Lucky，#40）在 D07.1 之前决定。
