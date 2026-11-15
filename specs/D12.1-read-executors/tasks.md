# D12.1 读取类执行器（tasks）

spec：[spec.md](spec.md)。状态截至 2026-11-15：第 1 部分（VS1-12）评审中。

## 第 1 部分：点查执行器（VS1-12）

函数清单是 `specs/VS1-vertical-slice/functions.tsv` 中 VS1-12 / D12.1 的 33 个函数，converge 31/33 + 2 个桩（spec §1.1）。

- [x] T1 `point_get.go`：`buildPointGet`、`PointGetExecutor`、`DecodeRowValToChunk` 及解码辅助函数、`runtimeStatsWithSnapshot`
- [x] T2 `builder.go`：`NewRowDecoder`、快照（`getSnapshot` / `getSnapshotTS`）、临时表 / 缓存表检查、index usage reporter、replica read adjuster、`buildHandleColsForExec`
- [x] T3 `glue::PointGetPlan`（VS1-11 之前的计划承载类型，VS1-11 已换成 planner 的类型）、`SessionContext` 的 `get_expr_ctx` / `get_snapshot_with_stmt_read_ts`、async `build`
- [x] T4 govec `pointget` 领域与回放（`decode_row_val`、`row_checksum`）；`tablecodec` 的 `keys` 记录经执行器回放
- [x] T5 amd64 向量（tracker#89 CI artifact，run 43655916945；两个目标的记录相同）
- [ ] T6 ab `Point_Get`：VS1-11 / VS1-14 接上后（VS1-20）

## 块内其余（`blocked-by:D12.1`）

- BatchPointGet（`batch_point_get.go`）
- TableReader（`table_reader.go` 除 `buildVirtualColumnIndex`，VS1-34）、`newClosestReadAdjuster`（桩，`blocked-by:D07.3`）
- IndexReader、IndexLookUp（`distsql.go`）、IndexMerge、UnionScan、mem reader、TABLESAMPLE
- 点查的加锁路径（`SELECT ... FOR UPDATE`）：写路径，`blocked-by:X`
