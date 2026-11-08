# D01.11 tablecodec / rowcodec：任务

- [x] T1 tracker：读路径划分（169 → 106，63 个 X），`func_blocks.tsv`、`block_edges.tsv`。
- [x] T2 crate `tidb-tablecodec`（rank 6）：tablecodec、rowindexcodec、rowcodec 的 106 个函数。
- [x] T3 D07.0 的后续：`NewCommonHandle`、`CommonHandle.Data`、`IntHandle.Encoded` 改用 `tidb-codec`，去掉 `glue:`；kv 的 govec 记录加入 handle 的 `String` / `Data`，删除 3 个豁免（D07.0 T7）。
- [x] T4 govec `tablecodec` 领域：minelits 字面量（38 个测试函数）、随机行与索引、tracker CI 生成种子 1 到 8；两个平台的向量。
- [x] T5 回放测试（debug、release-checked），种子 1 到 8，反证 8 项；`TestError` 单元测试；CI 加入 `tidb-tablecodec`。
- [x] T6 TiKV 双向对照（key、行 v1 / v2、v2→v1 datum 转换）；解码变异测试。
- [x] T7 benchmark（`benches/tablecodec.rs`，与 Go 侧输入相同），spec §7。
- [ ] T8 TiKV 索引值 / restored data 的对照：TiKV 的实现在 `tidb_query_executors`（没有 vendored）。D07.3 用真实 coprocessor 响应补上，或者届时 vendor 该 crate 的解码部分。
- [ ] T9 性能：`DecodeHandleInIndexValue`（common handle，46.8 ns 对 Go 35.7 ns）、`DecodeToChunk`（3 列、每行 Reset，37.0 ns 对 31.7 ns）、`DecodeIndexKV`（restored，164.1 ns 对 137.4 ns）。x86 上两边都测一次再决定是否处理。
- [ ] T10 用真实集群的数据验证：在 v8.5.8 playground 上建多种类型、collation、索引的表，扫描原始 KV 并解码对照（本块只用了 Go 生成的行）。
