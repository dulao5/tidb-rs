# D08.3：plan

## 第 1 部分（本 PR）

1. 新 crate `crates/expr-pushdown`（`tidb-expr-pushdown`），`layers.toml` 去掉 `planned`（rank 12）。
2. `tools/expr-gen/gen_tipb_sigs.py`：从 TiDB v8.5.8 所用的 tipb Go 模块生成 `src/tipb_sig_gen.rs`（`ScalarFuncSig` 常量与名字，spec §3.1）；`src/tipb_sig.rs` 是 `ScalarFuncSig.String()`。
3. `src/expr_to_pb.rs`：`expr_to_pb.go` 全文件，按 Go 顺序。
4. `src/infer_pushdown.rs`：`infer_pushdown.go` 全文件，按 Go 顺序（`init` 在文件末尾）。
5. govec `pushdown` 领域（tracker 分支 `block/D08.3-pushdown`）；Rust 回放 `tests/govec_pushdown.rs`，单元测试 `tests/pb_code_unit.rs`；CI 的测试列表加上本 crate。

回放：`cargo test -p tidb-expr-pushdown --test govec_pushdown`；额外的种子：`GOVEC_PUSHDOWN_FILES=a.jsonl.gz,b.jsonl.gz`。

### 反向引用（N0 §4）

`--block-edges` 中指向 D08.3 的引用：

| 方向 | 引用数 | 内容 | 处理 |
|---|--:|---|---|
| D08.11 → D08.3 | 17 | `aggregation/agg_to_pb.go`（`AggFuncToPBExpr`、`baseFuncDesc.GetTiPBExpr` 等）调用 `ExpressionsToPBList`、`PbConverter`、`ToPBFieldType` | N0 规则 6：这些函数移植时**上移**到本 crate，写成自由函数（D08.11 尚未移植它们） |
| D08.2 → D08.3 | 1 | `GroupingSet.ToPB`（已移交 D09.4） | 同上，随 D09.4 |

本块引用下层的 D08.4–D08.10 只在第 2 部分（`getSignatureByPB` 构造各函数族的签名）。

## 第 2 部分（本 PR）

1. `tools/expr-gen/gen_pb_sigs.py`：从 `distsql_builtin.go` 生成 `src/distsql_builtin_gen.rs`（每个 pb code 的 Go 签名类型、字面量、所属块；tidb-expression 的全部 `SigDesc`），spec §6.1。
2. `src/distsql_builtin.rs`：`distsql_builtin.go` 全文件，按 Go 顺序；未移植函数族的 pb code 为边界桩。
3. govec `pushdown` 追加 `pbtps`、`pbexpr`、`pbraw`（tracker 分支 `block/D08.3-pbtoexpr`）；回放在 `tests/govec_pushdown.rs`。
4. D08.6、D08.7、D08.9 的 tasks.md 登记桩的消失条件；tracker `blocks_done.tsv` 登记 D08.3。
