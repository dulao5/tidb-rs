# D05.1 系统变量定义与校验：块 spec

依据：基础设计 spec N8（`specs/exceptions/N8-sysvar/spec.md`，§3.1、§3.4）。crate `tidb-sessionctx`（rank 7，与 D00.1、D05.2 同一 crate），模块 `variable`。

## 1. Go 范围与划分

Go 文件：`pkg/sessionctx/variable/` 的 `sysvar.go`、`noop.go`（表）、`variable.go`、`varsutil.go`、`tidb_vars.go`、`statusvar.go`、`removed.go`、`setvar_affect.go`、`sequence_state.go`。

tracker 划分（`scripts/blocks/rules.py`，2026-11-08）：D05.1 从 96 个函数调整为 **79 个**。

| 去向 | 函数 | 理由 |
|---|---|---|
| X | `GlobalSystemVariableInitialValue` | 引导时写 `mysql.global_variables`，写路径，由 Go TiDB 承担 |
| X | `setTiDBTableValue`、`switchDDL`、`switchStats` | 写 `mysql.tidb`；DDL owner、统计 owner 的切换，tidb-rs 从不承担 |
| X | `RegisterSysVar`、`UnregisterSysVar` | 只服务于 extension / 插件和测试（N8 §3.1 第 5 条） |
| X | `mustParseTime` | 只用于初始化 TTL 任务窗口的原子量，TTL 归 X |
| D05.2 | `SysVar.{Get,Set}{Global,Session}FromHook`、`getTiDBTableValue`、`setSnapshotTS`、`setTxnReadTS`、`setReadStaleness`、`parseTSFromNumberOrTime`、`setTiFlashComputeDispatchPolicy` | 需要 `SessionVars` 的状态（`systems`、强类型字段）和 SetSession / GetGlobal 钩子，随 D05.2 移植 |

**变量本身全部保留**：944 个变量（`defaultSysVars` 499 个 + `noopSysVars` 445 个）都按 Go 的名字、默认值、作用域、类型、范围、枚举值和校验存在，SET 和 SHOW VARIABLES 的行为与 Go 相同。只影响写路径、DDL 或推迟功能（MPP、TiFlash）的变量，其效果钩子属于 D05.2 / 各功能块，届时按 N8 §3.4 和各自的 spec 处理。

## 2. Rust 布局

| 文件 | 内容 |
|---|---|
| `variable/variable.rs` | `ScopeFlag`、`TypeFlag`、`SysVar` 与校验（`Validate`、`ValidateFromType`、各 `check*`、`ValidateWithRelaxedValidation`、`GetNativeValType`、`SkipInit` 等），注册表（`get_sys_var`、`get_sys_vars`、`set_sys_var`、`order_by_dependency`） |
| `variable/sysvar_data.rs` | **生成**（tracker `rsgen -sysvar`）：944 项的静态字段，按 Go 源码顺序，每项标注 Go 行号和名字常量 |
| `variable/hooks.rs` | 93 个 `Validation` 钩子（Go 中是表字面量里的闭包），按变量名查找 |
| `variable/varsutil.rs` | varsutil.go：取值转换、共享检查、字节数 / 百分比解析、DDL 原子量 |
| `variable/tidb_vars.rs` | 校验用到的 `Def*` 常量和进程级原子量 |
| `variable/names.rs` | 用到的变量名常量（Go 标识符） |
| `variable/error.rs` | error.go 的错误原型 |
| `variable/statusvar.rs`、`removed.rs`、`sequence_state.rs`、`setvar_affect.rs` | 对应 Go 文件 |
| `variable/timeparse.rs` | `time.ParseInLocation` / `Format` 在 `"15:04"`、`"15:04 -0700"` 两个布局下的移植（含 Go 的 `ParseError` 文字） |
| `variable/util.rs` | 还没有块移植的几个工具函数：`timeutil.ParseTimeZone`、`util.IsInCorrectIdentifierName`、`servicescope.CheckServiceScope`（`glue`） |
| `variable/session.rs` | **D05.2 的桩**：`SessionVars` 和 `StatementContext` 中校验代码读写的字段（警告、语句类型、`systems`、`NoopFuncsMode`、`allowMPPExecution`、`ForeignKeyChecks`、`TimeZone`、`TLSConnectionState`）与 `GlobalVarAccessor` |

