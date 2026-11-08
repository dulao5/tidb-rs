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


## PR B 交接（2026-11-08）

分支：tidb-rs `block/D05.2-sessionvars-b`（draft，叠在 PR A #57 上），tracker `govec/sessionvars-D05.2-b`（draft #29，叠在 tracker #27 上）。PR A 合并（以及 PR A 的评审修改落地）后 rebase 到 main / PR A 新的末端。

**已完成**
- T6 的钩子所需部分、T4 全部（除 tidb_slow_log_rules 的 SetSession）、T9。
- T5：tidb_vars.go 的进程级 atomic 全部移植；39 个 SetGlobal、64 个 GetGlobal（只读写这些 atomic 的单句闭包，模式翻译，之后手工维护）。
- `SetSystemVarWithOldValAsRet` 在钩子出错时仍返回旧值（Go 如此；原实现返回空串，PR A 时这些记录被跳过而没有发现）。
- govec：INSTANCE 变量的 `set_global` 记录在 SetGlobal 之后经 GetGlobal 读回（`after`），回放比对并恢复原值。反证：去掉 INSTANCE 变量的 SetGlobal 调用，回放报 22 条不一致。
- 回放：39,279 条比对，2,168 条因 pending 钩子跳过（PR A 时 36,046 / 5,401）。converge D05.2 33/237。

**生成器重置清单（与 PR A 评审修改的进程级状态重置对齐）**：PR B 的钩子会写以下进程级状态，每条记录前要重置为默认值——
- SetSession 写的：`errors.RedactLogEnabled`（tidb_redact_log）。
- SetGlobal 写的 variable 包 atomic（INSTANCE 变量的记录本身会恢复，GLOBAL 变量的 SetGlobal 在读阶段只由缓存重建调用）：AccelerateUserCreationUpdate、ConnectAttrsSize、DDLSlowOprThreshold、EnableAutoAnalyzePriorityQueue、EnableBatchDML、EnableCheckConstraint、EnableFastReorg、EnableForeignKey、EnableHistoricalStatsForCapture、EnableInstancePlanCache、EnableNoopVariables、EnableRCReadCheckTS、EnableRowLevelChecksum、EnableTTLJob、EnableTmpStorageOnOOM、ExpensiveQueryTimeThreshold、ExpensiveTxnTimeThreshold、GCMaxWaitTime、IgnoreInlistPlanDigest、MaxUserConnectionsValue、MemQuotaBindingCache、MemoryUsageAlarmKeepRecordNum、MemoryUsageAlarmRatio、OOMAction、PasswordHistory、PasswordReuseInterval、PasswordValidation*（4 个）、ProcessGeneralLog、QueryLogMaxLen、RunAutoAnalyze、SchemaVersionCacheLimit、StatsLoadPseudoTimeout、StatsLoadSyncWait、TiFlashEnablePipelineMode、VarTiDBSuperReadOnly。
- 回放侧的跳过上限（PR A 评审）在 PR B 完成时要降到 0；当前仍有 2,168 条。

**余下（按建议的顺序）**
1. **T5 余下的 116 SetGlobal / 70 GetGlobal**：落点在其他包。建议按 spec §2.3 的处置逐个归类：
   - `config.GetGlobalConfig().Instance.*`（slow log、check_mb4、collect_execution_info、max_connections、plugin_*、stmt_summary_*、enable_ddl / stats_owner）：tidb-base 已有 `Instance`（atomic 字段直接 store，普通字段按 M-SNAP 克隆后 `store_global_config`），可以 L1/L2 移植。
   - ldap（LDAPSASLAuthImpl / LDAPSimpleAuthImpl 的 getter/setter）、tikvstore（StoreLimit、TxnCommitBatchSize、CommitterConcurrency）、topsqlstate、tls.RequireSecureTransport、memory（ServerMemoryLimit*、EnableGCAwareMemoryTrack）、gctuner：按 D05.1 已有的 `go-dep` 静态量做法（`minGCPercent` 等）移植状态单元；纯副作用（logutil.ReplaceLogger、stmtsummaryv2.Set*、PD 客户端动态选项、switchDDL / switchStats）归 runtime-only 或 X，原因写进 `sysvar_hooks.tsv` 的处置列。
   - `getTiDBTableValue`（tikv_gc_*）：需要 GlobalVarAccessor.GetTiDBTableValue，随 T10。
