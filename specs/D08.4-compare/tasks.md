# D08.4 比较函数：tasks

- [x] T1 第 1 批：`compareFunctionClass`（refineArgs 全部分支）、56 个签名、`CompareXxx`、`GetAccurateCmpType`、`GetCmpFunction`；`eval::Args` 增加 time/duration/json/vector_float32
- [x] T2 govec `cmpbuild` 与 `newfunc`（比较函数名）记录；D08.1 T6c 的 (a)–(d) 见 spec §3.4
- [x] T3 `function_class_registry` 测试改为按实例检查（7 个 `compareFunctionClass` 实例对应一个 `impl`）
- [ ] T4 第 2 批：COALESCE
- [ ] T5 第 3 批：GREATEST/LEAST、INTERVAL、`ResolveType4Between`
- [ ] T6 D08.5 之后：去掉 `cmpCeilPath` 过滤
