# D01.7 Chunk：任务

- [x] T1 新 crate `tidb-chunk`（rank 3），`layers.toml` 去掉 planned，workspace 成员与依赖。
- [x] T2 `goslice.rs`：Go 切片的 len/cap、`growslice` 与 size class、共享缓冲写时复制。
- [x] T3 column / chunk / row / codec / chunk_util / iterator / list / compare / alloc 的可移植函数（276 个）。
- [x] T4 tracker：复用池内部归 X（`rules.py`、`func_blocks.tsv`）。
- [x] T5 govec `chunk` 领域：脚本式 op、20 个 Go 测试场景（`minedGoTests["chunk"]`）、1,600 个随机脚本、`type_width`。
- [x] T6 回放测试（debug、release-checked），反证 6 项。
- [x] T7 单元测试（分配器、记账、写时复制、别名断言）；CI 加入 `tidb-chunk`。
- [ ] T8 D01.1 合并后：`AppendEnum`、`AppendSet`、`GetEnum`、`GetSet`（Column、Chunk、Row）、`AppendDatum`、`GetDatum*`、`DatumWithBuffer`、`ToString`（Chunk、Row）、`Compare`、`LowerBound`、`UpperBound`、`mutrow.go` 全部；`binary_literal_compare` 换成 D01.1 的 `BinaryLiteral::compare`。govec 去掉对应豁免。
- [ ] T9 性能：AppendRow、Access、DecodeToChunk、ListGetRow、ChunkMemoryUsage 追平 Go（plan §3）。
- [ ] T10 chunk 在算子之间移交时改挂记账账户（N5a 3.7 第 2 条），随 D00.1 / D12 接线。
- [ ] T11 D01.10：`diskFileReaderWriter`、`iterator4RowContainer`。
- [ ] T12 用 TiKV 实际返回的 chunk 响应做向量（N5a §5"线上格式"中的真实数据部分），随 D07.3 的 distsql 解码一起做。
- [ ] T13 D01.1 合并后改名：临时的 Enum/Set 接口 `Chunk::append_name_value`、`Row::get_name_value_pub`、`Column::append_name_value_for_test` 换成 `append_enum`/`append_set`/`get_enum`/`get_set`（`types.Enum`/`Set`）。这三个是 `glue:` 标注，converge 不把它们算作已匹配的 Go 函数；对应的 Go 函数仍在豁免表中（"waits D01.1"）。