2. **T7**：`ParseSessionSlowLogRules` / `ParseGlobalSlowLogRules`（slow_log.go 的规则解析与字段解析表）→ tidb_slow_log_rules 的 SetSession、tidb_slow_log_rules / tidb_slow_log_max_per_sec 的全局钩子。
3. **生成器**：GetGlobal 已移植的变量，`get` / `set` 记录不再记为 `<global-hook>`，改记真实值（Rust 侧按 pending 跳过）；非标量字段的导出（IsolationReadEngines、TxnScope、KVVars……），使 T6 的效果可比对；minelits（session_test.go、varsutil_test.go）与多种子。
4. amd64 向量：tracker #29 的 CI 产物（arm64 已更新）。

**发现**
- `fixcontrol.ParseToMap("1:")`：冒号后为空时 Go 越界 panic（`s[0]`），Rust 照样 panic（tidb_opt_fix_control 没有 Validation）。是否可由 SQL 触发（`SET tidb_opt_fix_control='1:'`）待验证，候选 upstream-pitfall。
- `ToRuntimeFilterType` 按 Go map 的随机顺序生成列表；Rust 排序（M-MAPORDER）。

## PR B 交接（第二轮，2026-11-08）

PR A 已合并（#57 @a2307d0、tracker #27 @bd6450e）；PR B 的分支已 rebase 到 main（tidb-rs #63、tracker #29，base = main，仍为 draft）。

**本轮完成**
- **生成器与回放的进程级状态重置**：在 PR A 的 `RedactLogEnabled` 之外，每条记录前恢复 SetGlobal 钩子写的 variable 包 atomic（交接清单中的 38 个，加上 HistoricalStatsDuration、AdvancerCheckPointLagLimit、EnablePProfSQLCPU、ForcePriority）、memory 包的 EnableGCAwareMemoryTrack / ServerMemoryLimitSessMinSize / ServerMemoryLimit / ServerMemoryLimitOriginText、全局 config（INSTANCE 钩子会写），以及两个 LDAP impl 的设置。Go 侧在 init 时取快照（`processGlobalsRestore`），Rust 侧在第一条记录前取快照（`restore_process_globals`）。
- **钩子**：SetGlobal 77/155（PR B 第一轮 39），GetGlobal 112/134（第一轮 64）。新文件：
  - `ldap_state.rs`：`ldap.LDAPSASLAuthImpl` / `LDAPSimpleAuthImpl` 的设置与 getter/setter（认证本身与连接池归 D04.1；`initializePool` 为 runtime-only）。
  - `session_hooks_ext.rs`：instance config（max_connections、check_mb4、collect_execution_info、enable_ddl / stats_owner（switchDDL / switchStats 无回调，tidb-rs 不跑 DDL 与 stats owner）、slow log、record_plan、service_scope、plugin_*、stmt_summary 文件参数）、LDAP 的 43 个钩子、pprof_sql_cpu、force_priority、disconnect_on_expired_password、column_tracking、memory_usage_alarm_ratio、historical_stats_duration、advancer_check_point_lag_limit、gc_aware_memory_track、server_memory_limit(_sess_min_size)。
  - 写 config 中非原子字段的钩子（Go 原地写）在 Rust 中克隆后 store（M-SNAP，L2）。
- **生成器**：
  - `get` / `set` / `old` 记录不再写 `<global-hook>`，记 GetGlobal 的真实值（tidb_slow_log_max_per_sec 记为 `<volatile>`：int(rate.Inf) 随平台不同，#27）。
  - 新 op `set_global_hook`：对"写入状态可被重置"的 GLOBAL 变量（`sessionvarsGlobalHookVars`，60 个），执行 Validate → SetGlobal 钩子 → GetGlobal 读回，之后重置。之前 GLOBAL 变量的 SetGlobal 钩子从未被向量覆盖。
  - 向量 43,486 条（新增 set_global_hook 2,039 条），amd64 取自 tracker CI，与 arm64 记录相同。
