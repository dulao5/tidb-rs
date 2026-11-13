# D10.1 物理算子定义（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.12）`SubstituteCorCol2Constant`：读 `CorrelatedColumn.Data`，tidb-rs 中在 `EvalState`（D08.1 spec §3.2）；调用者是物理计划和 index lookup 执行器（D12.1），由它们决定值从哪里来
