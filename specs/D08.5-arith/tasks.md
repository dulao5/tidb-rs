# D08.5 算术、数学与逻辑运算：tasks

- [x] T1 第 1 批：6 个算术函数类、23 个签名；CEIL/CEILING/FLOOR 两个函数类、10 个签名；Go `math.Mod`
- [x] T2 govec `arbuild`、`ceilcmp`、算术 `newfunc`；D08.1 T6c (a)、(c)；D08.4 T6（ceil/floor、YEAR 记录）
- [x] T3 第 2 批：逻辑运算、IS TRUE/FALSE、一元负号（`builtin_op.rs`；AND/OR 的 `RpnNode::RowFallback`）；govec `logic`、`logicfb`、`uminus`
- [x] T3b 常量传播（D08.2 第 2b 部分；govec `cprop` 含 YEAR 列与 DOUBLE / DECIMAL / 字符串常量的 `year` 记录，Lucky 评审 #98）。IS NULL（`isNullFunctionClass` 与 7 个签名）随之移植（`BuildNotNullExpr` 需要），`arbuild` 的 `isnull` 记录覆盖；`builtinStringIsNullSig` 的 `vecEvalInt`/`vectorized` 在 builtin_string_vec.go（D08.6）
- [x] T4 第 3 批：位运算 `& | ^ << >> ~`（5 + 1 个函数类、6 个签名）；govec `bitop` 1,500 条、`bitfb` 400 条（spec §1c、§3.2）
- [ ] T5 第 4 批：其余数学函数（RAND 为第一个带状态的签名：D08.1 T6c (b)）
- [ ] T9 UPDATE 的 SET 赋值（以及其他按行求值的执行器）必须用逐行驱动求值，和 Go 一致：例如严格模式下 `UPDATE bt SET c = a & (b DIV 0)`（a 为 NULL）在 Go 中成功、`INSERT ... SELECT` 报 1365（#83，Lucky 评审 #105）。有 SQL 路径后用 SQL 测试固定