## 3. 设计决定

1. **注册表形态（N8 §3.1 方案 c）**：静态表 + 手写钩子。静态字段由生成器从 Go 运行时注册表（init 之后的值）和 AST（顺序、行号、名字常量）生成，不手抄；钩子按普通函数做 L1 移植。注册表构造时断言：Go 有 `Validation` 的变量恰好有 Rust 钩子；`setHintUpdatable` 列出的变量带 `IsHintUpdatableVerified`。
2. **钩子以外的四类钩子**（SetSession / SetGlobal / GetSession / GetGlobal / GetStateValue）属于 D05.2。`SysVar.go_hooks` 记录 Go 有哪些钩子，D05.2 照此核对。
3. **实例覆盖层**：`SetSysVar` 写一个 `RwLock<HashMap>` 覆盖层（N8 §3.1 第 4 条，L2），`get_sys_var` 先查覆盖层。`get_sys_vars` 不深拷贝（L2）。
4. **计算出的默认值**：`version_compile_os` / `version_compile_machine` 按本次构建的目标换算成 Go 的 `GOOS` / `GOARCH`；来自 `config.GetGlobalConfig()` 的默认值取默认配置的值（Go 在包初始化时求值，早于读配置文件）。
5. **`init_connect` 的校验**用 PoC 的解析入口（`tidb_parser::parse`），等 D02.2 的解析驱动（L2）。
6. **Go-runtime 变量**（N8 §3.4）：GOGC 调优等钩子读的 `gctuner` 上下界保留为原子量（默认 100 / 500），校验行为与 Go 相同。

## 4. 已知差异与豁免

1. **`SessionVars` 是桩**（`session.rs`，`glue: D05.2 stub`）：字段名、默认值与 Go 的 `NewSessionVars` 相同。`set_system_var_for_test` 只写 `systems` 和别名，不跑 SetSession 钩子。
2. **`init_connect`**：见 §3 第 5 条。
3. **状态变量**：桩里的 TLS 状态没有 cipher / version，按 Go 的零值处理（`Ssl_cipher` 为空、`Ssl_version` 为 `unknown_tls_version`）。
4. **`OrderByDependency`**：Go 遍历 map，组内顺序随机（M-MAPORDER）；Rust 保持输入顺序。

## 5. 验收

- converge：`matched 79/79`。
- govec `sysvar` 领域（tracker `govec/domains/sysvar.go`）：65,811 条，debug 和 release-checked 回放全部通过；种子 1–3 本地生成后回放也全部通过。
  - `registry`：每个变量的全部静态字段和字段方法（944 条）。
  - `validate` / `relaxed`：每个变量 × 按类型构造的取值（默认值、空串、大小写、边界和越界、溢出、枚举序号、时长、时刻、字节数、字符集 / 排序规则名、时区等）× 作用域，加上读上下文的钩子（语句类型、NoopFuncsMode、全局值、TLS、MPP）。每条记录用新会话、重置进程级原子量、TimeZone UTC、MemTotal 64 GiB。
  - Go 测试 `sysvar_test.go`、`variable_test.go`、`varsutil_test.go` 的字面量（`cmd/minelits`，`minedGoTests["sysvar"]`）：与同一测试中出现的变量名配对，或按测试名对应的变量。
  - varsutil 各函数、`GetNativeValType`、`ScopeFlag.String`、removed、SequenceState、OrderByDependency、状态变量、DDL 原子量、默认值辅助函数。
- 反证（故意改坏后回放失败）：忽略 `AutoConvertNegativeBool` 68 条，去掉无符号最小值钳制 282 条，Sscanf 把换行当空白 1 条，checkReadOnly 在 WARN 时不加警告 10 条，时刻接受 24 点 24 条（第一版生成器发现不了，补上 `24:00` 等用例后才发现）。
