# D08.12 表达式工具（tasks）

- [x] T1 第 1 批：util.go 中依赖已移植的工具函数（spec §1a），以及 D08.1 的 `FlattenDNF/CNFConditions`
- [x] T2 govec `util` 操作（x / pnot / dnf / const / fmt / ub / elim / dnfdup），两个平台回放 0 条不一致，种子 1–4
- [x] T3 反证：故意弄坏后能报出不一致（PR 描述）
- [ ] T4 第 2 批：`explain.go`
- [ ] T5 第 2 批：`helper.go`（替换 `getStmtTimestamp` 的边界桩）
- [ ] T6 参数标记相关、`SubstituteCorCol2Constant`、`ExecBinaryParam`（plan.md 第 2 批）
- [ ] T7 `intset.FastIntSet`（D00.3）移植后，`ExtractColumnSet` 改用它（spec §4 第 1 条）
