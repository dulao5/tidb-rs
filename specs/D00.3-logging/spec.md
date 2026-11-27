# D00.3 运行时日志、错误堆栈与运维可观测格式（spec）

状态：草案（仅设计规格，对应 `dulao5/tidb-rs#447`，待评审；实现另开 issue；配套已按 `#448` 将审计插件钩子注释规则写入 `CLAUDE.md` §2 第 10 条）。

块与依赖模块：主归属 **D00.3**（通用工具 `pkg/util/*` 中与日志和版本输出相关的文件：`pkg/util/logutil`、`pkg/util/printer`、`pkg/util/versioninfo`，按需整文件移植）；落地为新建正式 crate **`tidb-log`**（在 `N0`（crate 分层与无环依赖方向基础设计）中置于 rank 1），配合 **`tidb-base`**（D00.2 常量、错误码与配置，rank 0：补充错误堆栈格式与轻量门面接口）以及 **`tidb-server`**（D03.1 协议与服务端启动入口）/ `poc-server` 启动接线与全仓调用点回改。

引用与实测口径：

- **Go 源码基线**：Go TiDB **v8.5.8**（`8b857efa2`），及 `go.mod` 固定版本：`pingcap/log@v1.1.1-0.20250917021125-19901e015dc9`、`go.uber.org/zap@v1.27.1`、`pingcap/errors@v0.11.5-0.20250523034308-74f78ae071ee`、`pingcap/sysutil@v1.0.1-0.20240311050922-ae81ee01f3a5`。下文路径均相对于各自模块根目录；依 `CLAUDE.md` §2.6 规则，所有模块路径及样本堆栈均省略仓库托管域名前缀。
- **已验证实测（字节级探针）**：通过 `/tmp/log_probe_447/main.go` 直接驱动 Go v8.5.8 同版本 `pingcap/log` + `pingcap/errors` + `zap` 生成 `/tmp/log_probe_447/out.txt`，逐字节验证文本/JSON 编码器、全部字段类型、引号与非 UTF-8 转义、`errorVerbose`、堆栈追踪及 panic 恢复格式。
- **已验证实测（真实服务端捕获）**：在本机通过 `tidb-server --store=unistore`（v8.5.8）实跑捕获启动横幅、配置 JSON、`SIGTERM` 优雅关闭序列、`command dispatched failed`、`tidb_redact_log` 三模式、`FOR UPDATE NOWAIT`、`KILL QUERY`、`expensive_query` 及 OOM `LOG`/`CANCEL` 动作日志（关键原始样本均已直接收录于下文 §4）。
- **未在本机实测或未本地检视源码的部分（明确标出）**：
  1. 混合集群 `INFORMATION_SCHEMA.CLUSTER_LOG` 跨节点端到端 SQL 查询：本机仅直接审计了 `pingcap/sysutil/search_log.go` 解析源码，尚未在真实混合集群执行（需待 `tidb-rs` status 端口接入 Diagnostics gRPC 服务后实测）。
  2. TiUP / TiDB Dashboard / PingCAP Clinic 外部日志与版本解析代码：未在本机检视其源码，下文对其版本标签兼容性的判断均属**基于 Go `ServerInfo` / `FormatTiDBVersion` 逻辑推断，需在集成环境实测验证**。

相关文档：[移植宪章](../../CONSTITUTION.md)（第一条 2、第九条）、[全局映射规则](../../docs/porting/mappings.md)、[来源标注规范](../../docs/porting/provenance.md)、[N0 crate 分层](../exceptions/N0-crate-layering/spec.md)、[N2 会话上下文](../exceptions/N2-session-context/spec.md)、[N3+A1 并发与内存](../exceptions/N3-A1-concurrency-memory/spec.md)、[N8 系统变量](../exceptions/N8-sysvar/spec.md)、[N9 可观测性](../exceptions/N9-observability/spec.md)、[D00.2 base spec](../D00.2-base/spec.md)、[D03.1 server spec](../D03.1-server/spec.md)。

---

## 0. 元信息与负责人已定事项（2026-11-27）

### 0.1 范围边界

- **本 spec 包含**：
  1. 统一运行日志格式与编码器（`text` 与 `json`），覆盖单行不变量、引号与转义、非 UTF-8 字节处理及全字段类型映射；
  2. 错误日志（`[error=...]`、`[errorVerbose=...]`、`log.disable-error-stack` / `log.enable-error-stack`、`ErrVerboseLogger`）、显式/致命堆栈（`[stack=...]`）、Rust `tidb_base::Error` 映射，以及 Rust `panic` 恢复与全局 panic hook 的单行输出；
  3. 运维高频优先日志清单（启动横幅 `Welcome to TiDB.` + `loaded config`、编译期版本注入、优雅关闭序列、`command dispatched failed`、`expensive_query`、`memory exceeds quota` / OOM 动作及其他核心诊断日志）；
  4. 日志后端架构、`N0` 分层落位、rank-0 结构化主门面与外部三门面（`log` / `tracing` / `slog-global`）直连路由、完整 `log.*` / `log.file.*` 配置键、CLI 参数、`general_log` 文件路由与动态热更新；
  5. 移植规则（`CLAUDE.md` 与 `docs/porting/mappings.md`）、CI 日志一致性检查脚本（`scripts/check_log_parity.py`）与棘轮机制、现有调用点盘点、分批回改计划及黄金测试计划。
- **本 spec 不包含**：
  1. 慢查询日志内容格式（`# Time:` 头部及约 70 个 `# Key: value` 字段）、`STATEMENTS_SUMMARY`、Top SQL 与 `TRACE`（已归 `N9` 与 `D14.2` 慢日志/语句摘要/Top SQL/TRACE）；本 spec 仅保留 `log.slow-query-file` 配置键与独立 `slow_query_log` sink 预留接口，供 `D14.2` 直接接入专用格式化器而无需重写文件轮转底座；
  2. 不移植功能块 `X`（DDL、DML 写路径、`ANALYZE`、BR/Lightning、mock store 等）内部的专属业务逻辑日志；
  3. 审计日志插件本体（归 `#448`；本任务已按 `#448` 在 `CLAUDE.md` §2 提前加上钩子注释规则）。

### 0.2 已由负责人决定事项（2026-11-27）

1. **推进节奏**：现在就做——先定稿本 spec，再按批次推进基础设施落地与全仓回改。
2. **源码位置 `[file:line]` 口径**：运行日志的 caller 字段写 **Rust 源文件 basename（经 `[A-Za-z0-9._-]` 字符白名单过滤）与 Rust 行号**（如 `[consistency.rs:67]`），不伪造或映射回 Go 文件名。
3. **单行不变量与错误/堆栈/panic 严格对齐**：每条日志必须严格占一行（行内零原始 `\n`/`\r`，仅行尾有一个 `\n`）；完整覆盖 `[error=...]`、`[errorVerbose=...]`、`log.disable-error-stack` / `log.enable-error-stack`、`[stack=...]`、文本与 JSON 全部转义与非 UTF-8 字节处理、Rust `tidb_base::Error` 映射，以及 Rust panic 恢复与未捕获 panic backtrace 单行输出（除 Go 自身直接写 stderr 的极窄路径外，禁止向 stderr 输出裸多行 panic）。
4. **运维高频检索日志逐字节一致**：以下日志（除时间戳、动态连接/事务 ID 与 `[file:line]` 外）优先移植并配 Go v8.5.8 黄金测试：`Welcome to TiDB.` + `loaded config`、优雅关闭序列、`command dispatched failed`、`expensive_query` + `memory exceeds quota` / OOM 动作，以及 §4.5 所列运维高频日志。
5. **启动横幅版本字段**：附加字段照 Go `Makefile.common` `-ldflags -X`、`pkg/util/versioninfo` 与 `pkg/util/printer` 在编译期通过 `build.rs` / `env!` 注入；`GoVersion` 留空（`[GoVersion=]`）；紧接新增 `Rust Version`（`rustc` 版本）；`Edition` 保持 Go 默认值 `Community`，企业版扩展字段（`Enterprise Extension Commit Hash`）暂不管。

---

## 1. Go TiDB v8.5.8 与 `pingcap/log` 统一日志格式逐字节契约

本节所有规则均经 `pingcap/log/zap_text_encoder.go`（`v1.1.1-0.20250917021125-19901e015dc9`）源码核对，并经 `/tmp/log_probe_447/out.txt`（以下简称 `probe`）字节级实测证实。

### 1.1 文本编码器（`format = "text"`）段落顺序与空白规则

文本格式严格按下列顺序拼装一条记录（`zap_text_encoder.go:406-483`）：

```text
[Time] [LEVEL] [LoggerName] [Caller] [Message] <BoundWithFields> <CallSiteFields><CloseNamespaces> [stack="..."]\n
```

各段落的精确出现条件与格式化规则：

| 段落 | 出现条件 | 字节格式与规则 | 实测依据（`probe`） |
|---|---|---|---|
| `Time` | `!disable_timestamp`（`TimeKey != ""`） | `[2006/01/02 15:04:05.000 -07:00]`；使用本地时区（跟随进程 `TZ`）。**当 `disable-timestamp = true` 时整段省略，且 `[LEVEL]` 前不保留前导空格** | `0_REAL_CLOCK_LOCAL_TZ`, `1B`, `1C` (`[INFO] ["no ts no caller"]`) |
| `LEVEL` | 始终出现 | `[<LEVEL>]`，`CapitalLevelEncoder` 固定输出 `DEBUG` / `INFO` / `WARN` / `ERROR` / `DPANIC` / `PANIC` / `FATAL` | `1A [00]-[06]` |
| `LoggerName` | `name != ""` | 空格后输出 `[<quoted_or_raw_name>]`，按 §1.3 `needDoubleQuotes` 判定是否加引号并转义 | `1A [07]` (`[sub.comp]`), `5A` (`["bad\nname"]`) |
| `Caller` | `!disable_caller && caller_defined` | 空格后输出 `[<filtered_basename>:<line>]`。`ShortCallerEncoder`（`zap_text_encoder.go:68-94`）截取最后一个 `/` 之后的 basename，仅保留 ASCII `[A-Za-z0-9._-]`，其余字符（含空格、`\`, `+`, 中文等）**直接丢弃**，随后加 `:<line>`；外部 `[` `]` 内不加引号 | `1A`, `5A` (`/dir/z\test file+1.go:99` -> `[ztestfile1.go:99]`) |
| `Message` | **`len(msg) > 0` 时出现** | 空格后输出 `[<quoted_or_raw_msg>]`。**空消息 `""` 在 text 模式下整段省略，绝不输出 `[]`** | `1A [08]` (`[2025/... ] [INFO] [main.go:149]\n`) |
| `BoundWithFields` | logger 预先绑定了 `.With(...)` 字段 | 空格后原样拼入预先编码好的 `[k1=v1] [k2=v2]`，排在所有调用点字段之前；链式 `.With(...)` 严格保持声明顺序 | `1A [09]-[10]` (`[bound1=v1] [bound2=2] [call1=c1] [call2=3]`) |
| `CallSiteFields` | 调用点携带字段 | 每字段前加一个空格，输出 `[<key>=<value>]`（`Error` 见 §2.1；已知怪癖 `Skip`/`Error(nil)` 输出 `[]` 见 §1.4） | `3A_TEXT_ALL_FIELDS` |
| `Stack` | 触发 `AddStacktrace`（TiDB 设 `FatalLevel`）且非空 | 所有字段与闭合命名空间之后追加空格与 `[stack="..."]`。若调用点也显式传 `zap.Stack("stack")`，则会先作为调用点字段输出一次，末尾再由 `AddStacktrace` 输出一次 | `4D [17]`, `4E [18]` |
| 行结束符 | 始终 | 单个 `\n`（`0x0a`）。单行不变量：两条 `\n` 之间允许出现的原始字节集合中**绝不能出现 `0x0a`（`\n`）或 `0x0d`（`\r`）** | `probe` 全部段落断言 `trailing_newline=true` 且行内 0 个原始 `\r`/`\n` |

> **说明**：Rust `log::Level::Trace` 在 Go `zap` 中没有对应级别。本 spec 约定 `Trace` 仅作内部开发过滤级别，若启用并输出至统一编码器，按级别标签映射为 `[DEBUG]`（不额外造 `[TRACE]`，避免干扰 `CLUSTER_LOG` 与现有解析器；见 §3.3）。

### 1.2 JSON 编码器（`format = "json"`）布局与差异

`pingcap/log` 在 `Format == "json"` 时调用 `zapcore.NewJSONEncoder(cc)`（`zap_text_encoder.go:166-167`）。实测（`probe` `1D`、`2B`、`3B`、`4C`）布局如下：

```json
{"level":"INFO","time":"2025/01/02 15:04:05.123 +08:00","name":"sub.comp","caller":"main.go:174","message":"named msg","bound1":"v1","call1":"c1","stack":"..."}\n
```

与 `text` 格式的四项关键差异：
1. **键顺序**：`"level"` 在前，`"time"` 在后（当 `disable-timestamp = true` 或时间为零值时省略 `"time"`）；随后依次为 `"name"`（非空时）、`"caller"`（启用时）、`"message"`、绑定字段、调用点字段、`"stack"`（非空时）。
2. **空消息**：JSON 中 `"message":""` **始终输出**，不省略（`probe` `1D [08]`）。
3. **HTML 字符不转义**：字符串内的 `<`、`>`、`&` 在 JSON 中直接保留原字符（例外仅见 `Reflect` 在 text 模式下调用 Go `json.Encoder`，下文详述）。
4. **`DisableErrorVerbose` 行为**：Go v8.5.8 实测 `zapcore.NewJSONEncoder` **忽略** `DisableErrorVerbose`，始终输出 `"errorVerbose"`（`probe` `4C`；该差异见 §9.2 D2 决策）。

### 1.3 `needDoubleQuotes` 与逐字节转义规范

`textEncoder` 对**消息体、字段键、字符串值、ByteString、LoggerName** 使用完全统一的引号判定与转义算法（`zap_text_encoder.go:531-629`）：

1. **引号判定 `needDoubleQuotes(bytes)`**：按字节遍历输入，若存在任意字节 `b` 满足
   `b <= 0x20 || b == b'\\' || b == b'"' || b == b'[' || b == b']' || b == b'='`
   则返回 `true`，在输出首尾包裹 `"`...`"`；否则不加双引号直接写入。
   - **实测边界**（`probe` `2A`）：
     - 空串 `""`：返回 `false`，无引号（`[empty=]`、`[=emptykey]`）；
     - `0x7f`（DEL）：`> 0x20` 且不在特殊字符集内，单独出现时**不加引号且原字节保留**（`[del_only=a\x7fb]`）；
     - 合法多字节 UTF-8（如 `"欢迎来到 TiDB 💖 ‹secret›"`）：若含空格（`0x20`）则加引号，若无特殊 ASCII（如 `"欢迎"`）则**不加引号、UTF-8 原字节保留**；
     - **非法 UTF-8 字节**：`needDoubleQuotes` 只扫描原始字节，不解码 UTF-8。因此不含特殊 ASCII 的非法字节串（如 `[0xff, 0xfe, b'a']`）**不加引号**，经下面第 2 步转义后直接输出 `[bad_str=\ufffd\ufffda]`；若同时含空格（如 `[0xff, b' ', b'a']`）则加引号输出 `["\ufffd a"]`。
2. **逐字节转义表（`safeAddString` / `safeAddByteString`）**：
   - `b < 0x80`（ASCII 单字节）：
     - `\` (`0x5c`) → `\\`
     - `"` (`0x22`) → `\"`
     - `\n` (`0x0a`) → `\n`
     - `\r` (`0x0d`) → `\r`
     - `\t` (`0x09`) → `\t`
     - 其他 `b < 0x20`（含 `0x00`、`0x1b` 等）→ `\u00xx`（小写十六进制）
     - `0x20..=0x7f`（除 `\` 和 `"`）→ 原字节输出
   - `b >= 0x80`（按 UTF-8 解码）：
     - 合法 UTF-8 码点 → 原 UTF-8 字节序列原样输出（不做 `<`/`>`/`&` HTML 转义）；
     - 非法 UTF-8 序列中的**每一个错误字节**（Go `utf8.DecodeRune` 返回 `(RuneError, 1)`）→ 输出 **6 字节 ASCII 字面量 `\ufffd`**（注意：不是 3 字节 UTF-8 字符 `U+FFFD`，即 `0xef 0xbf 0xbd`！）。
