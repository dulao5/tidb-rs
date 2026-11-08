# D05.2 任务

PR 系列（每个可以单独合并，base main）：
- **PR A**（tidb-rs #57、tracker #27）：T0–T3、T4 的一部分、T11、T12，以及钩子框架与清单检查。
- **PR B**：T4 余下部分、T5、T6、T7、T9。
- **PR C**：T10 + T13（§6 的设计评审通过后）。

- [x] T0 范围划分（tracker rules.py：39 个写路径函数归 X）
- [x] T1 spec / plan
- [~] T2 SessionVars 结构与 NewSessionVars：标量 282 个已生成并用 govec `fresh` 验证；`systems` 的初值（连接字符集和 collation）已加；其余非标量（UserVars、TxnCtx、StmtCtx 等）待 T6
- [x] T3 系统变量存取：`GetSystemVar`、`GetSessionOrGlobalSystemVar`、`GetGlobalSystemVar`、`GetSessionStatesSystemVar`、`SetSystemVar`、`SetSystemVarWithOldValAsRet`、`SetSystemVarWithoutValidation`、`SetSystemVarWithRelaxedValidation`；`SysVar` 的 `Get/SetSessionFromHook`、`Get/SetGlobalFromHook`。全局值经 `GlobalVarAccessor`（D06.5 之前由调用方提供；T10 换成读集群的实现）
- [x] T4 SetSession / GetSession / GetStateValue 钩子（PR B）：SetSession 272/273（余下 tidb_slow_log_rules，随 T7），GetSession 27/27，GetStateValue 6/6。PR A 时为 SetSession 227/273（173 个按"赋值给标量字段"的形状机械翻译，44 个手工，10 个写的是 X 字段，只改 systems[]），GetSession 12/27；余下 46 + 15 个在 `session_hooks.rs` 的 `PENDING_*` 中，大多写非标量状态（TxnScope、Rng、IsolationReadEngines、StmtCtx、MemTracker、PartitionPruneMode、KVVars 等），随 T6 一起做
- [~] T5 SetGlobal / GetGlobal 钩子（PR B 部分完成）：SetGlobal 39/155、GetGlobal 64/134，只读写 variable 包 atomic 的按单句模式翻译（`session_hooks_global.rs`）；tidb_vars.go 的进程级 atomic 全部移植（`tidb_vars.rs`，`GoAtomicString`）。余下 116 + 70 个落点在其他包（config.Instance、ldap、gctuner、memory、tikvstore、topsqlstate、tls、stmtsummary、logutil、PD 客户端、DDL owner……），见 plan.md「PR B 交接」。原文：（155 + 134，全部 pending）：大多读写进程级的 atomic 或配置，需要先移植这些全局量；GetStateValue 3/6
- [~] T6（PR B 部分完成）：钩子用到的非标量状态已移植（`session_state.rs` 与 `SessionState`：status、TxnCtx、KVVars、one-shot 隔离级别、TxnReadTS、IsolationReadEngines、AllowFallbackToTiKV、PartitionPruneMode、TxnScope、Rng、StatsLoadSyncWait、MemTracker 限额、OptimizerFixControl、runtimeFilterTypes、AnalyzeSkipColumnTypes、SlowLogRules、RowEncoder.Enable、LastQueryInfo、LastDDLInfo）。仍未做：UserVars、TransactionContext（保留部分）、TxnReadTS、Concurrency、prepared 语句、plan cache 参数、游标、session token
- [ ] T7 slow_log.go
- [~] T8 govec `sessionvars`：op `fresh`、`set`、`get`、`set_global`、`init`，41,447 条；回放跳过仍有 pending 钩子的记录（36,046 条已比对，5,401 条跳过）；反证见 spec §5。minelits 和多种子待 PR B
- [x] T9 编译期断言（PR B）：`SessionVars: Send` 与 `!Sync`（session.rs 末尾）。原文：编译期断言 SessionVars: Send + !Sync（`GlobalVarAccessor: Send`，`Box<dyn GlobalVarAccessor>` 已是 Send）
- [ ] T10 读集群全局值（启动时 + etcd watch + 30 秒），并入 §6 的设计（#58 → 方案 (c)）
- [ ] T13 SET GLOBAL 持久化写路径（§6.2，新块 D06.7 的提议，设计待 Lucky 评审）
- [x] T11 回放：govec `init` 把同一组设置按两种顺序生成两条记录，Rust 各自与 Go 逐字节相同
- [x] T12 SET GLOBAL 的过渡行为（`SessionVars::set_global_sys_var`）：按 Go 校验；INSTANCE 作用域执行 SetGlobal 钩子后返回（Go 也不持久化）；其余暂报 ErrNotSupportedYet，直到 T13 落地（负责人已选方案 (c)）
