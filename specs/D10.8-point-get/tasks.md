# D10.8 点查快速路径（tasks）

spec：[spec.md](spec.md)。状态截至 2026-11-15：第 1 部分（VS1-11）评审中。

## 第 1 部分：TryFastPlan 与 PointGetPlan（VS1-11）

函数清单是 `specs/VS1-vertical-slice/functions.tsv` 中 VS1-11 的 55 个函数（D10.8 52 个，D09.0 2 个，D00.3 1 个），converge D10.8 46/52 + 6 个桩，D09.0 2/2，D00.3 1/1（spec §1.1）。

- [x] T1 `point_get_plan.go`：`PointGetPlan` 及其方法、`TryFastPlan`、`tryPointGetPlan`、`checkTblIndexForPointPlan`、`indexIsAvailableByHints`、名字和值的提取、`buildSchemaFromFields`、权限输入
- [x] T2 `util.go`：`EncodeUniqueIndexKey`、`EncodeUniqueIndexValuesForKey`；`consistency.Reporter.ReportLookupInconsistent`
- [x] T3 执行器改用 planner 的 `PointGetPlan`，删除 `glue::PointGetPlan` 和三个桩（唯一索引 key、不一致错误）
- [x] T4 govec `fastplan` 领域与回放（`fast`、`uidx`，后者经点查执行器）
- [x] T5 amd64 向量（tracker#92 CI artifact，run 43667682707；两个目标的记录相同）
- [ ] T6 `Optimize` 中 `tidb_isolation_read_engines` 的检查（随 VS1-24 的 `Optimize`，spec §3）

## 块内其余（`blocked-by:D10.8`）

- `BatchPointGetPlan`（`tryWhereIn2BatchPointGet`、`newBatchPointGetPlan` 及方法）：VS1 不做
- 点更新 / 点删除（`tryUpdatePointPlan`、`tryDeletePointPlan` 等）：写路径，`blocked-by:X`
