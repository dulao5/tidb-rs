# D05.1 任务

- [x] T1 静态表生成器（rsgen -sysvar），944 项
- [x] T2 variable.go：类型、校验、注册表、实例覆盖层
- [x] T3 93 个 Validation 钩子
- [x] T4 varsutil.go、tidb_vars.go（校验用到的部分）、statusvar.go、removed.go、sequence_state.go、setvar_affect.go
- [x] T5 govec sysvar 领域与回放；minelits；多种子；反证
- [x] T6 amd64 向量（tracker CI 产物，与 arm64 记录相同）
- [ ] T7（D05.2）`session.rs` 桩替换为真实 `SessionVars`；会话钩子；FromHook 方法
- [ ] T8（D02.2）`init_connect` 校验改用忠实的解析驱动
- [x] T9（评审后续，Lucky 对 #43 的不阻塞建议）
  - 注册表与手写 hook 的一致性检查抽成 `registry_mismatches`，构建注册表时断言，并有单元测试 `registry_matches_hand_written_hooks`，不一致时 CI 失败。反证：去掉 `timestamp` 的 Validation 映射，测试报 `Validation hook rust=false go=true`。
  - `session.rs` 标为 "stand-in: D05.2"，其中全部是 `glue:`，不计入 converge。
  - 非校验 hook 的清单：tracker `data/sysvar_hooks.tsv`（444 个变量），归 D05.2，作为 T7 的验收清单。
  - govec `sys/whitespace`：Sscanf 解析的 5 个变量和全部 TypeDuration 变量的首尾空白输入，共 1,710 条，回放 0 条不一致（#45 的来源）。
  - server 注册的 status vars 按注册方归块：pkg/server → D03.1，bindinfo → D13.2，ddl、gcworker → X。
