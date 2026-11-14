# D08.3 下推判定与 protobuf 转换（tasks）

## 第 1 部分

- [x] T1 crate `tidb-expr-pushdown`（N0 rank 12）；`gen_tipb_sigs.py` 与 `tipb_sig.rs`
- [x] T2 `expr_to_pb.go` 全文件（13 个函数）
- [x] T3 `infer_pushdown.go` 全文件（21 个函数，含 `init`；TiFlash 表按 L1，spec §2）
- [x] T4 govec `pushdown` 领域与回放（7,259 条，额外种子 1–8）、单元测试、反证（spec §5）

## 第 2 部分

- [x] T5 `distsql_builtin.go`（16 个函数）：`PBToExpr`、`getSignatureByPB`（生成的签名表 `gen_pb_sigs.py`；未移植函数族的 316 个 pb code 为 `blocked-by` 边界桩，登记在 D08.6/D08.7/D08.9 的 tasks.md，spec §6.1）
- [x] T6 govec：`ExprToPB` → `PBToExpr` 往返（`pbexpr`）、手工构造的消息（`pbraw`）；反证（spec §6.3）
- [x] T7 收尾：D08.3 converge 50/50，tracker `blocks_done.tsv`

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [x] P1-1 （D08.1 可选项，Lucky 评审 #102）IS TRUE/FALSE 的 `...WithNull` pb code 不依赖 `+= 20` 的编号规律：直接写进签名表，或加单元测试核对这 6 个编号——单元测试 `tests/pb_code_unit.rs`（spec §5）
