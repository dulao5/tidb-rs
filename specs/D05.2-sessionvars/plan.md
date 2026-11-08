# D05.2 计划与交接

## 状态（2026-11-08）

- [x] 范围划分：276 → 237（tracker `rules.py`，分支 `govec/sessionvars-D05.2`）。
- [x] spec（本目录 `spec.md`）。
- [ ] 实现：尚未开始。`crates/sessionctx/src/variable/session.rs` 仍是 D05.1 的替身。

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

## 需要决定的事

- **N8 §6 待定 1（SET GLOBAL）**：负责人尚未决定。spec 按建议 (a) 写。
