# D08.12 表达式工具（tasks）

- [x] T1 第 1 批：util.go 中依赖已移植的工具函数（spec §1a），以及 D08.1 的 `FlattenDNF/CNFConditions`
- [x] T2 govec `util` 操作（x / pnot / dnf / const / fmt / ub / elim / dnfdup），两个平台回放 0 条不一致，种子 1–4
- [x] T3 反证：故意弄坏后能报出不一致（PR 描述）
- [x] T4（P1 收尾时移交 D10.9：EXPLAIN 输出的使用者）第 2 批：`explain.go`
- [x] T5 第 2 批：`helper.go`（P1 收尾）：`getStmtTimestamp`（替换 CAST 中的边界桩，cast 记录覆盖）、`getTimeCurrentTimeStamp`、`GetTimeCurrentTimestamp`、`boolToInt64`（`helper.rs`，单元测试 `helper_unit.rs`）；`GetTimeValue` 移交 D06.4；`IsValidCurrentTimestampExpr`（只有 DDL）、`pickRandomLocationForTest`（intest）为正式豁免
- [x] T6（P1 收尾时移交）参数标记相关、`SubstituteCorCol2Constant`、`ExecBinaryParam`：`ParamMarkerExpression` 给 D09.2，位置项与 `ParamMarkerInPrepareChecker` 给 D09.4，二进制参数 7 个给 D03.1，`SubstituteCorCol2Constant` 给 D10.1；`SQLDigestTextRetriever` 与 `PopRowFirstArg` 给 D08.9，`locateStringWithCollation` 给 D08.6，`timeZone2int` 给 D08.7
- [x] T7（P1 收尾时移交 D09.13）`intset.FastIntSet`（D00.3）移植后，`ExtractColumnSet` 改用它（spec §4 第 1 条）
