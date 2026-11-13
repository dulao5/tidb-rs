# D06.4 table 抽象（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.12）`GetTimeValue`：列默认值的时间值（`table/column.go` `getColDefaultValue`；改写器的 DEFAULT 也用），需要 parser driver 的 `ValueExpr` 和 `EvalSimpleAst`。它调用的 `getTimeCurrentTimeStamp` 已在 `crates/expression/src/helper.rs` 移植