3. **JSON 模式非 UTF-8 实测（`probe` `2B`）**：`zapcore` JSON 编码器同样将每个非法字节转义为 `\ufffd`（如 `"bad_str":"\ufffd\ufffda"`），并对其余 JSON 控制字符按 `\n`/`\r`/`\t`/`\"`/`\\`/`\u00xx` 转义。

### 1.4 全字段类型编码表与 `pingcap/log` 历史行为实测

依据 `probe` `3A_TEXT_ALL_FIELDS` 与 `3B_JSON_ALL_FIELDS`（`out.txt:83-94`），全字段类型对照如下：

| zap 字段类型 | Text 格式输出（`3A` 实测） | JSON 格式输出（`3B` 实测） | Rust / `tidb-log` 实现规范 |
|---|---|---|---|
| `String` / `ByteString` | `[k=val]` 或 `[k="val"]`；空串 `[k=]` | `"k":"val"`（含 `""`） | 直接接收 `&[u8]`（可载非 UTF-8），按 §1.3 转义 |
| `Stringp`/`Intp`/`Boolp`/`Timep`/`Durationp` (`nil`) | `[k=null]`（无引号） | `"k":null` | `Option::None` 输出裸 `null` |
| `Binary([]byte)` | 标准 Base64；空切片 `[k=]`；无 `=` 填充无引号（`[k=YWJj]`）；带 `=` 填充触发 `needDoubleQuotes` 加引号（`[k="YWIxMjM="]`） | `"k":"..."`（标准 Base64） | `base64::engine::general_purpose::STANDARD` 后按字符串规则编码 |
| `Int` / `Int64` / `Uint64` | 十进制无引号（`[int=-42]`、`[uint64=18446744073709551615]`） | JSON 数字 | `itoa` 直接写十进制 |
| `Float64` / `Float32` | 最短表示无引号；特殊值 `[f_nan=NaN]`、`[f_pinf=+Inf]`、`[f_ninf=-Inf]` | 正常为 JSON 数字；特殊值加引号 `"NaN"`、`"+Inf"`、`"-Inf"` | 直接复用 `tidb-base` 已有移植 `tidb_base::gostd::strconv::{format_float, append_float}`（`crates/base/src/gostd/strconv.rs:84-93`），不重复移植 |
| `Bool` | `[b_true=true]` / `[b_false=false]` | JSON `true` / `false` | 字面量 `true` / `false` |
| `Duration` | Go `time.Duration.String()` 无引号（`[dur_0=0s]`、`[dur_1=1.3s]`、`[dur_neg=-5ms]`） | `"0s"` / `"1.3s"` / `"-5ms"` | 直接复用 `tidb-base` 已有移植 `tidb_base::gostd::time::duration_string`（`crates/base/src/gostd/time/mod.rs:313`），避免重复移植触发 `check_one_translation.py`（结果仅含数字与 `s`/`ms`/`µs`/`m`/`h`，text 下恒无引号） |
| `Time` | `[k=2025/01/02 15:04:05.123 +08:00]`（**text 下无引号**，即使中间含两个空格！因 `DefaultTimeEncoder` 直接写 `e.buf` 绕过 `AppendString`） | `"k":"2025/01/02 15:04:05.123 +08:00"` | `chrono::DateTime<Local>` 直接写入，不走 `needDoubleQuotes`；零值时间为 `0001/01/01 00:00:00.000 +00:00` |
| `Strings` / `Uint64s` / `Bools` | 嵌套子编码器生成 `[...]` 再按 `ByteString` 输出：简单元素 `[strs_simple="[a,b]"]`，复杂元素内部加引号再外层二次转义（`[strs_complex="[\"a b\",\"c\\\"d\",\"[x]\",]"]`），空切片 `[strs_empty="[]"]` | 标准 JSON 数组 `["a","b"]` / `[1,4,5]` / `[true,false]` | 序列类型先用克隆子缓冲编码，再对整体结果做一次 `ByteString` 编码 |
| `Stringer` | 正常无/有引号同 `String`；nil 指针 `<nil>`；`String()` 内 panic 捕获输出 `[kError="PANIC=..."]` | `"<nil>"` / `"kError":"PANIC=..."` | `Display` 输出后按字符串规则处理 |
| `Object` / `Inline` | `Object`：`[obj="{username=alice,len=5}"]`（出错时先 `obj_err={}` 再 `,obj_errError="..."`）；`Inline` 直接平铺为 `[username=inlined,len=7]` | 标准嵌套 JSON 对象或平铺字段 | `LogObject` trait 支持字段嵌套 |
| `Reflect` / `Any` | `Reflect`：text 用 Go `json.Encoder`（默认 HTML 转义 `\u003ca\u0026b\u003e`）输出后按 `ByteString` 引号包裹；`Any` 按实际底层类型分派 | 标准 JSON 值（不转义 `<>&`） | Rust 结构化 KV 按类型分派 |
| `Error` / `NamedError` | 见 §2.1 | 见 §2.1 | 见 §2.2 |

**实测发现的 4 项 `pingcap/log` 文本编码器怪癖（决策见 §9.2 D3）**：
1. `zap.Skip()` 与 `zap.Error(nil)`：`addFields` 先写 `[` 再调 `f.AddTo(enc)`（空操作）再写 `]`，导致 text 模式产生无意义的 **`[]`**（`3A [00]` 末尾、`4A [13]` `[nil_err] []`；JSON 模式下静默省略）。
2. `zap.Namespace("ns")`：text 模式下 `OpenNamespace` 写 `[ns={`（缺少匹配 `]`），结尾 `closeOpenNamespaces` 在行末追加裸 `}`，实测产出 `[ns={] [in_ns=v] [in_ns_i=7]}`（JSON 模式正常输出 `"ns":{"in_ns":"v","in_ns_i":7}`）。
3. `Complex64/128`：text 模式下负虚部直接拼 `+`，产出 `1+-2i`（JSON 输出 `"1-2i"`）。
4. `ReplaceLogger`（`pkg/util/logutil/log.go:191-217`）：动态替换全局 logger 时未更新 `errVerboseLogger`。

### 1.5 `INFORMATION_SCHEMA.CLUSTER_LOG` 解析契约

依据 `pingcap/sysutil@v1.0.1-0.20240311050922-ae81ee01f3a5/search_log.go:307-373` 源码直接核对：
- `parseLogItem` 逐行读取日志文件（含 `.gz` 压缩归档文件），按第一个 `[...]` 解析 `TimeStampLayout = "2006/01/02 15:04:05.000 -07:00"`，按紧随其后的 `[...]` 调用 `ParseLogLevel` 解析级别（仅识别 `debug`/`info`/`warn`/`trace`/`critical`/`error` 大小写，**`DPANIC`/`PANIC`/`FATAL` 均落入 `LogLevel_UNKNOWN`**），并将剩余切片 `TrimSpace` 作为 `LogMessage.Message`。
- 若一行文本中出现未转义的原始 `\n` 或 `\r`，`logIterator` 读到下一行时 `parseLogItem` 会因缺少 `[Time]` 头而判定为前一条日志的续行或解析错误；若开启 `format = "json"` 或 `disable-timestamp = true`，则完全无法通过 `CLUSTER_LOG` 解析。因此**默认配置下的单行统一文本格式是混合集群 `CLUSTER_LOG` 查询的硬契约**。

---

## 2. 错误格式、堆栈追踪、Panic 处理与单行不变量

### 2.1 Go `zap.Error`、`errorVerbose` 与堆栈控制机制

1. **`textEncoder.encodeError`（`zap_text_encoder.go:645-664`）**：
   - 先写入 `[<key>=<err.Error()>]`（`zap.Error` 键名固定为 `"error"`，`zap.NamedError(k, err)` 用自定义键名 `k`）；
   - 当且仅当 **`!enc.disableErrorVerbose`** 且 `err` 实现 `fmt.Formatter` 且 `fmt.Sprintf("%+v", err) != err.Error()` 时，紧接追加 `[<key>Verbose="..."]`，其内部所有 `\n`、`\t` 均按 §1.3 转义成 `\n`、`\t`，确保整条日志仍在单行内（`probe` `4A`）。
   - Typed-nil `*errors.Error` 实测（`probe` `5B`/`5C`）：`(*errors.Error)(nil).Error()` 返回 `"<nil>"`，text 输出 `[error=<nil>]`，JSON 输出 `"error":"<nil>"`；而无类型 `nil` 接口值走 `zap.Skip()`。
2. **`log.disable-error-stack` / `log.enable-error-stack` 解析（`pkg/config/config.go:577-585,1322-1326`）**：
   - `Config.Valid()`：若 `EnableErrorStack == DisableErrorStack && EnableErrorStack != nbUnset`，打警告并用 `EnableErrorStack` 覆盖（将 `DisableErrorStack` 重置为 `nbUnset`）；
   - `getDisableErrorStack()`：两者均未设置时**默认返回 `true`**（即默认关闭 `errorVerbose` 堆栈输出，仅保留 `[error=...]`）；设置 `enable-error-stack` 时优先取 `!EnableErrorStack.toBool()`，否则取 `DisableErrorStack.toBool()`。
   - **JSON 格式例外**：Go v8.5.8 的 `pingcap/log` JSON 编码器未接收 `DisableErrorVerbose`，因此 JSON 输出始终包含 `"errorVerbose"`（`probe` `4C`；见 §9.2 D2）。
3. **`BgLogger()` / `Logger(ctx)` 与 `ErrVerboseLogger()`（`pkg/util/logutil/log.go:139-149,237-259`）**：
   - `BgLogger()` 与 `Logger(ctx)` 继承全局配置（默认 `DisableErrorVerbose = true`）；
   - `ErrVerboseLogger()` 在 `InitLogger` 时基于相同 `Syncer` 克隆 `DisableErrorVerbose = false` 的专用 logger，供明确需要深度调用栈的组件（如 DXF、stats、部分 planner 路径）无视全局配置输出 `[errorVerbose=...]`。
4. **`terror.Error` 三种构造器的堆栈行为（`probe` `4A [09]-[11]`）**：
   - `GenWithStack*` / `New` / `Errorf` / `Trace` / `Annotate` / `Wrap`：携带调用栈，`%+v != Error()`，开启 `errorVerbose` 时输出完整堆栈；
   - `FastGen*`：通过 `SuspendStack` 禁用堆栈，即使开启 `errorVerbose` 也**不输出** `errorVerbose`；
   - `Synthesize`：裸原型，同样**不输出** `errorVerbose`。
5. **`AddStacktrace` 与 `zap.Stack("stack")`**：
   - `pingcap/log` 默认在 `ErrorLevel`（`development = true` 时 `WarnLevel`）自动附带 `[stack="..."]`，但 TiDB 在 `logutil/log.go:132` **强制传入 `zap.AddStacktrace(zapcore.FatalLevel)`**；因此日常 `WARN` / `ERROR` 不会自动追加 `[stack=...]`，仅 `FATAL` 级别（含 `log.Fatal`、`terror.MustNil`）或显式传入 `zap.Stack("stack")` / `zap.Stack("stack trace")` 时输出（`probe` `4D` vs `4E`）。
6. **`tidb_redact_log` 脱敏交互（`probe` `4E2_REDACT_MODES`）**：
   - `OFF`：`[error="[kv:1062]Duplicate entry 'secretVal' for key '‹k›'"]`
   - `ON`：`[error="[kv:1062]Duplicate entry '?' for key '?'"]`（且 `session.go:1590` 的 `redact.String("ON", sql)` 返回空串 `""` -> `[SQL=]`；`conn.go:1245` 对 `ErrParse` 整句替换为 `"fail to parse SQL, and must redact the whole error when enable log redaction"`）
   - `MARKER`：`[error="[kv:1062]Duplicate entry '‹secretVal›' for key '‹‹‹k›››'"]`（参数内置 `‹` / `›` 各自双写转义）。

### 2.2 Rust `tidb_base::Error` 映射与现存缺口修复

1. **原始字节保留（`M-ERR` Go 错误映射与 `M-BYTES` Go 任意字节串表示规则）**：
   - 现状：`crates/base/src/errors.rs:176-181` 的 `impl Display for Error` 使用 `String::from_utf8_lossy(&self.error())`，而 `Error::error(&self) -> Vec<u8>`（`:451-465`）保留完整的 `[rfc_code]` 前缀、`: cause` 链与任意非 UTF-8 字节。
   - **修复方案**：`tidb-log` 对 `tidb_base::Error`（或携带原始字节的 `LogValue::Bytes`）直接读取 `&err.error()` 原始字节送入 `safeAddByteString`（§1.3）。这样非 UTF-8 字节会按 Go 规范原样转义为 6 字节 ASCII `\ufffd` 且不触发双引号误判；若经 `Display` 有损转换则会提前生成 3 字节 UTF-8 替换字符 `U+FFFD`（`0xef 0xbf 0xbd`），导致与 Go 字节级不一致。
2. **关键缺口：`tidb_base::Error` 堆栈采集（`crates/base/src/errors.rs:483-492`）**：
   - 现状：`Inner.stack`（`errors.rs:142,488-491`）当前仅调用 `Backtrace::capture()`，在未设置 `RUST_BACKTRACE=1` 时返回 `BacktraceStatus::Disabled`；即使 `enable-error-stack = true`、调用 `ErrVerboseLogger()` 或到达 `conn.go:1222` 的 `errStrForLog`（`errors.ErrorStack(err)`），也无法产出任何堆栈帧。
   - **架构设计**：
     - 在 `Inner` 增加 `has_stack: bool`（或区分 `gen_with_stack*` / `new` / `errorf` / `plain` 置 `true`，`fast_gen*` / `synthesize` / `Error::share()` 置 `false`）；
     - 性能权衡：Go 的 `runtime.Callers` 单次捕获约微秒级，而 Rust `std::backtrace::Backtrace::force_capture()` 涉及 DWARF 符号化（尤其在首次格式化输出或每错捕获时）若对所有热路径表达式错误（例如 `types::convert` 试探转换错误、`ErrTableNotExists`）无条件捕获会产生明显开销。因此默认采集策略在 §9.2 **D4** 由负责人确认：推荐仅当全局配置开启 `enable-error-stack = true`（或环境变量 `RUST_BACKTRACE=1`）时在 `fill_line_and_file` 触发 `force_capture()`，且对 `ErrKeyExists` / `ErrParse` / `ErrTableNotExists` 等热点路径跳过栈采集；若未采集完整回溯但需要 `errorVerbose` 或 `error_stack`（包括默认配置下的 `ErrVerboseLogger()` 与 `command dispatched failed` 的 `[err=...]`），则按 `#[track_caller]` 提供的 `(file, line)` 零开销输出 `<error>\n<file>:<line>`（与 Go 多帧堆栈的这一取舍在 D4 中明示）。
     - 在 `crates/base/src/errors.rs` 新增：
       - `pub fn has_stack(&self) -> bool`（对应 `GenWithStack*` vs `FastGen*` 区分）
       - `pub fn error_verbose(&self) -> Vec<u8>`（格式化为 `<error()>\n<frames or file:line>`，含 `cause` 链，对齐 Go `pingcap/errors` `%+v`；无栈时返回 `self.error()`）
       - `pub fn error_stack(&self) -> Vec<u8>`（`errors.ErrorStack(err)` 的移植，供 `conn.rs` `err_str_for_log` 使用）。
