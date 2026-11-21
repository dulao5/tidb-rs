# D10.1 物理算子定义（tasks）

本块按 VS1 的条目分部分移植，spec 见 [spec.md](spec.md)。

## 进度

- [x] VS1-19（规划器基础）：`BasePhysicalPlan`、`baseimpl`。
- [x] VS1-27（物理算子、task、物理实现枚举）：第一条纵向切片的八种算子、三种 task、它们的 `Attach2Task`，Selection / Projection / Sort / TopN / Limit 的 `exhaustPhysicalPlans`（D10.3）；govec `physops`。边界桩见 spec §4。
- [ ] 其余算子（join、聚合、窗口、union、MPP、index 读取）和 spec §4 的边界桩，随各自的条目。spec §7 待定问题 1（虚拟生成列的原地展开）在 FindBestTask（VS1-28）时决定。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.12）`SubstituteCorCol2Constant`：读 `CorrelatedColumn.Data`，tidb-rs 中在 `EvalState`（D08.1 spec §3.2）；调用者是物理计划和 index lookup 执行器（D12.1），由它们决定值从哪里来