- **回放**：41,167 条比对，0 条不一致；2,319 条因 pending 钩子跳过。跳过数比第一轮（2,168）多，是因为分母变了：`get` 记录现在比对 GetGlobal，GetGlobal 仍 pending 的变量（tidb_gc_* 等）的记录改为跳过。上限设为 2,319。
- **反证**：LDAP 的 bind_root_pwd 掩码改成 `****` → 67 条不一致；`ServerMemoryLimitSessMinSize` 存 +1 → 17 条。

**余下**
1. SetGlobal 78 / GetGlobal 22 个仍 pending：
   - `getTiDBTableValue` / `setTiDBTableValue`（tidb_gc_*，5 个变量）：随 T10 / §6（需要 GlobalVarAccessor 读写 mysql.tidb）。
   - client-go 的状态单元：`tikvstore.StoreLimit`（初值 0）、`TxnCommitBatchSize`（init 时 16 KiB）、`tikvutil.CommitterConcurrency`（加 `tikvcfg.StoreGlobalConfig` 的 runtime 副作用）；`tls.RequireSecureTransport`；`topsqlstate.GlobalState.MaxCollect / MaxStatementCount`（5000 / 100）：照 `ldap_state.rs` 的做法建状态单元，并加入两侧的重置与 `sessionvarsGlobalHookVars`。
   - gctuner（max/min GC percent、GlobalMemoryLimitTuner 的百分比、Tuning、EnableGOGCTuner）、`tidb_server_memory_limit` 的 SetGlobal（parseMemoryLimit + gctuner）。
   - TTL 调度窗口（`TTLJobScheduleWindowStartTime/EndTime`，`FullDayTimeFormat` 的时间解析与格式化）、`GetMemQuotaAnalyze / SetMemQuotaAnalyze`：Rust 的 tidb_vars 还没有这些状态。
   - 只影响 tidb-rs 不运行的子系统的 SetGlobal（DDL reorg / flashback / disk quota、dist task、TTL worker、GC worker、PD 客户端动态选项、stmtsummary v2、logutil.ReplaceLogger、resource control）：按 spec §2.3 的处置归 runtime-only 或 X，原因写进 `sysvar_hooks.tsv` 的处置列（tidb-rs 持久化值后，Go 实例在重载时自己执行这些钩子，§6）。
   - tidb_config（config JSON）、tidb_external_ts（读外部时间戳）、tidb_cloud_storage_uri（`ast.RedactURL` + `ValidateCloudStorageURI`）、tidb_opt_fix_control 的 SetGlobal、tidb_slow_log_rules / max_per_sec（T7）。
2. T7：slow log 规则的解析（tidb_slow_log_rules 的 SetSession 与全局钩子）。
3. 生成器：导出非标量字段（让 T6 的效果可比对）；minelits（session_test.go、varsutil_test.go）；多种子。
4. converge 中第一轮留下的 4 个 ERROR：`session_hooks_global.rs` 的 crate 级 `#![allow(clippy::unnecessary_cast)]` 不在允许表；`session_state.rs:186` 与 `session.rs:459` 的级别写法；`session.rs:531` 的 `assert_send` 没有来源标注。

**发现（upstream-pitfall 候选，未开 issue）**
- `ldapAuthImpl.initializeCAPool`：`errors.Wrapf(err, "read ca certificate at %s", caCert)` 格式化的是读失败后为 nil 的 `caCert`，不是路径，消息是 `read ca certificate at : open <path>: …`；而且 `caPath` 在报错前已经被改掉，之后 GetGlobal 返回的是读不了的路径。Rust 照样复现。
- `fixcontrol.ParseToMap("1:")`：tidb_opt_fix_control 没有 Validation，SetSession / SetGlobal 钩子直接调用 ParseToMap；冒号后去空白为空时 `s[0]` 越界 panic。静态可达：`SET tidb_opt_fix_control='1:'`、`SET GLOBAL …`、`SET_VAR(tidb_opt_fix_control='1:')`。会话层的 recover 会把 panic 转成返回给客户端的错误（未在 v8.5.8 server 上用 SQL 验证）。