3. **修复 `gen_with_stack` 与 `fast_gen` 脱敏 TODO（`crates/base/src/errors.rs:499,523`）**：
   - Go `pingcap/errors` 的 `GenWithStack` / `FastGen` 与 `GenWithStackByArgs` / `FastGenByArgs` 同样受 `RedactLogEnabled` 约束。补齐对 `self.redact_args_pos` 调用 `redact_error_arg(&mut args, &self.redact_args_pos)`，消除两处 `// TODO: RedactErrorArg`。
4. **修复 `terror::{must_nil, call, log}`（`crates/base/src/terror.rs:388-422`）**：
   - 现状：三者均用 `"unexpected error; error={err}; stack={}"` 字符串插值，且 `must_nil` 用 `log::error!` 而非 `FATAL`；`call` 与 `log` 在无 `RUST_BACKTRACE=1` 时 `stack=` 为空。
   - 改造方案：直接调用 §3.3 在 `tidb-base`（rank 0）定义的零新增依赖结构化日志门面（`tidb_base::structured_log`，由 `tidb-log::init_logger` 通过 `OnceLock<&'static dyn StructuredLogger>` 注入真实后端，初始化前退回默认单行 stdout 编码器）：
     - `terror::must_nil`：先执行全部 `close_funs`，再通过 `LogTarget::Fatal` 以 **`FATAL`** 级别输出 `"unexpected error"` `[error=...] [stack="..."]`（保留 `&tidb_base::Error` 原始字节并强制捕获单行 backtrace），刷新 sink 后 `std::process::exit(1)`；
     - `terror::call`：级别 `ERROR`，消息 `"function call errored"`，字段 `[error=...] [stack="..."]`；
     - `terror::log`：级别 `ERROR`，消息 `"encountered error"`，字段 `[error=...] [stack="..."]`。

### 2.3 Rust Panic、Backtrace 处理与单行不变量

1. **进程级全局 `std::panic::set_hook`（后端安装在 `tidb-log::init_logger`，作用域原语置于 `tidb-base` rank 0）**：
   - Rust 默认 panic hook 会直接向 `stderr` 打印多行裸文本与多行 `stack backtrace:`，严重破坏单行不变量与日志采集。
   - **避免 4 类生产陷阱的 rank-0 作用域设计（`tidb_base::panic_guard`）**：
     1. **层级可达性**：将线程局部 `PanicMode`（`FatalLog` | `CaptureBacktrace` | `Silent`）、可嵌套保存/恢复的 RAII `PanicScopeGuard`（依 `M-DEFER` `defer` 清理映射到 `Drop` 守卫的规则确保异常返回时自动恢复）、`LAST_RECOVERED_BACKTRACE`、`IN_PANIC_HOOK` 重入守卫，以及安全包装原语（`catch_unwind_silent`、`catch_unwind_capture`、`catch_unwind_future`）置于 **`tidb-base`（rank 0）**，使 `tidb-runtime`（rank 1）、`tidb-tablecodec`（rank 6）、`tidb-store`（rank 11）、`tidb-planner`（rank 13）、`tidb-executor`（rank 14）与 `tidb-server`（rank 16）均可无环直接调用；
     2. **嵌套与异步安全**：用保存/恢复前值的 `PanicScopeGuard` 替代扁平 `bool`（避免内层 `safe_clone` 恢复时提前解除外层 `ExecPool` 抑制）；`catch_unwind_future` 仅在每次同步调用 `inner.poll(cx)` 期间挂载 `PanicScopeGuard` 并在 `poll` 返回前立即释放（绝不跨 `.await` 泄漏线程局部状态，从而彻底解决 `futures::FutureExt::catch_unwind()` 在 `conn.rs:420`、`coprocessor.rs:1470,1738`、`projection.rs:583,676`、`sort.rs:1084,1241`、`parallel_sort_worker.rs:353`、`agg_hash_*.rs` 上绕过 hook 导致的双写 `[FATAL]` + 恢复日志问题）；
     3. **值恢复静默与回溯消费约束**：对仅作错误值转换而不打日志的 `catch_unwind`（如 `crates/planner/src/core/physical_plans.rs:1808` `safe_clone`、`crates/tablecodec/src/rowcodec/common.rs:281` `append_datum_for_checksum`）使用 `catch_unwind_silent`（`PanicMode::Silent`），防止误输出 `[FATAL]` 或无谓采集回溯；对可恢复边界使用 `catch_unwind_capture` / `catch_unwind_future`（`PanicMode::CaptureBacktrace`），且**仅在 `Err(payload)` 分支消费 `LAST_RECOVERED_BACKTRACE`**（绝不在 `Ok(Err(oom_err))` 等常规错误路径读取残留槽位）；
     4. **未捕获致命 panic 与重入保护**：当 `PanicMode::FatalLog` 时，全局 hook 先置线程局部 `IN_PANIC_HOOK` 守卫（若写日志自身因 `log.timeout` 触发二次 panic，立即降级为单行转义写 `stderr` 避免死循环），随后提取 `PanicHookInfo` payload 与 `location()`、捕获 `Backtrace::force_capture()`，经统一单行编码器以 `FATAL` 级别输出 `"unhandled panic"` `[panic=<escaped>]` `[location="<file>:<line>"]` `[stack="<escaped_single_line_bt>"]` 并调用 `flush()` / `sync()` 落盘。
   - 不复用 `third_party/tikv/panic_hook`（该 vendored crate 文档注释明确标明仅限测试使用且受 `N0` `ADR 0007`（关于禁止生产代码直接依赖 TiKV 内部实现的架构决策记录）限制）；`scripts/check_log_parity.py` 规则 A 禁止分层生产代码直接调用裸 `std::panic::catch_unwind` 或 `FutureExt::catch_unwind`，统一强制使用 `tidb_base::panic_guard::catch_unwind_{silent,capture,future}`。
2. **全仓 `catch_unwind` 恢复点映射（遵循 `M-PANIC` `panic`/`recover` 映射到任务边界 `catch_unwind` 的规则）**：

| 恢复点（Rust 位置） | Go 对应位置 | 级别 / 策略 | 消息与字段（严格单行） |
|---|---|---|---|
| `crates/runtime/src/pool.rs:379`（`ExecPool::spawn_with_recover`） | `pkg/util/misc.go:95` `WithRecovery` | `ERROR`（`catch_unwind_capture`） | `"panic in the recoverable goroutine"` `[r=<msg>] ["stack trace"="<bt>"]`（`r` 与 `"stack trace"` 对齐 `probe` `4F [00]`） |
| `crates/mysql-protocol/src/conn.rs:420-426`（`Conn::command_loop`） | `pkg/server/conn.go:1076` `clientConn.Run` | `ERROR`（`catch_unwind_future`） | `"connection running loop panic"` `[conn=...] [session_alias=...] [lastSQL=...] [err=<msg>] [stack="<bt>"]`（对齐 `probe` `4F [01]`） |
| `tidb-server` (`D03.1`) `write_result_set`（当前 `crates/server/src/conn.rs:67-71` 按 `A1` 以 `Result::Err` 移植、尚无 `catch_unwind`） | `pkg/server/conn.go:2364` `writeResultSet` | 若后续补建恢复边界则用 `ERROR`（`catch_unwind_future`） | `"write query result panic"` `[conn=...] [session_alias=...] [lastSQL=...] [stack="<bt>"] [recover=<msg>]` |
| `crates/store/src/copr/coprocessor.rs:1470-1475, 1738-1741` | `pkg/store/copr/coprocessor.go:1170-1172, 1258-1260` | `ERROR`（1475） / `WARN`（1741）（`catch_unwind_future`） | `"copIteratorWork meet panic"` **`[r=<msg>] ["stack trace"="<bt>"]`**（键名严格对齐 Go `zap.Any("r", r)` 与 `zap.Stack("stack trace")`） |
| `crates/executor/src/{projection.rs:583,676,755, sortexec/sort.rs:1084,1241, sortexec/parallel_sort_worker.rs:353, sortexec/sort_util.rs:171, aggregate/agg_util.rs:130}` | `projection.go:488` / `sort_util.go:52` / `agg_util.go:93` | **均为 `WARN`**（`catch_unwind_future` / `catch_unwind_capture`） | `projection.go:488` 用 `zap.String("error", ...)`（无 `errorVerbose`）；`sort_util.go:52` 与 `agg_util.go:93` 用 `zap.Error(err)`；均严禁从 `Ok(Err(...))` 读回溯槽 |
| `crates/planner/src/core/physical_plans.rs:1808`（`safe_clone`）、`crates/tablecodec/src/rowcodec/common.rs:281`（`append_datum_for_checksum`） | 内部值转换边界 | 静默不打日志（`catch_unwind_silent`） | 仅压制全局 hook 避免误报 `[FATAL]` |

3. **仅允许裸写 stderr/stdout 的 3 类极窄例外（与 Go v8.5.8 相同）**：
   - `SIGUSR1` 触发的任务/线程栈转储（对应 `pkg/util/signal/signal_posix.go:40`）；
   - 日志器尚未初始化前的 CLI 校验与 `--config-check` 输出（对应 `pkg/config/config.go:1200,1222,1234-1235,1239`，即当前 `crates/base/src/config/mod.rs:692-734`）；
   - 进程退出前 `syncLog()` 刷新日志器自身失败的最后兜底报错（对应 `cmd/tidb-server/main.go:349`）。
   除上述 3 处外，任何生产路径禁止调用 `println!` / `eprintln!` 或向 stdout/stderr 打印未编码的多行堆栈。

---

## 3. 日志后端架构、分层（N0）与配置/CLI 接线

### 3.1 TiKV `tikv_util::logger` 复用评估（结论：不直接复用）

直接审阅 `/home/admin/work/tikv-src/components/tikv_util/src/logger/{mod.rs,formatter.rs,file_log.rs}` 后，确认存在以下 **8 项明确不兼容**：

1. **反斜杠 `\` 漏判转义**：`formatter.rs:46-53` 的 `need_json_encode` 仅检查 `<= 0x20`、`"`、`=`、`[`、`]`，漏掉了 `\`（`0x5c`），导致含反斜杠但无空格的值不加引号且不转义（`a\b` 无法与 `"a\\b"` 字节对齐）。
2. **空消息输出 `[]` 且 JSON 键序相反**：`mod.rs:642-645` 对空消息无条件写入 `[]`（Go 省略整段）；`json_format`（`mod.rs:235-244`）将 `"message"` 放首位而 Go 把 `"level"` 放首位。
3. **`disable-timestamp` 遗留行首空格**：`mod.rs:602-612` 在关闭时间戳时仍输出 `" "`，导致行首出现多余前导空格。
4. **`slog` 绑定字段与调用点字段逆序**：`mod.rs:658-660` 先序列化调用点 `record.kv()`，再序列化头插链表 `OwnedKVList`（`mod.rs:822` 实测可见），与 Go（绑定字段在前、调用点在后、正序排列）完全相反。
5. **强行附加 `[thread_id=...]`**：`ThreadIDrain`（`mod.rs:667-683`）在每条日志中强行注入 `thread_id`，Go TiDB 不输出该字段。
6. **无 `errorVerbose`、`AddStacktrace(FatalLevel)` 与 `ErrVerboseLogger`**：`slog` 仅按 `Display` 展开错误，无法基于 `disable-error-stack` 与 `ErrVerboseLogger` 动态切换 `[errorVerbose=...]`。
7. **文件轮转能力缺失**：`file_log.rs:54-55` 注释明确不支持压缩（缺 `compression = "gzip"`）；缺少 `is-buffered` / `buffer-size` / `buffer-flush-interval` 异步缓冲刷新与 `log.timeout` 写阻塞超时保护。
8. **违反 `N0` 分层与 ADR 0007 约束**：`tikv_util::logger` 依赖 `slog_async`、`slog_term`、`slog_json`、`tikv_util::worker` 等重型 TiKV 运行时，无法放入 rank 1 正式 crate，也不符合“不依赖 TiKV 内部实现（除 ADR 0007 允许的字节格式外）”原则。

**架构结论（维护者已定）**：不复用 `tikv_util::logger`；仅借鉴其 `RotatingFileLogger` 按大小/份数/天数的精简轮转思路，在 `tidb-log` 中自建轻量统一编码器与文件 sink。

### 3.2 `N0` 分层落位与 `N0 §3` 关系说明

> **与 `N0 §3` 的关系说明**：`specs/exceptions/N0-crate-layering/spec.md` §3 规定“不建 `util` 这种大杂烩 crate”。本 spec 新建的 **`tidb-log`（rank 1）** 并非 `util` 杂物箱，而是与同层 `tidb-memory`（`util/memory`）、`tidb-runtime`（`util/tokenlimiter`）及 rank 0 `tidb-metrics` 完全同构的**单一职责日志与版本输出后端 crate**——其独立成 crate 可避免将 `flate2`、`chrono`、`tracing-subscriber`、`slog`、`base64`、`serde_json` 引入全仓共同依赖的 rank 0 `tidb-base`；而在 `tidb-base`（rank 0）仅保留零新增外部依赖的结构化日志契约（`LogField`、`StructuredLogger` trait + `OnceLock` 全局注册表、rank-0 结构化宏）与 `panic_guard`。Batch 0 落地时将同步更新 `layers.toml`（新增 `[crate.tidb-log] rank = 1 blocks = []`）与 `specs/exceptions/N0-crate-layering/spec.md` §3 分层表（如评审认为该 `N0 §3` 补充需按宪章第九条 1(b) 一并确认，见 §9.3 备注）。

