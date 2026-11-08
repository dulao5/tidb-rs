# D05.2 任务

PR 系列（每个可以单独合并，base main）：
- **PR A**（tidb-rs #57、tracker #27）：T0–T3、T4 的一部分、T11、T12，以及钩子框架与清单检查。
- **PR B**：T4 余下部分、T5、T6、T7、T9。
- **PR C**：T10。

- [x] T0 范围划分（tracker rules.py：39 个写路径函数归 X）
- [x] T1 spec / plan
- [~] T2 SessionVars 结构与 NewSessionVars：标量 282 个已生成并用 govec `fresh` 验证；`systems` 的初值（连接字符集和 collation）已加；其余非标量（UserVars、TxnCtx、StmtCtx 等）待 T6
- [x] T3 系统变量存取：`GetSystemVar`、`GetSessionOrGlobalSystemVar`、`GetGlobalSystemVar`、`GetSessionStatesSystemVar`、`SetSystemVar`、`SetSystemVarWithOldValAsRet`、`SetSystemVarWithoutValidation`、`SetSystemVarWithRelaxedValidation`；`SysVar` 的 `Get/SetSessionFromHook`、`Get/SetGlobalFromHook`。全局值经 `GlobalVarAccessor`（D06.5 之前由调用方提供；T10 换成读集群的实现）
- [~] T4 SetSession / GetSession 钩子：SetSession 227/273（173 个按"赋值给标量字段"的形状机械翻译，44 个手工，10 个写的是 X 字段，只改 systems[]），GetSession 12/27；余下 46 + 15 个在 `session_hooks.rs` 的 `PENDING_*` 中，大多写非标量状态（TxnScope、Rng、IsolationReadEngines、StmtCtx、MemTracker、PartitionPruneMode、KVVars 等），随 T6 一起做
- [ ] T5 SetGlobal / GetGlobal 钩子（155 + 134，全部 pending）：大多读写进程级的 atomic 或配置，需要先移植这些全局量；GetStateValue 3/6
- [ ] T6 UserVars、TransactionContext（保留部分）、TxnReadTS、Concurrency、prepared 语句、plan cache 参数、游标、session token
- [ ] T7 slow_log.go
- [~] T8 govec `sessionvars`：op `fresh`、`set`、`get`、`set_global`、`init`，41,447 条；回放跳过仍有 pending 钩子的记录（29,152 条已比对，12,295 条跳过）；反证见 spec §5。minelits 和多种子待 PR B
- [ ] T9 编译期断言 SessionVars: Send + !Sync（`GlobalVarAccessor: Send`，`Box<dyn GlobalVarAccessor>` 已是 Send）
- [ ] T10（同系列单独 PR，依赖 D01.11）启动时和每 30 秒读 `mysql.global_variables`，生成全局值快照并执行 SetGlobal 钩子（D06.5 的读的一半提前）
- [x] T11 回放：govec `init` 把同一组设置按两种顺序生成两条记录，Rust 各自与 Go 逐字节相同
- [x] T12 SET GLOBAL（`SessionVars::set_global_sys_var`）：按 Go 校验；INSTANCE 作用域执行 SetGlobal 钩子后返回（Go 也不持久化）；其余报 ErrNotSupportedYet（#58 方案 (a)，负责人尚未决定）
