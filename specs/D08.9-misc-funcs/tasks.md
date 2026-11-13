# D08.9 控制流、信息、杂项函数（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.12）TIDB_DECODE_SQL_DIGESTS 的 digest 文本查询（需要对 statements_summary 的 SQLExecutor；infoschema reader D12.13 也用）：`NewSQLDigestTextRetriever`、`SQLDigestTextRetriever.RetrieveGlobal`、`SQLDigestTextRetriever.RetrieveLocal`、`SQLDigestTextRetriever.runFetchDigestQuery`、`SQLDigestTextRetriever.runMockQuery`、`SQLDigestTextRetriever.updateDigestInfo`
- [ ] P1-2 （D08.12）`PopRowFirstArg`：需要 ROW()
- [ ] P1-3 （D08.1 T6e，Lucky 评审 #108）IF/IFNULL 的第三种 `ArgsFallback`（warning 基准在 `args[0]` 之后，回退时 `args[0]` 的 warning 出现两次）；守护测试：Go 中有 `fallbackEvalXxx` 的 30 个签名，已注册的都必须 `args_fallback != None`；`builtin_control.rs` 的 7 个函数仍归 D08.9，D08.9 第一批要覆盖 IF/IFNULL/CASE 的所有分支后再列入覆盖清单
- [ ] P1-4 （D08.2）`constant_fold.go` 的 `ifFoldHandler`、`ifNullFoldHandler`、`caseWhenHandler` 已移植（`constant_fold.rs`），IF / IFNULL / CASE WHEN 注册后由 govec 覆盖