| 层（rank） | crate | 职责与边界 | 依赖校验（`N0` / `Cargo.lock`） |
|---|---|---|---|
| **rank 0** | `tidb-base`（已有，不改 rank） | 定义零新增外部依赖的结构化日志门面 `tidb_base::structured_log`（`LogLevel`、`LogTarget`、`LogField<'a>`、`StructuredLogger` trait、`OnceLock` 注册表、默认初始化前 stdout 单行回退，以及跨全层可用的结构化日志宏 `log_debug!`/`log_info!`/`log_warn!`/`log_error!`/`log_fatal!`）与 `tidb_base::panic_guard`；补充 `Error::has_stack()`、`Error::error_verbose()`、`Error::error_stack()`；补全 `gen_with_stack`/`fast_gen` 脱敏；重构 `terror::{must_nil, call, log}`；提供 `LogConfig`（已有）与版本常量 | 零新增 `Cargo.lock` 依赖；无向上依赖 |
| **rank 1** | **`tidb-log`**（新建 `crates/log`） | 承载 `D00.3` 日志与版本打印后端：`UnifiedTextEncoder`、`UnifiedJsonEncoder`（直接复用 `tidb_base::gostd::{time::duration_string, strconv::format_float}`）、轮转文件与可选缓冲/gzip/超时保护 sink、实现并注册 `StructuredLogger` + `log`/`tracing`/`slog_global` 三桥接适配器、运行时 `set_level` / `replace_logger` / `sync`、`ErrVerboseLogger`、`GeneralLogger` 与 `SlowQueryLogger` sink 预留、全局 `set_panic_hook`、`PrintTiDBInfo` / `GetTiDBInfo`（含 `build.rs` 版本注入） | 仅依赖 `tidb-base`（rank 0）及 `Cargo.lock` 中已有外部 crate（`log`、`tracing`、`tracing-subscriber`、`slog`、`slog-global`、`chrono`、`flate2`、`base64`、`serde_json`、`parking_lot`、`arc-swap`）；绝不依赖 `third_party/`；在 `layers.toml` 登记 `[crate.tidb-log] rank = 1 blocks = []` |
| **rank 1..15** | 业务 crate（`tidb-memory`, `tidb-runtime`, `tidb-types`, `tidb-store`, `tidb-planner`, `tidb-executor`, `tidb-session` 等） | 全部 Go 移植日志点统一通过 rank-0 `tidb-base` 结构化日志宏发出（直接支持 `&tidb_base::Error`、`&[u8]`、`Duration` 与含空格字符串字面量键，因此 **rank 1 的 `tidb-memory` 与 `tidb-runtime` 无需依赖 `tidb-log`** 即可合法输出 `"connection ID"` 与 `"stack trace"`）；rank >= 7 需要 `LogContext`（`conn`/`session_alias`）、`GeneralLogger` 或 `replace_logger` 的 crate（如 `tidb-sessionctx`、`tidb-session`）可合法向下依赖 `tidb-log`（rank 1） | 100% 满足 `dep.rank < self.rank`（`scripts/check_layers.py`） |
| **rank 16 / legacy** | `tidb-server` / `poc-server` | 启动时调用 `tidb_log::init_logger(...)`、`tidb_log::set_panic_hook()`、`tidb_log::print_tidb_info()`；接线 `POST /settings` `log_level`、优雅关闭 `sync()`、`general_log` 与 `log_wrappers` 脱敏回调 | 在进程入口完成统一注册 |

### 3.3 统一核心与多门面直连路由架构

当前 workspace（含外部与 vendored 依赖）存在三套外部/历史日志门面：`log`（15 个正式 crate + `poc-session` + `client-rust` 137 处）、`tracing`（`tidb-memory`、`tidb-runtime` + `poc-domain`/`poc-server`/`tidb-mysql-protocol`）、以及 `slog-global`（`third_party/tikv` 2 处 + `third_party/tikv-shims` 1 处）。

- **备选方案对比**：
  - **否决方案 A（`tracing` 作为底层 + `tracing-log`）**：`tracing::*!` 宏语法严格要求标识符键（`$($k:ident).+`），无法表达 `"connection ID"` / `"stack trace"` / `"affected rows"` 等带空格键名，且 `tracing-log` 会丢失精确值类型与 `tidb_base::Error` 原始字节。
  - **否决方案 B（仅靠 `log = { features = ["kv", "std"] }` 承载全仓错误与字节串）**：经核对 `log-0.4.34` 与 `Cargo.lock`，`value-bag`（`kv_std` 所需）**不在当前 `Cargo.lock` 中**；无 `kv_std` 时 `key:err = err` 无法编译、`visit_error` 不存在、`Value::downcast_ref` 恒返回 `None`，且即使引入 `value-bag`，`log::kv::Value` 也没有无损 `&[u8]`（`ByteString`）变体，经 `Display` 会提前 `from_utf8_lossy` 破坏非 UTF-8 `\ufffd` 字节对齐并丢弃 `errorVerbose`。
  - **推荐方案（rank-0 `tidb_base::structured_log` 主门面 + `tidb-log` `UnifiedLoggerCore` + 三外部门面直连适配器，零新增 `Cargo.lock` 依赖）**：
    1. **第一方主入口（`tidb_base::structured_log` -> `OnceLock<&'static dyn StructuredLogger>`）**：按 `N0 §4` 规则 3（全局 `OnceLock` 注册表），`tidb-base`（rank 0）定义强类型 `LogField<'a>` 枚举（`Null`、`Bool`、`I64`、`U64`、`F64`、`Str(&'a str)`、`Bytes(&'a [u8])`、`Binary(&'a [u8])`、`Duration(i64)`、`Time`、`Error(&'a tidb_base::Error)`、`Strings(&'a [&'a str])`、`JsonBytes(&'a [u8])` 等）、路由标签 `LogTarget`（`Main` / `ErrVerbose` / `General` / `Fatal`）以及支持任意字符串字面量键（含空格）的轻量宏。`tidb_log::init_logger` 注册全局实现后，同一套 `UnifiedLoggerCore` 原生接收 `&tidb_base::Error`（同时导出原始字节 `[error=...]` 与 `[errorVerbose=...]`）与任意 `&[u8]`，且 `ErrVerboseLogger` 共享同一个全局原子句柄（带 `force_error_verbose = true` 标记），**天然修复 Go `ReplaceLogger` 漏同步 `errVerboseLogger` 的缺陷**。此外，在 `init_logger` 调用前，`tidb-base` 默认挂载一个极简同步单行 stdout 回退输出器（对齐 Go `pingcap/log/log.go:37-41` 进程启动默认 stdout logger），确保 `initialize_config` / `Config::valid()` 期间的警告（`crates/base/src/config/mod.rs:722,863,874`）不会被静默丢入 `NopLogger`。
    2. **`log` 外部直连适配器（`log::set_logger`）**：供外部依赖（尤其是 `client-rust` 137 处 `log::*!`）及迁移过渡期的旧调用直达 `UnifiedLoggerCore`；仅使用 `Cargo.lock` 已有的 `log = "0.4"`（可选开启 `kv` 特性支持基本整型/浮点/布尔/字符串字段，不引入 `value-bag`）。
    3. **`tracing` 外部直连适配器（`tracing_subscriber::Layer`）**：`UnifiedTracingLayer` 捕获外部 crate 及 Batch 4 过渡期 legacy `poc-*` / `tidb-mysql-protocol` 的 `tracing::*!` 事件，按声明顺序抽取字段送入 `UnifiedLoggerCore`（并支持线程/任务局部 `LogContext` 绑定 `conn`/`session_alias` 与全局 `keyspaceName`）。Batch 1 中 `tidb-memory`（rank 1）与 `tidb-runtime`（rank 1）的 Go 移植日志点切换至 rank-0 `tidb-base` 结构化宏，彻底解除 `tracing` 对含空格键名的语法限制。
    4. **`slog_global` 外部直连适配器（`slog_global::set_global(BridgeDrain)`）**：极简 `BridgeDrain: slog::Drain` 反转 `OwnedKVList` 头插链表以对齐 Go 绑定字段正序，再写调用点 `record.kv()`，桥接 `third_party/tikv`（2 处）与 `third_party/tikv-shims`（1 处）。**关键规则：严禁调用 `slog_global::redirect_std_log(...)`**，防止与 `log::set_logger` 冲突或递归死循环。
    5. **`log_wrappers::Value` 脱敏接线（`third_party/tikv-shims/log_wrappers/src/lib.rs`）**：`third_party/tikv-shims/` 属于本项目自建胶水层（不同于上游 vendored `third_party/tikv/`，遵循 `CLAUDE.md` 规则 7 不改 vendored 源码）；Batch 4 时仅在 `log_wrappers` 暴露零依赖脱敏读取原子/回调钩子，由进程入口 `tidb-server` / `poc-server` 注入 `tidb_base::errors::redact_log_enabled_load()`（`tidb-log` 自身绝不引入任何 `third_party/` 依赖，保持 `[third_party.users]` 零新增）。

### 3.4 完整配置键、CLI 参数与动态热更新对照表

以下全部配置字段与默认值已在 `crates/base/src/config/types_gen.rs:914-1149`、`defaults_gen.rs:42-81` 及 Go `pkg/config/config.go:474-585,939-953`、`cmd/tidb-server/main.go:95-225,556-567` 核对：

| 配置路径（TOML / JSON） | 类型与 Go 默认值 | 校验与运行语义（`tidb-base` + `tidb-log`） | 对应 CLI / 动态热更新 |
|---|---|---|---|
| `log.level` | `string` = `"info"` | 支持 `debug`/`info`/`warn`/`error`/`dpanic`/`panic`/`fatal`（大小写不敏感，经 `zap::Level::UnmarshalText`） | `-L <level>`；动态 `POST /settings` 表单 `log_level=<lv>` |
| `log.format` | `string` = `"text"` | 仅 `"text"`、`""`（等价 `"text"`）或 `"json"`；其他值 `InitLogger` 报错 | 启动配置 |
| `log.disable-timestamp` / `log.enable-timestamp` | `NullableBool` = `nbUnset` | 默认开启时间戳（`disable = false`）；两者同为 `true`/`false` 时经初始化前默认 stdout 回退日志器打警告（不丢失，对齐 Go `pingcap/log/log.go:37-41`）并用 `enable-timestamp` 覆盖 | 启动配置 |
| `log.disable-error-stack` / `log.enable-error-stack` | `NullableBool` = `nbUnset` | 默认抑制错误堆栈（`disable = true`）；两者同为 `true`/`false` 时经初始化前默认 stdout 回退日志器打警告并用 `enable-error-stack` 覆盖 | 启动配置（`ErrVerboseLogger` 始终输出） |
| `log.file.filename` | `string` = `""` | 空串写 `stdout`；非空先确保父目录存在并验证非目录且可写（`log.go:194-222`） | `--log-file <path>` |
| `log.file.max-size` | `isize` = `300` (MB) | `0` 回退 `300` MB；超过 `MaxLogFileSize = 4096` 在 `Config::valid()` 报错 | 启动配置 |
| `log.file.max-days` | `isize` = `0` | `0` 不按天清理；`> 0` 在初始化及每次轮转后异步清理超过 `max-days` 的旧归档 | `SET GLOBAL tidb_log_file_max_days = N`（调用 `replace_logger`） |
| `log.file.max-backups` | `isize` = `0` | `0` 不限份数；`> 0` 按时间戳倒序保留最新 `max-backups` 份 | 启动配置 |
| `log.file.compression` | `string` = `""` | `""` 不压缩；`"gzip"` 轮转后用 `flate2` 异步压缩为 `.gz` 并删除原旧文件；其余值报错 | 启动配置 |
| `log.file.is-buffered` | `bool` = `false` | `true` 启用内存写缓冲（默认容量/自定义 `buffer-size`）+ 定时刷盘协程/任务 | 启动配置 |
| `log.file.buffer-size` | `isize` = `0` | 缓冲区字节大小（`0` 时 zap `BufferedWriteSyncer` 用默认 256 KiB） | 启动配置 |
| `log.file.buffer-flush-interval` | `Duration` = `0` | 缓冲自动刷新间隔（`0` 时 zap 用默认 30s）；进程退出时 `sync()` 强制落盘 | 启动配置 |
| `log.slow-query-file` | `string` = `"tidb-slow.log"` | 空则退回主文件；**本 spec 仅保留配置与 `slow_query_log` sink 预留接口，格式归 `D14.2`** | `--log-slow-query <path>`；会话变量 `tidb_slow_query_file` |
| `log.general-log-file` | `string` = `""` | 空则共享主日志配置（独立于 `log.level` 始终以 `INFO` 记录）；非空路由至独立轮转文件 | `--log-general <path>`；`SET GLOBAL tidb_general_log = ON\|OFF` |
| `log.timeout` | `isize` = `0` (秒) | `> 0` 时对写锁与刷盘施加秒级超时保护，超时触发 panic（`log.go:111-114,164-182`） | 启动配置 |
| `log.expensive-threshold` / `query-log-max-len` / `enable-slow-log` / `slow-threshold` / `record-plan-in-slow-log` | 见 `types_gen.rs:923-928` | 已移入 `[instance]` 或已废弃（`config.go:1145-1159`）；仅保留反序列化与升级迁移兼容（`setGlobalVars`） | — |
| `keyspace-name`（根配置） | `string` = `""` | 非空时在初始化和 `replace_logger` 为根 logger 绑定 `[keyspaceName=<name>]`（`pkg/keyspace/keyspace.go:63-71`） | `--keyspace-name <name>` |
| 脱敏恢复子命令 | CLI | `./tidb-server [--redact] collect-log <input> [output]`（`main.go:263-270` -> `tidb_base::redact::de_redact_file`，已移植） | `--redact` + `collect-log` |

**`replace_logger` 行为细节**：执行 `SET GLOBAL tidb_log_file_max_days = N` 后，`replace_logger` 用更新后的 `LogConfig` 重建主 sink、`GeneralLogger`（与预留 `SlowQueryLogger`），切换全局 logger 原子句柄并记录 `[INFO] ["replaced global logger with config: <json>"]`（`logutil/log.go:214`）。注意先刷出旧缓冲再释放句柄。

---

## 4. 运维高频优先日志逐字节清单与编译期版本注入

### 4.1 启动横幅 `["Welcome to TiDB."]`、`["loaded config"]` 与编译期版本注入

1. **Go v8.5.8 调用链（已核对 `cmd/tidb-server/main.go:273-277,646-657,853-859`、`pkg/util/printer/printer.go:38-87`、`pkg/util/versioninfo/versioninfo.go`、`Makefile.common:71-80`）**：
   - `printInfo()` 先保存当前日志级别、临时切换为 `INFO`、调用 `printer.PrintTiDBInfo()`、再恢复级别（保证 `-L warn` 或 `-L error` 时依然打印启动横幅和配置）。
   - `PrintTiDBInfo()` 先输出 `"Welcome to TiDB."`，再输出 `"loaded config"` `[config="<json>"]`（使用 `tidb_base::config::get_json_config()`）。
