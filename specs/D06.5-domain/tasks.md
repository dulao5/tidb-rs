# D06.5 schema 同步与 domain（tasks）

状态截至 2026-11-15。spec 见 spec.md。

| # | 任务 | 完成条件 | 状态 |
|---|---|---|---|
| T1 | 建立 crate `tidb-domain`（N0 rank 12） | `check_layers.py` 通过 | 完成 |
| T2 | 第 1 部分（VS1-17，schema 加载）：`loadInfoSchema`、`tryLoadSchemaDiffs`、`fetch*`、`Reload`（时间戳由调用方给出） | converge 15/16 + 1 个桩；`load_info_schema` 测试 | 完成（评审中） |
| T3 | PoC domain 驱动：同步循环在同一时间戳上 reload；新会话按命令取最新版本 | 路由器测试 | 完成（评审中） |
| T4 | 第 2 部分（VS1-17b）：schema validator / checker 按 L1 移植 | converge VS1-17b | 未开始 |
| T5 | 第 2 部分：lease 用于准入、reload 循环、升级检查 | N11 D3–D5、D7 确认 | 等 N11 |
| T6 | `getTimestampForSchemaVersionWithNonEmptyDiff`：`store/helper` 的 MVCC 接口 | 随 D07.1 | 未开始 |
| T7 | 混合集群：v8.5.8 playground 上用 Go 执行 DDL，新栈增量应用 diff，与 PoC / Go 的结果对照 | — | 未开始 |

下面是从其他块移交来的条目。

## 从 D07.2 继承

- [ ] D07.2-1 （D07.2 第 1 部分）`buildTiDBMemCopTasks`（`crates/store/src/copr/coprocessor.rs`）是边界桩（`blocked-by:D06.5`）：发往 `kv.TiDB` 的 coprocessor 请求（集群各 TiDB 的内存表）需要 `infosync.GetAllServerInfo` 列出 TiDB 实例，移植后按 Go 生成每个实例一个任务，去掉 `tidb_store_type_is_a_boundary_stub` 测试，tracker govec `copr` 的豁免改为由记录覆盖（D07.2 spec §4）
