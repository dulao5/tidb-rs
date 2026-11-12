# D08.4 比较函数：tasks

- [x] T1 第 1 批：`compareFunctionClass`（refineArgs 全部分支）、56 个签名、`CompareXxx`、`GetAccurateCmpType`、`GetCmpFunction`；`eval::Args` 增加 time/duration/json/vector_float32
- [x] T2 govec `cmpbuild` 与 `newfunc`（比较函数名）记录；D08.1 T6c 的 (a)–(d) 见 spec §3.4
- [x] T3 `function_class_registry` 测试改为按实例检查（7 个 `compareFunctionClass` 实例对应一个 `impl`）
- [x] T4 第 2 批：COALESCE（spec §1b），先完成 D08.1 T6d（`RowFallback` 泛化为 `ArgsFallback`）；govec `coal` 1,500、`coalfb` 600 条
- [x] T5 第 3 批：GREATEST/LEAST、INTERVAL、`ResolveType4Between`（spec §1c）；govec `gl` 2,000、`gltime` 1,200、`glstr` 400、`glfb` 600、`intv` 1,200、`glclone` 400、`between` 600 条；Lucky 评审 #109：`gltnull` 400 条（Time 向量版在 NULL 行上的转换错误）
- [x] T6 D08.5（D08.5 第 1 批，#100）：ceil/floor 已移植，门禁已解除。D08.4 的原有记录**保留** `cmpReachesCeil` 过滤（保持逐字节不变），这类比较由 D08.5 的 `ceilcmp` 记录覆盖（YEAR 列对 DOUBLE、短 DECIMAL、长 DECIMAL、字符串常量；spec §4 第 2 条，#99）。Lucky 在 #100 评审中关掉过滤、把 D08.4 的循环放大 10 倍（种子 224，`cmpbuild` 45,084 条）回放 0 条不一致（#98 时同样的实验有 368 条）
- [x] T5c 下次重新生成 cast 向量时加入 Lucky 评审 #108 中的两类记录：`coalesce(a, rand(seed))` 300 条（两种模式结果不同，#83）、`CAST(COALESCE(t0,t3) AS CHAR)` 2,000 条（覆盖 Duration 结果的 fsp）（`coalrand` 300、`coalfsp` 2,000 条，随第 3 批加入）
- [ ] T7 upstream-pitfall 候选：`builtinIntervalIntSig.Clone` 不复制 `hasNullable`（spec §1c.1，v8.5.8 已验证）；#83 候选：GREATEST/LEAST String 的 collation 相等规则
- [ ] T8 以后移植的 `getRetTp` 调用方要用 `get_ret_tp()`（不能直接读 `tp`）：D08.9 `TryPushCastIntoControlFunctionForHybridType`、PB → 表达式 `distsql_builtin.go:1168`（spec §1c）
