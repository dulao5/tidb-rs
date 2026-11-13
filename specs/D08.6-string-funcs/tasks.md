# D08.6 字符串函数（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.2 T9）`builtin_func_param.go`（REGEXP_* / ILIKE 的参数辅助）：`funcParam.setStrVal`、`funcParam.setIntVal`、`funcParam.setCol`、`funcParam.getCol`、`funcParam.getStringVal`、`funcParam.getIntVal`、`buildStringParam`、`buildIntParam`
- [ ] P1-2 （D08.12）`locateStringWithCollation`：LOCATE / INSTR 的按 collation 查找
- [ ] P1-3 （D08.2 T8d）govec 配方覆盖 `ColumnSubstituteImpl` 的 #53730 `LENGTH` 分支（替换进 `LENGTH(col)` 的参数时检查 collation/类型），LENGTH 移植后加
- [ ] P1-4 （D08.1 T6c (b)）第一个带状态的签名（LIKE / REGEXP 的 `builtinFuncCache`）按 D08.1 spec §3.5 对照 Go 的 `Clone`；`builtinFuncCache.getCache` / `getOrInitCache` 随这些函数族进入 govec 覆盖清单