2. **Rust `build.rs` 编译期注入方案与版本字段契约（按 2026-11-27 负责人决定）**：
   - 在 `crates/log/build.rs`（继承 `crates/poc/domain/build.rs` 思路并扩展）注入以下编译期环境变量，同时允许外部打包脚本通过同名环境变量覆盖（与 Go `Makefile.common` `-ldflags -X` 对等）：
     - `TIDB_RS_RELEASE_VERSION`：未覆盖时取默认发布版本串（见 §9.2 **D1** 决定 `v8.5.8-tidb-rs-<7位>` 或 `v8.5.8`）；可被配置项 `tidb-release-version` 在运行时覆盖；
     - `TIDB_RS_GIT_HASH`：`git rev-parse HEAD`（失败回退 `"None"`），需监听 `.git/HEAD` 与 `.git/logs/HEAD`（兼容 git worktree）；
     - `TIDB_RS_GIT_BRANCH`：`git rev-parse --abbrev-ref HEAD`（失败回退 `"None"`）；
     - `TIDB_RS_BUILD_TS`：UTC 构建时间戳 `YYYY-MM-DD HH:MM:SS`（`date -u` 或 `chrono`/纯算法转换 `SOURCE_DATE_EPOCH`/当前 UTC，失败回退 `"None"`）；
     - `TIDB_RS_RUST_VERSION`：执行 `env::var("RUSTC")` `--version` 的第一行输出（如 `rustc 1.93.0 (...)`，失败回退 `"unknown"`）。
   - **字段顺序与负责人拍板格式**：
     1. `"Release Version"` = `<release_version>`
     2. `Edition` = `config.tidb_edition` 非空时取之，为空时回退编译期 `versioninfo::TIDB_EDITION`（默认值 `"Community"`；对照 `crates/base/src/config/defaults_gen.rs:39` 与 `config/mod.rs:1286`；企业版扩展暂不管）
     3. `"Git Commit Hash"` = `<git_hash>`
     4. `"Git Branch"` = `<git_branch>`
     5. `"UTC Build Time"` = `<utc_build_time>`
     6. **`GoVersion` = `""`（固定留空，文本输出 `[GoVersion=]`）**
     7. **`"Rust Version"` = `<rustc_version>`（紧跟 `GoVersion` 之后新增）**
     8. `"Race Enabled"` = `false`（bool）
     9. `"Check Table Before Drop"` = `false`（bool，读自 `config::CheckTableBeforeDrop`）
     - 注：与 Go 一致，`Store` **不出现在** `Welcome to TiDB.` 日志行内，仅出现在 `GetTiDBInfo()`（供 `-V` 与 `SELECT tidb_version()` 使用，`pkg/util/printer/printer.go:61-87` 输出 9 行键值对，其余 8 项同上；是否同步保留空 `GoVersion:` 并加 `Rust Version:` 见 §9.2 **D1**）。
   - **清理当前 `tidb-rs` 多处版本口径割裂**：
     - 目前存在：`crates/poc/domain/src/serverinfo.rs:16-50`（`8.0.11-TiDB-v8.5.8-tidb-rs-<7位>`）、`crates/mysql-protocol/src/constants.rs:161`（过期 `8.0.11-TiDB-v8.4.0-tidb-rs`）、`crates/base/src/mysql/const.rs:36-48`（未接线的 `v8.4.0-this-is-a-placeholder`）、`crates/poc/session/src/constant.rs:19-20` 与 `crates/sessionctx/src/variable/sysvar_data.rs:23,25` 不一致的 `version` / `version_comment`，以及 `crates/poc/server/src/main.rs:200-206`（`tidb_metrics::init`）与 `crates/metrics/src/lib.rs:520`（`tidb_server_info` 指标标签）。
     - 实施 Batch 0 时统一由 `tidb-log` 提供的版本构建结果初始化 `tidb_base::mysql::{TiDBReleaseVersion, ServerVersion}`，同步更新 `crates/mysql-protocol/src/constants.rs:161`、`crates/poc/domain/src/serverinfo.rs`、`crates/sessionctx/src/variable/sysvar_data.rs:25`、`crates/poc/server/src/main.rs:200-206`（`tidb_metrics::init` -> `tidb_server_info`）与 `/status` JSON（`crates/poc/server/src/status.rs:219`）；具体后缀是否保留 `-tidb-rs-<short>` 见 §9.2 **D1**。
3. **Go v8.5.8 启动阶段实测黄金样本（`tidb-server --store=unistore`）**：

```text
[2026/11/27 01:23:37.272 +00:00] [INFO] [printer.go:52] ["Welcome to TiDB."] ["Release Version"=v8.5.8] [Edition=Community] ["Git Commit Hash"=8b857efa20363d50a8fa2ea7dd9809a85a61b115] ["Git Branch"=HEAD] ["UTC Build Time"="2026-08-27 07:22:26"] [GoVersion=go1.25.12] ["Race Enabled"=false] ["Check Table Before Drop"=false]
[2026/11/27 01:23:37.272 +00:00] [INFO] [printer.go:57] ["loaded config"] [config="{\"host\":\"127.0.0.1\",\"advertise-address\":\"127.0.0.1\",\"port\":14001,\"cors\":\"\",\"store\":\"unistore\",...}"]
[2026/11/27 01:23:38.349 +00:00] [INFO] [http_status.go:102] ["for status and metrics report"] ["listening on addr"=127.0.0.1:11081]
[2026/11/27 01:23:38.351 +00:00] [INFO] [server.go:315] ["server is running MySQL protocol"] [addr=127.0.0.1:14001]
[2026/11/27 01:23:38.351 +00:00] [INFO] [server.go:329] ["server is running MySQL protocol"] [socket=/tmp/tidb-golden-run2/tidb.sock]
```

*(若开启 TLS，`pkg/server/server.go:295-296` 还在 MySQL 监听器之前输出 `[INFO] [server.go:295] ["mysql protocol server secure connection is enabled"] ["client verification enabled"=<bool>]`。)*

### 4.2 优雅关闭序列

经核对 `cmd/tidb-server/main.go:327-352,901-932`、`pkg/server/server.go:590-639,989-1072`、`pkg/domain/domain.go:1254-1321` 及实测 `SIGTERM` 捕获：**Go v8.5.8 不存在 `"tidb-server exits"` 这条日志**（全库检索 0 匹配），最后一条退出日志在清理完成后由 `cpuprofile` 写出（`[cpuprofile.go:130] ["parallel cpu profiler stopped"]`）。

`tidb-rs` 在只读节点上不运行 DDL owner（`X`）、`ttlJobManager`（`X`）及常驻 `cpuprofile`（`N9` §4 已定不需要常驻采样 profiler），因此**不伪造未运行子系统的退出日志**，但严格逐字节对齐以下主关闭序列：

1. **信号接收**（`signal_posix.go:54`）：`INFO` `"got signal to exit"` `[signal=<terminated|interrupt|hangup|quit>]`；
2. **摘除健康检查**（`server.go:591`）：`INFO` `"setting tidb-server to report unhealthy (shutting-down)"`（若配置 `graceful-wait-before-shutdown > 0` 则再记 `server.go:597` `"waiting for stray connections before starting shutdown process"` `[waitTime=...]`）；
3. **关闭监听与排空连接**（`server.go:1014,1060,1062,990,1070`）：
   - `INFO` `"start drain clients"`
   - 全部退出：`INFO` `"all sessions quit in drain wait time"`；否则 `INFO` `"timeout waiting all sessions quit"`
   - 强制清理：`INFO` `"kill all connections."` `[category=server]`；若 1s 内仍未退出：`WARN` `"some sessions do not quit in cancel wait time"`；
   - 注：关闭监听期间 status HTTP/gRPC accept 结束若报关闭错误，如与 Go `http_status.go:535-545` 一致则按 `[WARN]` 记录 `"start status/rpc server error"` / `"http server error"` / `"grpc server error"`。
4. **Domain / SchemaSync 关闭**（`domain.go:1145,1320`）：Schema sync 退出及注销后输出 `INFO` `"domain closed"` `["take time"=...]`；
5. **刷盘**：调用 `tidb_log::sync()` 退出。

Go v8.5.8 实测 `SIGTERM` 核心捕获片段：

```text
[2026/11/27 01:26:00.174 +00:00] [INFO] [signal_posix.go:54] ["got signal to exit"] [signal=terminated]
[2026/11/27 01:26:00.174 +00:00] [INFO] [server.go:591] ["setting tidb-server to report unhealthy (shutting-down)"]
[2026/11/27 01:26:00.175 +00:00] [INFO] [server.go:1014] ["start drain clients"]
[2026/11/27 01:26:00.175 +00:00] [INFO] [server.go:1060] ["all sessions quit in drain wait time"]
[2026/11/27 01:26:00.175 +00:00] [INFO] [server.go:990] ["kill all connections."] [category=server]
... (schema sync / domain 后台循环退出) ...
[2026/11/27 01:26:00.179 +00:00] [INFO] [domain.go:1320] ["domain closed"] ["take time"=4.636049ms]
```

### 4.3 命令分发错误 `["command dispatched failed"]` 与伴随 `session.go` 日志

`pkg/server/conn.go:1222-1230` 使用带连接上下文的 `logutil.Logger(ctx)`（预先通过 `.With` 绑定 `conn` 和 `session_alias`），严格按以下 9 个字段顺序输出：

1. `conn`（`u64`，来自绑定上下文）
2. `session_alias`（`string`，来自绑定上下文，空串时 `[session_alias=]`）
3. `connInfo`（`cc.String()`，`conn.go:226-234`：`"id:%d, addr:%s status:%b, collation:%s, user:%s"` —— **注意 `status:%b` 为二进制表示**，如 `10` 表示 autocommit，`11` 表示 inTxn+autocommit）
4. `command`（`mysql.Command2Str[cmd]`，如 `Query`、`Execute`）
5. `status`（`cc.SessionStatusToString()`：`"inTxn:%d, autocommit:%d"`）
6. `sql`（`parser.Normalize(sql, redact_mode)` → `executor.FormatSQL(sql)`：超 `QueryLogMaxLen` 默认 `4096` 截断并附 `(len:%d)`，且经 `QueryReplacer` 把 `\r`/`\n`/`\t` 替换为单个空格 `' '`）
7. `txn_mode`（`GetReadableTxnMode()`，如 `PESSIMISTIC`、`OPTIMISTIC`）
8. `timestamp`（`u64`，`TxnCtx.StartTS` 非零取之，否则 `StaleReadTs` 非零取之，否则 `0`）
9. `err`（`errStrForLog(err, EnableRedactLog)`，注意键名是 **`err`** 而非 `error`！）

**分支级别与 `errStrForLog` 矩阵**（`conn.go:1138-1257, 2349-2365` & `session.go:1590,2134,2176`）：

- `readPacket` `io.EOF` / `ComQuit`：静默返回；
- `readPacket` 超时：`connStatusWaitShutdown` → `INFO` `"read packet timeout because of killed connection"`；空闲 `wait_timeout` → `INFO` `"read packet timeout, close this connection"` `[idle=...] [waitTimeout=...] [error=...]`；
- `readPacket` 其他 I/O 错（排除 `"use of closed network connection"`）：`WARN` `"read packet failed, close this connection"` `[error=...]`（`SuspendStack` 无栈）；
- `terror.ErrResultUndetermined`：`WARN` `"result undetermined, close this connection"` `[error=...]` 后关闭连接；
- `terror.ErrCritical`：**`FATAL`** `"critical error, stop the server"` `[error=...]` 并中止进程；
- `ErrLockAcquireFailAndNoWaitSet`（`[tikv:3572]`）：**`DEBUG`** `"Expected error for FOR UPDATE NOWAIT"` `[error=...]`（跳过 `command dispatched failed`）；
- 其余 `dispatch` 错误：**`INFO`** `"command dispatched failed"`（9 字段）；
  - `errStrForLog`：`redact != "OFF"` 且为 `ErrParse` → 固定串 `"fail to parse SQL, and must redact the whole error when enable log redaction"`；`ErrKeyExists` / `ErrParse` / `ErrTableNotExists` → 仅 `err.Error()`（无堆栈）；其余错误 → `errors.ErrorStack(err)`（有栈则打印，无栈哨兵错如 `ErrLockWaitTimeout` 只打印消息）；
- 伴随 `session.go` 日志：
  - 语法错（非内部 SQL）：`WARN` `[session.go:1590]` `"parse SQL failed"` `[conn=...] [session_alias=...] [error=...] [SQL=redact.String(mode, sql)]`（**保留原始 `\n`**，`"ON"` 时 `[SQL=]`）；
  - 编译错（非 `ErrUnknownSystemVar` 且非内部 SQL）：`WARN` `[session.go:2134]` `"compile SQL failed"` `[conn=...] [session_alias=...] [error=...] [SQL=parser.Normalize(sql, mode)]`；
  - 执行打开阶段错（排除 `ErrKeyExists`、`ErrLockAcquireFailAndNoWaitSet` 等噪声错误）：`WARN` `[session.go:2176]` `"run statement failed"` `[conn=...] [session_alias=...] [schemaVersion=...] [error=...] [session=...]`。

**Go v8.5.8 实测黄金样本**（模块路径按 `CLAUDE.md` §2.6 省略仓库托管域名前缀）：

```text
# 1. 表不存在（无堆栈）
[2026/11/27 01:22:37.957 +00:00] [WARN] [session.go:2134] ["compile SQL failed"] [conn=2097158] [session_alias=] [error="[schema:1146]Table 'test.no_such_table' doesn't exist"] [SQL="SELECT * FROM test.no_such_table"]
[2026/11/27 01:22:37.958 +00:00] [INFO] [conn.go:1222] ["command dispatched failed"] [conn=2097158] [session_alias=] [connInfo="id:2097158, addr:127.0.0.1:52926 status:10, collation:utf8mb4_0900_ai_ci, user:root"] [command=Query] [status="inTxn:0, autocommit:1"] [sql="SELECT * FROM test.no_such_table"] [txn_mode=PESSIMISTIC] [timestamp=0] [err="[schema:1146]Table 'test.no_such_table' doesn't exist"]

# 2. 多行 SQL 语法错（session.go 保留 \n，conn.go 折叠为空格）
[2026/11/27 01:22:37.964 +00:00] [WARN] [session.go:1590] ["parse SQL failed"] [conn=2097160] [session_alias=] [error="[parser:1064]You have an error in your SQL syntax; check the manual that corresponds to your TiDB version for the right syntax to use line 3 column 7 near \"FROM nonexistent\" "] [SQL="SELECT\n  1,\n  FROM nonexistent"]
[2026/11/27 01:22:37.964 +00:00] [INFO] [conn.go:1222] ["command dispatched failed"] [conn=2097160] [session_alias=] [connInfo="id:2097160, addr:127.0.0.1:52928 status:10, collation:utf8mb4_0900_ai_ci, user:root"] [command=Query] [status="inTxn:0, autocommit:1"] [sql="SELECT   1,   FROM nonexistent"] [txn_mode=PESSIMISTIC] [timestamp=0] [err="[parser:1064]You have an error in your SQL syntax; check the manual that corresponds to your TiDB version for the right syntax to use line 3 column 7 near \"FROM nonexistent\" "]

# 3. 多行运行时溢出错误（err 带转义堆栈）
[2026/11/27 01:22:37.970 +00:00] [INFO] [conn.go:1222] ["command dispatched failed"] [conn=2097162] [session_alias=] [connInfo="id:2097162, addr:127.0.0.1:52940 status:10, collation:utf8mb4_0900_ai_ci, user:root"] [command=Query] [status="inTxn:0, autocommit:1"] [sql="SELECT   9223372036854775807 + 1"] [txn_mode=PESSIMISTIC] [timestamp=0] [err="[types:1690]BIGINT value is out of range in '(9223372036854775807 + 1)'\npingcap/errors.(*Error).GenWithStackByArgs\n\t/root/go/pkg/mod/.../normalize.go:177\n...\npingcap/tidb/pkg/server.(*Server).onConn\n\t/workspace/source/tidb/pkg/server/server.go:753"]

# 4. 语法错在 tidb_redact_log = ON 下整句脱敏
[2026/11/27 01:22:39.047 +00:00] [WARN] [session.go:1590] ["parse SQL failed"] [conn=2097178] [session_alias=] [error="[parser:1064]You have an error in your SQL syntax; check the manual that corresponds to your TiDB version for the right syntax to use line 1 column 55 near \"syntax here\" "] [SQL=]
[2026/11/27 01:22:39.047 +00:00] [INFO] [conn.go:1222] ["command dispatched failed"] [conn=2097178] [session_alias=] [connInfo="id:2097178, addr:127.0.0.1:52990 status:10, collation:utf8mb4_0900_ai_ci, user:root"] [command=Query] [status="inTxn:0, autocommit:1"] [sql="select * from `test` . `t_err` where `v` = ? and `bad` `syntax` `here`"] [txn_mode=PESSIMISTIC] [timestamp=0] [err="fail to parse SQL, and must redact the whole error when enable log redaction"]
```

