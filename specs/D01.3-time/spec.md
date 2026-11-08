# D01.3 时间与 Duration：块 spec

状态：草案（2026-11-07）。阶段：P1，A 线第 1 块（ROADMAP §4 L3）。基线：Go TiDB v8.5.8（`8b857efa2`）。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[mappings.md](../../docs/porting/mappings.md)、[N0 crate 分层](../exceptions/N0-crate-layering/spec.md)。本块落在 `tidb-types`（rank 2），Go 的 `time` 包移植落在 `tidb-base::gostd::time`（rank 0）。

## 1. Go 范围

函数清单来自 tracker `data/func_blocks.tsv` 中 block = D01.3 的行，共 202 个函数。

| Go 文件 | 函数数 | Rust 文件 |
|---|--:|---|
| `pkg/types/time.go` | 150 | `crates/types/src/time.rs` |
| `pkg/types/core_time.go` | 49 | `crates/types/src/core_time.rs` |
| `pkg/types/fsp.go` | 3 | `crates/types/src/fsp.rs` |

**同 crate 中提前移植的部分**：都只引用 tidb-base 和本 crate，依据宪章 §4.5。

| 来源 | 内容 | Rust |
|---|---|---|
| `pkg/types/context.go`（D01.1） | 整个文件：`Flags` 及其 26 个方法、`Context`、`NewContext`、`DefaultStmtFlags` 等 | `context.rs` |
| `pkg/types/errors.go`（D01.1） | 时间代码用到的错误原型和类型名常量，以 Go 名保留（M-GONAME），实现为 `tidb_base::Error` | `errors.rs` 中的 `go_errors` |
| `pkg/types/helper.go`（D01.1） | `isPunctuation` | `helper.rs` |
| `pkg/types/overflow.go`（D01.4） | `AddInt64`、`SubInt64` | `time.rs` |
| `pkg/util/parser/parser.go`（D00.3，按需移植） | 整个文件；N0 §3 规定放进用到它的最底层 crate | `util_parser.rs` |

**块外依赖**：都标为 `go-std:`。

| 来源 | 内容 | Rust |
|---|---|---|
| Go `time` 包：`zoneinfo.go`、`zoneinfo_read.go`、`zoneinfo_unix.go`、`time.go`、`format.go` | `Location`：读 TZif、`lookup`、`tzset` 规则串、`FixedZone`、`LoadLocation`、`Local`。`Time`：内部表示、日期算法、`Date`（规范化与夏令时取偏移）、`In`、`Add`、`Sub`、`AddDate`、`Round`、`Truncate`、`ZoneBounds`、`String`。`Duration.String` | `tidb-base` `gostd/time/{mod,zoneinfo,instant}.rs` |
| Go `strconv` | `ParseInt`、`ParseUint`、`Atoi`（十进制），以及 `NumError` 的错误文本 | `types/src/gostd.rs` |
| Go `fmt` | `Sscanf` 中 `parseDatetime` 用到的 `%Nd` 子集，包括结尾空格的匹配规则 | `time.rs` `sscanf_digits` |
| Go `unicode`、`strings` | `IsSpace`、`IsPunct`、`IsNumber`、`IsLetter`、`ToUpper`、`ToLower`、`EqualFold`，均作用于 Go 字节串 | `util_parser.rs`、`time.rs`、tidb-base `gostd` |
| Go `encoding/json` | `Time.UnmarshalJSON` 解码到 `types.CoreTime`（`uint64`） | `types/src/gostd.rs` |

## 2. 设计决定

### 2.1 时区：移植 Go 的 `time.Location`，不用 chrono-tz

TiDB 把 `*time.Location` 放在 `types.Context` 和会话里，下面这些行为全部由 Go `time` 包决定：
- **夏令时跳过的时刻**：`CoreTime.GoTime` 检测到"转换后对不上"，再由 `AdjustedGoTime` 调到最近的区间边界，并报 `ErrTimestampInDSTTransition`。
- **重复时刻取哪个偏移**：`time.Date` 的两次 `lookup`。
- **区间边界**：`ZoneBounds`，包括 `tzset` 在年末用的"年初 + 365 天"近似。
- **错误消息里的时间**：`time.Time.String()`，带时区缩写。

chrono-tz 自带一份编译时的 tz 数据库，版本由 crate 决定，不读系统文件；在跳过、重复时刻上的取值规则也不同。所以本块逐行移植 Go 的 `zoneinfo*.go`：
- 读 TZif v1、v2、v3。
- 实现 POSIX TZ 规则串的 `tzset`。slim 格式的 tz 文件只记录到 2007 年左右，之后全靠规则串计算，macOS 和 Go 自带的 `zoneinfo.zip` 都是 slim。
- `lookup` 的二分查找、`lookupFirstZone`、`Date` 的两次查找取偏移，都与 Go 一致。

`LoadLocation` 按 Go 的顺序查找 `$ZONEINFO` 和系统目录。`Local` 按 Go 的 `initLocal` 初始化，依据 `$TZ` 和 `/etc/localtime`。

