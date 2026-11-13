# D13.1 Plan Cache（例外 A3）（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.2 T8e）参数标记：`ConstExprConsiderPlanCache`、`MaybeOverOptimized4PlanCache` + `SetSkipPlanCache` 在常量传播中的路径（跳过 plan cache 是正确性保护）；需要 `ParamMarker` 常量与 plan cache 上下文后用 govec 覆盖
- [ ] P1-2 （D00.1 T12，#68 评审）`exprstatic` 的 `with_param_list` 现在深拷贝参数列表，Go 共享底层的字节；`PlanCacheParamList` 改为 `Arc<[Datum]>` 共享
