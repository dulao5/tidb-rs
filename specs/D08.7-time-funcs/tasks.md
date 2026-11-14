# D08.7 时间函数（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.12）`timeZone2int`：时间函数用的时区偏移

## 从 D08.3 继承

- [ ] D08.3-1 （D08.3 第 2 部分）`getSignatureByPB` 中本块签名的 145 个 pb code 是边界桩（`blocked-by`）：移植签名后重跑 `tools/expr-gen/gen_pb_sigs.py`，桩自动消失，govec `pushdown` 的 `pbexpr`/`pbraw` 记录随之逐条比对（D08.3 spec §6）；`ADDDATE`/`SUBDATE` 的 56 个 pb code 调用 `addSubDateFunctionClass.getFunction`（`Ctor::AddSubDate`）需在 `distsql_builtin.rs` 中补上