### 4.4 慢查询监控 `[expensive_query]` 与内存超限 `["memory exceeds quota"]` / OOM 动作

1. **统一负载构造 `util.GenLogFields(costTime, info, true)`（`pkg/util/util.go:101-168`、`pkg/util/execdetails/execdetails.go:316-396,611-626`）**：
   - `tidb-execdetails`（`D14.0` 执行统计数据结构，`crates/execdetails/src/execdetails.rs:434,879`）**已移植** `ExecDetails::to_zap_fields` 与 `CopTasksDetails::to_zap_fields`（返回 `Vec<LogField>`），`tidb-log` 只需直接消费该列表；
   - **字段精确顺序**（非零/非空才输出的可选段已标出）：
     1. `cost_time`（`"<float>s"`，必出）
     2. `execDetail.ToZapFields()`：`cop_time`, `process_time`, `wait_time`, `backoff_time`, `request_count`, `total_keys`, `process_keys`（及写路径事务提交细节；**无 `rocksdb_*` 字段**）
     3. `copTaskInfo.ToZapFields()`（`num_cop_tasks > 0` 时）：`num_cop_tasks`, **`process_avg_time`**, **`process_p90_time`**, **`process_max_time`**, **`process_max_addr`**, **`wait_avg_time`**, **`wait_p90_time`**, **`wait_max_time`**, **`wait_max_addr`**（**纠偏**：与慢日志 `cop_proc_*` 键名不同！）
     4. `stats`（非空时，`t:ver` 或 `t:pseudo`）
     5. `conn`（`info.ID != 0` 时）
     6. `user`（非空时）
     7. `database`（非空时）
     8. `table_ids`（非空时，如 `"[112]"`）
     9. `index_names`（非空时）
     10. `txn_start_ts`（必出，`u64`）
     11. `mem_max`（`MemTracker != nil` 时，`"<N> Bytes (<human>)"`）
     12. `sql`（`parser.Normalize`，超 `8192` 时截断为 `"<prefix> len(<total>)"`——**无 `QueryReplacer`，保留原始 `\n`**）
     13. `session_alias`（必出）
     14. `"affected rows"`（必出，带空格，加双引号，`u64`）
2. **OOM 动作纠偏（`pkg/util/memory/action.go:114-191`、`pkg/executor/select.go:1026-1038`、`pkg/util/sqlkiller/sqlkiller.go:66,171`）**：
   - Go v8.5.8 的 `tidb_mem_oom_action` 仅含 `LOG`（`LogOnExceed`）与 `CANCEL`（`PanicOnExceed` + `SQLKiller`），**不存在 `CancelOnExceed`**；
   - 两者的 `SetLogHook` 均绑定到 `ExpensiveQueryHandle.LogOnQueryExceedMemQuota`，产出 `[WARN]` `"memory exceeds quota"` + 上述 `GenLogFields`（若 `logHook == nil` 才退回到 `crates/memory/src/action.rs` 的简单 `"memory exceeds quota"` + `[conn]`/`[error=...]`）；
   - **`crates/memory/src/sqlkiller.rs:194` 现有键名纠偏**：该处当前使用 `connection_id = ...`，必须改为 Go 一致的 **`"connection ID"=...`**。

**Go v8.5.8 实测黄金样本**：

```text
# 1. expensive_query（100ms ticker，单条按 60s 节流，多行 SQL 保留 \n）
[2026/11/27 01:25:30.249 +00:00] [WARN] [expensivequery.go:153] [expensive_query] [cost_time=10.043440202s] [cop_time=0.000204207s] [request_count=1] [num_cop_tasks=1] [process_avg_time=0s] [process_p90_time=0s] [process_max_time=0s] [process_max_addr=store1] [wait_avg_time=0s] [wait_p90_time=0s] [wait_max_time=0s] [wait_max_addr=store1] [stats=big:pseudo] [conn=2097176] [user=root] [table_ids="[112]"] [txn_start_ts=470743179645681664] [mem_max="2146 Bytes (2.10 KB)"] [sql="SELECT\n  COUNT(*),\n  SUM(b1.id + SLEEP(1.25 + b1.id * 0.0))\nFROM test.big b1"] [session_alias=] ["affected rows"=0]

# 2. OOM CANCEL（PanicOnExceed -> SQLKiller -> session -> conn.go）
[2026/11/27 01:22:45.887 +00:00] [WARN] [expensivequery.go:153] ["memory exceeds quota"] [cost_time=0.000197248s] [conn=2097198] [user=root] [txn_start_ts=0] [mem_max="2032 Bytes (1.98 KB)"] [sql="WITH RECURSIVE cte(n, s) AS (SELECT 1, REPEAT('x', 2048) UNION ALL SELECT n + 1, CONCAT(s, REPEAT('x', 2048)) FROM cte WHERE n < 64) SELECT COUNT(*), MAX(LENGTH(s)) FROM cte"] [session_alias=] ["affected rows"=0]
[2026/11/27 01:22:45.887 +00:00] [WARN] [sqlkiller.go:66] ["kill initiated"] ["connection ID"=2097198] [reason="[executor:8175]Your query has been cancelled due to exceeding the allowed memory limit for a single SQL query. Please try narrowing your query scope or increase the tidb_mem_quota_query limit and try again.[conn=2097198]"]
[2026/11/27 01:22:45.887 +00:00] [INFO] [tidb.go:328] ["rollbackTxn called due to ddl/autocommit failure"]
[2026/11/27 01:22:45.887 +00:00] [WARN] [session.go:2176] ["run statement failed"] [conn=2097198] [session_alias=] [schemaVersion=57] [error="[executor:8175]..."] [session="..."]
[2026/11/27 01:22:45.887 +00:00] [WARN] [sqlkiller.go:171] ["kill finished"] [conn=2097198]
[2026/11/27 01:22:45.887 +00:00] [INFO] [conn.go:1222] ["command dispatched failed"] [conn=2097198] [session_alias=] [connInfo="id:2097198, addr:127.0.0.1:60416 status:10, collation:utf8mb4_0900_ai_ci, user:root"] [command=Query] [status="inTxn:0, autocommit:1"] [sql="WITH RECURSIVE cte(n, s) AS ..."] [txn_mode=PESSIMISTIC] [timestamp=0] [err="[executor:8175]...\n..."]
```

### 4.5 其他运维高频日志优先清单

| 类别 | Go 位置、级别、消息与关键字段（按顺序） | `tidb-rs` 归属块与当前状态 | 优先级 |
|---|---|---|---|
| **连接生命周期与握手认证** | `server.go:518` `DEBUG` `"accept new connection success"`；`server.go:719` `DEBUG` `"new connection"` `[conn] [remoteAddr]`；`server.go:723` `DEBUG` `"connection closed"` `[conn]`；`conn.go:564` `DEBUG` `"wait handshake response fail due to connection has be closed by client-side"`；`server.go:702` `DEBUG` `"EOF"` `[conn] ["remote addr"]`；`conn.go:686` `WARN` `"failed to check the user authplugin"` `[conn] [error]`；`conn.go:677` `WARN` `"open new session or authentication failure"` `[error]`；`server.go:712` `WARN` `"Server.onConn handshake"` `[conn] [error] ["remote addr"]`；`server.go:513` `ERROR` `"accept failed"` `[error]`；`server.go:819` `ERROR` `"too many connections"` `["max connections"] [error]` | **D03.1**（`tidb-server`）+ **D04.1**（`tidb-privilege`）+ legacy `tidb-mysql-protocol`（`crates/mysql-protocol/src/{conn,server}.rs` 当前自创了英文消息与 `connection_id` 键，见 Batch 4） | **P0** |
| **写冲突、事务重试、死锁与锁等待** | `session.go:819` `WARN` `"sql"` `[label] [error] [txn]`；`session.go:829` `WARN` `"can not retry txn"` `[label] [error] [IsBatchInsert] [IsPessimistic] [InRestrictedSQL] [tidb_retry_limit] [tidb_disable_txn_auto_retry]`；`session.go:845` `WARN` `"commit failed"` `["finished txn"] [error]`；`tidb.go:328` `INFO` `"rollbackTxn called due to ddl/autocommit failure"`；`tidb.go:332` `INFO` `"rollbackTxn for deadlock"` `[txn]`；`client-go/txnkv/transaction/txn.go:1588` `DEBUG` `"deadlock error received"` `[startTS] [deadlockInfo]`（写路径事务提交日志 `2pc.go:721,737` 属 `X`） | **D05.3**（`tidb-session`）+ **D05.4**（`tidb-sessiontxn`）+ **D07.1**（`tidb-store`）；`crates/session/src/session.rs:397` 当前插值 `"commit failed: {e}"` | **P1** |
| **TSO / PD 慢与重试** | `client-go/oracle/oracles/pd.go:58,260` `WARN` `"get timestamp too slow"` `["cost time"]`（`> 30ms`）；`pd.go:275` `WARN` `"get minimum timestamp too slow"` `["cost time"]`；`pd/client/clients/tso/{dispatcher,stream}.go` `[tso] ...`；`pd/client/pkg/retry/backoff.go:83` `WARN` `"[pd.backoffer] exec fn failed and retrying"` | `client-rust` fork（137 处调用回改，补齐 `> 30ms` 慢 TSO 告警）+ **D05.3**（`crates/session/src/txn.rs:97` `wait tso failed`） | **P1** |
| **Region 错误、退避与慢 Cop** | `client-go/config/retry/config.go:190` `DEBUG` `"backoff"` `[base] [sleep] [attempts]`；`backoff.go:179` `WARN` `"<cfg> backoffer.maxSleep <N>ms is exceeded, errors:..."`；`backoff.go:229` `DEBUG` `"retry later"` `[error] [totalSleep] [excludedSleep] [maxSleep] [type] [txnStartTS]`；`pkg/store/copr/coprocessor.go:1428,1455-1499` `INFO` `"[TIME_COP_PROCESS] ..."` / `"[TIME_COP_WAIT] ..."`（含 `rocksdb_*`）；`coprocessor.go:1601,1751` `WARN` `"other error"` | **D07.2**（`tidb-store`，`crates/store/src/copr/coprocessor.rs:2002`）+ `client-rust` fork（补齐 `retry later` / `maxSleep` 结构化日志） | **P1** |
| **Schema 加载、租约与 InfoSync** | `domain.go:355` `INFO` `"diff load InfoSchema success"`；`domain.go:368` `ERROR` `"failed to load schema diff"`；`domain.go:414` `INFO` `"full load InfoSchema success"`；`domain.go:857` `WARN` `"loading schema takes a long time"` `["take time"]`；`domain.go:1125,1129` `INFO` `"mdl gets lock, update self version to owner"`；`schema_validator.go:101,112` `"the schema validator stops/restarts"`（DDL owner `pkg/ddl/`、`pkg/owner/` 属 **`X`** 不移植） | **D06.5**（`tidb-domain` `crates/domain/src/domain.rs:235..455`）+ legacy `poc-domain`（`schema_validator.rs` / `sync.rs`） | **P1** |
| **Panic 恢复** | 见 §2.3 全量映射表 | `tidb-runtime`、`tidb-mysql-protocol`、`tidb-server`、`tidb-store`、`tidb-executor` | **P0** |
| **动态变量与 logger 替换** | `executor/set.go:170` `INFO` `"set global var"` `[conn] [name] [val]`；`logutil/log.go:214` `INFO` `"replaced global logger with config: <json>"`（触发源：`pkg/sessionctx/variable/sysvar.go:449` `tidb_log_file_max_days`）；`pkg/server/handler/tikvhandler/tikv_handler.go:418`（`POST /settings` `log_level` 动态调级） | **D12.12**（`tidb-executor`）+ `tidb-sessionctx` + `tidb-server` + `tidb-log` | **P2** |
| **数据一致性诊断** | `pkg/util/logutil/consistency/reporter.go:213,237,274` `ERROR` `"indexLookup found data inconsistency"` / `"admin check found data inconsistency"` `[table_name] [index_name] ... [stack]` | `D00.3` 归并入 `tidb-log` 或留 `tidb-executor`（`crates/executor/src/consistency.rs:67` 改结构化并补 `[stack="..."]`） | **P2** |

---

## 5. 移植规则（拟写入 `CLAUDE.md` 与 `docs/porting/`）

在实现 Batch 0 时，除本任务已预先合入 `CLAUDE.md` §2 第 10 条的 `#448` 审计插件钩子注释规则外，还将把以下日志移植规则写入 `CLAUDE.md` §2 与 `docs/porting/mappings.md`（不另占现有 `M-*` 代码编号，或由维护者决定是否记为规范条文）：

1. **禁止将字段插值进消息字符串**：
   - Go 传入 `logutil.Logger(ctx).Warn("compile SQL failed", zap.Error(err), zap.String("SQL", sql))`，Rust **严禁**写成 `log::warn!("compile SQL failed: {err}, SQL={sql}")`；
   - 必须保证消息字面量原文、级别、字段键名及字段顺序与 Go 逐字一致，不允许在消息中拼入变量占位符 `{}`、`{:?}` 或 `format!(...)`。
2. **字段值与结构化类型严格对齐**：
   - **Error**：不得使用 `%err` / `{err}`（否则会丢失 `[errorVerbose=...]` 及非 UTF-8 `\ufffd` 转义）；统一通过 rank-0 `tidb_base::structured_log` 宏传递 `&tidb_base::Error`（或字段 `error` / `err`），由统一编码器按 §1.3 与 §2.2 输出。
   - **Duration**：统一按 `tidb_base::gostd::time::duration_string`（Go `time.Duration.String()`）格式化（不写 Rust `{:?}`，因两者在纳秒/毫秒/秒的进位阈值与格式上存在差异）。
   - **ByteString / Binary**：Go `zap.ByteString` 传 `LogField::Bytes(&[u8])`（经 §1.3 转义）；`zap.Binary` 传 `LogField::Binary(&[u8])`（标准 Base64 编码后按字符串规则输出，带 `=` 时自动加引号）。
   - **Redact**：涉及用户数据、SQL、索引值或 key 的日志，必须严格照 Go 调用 `tidb_base::redact::string` / `stringer` / `value` / `key`。
   - **带空格或特殊字符的字段名**（如 `"connection ID"`、`"stack trace"`、`"affected rows"`、`"listening on addr"`、`"take time"`）：`tracing::*!` 宏语法要求标识符键而无法表达含空格字段名，且无 `value-bag` 时 `log::kv` 无法携带 `&tidb_base::Error` / `&[u8]`；第一方全层代码统一使用 **`tidb-base`（rank 0）提供的结构化日志宏**（支持字符串字面量键，如 `tidb_base::log_warn!("kill initiated", "connection ID" => conn_id, "reason" => reason)`），既在 rank 0–16 全部无环可用，又便于 CI 检查脚本直接静态抽取并比对键名列表与顺序。
