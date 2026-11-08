# D05.2 任务

- [x] T0 范围划分（tracker rules.py：39 个写路径函数归 X）
- [x] T1 spec / plan
- [ ] T2 SessionVars 结构与 NewSessionVars（rsgen 生成字段）
- [ ] T3 系统变量存取（globals 快照桩、delta、Get/Set、FromHook、OldValAsRet）；替换 session.rs 替身
- [ ] T4 SetSession / GetSession 钩子（300 个），sysvar_hooks.tsv 加处置列
- [ ] T5 SetGlobal / GetGlobal / GetStateValue 钩子（295 个）
- [ ] T6 UserVars、TransactionContext（保留部分）、TxnReadTS、Concurrency、prepared 语句、plan cache 参数、游标、session token
- [ ] T7 slow_log.go
- [ ] T8 govec sessionvars 领域、minelits、多种子、amd64 向量、反证；钩子清单单元测试
- [ ] T9 编译期断言 SessionVars: Send + !Sync