## PR B 拆分（2026-11-09）

PR B 太大，拆成 B1 与 B2，范围见 tasks.md 开头。B1 = 当前的 #63 / tracker #29，修好了第二轮交接第 4 条中 converge 的 4 个 ERROR（`unnecessary_cast` 改为去掉多余的类型转换；两处级别写法；`assert_send` 加 glue 标注），并把 `to_runtime_filter_type_ok` 改为调用 `session_state::to_runtime_filter_type`（去掉重复标注；大写转换统一用 `gostd::to_upper`，即 Go 的 `strings.ToUpper`）。converge：D05.2 34/237，0 个 ERROR。B2 在分支 `block/D05.2-sessionvars-b2` / `govec/sessionvars-D05.2-b2` 上继续第二轮交接的余下第 1–3 条。

## PR B2 交接（2026-11-09，第一轮）

分支 `block/D05.2-sessionvars-b2` / tracker `govec/sessionvars-D05.2-b2`（叠在 B1 #63 / tracker #29 上；B1 合并后 rebase 到 main）。

**本轮完成**
- **B1 评审修改**（在 B1 分支上，已推送，#63 / #29 的 CI 通过）：全局 config 写者串行化（`config::update_global_config`，2 线程测试）；移植 pingcap/tidb#65884（`fixcontrol.ParseToMap` 的 `'123:'`）；LDAP ca_path 返回客户端看到的错误（`errors.Cause`），生成器记录 `errors.Cause`。
- **钩子**：新文件 `hook_state.rs`（client-go 的 StoreLimit / TxnCommitBatchSize / CommitterConcurrency，tls.RequireSecureTransport，top SQL 两个上限，gctuner 开关，GenerateBinaryPlan，TTL 调度窗口，slow log 限速器，stmt summary v1 的选项；domain / DDL / executor / stale read 注册的回调，未注册时按 Go 的 nil func 调用 panic）与 `session_hooks_b2.rs`（71 个 SetGlobal、15 个 GetGlobal，含 `units.RAMInBytes`、TTL 窗口的 `"15:04 -0700"` 格式化）。Go GC tuner、logger、metrics、client-go 全局 config 的效果标为 `[runtime-only]`。
- **pending 只剩**：SetGlobal——tidb_gc_* 5 个（随 PR C / T10）、`tidb_cloud_storage_uri`（需要 `ast.RedactURL`，即 `url.Parse` 的移植）、`tidb_slow_log_rules`（T7）；GetGlobal——同样这 7 个。SetSession——`tidb_slow_log_rules`。
- **生成器**：注册 `SetExternalTimestamp` / `GetExternalTimestamp` 的桩（进程级的值），tidb_external_ts 的 get / set 不再是 nil func 的 panic；Rust 回放注册同样的桩，以及 executor 的 `GlobalAnalyzeMemoryTracker` 上限（初值 -1）。
- **回放**（arm64，本地生成）：42,944 条比对，0 条不一致；跳过从 2,319 降到 **554**（上限已改为 554）。

**余下**
1. amd64 向量：推送后取 tracker CI 的产物（tidb_external_ts 的 5 条记录变了），并更新 vectors README 的 sha256。
2. `tidb_cloud_storage_uri`：移植 `ast.RedactURL`（`net/url.Parse` 的相关部分）或经 parser 块；SetGlobal 的 `ValidateCloudStorageURI` 是回调（lightning / BR 注册），未注册时 nil func。
3. T7：`ParseSessionSlowLogRules` / `ParseGlobalSlowLogRules`（slow_log.go L918–L1095：`SlowLogRuleFieldAccessors` 字段表、正则、crc64）。
4. 生成器：把本轮的钩子加入 `set_global_hook`（`sessionvarsGlobalHookVars`）并把它们写的状态加入两侧的重置列表（hook_state 的各单元、gctuner 的百分比和 min/max、TTL 窗口、stmt summary 选项……）；非标量字段导出；minelits；多种子；Lucky 的两步记录（LDAP pool 容量、ca_path）。
5. 跳过上限降到只剩 tidb_gc_* 的记录。
