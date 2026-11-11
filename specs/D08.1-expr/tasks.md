# D08.1：tasks

- [x] T1 crate 骨架、layers.toml、errors.go、context.go
- [x] T2 collationInfo（D08.2 的 11 个方法）、Column、CorrelatedColumn
- [x] T3 Constant、ParamMarker
- [x] T4 Schema、KeyInfo；expression.go 第 1 部分
- [x] T5 govec `expr` 领域与回放；反证
- [x] T5a 评审 #73 后续：关联列的槽按外层列 `UniqueID` 取值，去掉全局计数器（spec §3.2、§4 第 9 条）；`memory_usage` 不带 state 的理由、`Column.Clone` 共享 `*FieldType` 的移植注意写入 spec §4
- [x] T6a 第 2 部分 2a：N4 §5 门禁（`checked` 形式，通过）、`#[eval_fn]`、两种驱动、RPN、ScalarFunction 规划侧方法、baseBuiltinFunc 字段
  - 验收：`scalar_function.rs` 中不再有临时桩（`part2_stub!` 全部删除，Lucky 评审 #73）——已完成
- [ ] T6b 第 2 部分 2b：EvaluatorSuite、chunk_executor、VecEvalBool；NewFunction*（随 D08.2/D08.8 的第一批签名）
- [ ] T7 第 3 部分：向量函数（随 D08.2）
