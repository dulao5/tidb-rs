# D01.3 时间与 Duration：plan

## 1. Rust 模块布局

| Go | Rust | 说明 |
|---|---|---|
| `pkg/types/time.go` | `crates/types/src/time.rs` | `Time`、`Duration`，以及解析、格式化、换算 |
| `pkg/types/core_time.go` | `crates/types/src/core_time.rs` | `CoreTime`、周与天数的计算、`AddDate`、`timestampDiff` |
| `pkg/types/fsp.go` | `crates/types/src/fsp.rs` | |
| `pkg/types/context.go` | `crates/types/src/context.rs` | 提前移植（spec §1） |
| `pkg/util/parser/parser.go` | `crates/types/src/util_parser.rs` | 按需移植（N0 §3） |
| Go `time` 包 | `crates/base/src/gostd/time/{mod,zoneinfo,instant}.rs` | 放在 rank 0，`tidb-sessionctx` 及其以上的 crate 都会用到 |

## 2. 写法映射

| Go | Rust |
|---|---|
| `int`（年、月、日、fsp 等） | `isize`（M-INT） |
| `gotime.Duration` | `i64`（`gostd::time::Duration`） |
| `*gotime.Location` | `Arc<Location>`；`== time.UTC` 写成 `is_utc` |
| `gotime.Time` | `gostd::time::Time`：内部秒数（自公元 1 年起）、纳秒、`Option<Arc<Location>>`（`None` 即 UTC，对应 Go 的 nil） |
| `fmt.Sprintf("%02d", ...)` 等固定格式 | `format!`。数值都是非负整数，两者输出相同 |
| 错误参数是 `CoreTime`、`*CoreTime`、`time.Time`、`time.Duration` | `Arg::Stringer(<String()>, "<Go 类型名>")` |
| `fmt.Sscanf(s, "%2d%2d...", ...)` | `sscanf_digits`（只有 `%Nd`，带可选的结尾空格） |
| `regexp` `^[0-9]{0,6}`、`[0-9]+` | 按字节扫描（标 `glue`） |
| `map[string]dateFormatParser`、`monthAbbrev`、`validIdxCombinations` | `match`（标 `glue`） |
| 解析函数返回剩余的 `string` | 返回已消费的字节数 `Option<usize>`。Go 返回的总是输入的后缀，两种写法等价 |

**M-INT 回绕位置**：以下位置在 Go 中会按整数宽度回绕，Rust 中显式写成 `wrapping_*` 或 `as` 截断：
- `calcTimeDiffInternal` 的 int64 运算；
- `getDateFromDaynr` 的 `uint` 中间值，其中 `dayOfYear` 的中间结果可能是负数；
- `timestampDiff` 的 `uint` 月份计数，`years--` 可能回绕；
- `parseNDigits` 的累加；
- `NewDuration`、`matchDuration` 的乘法；
- `parseSingleTimeValue`、`parseTimeValue` 中溢出后继续计算的部分，Go 注释写明了这一行为。

## 3. 测试

- **govec `time` 领域**（tracker PR，`govec/domains/time.go`、`time_cases.go`）：
  - 三个测试文件中全部 1354 个字面量，送进所有吃字符串或整数的 op；
  - 9 个时区在夏令时转换日凌晨各时刻的用例（`sys/dst`）；
  - 随机的日期串、数字、时长、原始字、格式串、区间值；
  - Go `time` 包本身的 `zone_lookup`、`go_date`。
- **TZif 数据**：作为 `zone` 记录随向量一起提交，Rust 回放时用完全相同的字节加载。
- **回放**：`crates/types/tests/govec_time.rs`。`out` 按 JSON 比对；错误和 warning 按错误码和消息比对；Go panic 的记录要求 Rust 也 panic（原始字中 fsp 位为 7 时，`Time.String` 会越界）。

## 4. 反向引用与处理（N0）

`check_layers --block-edges` 中 D01.3 的出边：

| 到 | 引用数 | 处理 |
|---|--:|---|
| D01.1（同在 `tidb-types`） | 111 | 同一 crate，不受约束。`Context`、错误、`isPunctuation` 提前移植（spec §1） |
| D00.2（`tidb-base`，rank 0） | 50 | 向下，直接使用 |
| D00.3（`util/*`） | 17 | `util/parser` 整个文件移入本 crate；`logutil` 用 `log` crate；`mathutil.Abs` 用 `wrapping_abs` |
| D01.2（同 crate） | 14 | 直接使用 MyDecimal |
| D01.4（同 crate） | 3 | `AddInt64`、`SubInt64` 提前移植；`StrToInt` 作为边界桩（spec §3 第 5 条），D01.4 开工时替换 |

没有跨 crate 的反向引用。`types.Context` 在 `pkg/types` 中，不在 `sessionctx`。

## 5. 遗留到其他块

- `StrToInt` 的完整实现，D01.4。
- MyDecimal 改用 `tidb_base::Error`，D01.1（D00.2 T12）。
