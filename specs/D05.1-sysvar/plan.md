# D05.1 计划

1. tracker：`rsgen -sysvar` 生成静态表；`scripts/blocks/rules.py` 划分（spec §1）；govec `sysvar` 领域、minelits 字面量。✅
2. tidb-rs：crate `tidb-sessionctx`（rank 7）的 `variable` 模块：类型、校验、注册表、93 个 Validation 钩子、varsutil、statusvar、removed、sequence_state。✅
3. 回放测试 `tests/govec_sysvar.rs`，debug 和 release-checked；多种子；反证。✅
4. amd64 向量：取 tracker CI 的产物（与 arm64 记录相同）。✅
5. D05.2 接手：用真实的 `SessionVars` 替换 `session.rs` 桩；移植四类会话钩子和 FromHook 方法；核对 `SysVar.go_hooks`。

重新生成：

```
(cd tidb-rs-tracker/govec && go run ./cmd/rsgen -sysvar <tidb-rs>/crates/sessionctx/src)
(cd tidb-rs-tracker && ./bin/govec sysvar -o vectors/v8.5.8/<target>/sysvar.jsonl.gz)
```