3. **上下文 Logger 与 `ErrVerboseLogger` 绑定**：
   - Go 调用 `logutil.Logger(ctx)` 时，Rust 侧必须通过当前任务/会话的上下文 logger 携带 `conn` 与 `session_alias`（或 `category`），不得降级为全局裸 `log::*`；
   - Go 调用 `logutil.ErrVerboseLogger()` 时，Rust 侧必须走强制开启 `errorVerbose` 的入口，确保在全局默认 `disable-error-stack = true` 下仍输出调用栈。
4. **`#448` 审计插件钩子注释（已生效）**：见 `CLAUDE.md` §2 第 10 条，凡在 `tidb-server`、`tidb-session`、`crates/poc/server` 移植碰到 Go 审计插件钩子处必须留 `// TODO(audit-plugin #448): ...`。

---

## 6. CI 日志一致性检查脚本（`scripts/check_log_parity.py`）与只减不增棘轮

脚本遵循 `scripts/check_one_translation.py`、`scripts/check_stub_ratchet.py` 与 `scripts/converge.py` 的纯 Python 3 标准库风格，配套单元测试 `scripts/test_check_log_parity.py` 及棘轮基线 `scripts/log_parity_ratchet.tsv`（支持 `--list` 查看全量问题明细与 `--write` 在消除存量偏差后单向压低基线）。

### 6.1 三层检查规则与 CI 接入点

| 规则 | 运行时机（`.github/workflows/ci.yml`） | 检查对象与判定准则 | 例外与棘轮 |
|---|---|---|---|
| **规则 A**（纯 Rust） | `check` job | 禁止分层正式 crate 生产代码出现 `println!` / `eprintln!` / `print!` / `eprint!`，以及裸 `std::panic::catch_unwind` / `FutureExt::catch_unwind`（须改用 `tidb_base::panic_guard::catch_unwind_{silent,capture,future}`） | 当前实测裸 print 共 **5** 处（`crates/base/src/config/mod.rs:692-734`，对应 Go pre-logger 写 `os.Stderr`/`os.Stdout`），Batch 0 封装为带 `// log-skip: pre-logger stderr/stdout parity with Go` 的显式原语；裸 `catch_unwind` 随 Batch 0–3 收敛至 0 |
| **规则 B**（纯 Rust） | `check` job | 禁止分层正式 crate 生产代码在 `log::*!`、`tracing::*!` 或 `tidb_base::log_*!` 的消息字符串中拼接 `{...}` / `key=` / `key:` / `error=` / `stack=`，或调用 `format!` / `from_utf8_lossy` 构造混合消息 | 当前实测分层日志调用含插值 **118** 处（另加 `config/mod.rs` 3 处含 `{}` 的 print，`716`/`734` 行无 `{}`）；写入 `scripts/log_parity_ratchet.tsv`，**只减不增** |
| **规则 C**（Go 对照） | `reference-tree` job（传 `--go-tree "$(scripts/tidb-reference.sh)"`） | 读取 Rust 文件 `//! go-file:` 与 `// go:` / `// go-part:` / `// go-fix:`，**按 `(go_file, go_symbol)` 合并所有 `go-part:` 片段后**，对比 Go 与 Rust 的：(1) 日志调用点数量、(2) 级别、(3) 消息字符串字面量、(4) 字段键名集合与顺序、(5) `zap.Stack` 存在性 | 合理差异（如 failpoint、未移植写路径 `X`、`helper.Storage` MVCC 不可用等）须在 Rust 函数对应行标注 `// log-skip: <Go函数/行号> <原因>`；其余未对齐偏差按 crate/符号记入 `scripts/log_parity_ratchet.tsv`，**只减不增** |

### 6.2 静态扫描与防误报边界

1. **精确跳过测试与非代码区域**：使用大括号深度配对精确跳过 `#[cfg(test)] mod <name> { ... }`（注意不可简单从 `#[cfg(test)]` 截断到文件末尾：实测如 `crates/store/src/distsql/mod.rs:22`、`crates/executor/src/sortexec/sort_util.rs:448` 等在测试模块之后仍有生产代码），并排除 `crates/spike/`、legacy `crates/poc/*`（单独统计或列为 Batch 4 过渡）、`tests/`、`*_test.rs`、`*_tests.rs`。
2. **字符串与注释词法边界**：先剥离 `//` / `/* ... */` 注释与字符串字面量（含 `r#"..."#`）内部的伪括号，再用多行括号配对提取完整宏调用及 Go 多行函数体（Go 正则兼容泛型接收者 `func (*T[P]) Method(...)`）。
3. **动态构造特例处理**：对极少数 Go 本身用 `var fs []zap.Field` 动态拼装字段的函数（如 `Util.GenLogFields`、`ExecDetails.ToZapFields`、`consistency/reporter.go`），在规则 C 中允许通过专门声明或基于已对齐函数签名跳过静态序列检查，交由 §8 黄金单测覆盖字节级一致性。

---

## 7. 现有调用点盘点与分批回改计划

### 7.1 现存调用点实时盘点（直接扫描当前工作树实测）

对当前仓库 `crates/`、`third_party/` 与 `/home/admin/work/client-rust` 非测试生产代码执行全量扫描的实测结果如下：

- **汇总**：
  - **分层正式 crate**：`log::*!` / `tracing::*!` 调用 **144** 处 + `crates/base/src/config/mod.rs` pre-logger `eprintln!`/`println!` **5** 处（合计 **149** 处非 `terror` 输出调用）+ `terror::{log, call, must_nil}` **25** 处；
  - **legacy PoC 服务端/协议 crate（4 个）**：`poc-domain` 28 + `poc-server` 27 + `tidb-mysql-protocol` 14 + `poc-session` 6 = **75** 处（CLI 对比工具 `tools/ab` / `tidb-ab` 的 36 处终端 `print!` 属交互式 CLI 输出，不计入服务端日志基线）；
  - **`client-rust` fork**：`src/` 内 **137** 处 `log::*!`；
  - **`third_party/`**：`third_party/tikv/tidb_query_datatype/src/codec/mysql/decimal.rs` **2** 处 + `third_party/tikv-shims/tikv_util/src/codec/bytes.rs:112` **1** 处（共 3 处 `slog-global` 调用，均经 `third_party/tikv-shims/tikv_util/src/log.rs` 转发），另含 `third_party/tikv-shims/log_wrappers/src/lib.rs` **1** 处脱敏占位。

| 类别 | Crate | 位置 / 门面 | 日志调用 | `terror::` | 输出宏(`e?print*`) | 含插值调用 | `glue:` 独有日志 |
|---|---|---|--:|--:|--:|--:|--:|
| 分层 (rank 0) | `tidb-base` | `log` | 10 | 0 | 5 | 10 (+3 print) | 0 |
| 分层 (rank 1) | `tidb-parser` | `log` | 1 | 0 | 0 | 1 | 0 |
| 分层 (rank 1) | `tidb-memory` | `tracing` | 9 | 0 | 0 | 0 | 0 |
| 分层 (rank 1) | `tidb-runtime` | `tracing` | 3 | 0 | 0 | 0 | 1 |
| 分层 (rank 2) | `tidb-types` | `log` | 11 | 9 | 0 | 11 | 0 |
| 分层 (rank 4) | `tidb-codec` | `log` | 1 | 5 | 0 | 1 | 0 |
| 分层 (rank 5–6) | `tidb-kv` / `tidb-tablecodec` | `log` | 6 / 0 | 1 / 2 | 0 | 4 / 0 | 0 |
| 分层 (rank 8–10) | `tidb-expression` / `tidb-infoschema` / `tidb-statistics` | `log` | 0 / 12 / 2 | 5 / 1 / 0 | 0 | 0 / 10 / 2 | 0 / 1 / 0 |
| 分层 (rank 11–12) | `tidb-store` / `tidb-sessiontxn` / `tidb-expr-pushdown` / `tidb-domain` | `log` | 17 / 4 / 5 / 9 | 0 / 0 / 0 / 0 | 0 | 14 / 4 / 5 / 7 | 1 / 0 / 1 / 1 |
| 分层 (rank 13–16) | `tidb-planner` / `tidb-executor` / `tidb-session` / `tidb-server` | `log` | 12 / 22 / 16 / 4 | 1 / 1 / 0 / 0 | 0 | 11 / 20 / 14 / 4 | 2 / 3 / 2 / 0 |
| **分层合计** | **19 个有输出的分层 crate** | `log` + `tracing` | **144** | **25** | **5** | **118 (+3)** | **12** |
| Legacy PoC | `poc-domain` / `poc-server` / `tidb-mysql-protocol` / `poc-session` | `tracing` / `log` | 28 / 27 / 14 / 6 (**75**) | 0 | 0 | 3 / 1 / 0 / 4 (**8**) | 0 / 11 / 0 / 1 (**12**) |

**归纳 6 类现存偏差**：
1. **字段插值进消息字符串**：分层代码中 `118 / 144` 处（如 `crates/executor/src/consistency.rs:67`、`crates/session/src/session.rs:1172,1570`、`crates/store/src/copr/coprocessor.rs:2002`）。
2. **缺失 `zap.Stack` 或结构化字段**：如 `consistency.rs:67` 漏掉 `[stack=...]`、`coprocessor.rs:1475,1741` 漏掉 `[r=...]` 与 `["stack trace"=...]`、`crates/runtime/src/pool.rs:379` 漏掉 `["stack trace"=...]`。
3. **键名或消息原文偏移**：如 `crates/memory/src/sqlkiller.rs:194` 把 `"connection ID"` 写成 `connection_id`、`crates/mysql-protocol/src/{conn,server}.rs` 自创英文消息而非 Go 原文。
4. **`terror::must_nil` 级别偏差**：`crates/base/src/terror.rs:393` 使用 `log::error!` 而非 `[FATAL]`，且 `must_nil`/`call`/`log` 均未结构化。
5. **已移植 Go 函数中的日志遗漏**：合并 `go-part:` 对比参照树，共检出 **73** 个已移植 Go 符号少于 Go 原文日志数量（累计差值 105 处），其中在 Rust 已有日志的 **10** 个核心函数内部存在 **21** 处明确分支遗漏（包括 `Domain.Reload`、`Domain.loadInfoSchema`、`ExecStmt.Exec`、`recordSet.Finish`、`ResetContextOfStmt`、`ColumnSubstituteImpl`、`tryLockMDLAndUpdateSchemaIfNecessary`、`session.doCommitWithRetry`、`autoCommitAfterStmt`、`InitMemoryHook`），其余多属未移植 `X` 边界或 Go 调试日志。
6. **Rust 独有 `glue:` 日志**：如 `crates/executor/src/prepared.rs:86`、`crates/executor/src/projection.rs:766`、`crates/poc/server/src/main.rs` 11 处等，统一规范为结构体字段输出且杜绝多行 `\n` 拼接（例如 `main.rs:328` `"statement timing\n{}"` 必须改为单行结构化字段）。

### 7.2 分批回改计划与验收标准

| 批次 | 范围 | 主要交付物 | 验收标准 |
|---|---|---|---|
| **Batch 0：底座、规则与优先运维日志** | `tidb-base`、`tidb-log`（新建）、`CLAUDE.md`、`docs/porting/mappings.md`、`scripts/check_log_parity.py`、`poc-server`/`tidb-server` | 新建 `tidb-log`（编码器/轮转/`general_log`/panic hook/`build.rs` 版本注入）；补齐 `tidb-base` 堆栈/脱敏/`terror`；接入 CI 规则 A/B/C 与棘轮；率先上线启动横幅、关闭、`command dispatched failed`、`expensive_query`/OOM 日志 | `check_layers.py` 通过；`check_log_parity.py` 规则 A 归 0；编码器、error/stack/panic 及优先运维日志黄金测试全绿 |
| **Batch 1：底层 crate**（rank 0–4） | `tidb-base`(10+5), `tidb-parser`(1), `tidb-memory`(9), `tidb-runtime`(3), `tidb-types`(11+9), `tidb-codec`(1+5) | 清零插值；`tidb-memory` 与 `tidb-runtime` 切换至 rank-0 `tidb-base` 结构化宏，修 `sqlkiller.rs` `"connection ID"`、`pool.rs` 补 `["stack trace"=...]` 并改用 `catch_unwind_capture`；回改全部 14 处 `terror::log` | 上述 crate 规则 A/B/C 零违规，下压 `log_parity_ratchet.tsv` |
| **Batch 2：存储、KV 与元数据**（rank 5–12） | `tidb-kv`(6+1), `tidb-tablecodec`(0+2), `tidb-infoschema`(12+1), `tidb-statistics`(2), `tidb-store`(17), `tidb-sessiontxn`(4), `tidb-expr-pushdown`(5), `tidb-expression`(0+5), `tidb-domain`(9) | `coprocessor.rs` panic/慢 cop 结构化；`Domain.Reload` 等 2 处遗漏补齐；`X` 写路径分支标注 `// log-skip:` | 上述 crate 规则 B/C 零违规，下压棘轮基线 |
| **Batch 3：规划、执行、会话与服务**（rank 13–16） | `tidb-planner`(12+1), `tidb-executor`(22+1), `tidb-session`(16), `tidb-server`(4) | `consistency.rs` 补 `[stack=...]`；`session.rs` `GENERAL_LOG` 迁入 `GeneralLogger` 与结构化字段；补 `compile SQL failed`/`run statement failed`/`command dispatched failed` 与遗漏分支 | 分层生产代码插值与未登记偏差清零（棘轮归 0） |
| **Batch 4：Legacy PoC、`client-rust` 与 `third_party` 接线** | `poc-server`(27), `tidb-mysql-protocol`(14), `poc-domain`(28), `poc-session`(6), `client-rust`(137), `third_party/tikv-shims` | PoC 接入 `tidb-log`（替换 `tracing_subscriber::fmt`）；协议层对齐握手/关闭/panic 日志；`client-rust` 137 处改结构化 KV 并补 `"get timestamp too slow"` 等运维日志；`log_wrappers` 接入脱敏 | 全进程不再依赖 `tracing_subscriber::fmt`，端到端输出 100% 符合统一单行契约 |

---

## 8. 格式一致性与黄金测试计划

1. **测试矩阵 1：编码器字节级单元测试（对照 §1 & `/tmp/log_probe_447/out.txt`）**：
   - 覆盖 `text` 与 `json` 双格式：全级别（含 `DPANIC`/`PANIC`/`FATAL`）、`LoggerName`、caller 过滤（`[A-Za-z0-9._-]`）、空消息省略 vs `"message":""`、`.With(...)` 绑定顺序、本地时区与非 UTC `TZ`（如 `Asia/Shanghai` / `America/Sao_Paulo`）、`disable-timestamp` 无前导空格、全 20+ 种字段类型及引号/转义/非 UTF-8（`\ufffd`）边界。
   - **单行不变量硬断言**：对所有编码输出断言字节流末尾恰为一个 `\n`，且去除末尾 `\n` 后内部零 `\n`（`0x0a`）与 `\r`（`0x0d`）。
