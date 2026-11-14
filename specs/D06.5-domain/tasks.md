# D06.5 schema 同步与 domain（tasks）

本块尚未开工，只有下面从其他块移交来的条目；开工时并入正式的任务列表。

## 从 D07.2 继承

- [ ] D07.2-1 （D07.2 第 1 部分）`buildTiDBMemCopTasks`（`crates/store/src/copr/coprocessor.rs`）是边界桩（`blocked-by:D06.5`）：发往 `kv.TiDB` 的 coprocessor 请求（集群各 TiDB 的内存表）需要 `infosync.GetAllServerInfo` 列出 TiDB 实例，移植后按 Go 生成每个实例一个任务，去掉 `tidb_store_type_is_a_boundary_stub` 测试，tracker govec `copr` 的豁免改为由记录覆盖（D07.2 spec §4）
