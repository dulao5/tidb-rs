# D06.3：tasks

状态截至 2026-11-14。

| # | 任务 | 完成条件 | 状态 |
|---|---|---|---|
| T1 | tracker：`internal/testkit.go`、`updateAutoIDForExchangePartition` 划到 X | `func_blocks.tsv` 270 个 D06.3 函数 | 完成 |
| T2 | spec：范围、v1 不移植（负责人决定）、A3 设计（§5）、N11 钩子（§6） | 负责人确认 §5.6、§6 | 待确认 |
| T3 | `sieve`、`context`、`error`、边界桩（`table`、`autoid`、`placement`） | 单元测试（sieve_test.go）通过 | 完成（PR 1a） |
| T4 | `infoschema_v2`：`Data`、`InfoSchemaV2`（PR 1a）、v2 builder 方法（PR 1b） | govec 回放一致 | 完成 |
| T5 | `builder`、`builder_misc`、`bundle_builder`（PR 1b）、`infoschema`（misc，PR 1a） | govec 回放一致 | 完成 |
| T6 | govec `infoschema` 领域与 Rust 回放（PR 1b），种子 1–8，反证 | 全部一致；豁免表理由合规 | 完成 |
| T7 | amd64 向量：取 tracker PR 的 CI artifact | x86_64 CI 通过 | 见 PR |
| T8 | 第 2 部分：`InfoCache` | converge +16 | 未开始 |
| T9 | 第 2 部分：`InfoSchema` trait、`SessionTables`、`SessionExtendedInfoSchema`、接口辅助函数、`init` | converge +26 | 未开始 |
| T10 | 第 2 部分：混合集群测试、N11 表级检查（随 D06.5） | — | 未开始 |
