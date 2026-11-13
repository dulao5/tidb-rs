# D07.1 store driver（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D00.1）`ValidateSnapshotReadTS`：需要 `kv.Storage.GetOracle`（oracle 客户端）
- [ ] P1-2 （D05.2）savepoint：`TransactionContext.AddSavepoint`、`TransactionContext.DeleteSavepoint`、`TransactionContext.GetCurrentSavepoint`、`TransactionContext.ReleaseSavepoint`、`TransactionContext.RestoreBySavepoint`、`TransactionContext.RollbackToSavepoint`：恢复事务 membuffer 的 staging
