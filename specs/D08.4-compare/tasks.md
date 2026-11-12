# D08.4 比较函数：tasks

- [x] T1 第 1 批：`compareFunctionClass`（refineArgs 全部分支）、56 个签名、`CompareXxx`、`GetAccurateCmpType`、`GetCmpFunction`；`eval::Args` 增加 time/duration/json/vector_float32
- [x] T2 govec `cmpbuild` 与 `newfunc`（比较函数名）记录；D08.1 T6c 的 (a)–(d) 见 spec §3.4
- [x] T3 `function_class_registry` 测试改为按实例检查（7 个 `compareFunctionClass` 实例对应一个 `impl`）
- [ ] T4 第 2 批：COALESCE
- [ ] T5 第 3 批：GREATEST/LEAST、INTERVAL、`ResolveType4Between`
- [x] T6 D08.5（D08.5 第 1 批，#100）：ceil/floor 已移植，门禁已解除。D08.4 的原有记录**保留** `cmpReachesCeil` 过滤（保持逐字节不变），这类比较由 D08.5 的 `ceilcmp` 记录覆盖（YEAR 列对 DOUBLE、短 DECIMAL、长 DECIMAL、字符串常量；spec §4 第 2 条，#99）。Lucky 在 #100 评审中关掉过滤、把 D08.4 的循环放大 10 倍（种子 224，`cmpbuild` 45,084 条）回放 0 条不一致（#98 时同样的实验有 368 条）