2. **测试矩阵 2：错误、堆栈、多行 SQL 与 Panic 恢复黄金测试**：
   - 验证 `tidb_base::Error` 原生字节传参（含非法 UTF-8 保持 6 字节 `\ufffd` 不二次失真）、`gen_with_stack*` vs `fast_gen*` vs `synthesize` vs `plain`、`disable-error-stack` 开关切换、`ErrVerboseLogger` 强制打印、3 种 `tidb_redact_log` 模式、`terror::{must_nil, call, log}`；
   - 验证多行 SQL 字段、`ExecPool::spawn_with_recover`、`Conn` 循环 panic 恢复、`catch_unwind_silent`（如 `safe_clone` 嵌套于可恢复作用域时不触发假 `[FATAL]` 且不破坏外层恢复）、`catch_unwind_future` 跨 `.await` 不泄漏线程局部状态、`Ok(Err(...))` 不误读回溯槽，以及通过独立子进程测试触发未捕获 `panic!` 验证全局 `set_panic_hook` 输出严格单行日志且 stderr 不含裸多行堆栈。
3. **测试矩阵 3：运维优先日志黄金测试**：
   - 将 Go v8.5.8 实测捕获样本纳入测试夹具，经统一归一化器（替换时间戳、动态连接/事务 ID、本地端口、`[file:line]`、具体堆栈帧与主机地址）后逐字节比对：`Welcome to TiDB.` + `loaded config`、优雅关闭序列、`command dispatched failed`（含缺表、多行语法错、溢出、KILL、NOWAIT 及 3 种 redact 模式）、`expensive_query` + `memory exceeds quota`（`LOG` 与 `CANCEL`）。
4. **测试矩阵 4：混合集群 `INFORMATION_SCHEMA.CLUSTER_LOG` 验证**：
   - **近期离线验证（可直接落地）**：在测试中复用 Go `pingcap/sysutil` `parseLogItem` / `logIterator` 解析 `tidb-log` 生成的普通日志及 `.gz` 轮转文件，验证全部记录时间戳、级别与内容零错误解析且无跨行污染；
   - **远期端到端验证（当前未实测，等待前置条件）**：待 `tidb-rs` status 端口接入 Go Diagnostics gRPC 服务后，由维护者在混合集群（Go TiDB + `tidb-rs`）中实跑 `SELECT * FROM INFORMATION_SCHEMA.CLUSTER_LOG`。

---

## 9. 已知差异、Go 上游缺陷与决策点（宪章第九条）

### 9.0 与 Go v8.5.8 的已知差异与上游缺陷

| # | 差异 / Go 缺陷内容 | Go 源码位置 | 性质与处理方式 |
|---|---|---|---|
| 1 | `[file:line]` 显示 Rust 源文件 basename 与行号（如 `[consistency.rs:67]`），不映射为 `.go` | `pingcap/log/zap_text_encoder.go:64-94` | 必然工程差异（已由负责人 2026-11-27 决定） |
| 2 | `Welcome to TiDB.` 中 `[GoVersion=]` 留空并紧跟新增 `["Rust Version"=...]` | `pkg/util/printer/printer.go:45` | 有意特性差异（已由负责人 2026-11-27 决定） |
| 3 | 堆栈帧格式为 Rust 符号与路径而非 Go goroutine 帧 | `pingcap/errors` / `zap.Stack` | 必然运行时差异（格式转义与单行契约仍严格对齐） |
| 4 | Go `pingcap/log` JSON 编码器忽略 `DisableErrorVerbose` | `pingcap/log/zap_text_encoder.go:166-167`、`pkg/util/logutil/log.go:138` | `upstream-pitfall`（复现见 `probe` `4C`），处理见 §9.2 **D2** |
| 5 | Go `pingcap/log` 文本编码器对 `zap.Skip()` / `zap.Error(nil)` 输出多余 `[]` | `pingcap/log/zap_text_encoder.go:639-641` | `upstream-pitfall`（复现见 `probe` `3A`, `4A [13]`），处理见 §9.2 **D3** |
| 6 | Go `pingcap/log` `OpenNamespace` 在 text 模式下括号错位 `[ns={] ...}` | `pingcap/log/zap_text_encoder.go:238-242,489-493` | `upstream-pitfall`（复现见 `probe` `3A [01]`），处理见 §9.2 **D3** |
| 7 | Go `pingcap/log` `Complex` 负虚部输出 `1+-2i` 与 `ReplaceLogger` 未更新 `errVerboseLogger` | `zap_text_encoder.go:303-306`、`pkg/util/logutil/log.go:191-217` | `upstream-pitfall`：`ErrVerboseLogger` 在 §3.3 共享全局原子句柄，随 `replace_logger` 自动同步（无对外格式偏差，`go-bug-fixed` 内部同步修复）；`Complex` 日志在 Rust 几乎不用，按需登记 |

### 9.1 已由负责人决定（2026-11-27）

汇总 §0.2 负责人已拍板全部 5 项决定：
1. 立即执行（先定稿 spec 再分批实现）；
2. `[file:line]` 使用过滤后的 Rust 文件名与行号；
3. 强制执行单行不变量并完整对齐 error/stack/panic 全部行为；
4. 运维高频检索日志按 §4 清单逐字节对齐并配黄金测试；
5. 启动横幅编译期通过 `build.rs` 注入，`GoVersion` 留空、`Rust Version` 新增（键名带空格 `"Rust Version"`）、`Edition` 保持 `Community`、企业版扩展暂不管。

### 9.2 需要负责人决定的剩余项（宪章第九条 1）

仅列涉及长期用户可见行为或资源/性能语义的 4 项问题：

| 编号 | 问题描述与背景 | 可选方案与影响 | 维护者建议 |
|---|---|---|---|
| **D1** | **`Release Version` 精确字符串及 `VERSION()` / `/status` / `tidb_version()` / `-V` / 指标版本口径联动**<br>负责人已定启动横幅中 `GoVersion` 留空并新增 `"Rust Version"`。目前仓内多处版本串割裂（`poc-domain` 用 `v8.5.8-tidb-rs-<7位>`，`mysql-protocol` 残存 `v8.4.0-tidb-rs`，`tidb-base` 含未接线 placeholder，`sysvar_data.rs` 与 `tidb_metrics::init` 口径也不统一）。需定两件事：(1) `Release Version` 是保留 `-tidb-rs-<7位>` 后缀还是写裸 `v8.5.8`；(2) `SELECT tidb_version()` 与 `-V` 是否同步留空 `GoVersion:` 并新增 `Rust Version:`。*(注：未本地检视 TiUP/Clinic 源码，仅据 Go `FormatTiDBVersion` 语义推断兼容)* | **选项 A**：`Release Version` 用 `v8.5.8-tidb-rs-<7位hash>`，统一 `SELECT VERSION()` / MySQL 握手 / `/status` / `tidb_server_info` 指标为 `8.0.11-TiDB-v8.5.8-tidb-rs-<7位>`（清理过期 `8.4.0`）；`tidb_version()` 与 `-V` 同步留空 `GoVersion:` 并新增 `Rust Version:`。<br>**选项 B**：`Release Version` 严格写裸 `v8.5.8`（不带 `-tidb-rs` 后缀）；或仅改启动横幅，暂不动 `tidb_version()` / `-V` / `/status`。 | **选 A**：保留 `-tidb-rs-<short>` 便于混合集群日志、`SELECT VERSION()` 与监控面板一眼区分 Go 与 Rust 节点，且与启动横幅口径完全一致（仍需在后续混合集群 TiUP 测试复核解析兼容性） |
| **D2** | **JSON 日志格式是否响应 `log.disable-error-stack` / `log.enable-error-stack`**<br>Go v8.5.8 的 `pingcap/log` JSON 编码器忽略该配置（始终输出 `errorVerbose`），文本模式则默认隐藏。 | **选项 A**：照 Go 现状，JSON 模式忽略 `disable-error-stack` 始终输出 `errorVerbose`（开 `upstream-pitfall` `go-bug-kept`）。<br>**选项 B**：修复该 Go 缺口，JSON 与 text 统一遵守 `log.disable-error-stack`（开 `upstream-pitfall` `go-bug-fixed` 并登记 `rulings.toml`）。 | **选 B**：配置项 `enable-error-stack` 在文档上并未区分格式，统一遵守配置对用户更符合直觉且默认避免 JSON 日志体积膨胀 |
| **D3** | **`pingcap/log` 文本编码器 `zap.Skip()` / `zap.Error(nil)` 产出 `[]` 与 `OpenNamespace` 括号错位** | **选项 A**：不复制这两个明显输出 bug，nil error / Skip 静默省略（登记 `upstream-pitfall` `go-bug-fixed`）。<br>**选项 B**：严格照 Go 输出多余的 `[]` 与 `[ns={] ...}`。 | **选 A**：`[]` 与错位括号毫无业务语义，仅是 Go 编码器拼接疏漏，不复制技术债 |
| **D4** | **`tidb_base::Error` 在无 `RUST_BACKTRACE=1` 时的堆栈采集策略**<br>`crates/base/src/errors.rs:488` 当前仅在设置 `RUST_BACKTRACE=1` 时捕获栈。若对所有 `gen_with_stack*` 无条件调 `Backtrace::force_capture()`，每个常规可恢复错误（如 `types` 转换尝试、预检错）将增加微秒级 unwind 开销；**反之，若默认不强制捕获，则在默认 `disable-error-stack = true` 且未设 `RUST_BACKTRACE=1` 时，不仅普通 `[errorVerbose=...]` 不打印，连 `ErrVerboseLogger()` 以及 `command dispatched failed` 中的 `[err=...]`（Go `conn.go:1254` 调 `errors.ErrorStack(err)`）也将退化为仅含零开销 `#[track_caller]` 创建点 `file:line` 而无多帧调用栈**。 | **选项 A**：当且仅当全局配置开启 `enable-error-stack = true`（或环境变量 `RUST_BACKTRACE=1`）时对 `gen_with_stack*` 启用 `Backtrace::force_capture()`（并跳过 `ErrKeyExists` / `ErrParse` / `ErrTableNotExists` 等已知热点无栈错）；默认关闭时，`ErrVerboseLogger()` 与 `command dispatched failed` 的 `[err=...]` 均仅输出零开销 `#[track_caller]` 创建点 `<error>\n<file>:<line>`，不采完整回溯。<br>**选项 B**：与 Go 一致，对非热点 `gen_with_stack*` 始终执行 `Backtrace::force_capture()`（接受常规查询报错路径增加微秒级 unwind 开销，换取默认配置下 `ErrVerboseLogger()` 与 `command dispatched failed` 仍输出完整多帧堆栈）。<br>**选项 C**：严格只依赖 `RUST_BACKTRACE=1` 控制 `Backtrace::capture()`，配置项 `enable-error-stack = true` 不主动触发强制回溯（未设环境变量时恒输出 `file:line`）。 | **选 A**：在默认 `disable-error-stack = true` 下热路径零开销；运维排障时开启 `enable-error-stack = true` 即可不重启/不配环境变量直接获得完整多帧调用栈，同时明确接受默认配置下 `ErrVerboseLogger` 与 `command dispatched failed` `[err=...]` 仅保留 `file:line` 创建点 |

### 9.3 维护者已定（宪章第九条 2）

以下事项属内部实现、任务划分、测试与检查手段，由维护者直接决定，不打断负责人：
1. **不复用 TiKV `tikv_util::logger`**：按 §3.1 所列 8 项不兼容，在 `tidb-log` 自建编码器与轮转后端；
2. **两层 crate 放置、接口分工与 `N0 §3` 同步**：`tidb-base`（rank 0，结构化日志主门面与 `OnceLock` 注册表、`panic_guard`、错误堆栈格式化/脱敏/`terror`）+ `tidb-log`（rank 1，单一职责日志后端/轮转/`general_log`/全局 panic hook/版本打印，同步更新 `layers.toml` 与 `N0 §3` 分层表；并预留 `slow_query_log` sink 接口给 `D14.2`；若评审按宪章第九条 1(b) 认为新增 `tidb-log` 对 `N0` 草案 §3 的说明需负责人一并确认，可随 D1–D4 合并确认）；
3. **多门面直连桥接与 `ErrVerboseLogger` 同步**：rank-0 `tidb_base::structured_log` 主门面（零新增 `Cargo.lock` 依赖）+ `log` / `tracing_subscriber::Layer` / `slog_global` BridgeDrain（禁止 `redirect_std_log`）；`ErrVerboseLogger` 共享全局原子句柄，随 `replace_logger` 自动同步（修复 Go `ReplaceLogger` 漏同步缺陷）；
4. **CI 检查脚本与棘轮**：`scripts/check_log_parity.py` 三条规则（含禁用裸 `catch_unwind`）、`scripts/log_parity_ratchet.tsv` 只减不增基线、按 `(go_file, go_symbol)` 合并 `go-part:` 以及 `// log-skip:` 消音约定；
5. **分批回改排期与内部参数**：Batch 0–4 回改顺序、内存缓冲默认刷新/批大小及写锁检查间隔等不改变对外格式契约的内部参数；
6. **`pingcap/log` 其余非关键内部缺陷（如 `Complex` 负虚部 `1+-2i`）**：按需开英文 `upstream-pitfall` issue 登记跟踪。

---

## 10. 本文所用 ID

| ID | 含义 |
|---|---|
| `D00.2` | 基础常量、MySQL 错误码、terror 与 TOML 配置加载（`tidb-base`） |
| `D00.3` | 通用工具包 `pkg/util/*`（含本 spec 覆盖的 `logutil`、`printer`、`versioninfo`） |
| `D03.1` | MySQL 协议层与服务端连接管理（`tidb-server`） |
| `D04.1` | 权限检查与认证（规划中的 `tidb-privilege`） |
| `D05.3` | 会话执行主流程与查询生命周期（`tidb-session`） |
| `D05.4` | 会话事务上下文（`tidb-sessiontxn`） |
| `D06.5` | Schema 同步与 Domain 生命周期（`tidb-domain`） |
| `D07.1` | 存储驱动（`tidb-store`） |
| `D07.2` | Coprocessor 客户端（`tidb-store`） |
| `D12.12` | `SHOW` / `EXPLAIN` / `SET` 等管理与简单语句执行器（`tidb-executor`） |
| `D14.0` | 执行统计数据结构 `execdetails`（`tidb-execdetails`） |
| `D14.2` | 慢日志格式、语句摘要、Top SQL 上报与 `TRACE`（规划中的 `tidb-observe`） |
| `N0` | crate 分层与无环依赖方向基础设计（`layers.toml`） |
| `ADR 0007` | `N0` 分层关于“生产代码不直接依赖 TiKV 内部实现（仅允许对齐字节格式）”的架构决策记录 |
| `N2` | 三层会话与语句上下文基础设计 |
| `N3` / `A1` | 异步运行时（`tidb-runtime`）与内存追踪/配额模型（`tidb-memory`） |
| `N8` | 系统变量注册表与全局钩子基础设计 |
| `N9` | 可观测性（profile、慢日志、语句摘要、EXPLAIN ANALYZE、Top SQL、TRACE）基础调研设计 |
| `M-ERR` | Go `error` / `errors.Error` 映射到 `tidb_base::Error` 的全局规则 |
| `M-BYTES` | Go `string`（任意字节串）与 UTF-8 文本在 Rust 中的表示规则 |
| `M-DEFER` | Go `defer` 清理映射到 Rust `Drop` / scope guard 的规则 |
| `M-PANIC` | Go `panic`/`recover` 映射到 `Result` 与 task 边界 `catch_unwind` 的规则 |
| `X` | 本项目明确不移植的 Go 模块或写路径功能（如 DDL、写路径 DML、`ANALYZE`、BR 等） |
| `D1` / `D2` / `D3` / `D4` | §9.2 需由负责人最终确认的用户可见或性能/资源语义决策项编号 |
