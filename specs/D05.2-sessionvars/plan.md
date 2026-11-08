# D05.2 计划与交接

## 状态（2026-11-08）

- [x] 范围划分：276 → 237（tracker `rules.py`，分支 `govec/sessionvars-D05.2`）。
- [x] spec（本目录 `spec.md`）。
- [x] T2（标量部分）：`rsgen -sessionvars` 从 Go 的 `NewSessionVars(nil)` 反射生成 `variable/session_fields.rs`：282 个标量字段（bool、整数、浮点、字符串、`time.Duration` 及以它们为底层类型的命名类型；嵌入的 `Concurrency`、`MemQuota`、`BatchSize` 等展开），带 Go 的注释和默认值。写路径的 11 个标量字段不生成（与 `rules.py` 的 X 一致，列表在生成器里）。`SessionVars` 替身通过 `Deref` 暴露这些字段，原来替身里重复的 5 个字段已删掉。
  - govec `sessionvars` 领域，op `fresh`：Go 侧反射导出 282 个字段（浮点记 IEEE-754 位），Rust 侧 `scalar_strings()` 对比，0 条不一致。反证：把 `foreign_key_checks` 的默认值改成 false，回放报 1 条不一致。
  - 其余 67 个非标量字段（map、指针、atomic、StmtCtx、TxnCtx、UserVars……）列在 `session_fields.rs` 末尾，待 T3/T6 手写。
  - tracker 中 D05.2 尚未覆盖的 234 个函数登记为 pending 豁免（`govec/domains/sessionvars_pending.go`），随 T3–T7 逐步删减。
- [ ] T3 起尚未开始。`session.rs` 仍是替身（加上了生成的标量字段）。

分支：tidb-rs `block/D05.2-sessionvars`、tracker `govec/sessionvars-D05.2`，都叠在 D05.1 评审后续（#55 / tracker #26）上；它们合并后 rebase 到 main。

## 实现顺序

1. **SessionVars 结构与 `NewSessionVars`**：296 个导出字段按 Go 的名字和默认值（`NewSessionVars` 207 行）。字段很多，建议用 rsgen 从 Go 的 `SessionVars` 结构（反射 + AST）生成字段声明和默认值，手写方法。归 X 的字段（§1.3）不生成。
2. **系统变量存取**：`globals` 快照（桩：默认值 + 实例覆盖）、`delta`、`GetSystemVar`、`GetSessionOrGlobalSystemVar`、`SetSystemVar*`、`*FromHook`、`SetSystemVarWithOldValAsRet`。替换 `session.rs` 替身，D05.1 的校验代码不变。
3. **SetSession / GetSession 钩子**（273 + 27）：按 `sysvar_hooks.tsv` 逐行，从 `sysvar.go` 的闭包手工移植到 `hooks.rs` 旁边的新文件 `session_hooks.rs`；同时给 tsv 加处置列。
4. **SetGlobal / GetGlobal / GetStateValue 钩子**（155 + 134 + 6）：同上；调用点留给 D06.5。
5. `UserVars`、`TransactionContext`（保留部分）、`TxnReadTS`、`Concurrency`、prepared 语句、plan cache 参数、游标、session token。
6. `slow_log.go`。
7. govec `sessionvars` 领域、minelits、多种子、amd64 向量、反证；钩子清单的单元测试。
8. 编译期断言（`Send` / `!Sync`）。

## 下一步（交接）

1. **T3**：在 `session.rs` 中加入 `globals: Arc<GlobalVarsSnapshot>`（桩：注册表默认值 + `SetSysVar` 的实例覆盖）和 `delta`，移植 `GetSystemVar`、`GetSessionOrGlobalSystemVar`、`SetSystemVar*`、`*FromHook`、`SetSystemVarWithOldValAsRet`；`systems` map 换成 delta + 快照。SET GLOBAL：校验后报"不支持"（owner issue #58，按 (a) 推进）。
2. **T4**：按 `data/sysvar_hooks.tsv` 移植 SetSession / GetSession 钩子到 `session_hooks.rs`，tsv 加处置列。govec 加 op `set`：SET 后导出 `fresh` 同样的标量字段，加上 `GetSystemVar`、warning。
3. **T9 注意**：替身里的 `global_vars_accessor: Box<dyn GlobalVarAccessor>` 不是 `Send`，加 `SessionVars: Send` 断言前要改成 `Box<dyn GlobalVarAccessor + Send>` 或换成 N8 的快照。
4. 每完成一组函数，从 `sessionvars_pending.go` 删掉对应名字。
5. **Lucky 对 #57 的三点**（已写入 spec §2.2、§3 和 tasks T10–T12）：INSTANCE 作用域的 SET GLOBAL 照 Go 只改本实例；读 `mysql.global_variables` 提前到 P1，作为同系列中单独的 PR（等 D01.11 合并）；回放中按两种顺序初始化变量，结果必须相同。

## 需要决定的事

- **N8 §6 待定 1（SET GLOBAL）**：已开 owner issue tidb-rs #58，按建议 (a) 推进，不等决定。
