# D09.8 谓词下推等（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.2 T8c）第一条经过 `PropagateConstant` 的 SQL 路径打通后，用 SQL 测试固定 tidb-rs 对 YEAR 原地修改偏离的结果（#104，`docs/compatibility.md`）：表 `yy(y1 YEAR, y2 YEAR)`，行按 #104 的复现；`y1 = y2 AND y2 < 1.0` 只返回 0000（v8.5.8 还返回 1990、2000），`y1 = y2 AND y2 >= 0.5` 与 v8.5.8 相同