Location 的身份用 `Arc::ptr_eq` 表示：Go 中有 `tz != time.UTC` 这类指针比较（`checkTimestampType`、`DateTimeIsOverflow`），对应 `is_utc`。

### 2.2 字符串是字节串（M-ERR）

所有 Go `string` 参数都是 `&[u8]`，结果是 `Vec<u8>`。原因：
- `skipAllNums`、`skipAllPunct`、`skipAllAlpha` 按 `i + 1` 推进，可能把一个多字节 rune 切在中间。
- `DateFormat` 按 rune 遍历格式串，非法字节写成 U+FFFD。

这些行为只有在字节串上才能如实复现。

### 2.3 错误（M-ERR、M-ERRVAL）

时间代码返回 `tidb_base::Error`。错误原型在 `errors.rs` 的 `go_errors` 中，用 `ClassTypes.NewStd`、`NewStdErr` 创建，与 Go 的 `errors.go` 相同。MyDecimal 仍返回 D01.2 试点留下的桩 `Error`（D00.2 tasks T12，留到 D01.1）。时间代码透传 MyDecimal 错误的唯一地方是 `ParseTimeFromDecimal`，在那里用 `From` 换成同码的 `tidb_base::Error`。

返回 `(T, error)` 的函数按 M-ERRVAL 选择签名：
- 出错时 T 仍有意义的，返回 `(T, Result<(), Error>)`。例如：
  - `ParseTime` 返回指定类型的零值；
  - `adjustTimestampErrForDST` 返回调整后的时间；
  - `Time.Add` 返回结果和检查错误；
  - `GoTime` 返回 `time.Time`。
- 出错时一律返回零值的，返回 `Result<T, Error>`。例如 `parseDatetime`、`Duration.Add`。

`strconv` 的错误按 Go 的 `NumError.Error()` 文本给出，例如 `strconv.Atoi: parsing "01-": invalid syntax`。`ParseTime("2020-01-01-")` 这类输入会把它原样返回给调用方。

### 2.4 Time 的内嵌字段

Go 的 `Time` 内嵌 `coreTime`（一个 `uint64`，低 4 位是 fsp 和类型），并提升 `CoreTime` 的方法。Rust 中：
- 字段是 `core`，用 `Time::raw()` / `Time::from_raw()` 读写原始字。
- 被提升的方法逐个写成转发（标 `glue`），不用 `Deref`。

### 2.5 浮点转整数（M-F2I）

全部 6 处都调用 `gostd::conv`：
- `ParseTimeFromFloat64` 的 `int64(f)` 和 `uint32(...)`；
- `roundTime`、`RoundFrac`、`TruncateFrac`、`Duration.RoundFrac` 的 `Duration(math.Pow10(9-fsp))`；
- `ParseFrac` 的 `int(float64(tmp) * math.Pow10(...))`。

其中 NaN、±Inf、越界值的结果依平台而定，向量按目标平台分别生成。本块没有登记的 FMA 位置。

## 3. 已知差异（都不影响 TiDB 的结果）

| # | 内容 | 理由 |
|---|---|---|
| 1 | ~~`Time.UnmarshalJSON` 的 JSON 语法错误文本是 serde_json 的~~（D01.1 消除） | 改用 `tidb_base::gostd::json`，语法错误与类型错误的文本都与 Go 一致；govec `time` 领域的 `sys/json_edge` 记录逐字比较 |
| 2 | `Duration.RoundFrac`、`DateTimeIsOverflow` 不再有 `loc == nil` 时退回 `time.Local` 的分支 | Rust 的 `Context` 总是持有一个 Location，`nil` 不可能出现 |
| 3 | 没有移植 `time/tzdata`（内嵌）和 `$GOROOT/lib/time/zoneinfo.zip` 这两个回退 | TiDB 两者都不链接。`$ZONEINFO` 指向 zip 文件的情况已支持（D01.3 后续 PR）；`Location` 的查找缓存也已按 L1 移植 |
| 4 | `gotime::now()` 不带单调时钟读数 | 移植代码中没有对两个 `Now()` 相减的地方 |
| 5 | `StrToInt` 只实现了 `parseDatetime` 唯一调用点的输入形状，即纯数字串，作为 D01.4 的边界桩 | 该处的 `seps[0]` 来自 `ParseTimeFromFloatString`，是 `strconv` 格式化出的数字串 |
| 6 | `scanTimeArgs` 参数个数不符时的错误参数（`[]string` 的格式化）是近似实现 | 调用方总是按 `len(seps)` 选择分支，这条路径不可达 |
| 7 | `strToDate` 的上下文 map 只保留 `%j`、`%p`、`%H`、`%h` 四个键 | Go 只读这四个键 |

## 4. 验收

1. converge：`matched 202/202, missing 0`。
2. govec `time` 领域的 101,814 条向量，在 x86_64 和 aarch64 上都全部通过（`tests/govec_time.rs`），debug 和 `release-checked`（溢出检查）两种构建都要跑。
3. clippy `-D warnings`、fmt、`check_layers --blocks` 都通过。
