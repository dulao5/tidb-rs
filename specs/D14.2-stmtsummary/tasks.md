# D14.2 slow log、stmt summary、Top SQL（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D00.1）SQL / plan digest：`StatementContext.SQLDigest`、`StatementContext.InitSQLDigest`、`StatementContext.ResetSQLDigest`、`StatementContext.GetPlanDigest`、`StatementContext.SetPlanDigest`、`StatementContext.GetResourceGroupTagger`。需要 `pkg/parser/digester.go`（`parser.Normalize`、`parser.Digest`；PoC parser 没有）
- [ ] P1-2 （D07.0）`ResourceGroupTagBuilder.EncodeTagWithKey`：Top SQL 的 resource group tag，需要 digest
