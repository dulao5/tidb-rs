# D05.2 计划与交接

## 状态（2026-11-08）

- [x] 范围划分：276 → 237（tracker `rules.py`，分支 `govec/sessionvars-D05.2`）。
- [x] spec（本目录 `spec.md`）。
- [x] T2（标量部分）：`rsgen -sessionvars` 从 Go 的 `NewSessionVars(nil)` 反射生成 `variable/session_fields.rs`：282 个标量字段（bool、整数、浮点、字符串、`time.Duration` 及以它们为底层类型的命名类型；嵌入的 `Concurrency`、`MemQuota`、`BatchSize` 等展开），带 Go 的注释和默认值。写路径的 11 个标量字段不生成（与 `rules.py` 的 X 一致，列表在生成器里）。`SessionVars` 替身通过 `Deref` 暴露这些字段，原来替身里重复的 5 个字段已删掉。
  - govec `sessionvars` 领域，op `fresh`：Go 侧反射导出 282 个字段（浮点记 IEEE-754 位），Rust 侧 `scalar_strings()` 对比，0 条不一致。反证：把 `foreign_key_checks` 的默认值改成 false，回放报 1 条不一致。
  - 其余 67 个非标量字段（map、指针、atomic、StmtCtx、TxnCtx、UserVars……）列在 `session_fields.rs` 末尾，待 T3/T6 手写。
  - tracker 中 D05.2 尚未覆盖的 234 个函数登记为 pending 豁免（`govec/domains/sessionvars_pending.go`），随 T3–T7 逐步删减。
- [x] T3、T11、T12 与 T4 的一部分（PR A），见 tasks.md。

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

PR A 已完成（见 tasks.md）。接手 PR B 时：

1. **钩子文件**：`session_hooks.rs` 的首版由一次性脚本翻译生成（"赋值给标量字段"形状的 SetSession 闭包），之后按手写代码维护：新钩子直接加函数、加进对应的 `match`，并从 `PENDING_*` 删掉名字。注册表构建时和单元测试 `registry_matches_hand_written_hooks` 会检查：Go 有的钩子要么已移植、要么在 pending 中，Rust 不能有 Go 没有的钩子。
2. **T5 的前提**：SetGlobal / GetGlobal 大多读写进程级 atomic（`variable.EnableXxx.Load()` 之类）。先按 Go 的名字在 `tidb_vars.rs` 中加这些全局量，再移植钩子；govec `set_global` 记录在生成时会把 INSTANCE 全局量恢复原值，避免影响后续记录。
3. **T6**：非标量字段（TxnScope、Rng、IsolationReadEngines、StmtCtx 的 PrevLastInsertID、MemTracker、PartitionPruneMode、KVVars……）按 Go 的名字加到 `SessionVars`，然后移植依赖它们的 46 个 SetSession / 15 个 GetSession。
4. **回放**：`tests/govec_sessionvars.rs` 会自动开始检查钩子落地后的记录（按 pending 跳过），不需要重新生成向量。
5. **D05.1 的 govec_sysvar 回放**仍用 `set_system_var_for_test`（只写 systems[]），Go 用 `SetSystemVarWithoutValidation`（会执行 SetSession）。钩子齐全后可以改成后者。

## 需要决定的事

- **N8 §6 待定 1（SET GLOBAL）**：已开 owner issue tidb-rs #58，按建议 (a) 推进，不等决定。
