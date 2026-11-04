# 流程试点记录（D01.2）

记录流程本身的发现，试点结束后据此修订宪章、方法论、来源标注规范和 spec 模板。

## 已发现

1. **块外的叶子函数**：`mydecimal.go` 用到 `types/helper.go`（D01.1）中的 `strToInt`、`isSpace`、`isDigit`。宪章第四条只允许"边界桩"或"`util/*` 叶子代码整文件移植"，没有覆盖"其他块里的、没有进一步依赖的小函数"。本试点的做法：按 L1 移植到 Rust 中对应的文件（`types/helper.rs`），由 D01.1 直接计为已完成。建议把这条写进宪章第四条 4：引用图中没有出边的函数可以提前移植到对应位置，不算"只拉进单个上游函数"。
2. **错误类型先于 D00.2**：几乎每个块都会用到 `terror`/`dbterror` 和各包的 `errors.go`。如果每个块都写错误桩，会重复很多次。建议把 D00.2（错误码与 terror）排到 P1 最前面，作为所有块的前置。
3. **govec 的 panic 记录**：tracker `govec/README.md` 写的是"Rust 必须返回错误"。plan 中改为"保持 panic，由 task 边界转换"（因为这些 panic 是 Go 的 bug 引起的，`M-PANIC` 只把控制流用途的 panic 改为 `Result`）。比对规则改为"Go panic 时 Rust 也 panic"，tracker README 需要同步修改。

## 实施结果（T1–T9，2026-11-04）

### 交付物

| 文件 | 内容 |
|---|---|
| `crates/types/src/mydecimal.rs` | 58 个函数，Go 顺序；`//! go-file: pkg/types/mydecimal.go` |
| `crates/types/src/helper.rs` | `isSpace`、`isDigit`、`strToInt`（D01.1 的叶子函数，L1） |
| `crates/types/src/errors.rs` | 5 个错误 + `terror.Log` 的临时桩（`glue: stub blocked-by D00.2`） |
| `crates/types/src/gostd.rs` | Go 标准库行为的按需移植：`strings.TrimSpace`、`utf8.DecodeRune/DecodeLastRune`、`strconv.FormatFloat(f,'g',-1,64)`（含 Ryū 的平局取偶规则）、`encoding/json` 解码 `jsonMyDecimal` 的语义 |
| `crates/types/src/mydecimal_test.rs` | `TestReset` |
| `crates/types/tests/govec_decimal.rs` | 向量回放；`tests/vectors/v8.5.8/decimal.jsonl` + README（tracker `334bd88`，sha256 `7fef6576…457f08`） |
| `crates/types/benches/mydecimal.rs` | Go 的 5 个基准（criterion 0.3） |
| `scripts/converge.py` | 收敛检查第一版 |

### 向量回放（27,641 条全部通过）

| op | 条数 | 通过 | 其中 Go panic |
|---|--:|--:|--:|
| add | 1540 | 1540 | 0 |
| bin_size | 675 | 675 | 0 |
| compare | 1535 | 1535 | 0 |
| div | 2265 | 2265 | 0 |
| from_bin | 1007 | 1007 | 11 |
| from_float64 | 262 | 262 | 0 |
| from_int | 222 | 222 | 0 |
| from_string | 714 | 714 | 0 |
| from_uint | 214 | 214 | 0 |
| marshal_json | 66 | 66 | 0 |
| max_or_min | 140 | 140 | 0 |
| mod | 1535 | 1535 | 0 |
| mul | 1536 | 1536 | 31 |
| neg | 151 | 151 | 0 |
| peak | 177 | 177 | 0 |
| props | 430 | 430 | 0 |
| round | 4245 | 4245 | 0 |
| round_inplace | 480 | 480 | 0 |
| shift | 2508 | 2508 | 4 |
| sub | 1539 | 1539 | 0 |
| to_bin | 3939 | 3939 | 240 |
| to_float64 | 985 | 985 | 0 |
| to_hash_key | 755 | 755 | 0 |
| to_int | 323 | 323 | 0 |
| to_uint | 322 | 322 | 0 |
| unmarshal_json | 76 | 76 | 0 |
| **合计** | **27641** | **27641** | **286** |

比对内容：`s`、`str`、`st`（经 `MarshalJSON` 读取，与 govec 的取法相同）、整数/hex/浮点字符串输出、错误码，**以及错误文案**（比 README 要求的更严，全部一致，包括 `Truncated incorrect DECIMAL value: '１'` 这类带参数的文案）。`err.name="other"`（Go `encoding/json` 的错误，3 条）只要求"返回无错误码的错误"，文案不比。286 条 Go panic 的记录在 Rust 中都 panic（`catch_unwind` 检查）。测试在 debug 构建（溢出检查开启）下运行，说明没有遗漏的回绕点被向量触发。

第一次运行 27,636/27,641 通过。剩下 5 条都是 `to_float64` 的结果格式化：浮点值恰好落在两个最短候选的正中（如 979980073090831.25），Go 的 Ryū（`ryuDigits32`）取偶，Rust 的 `{:e}` 向上取。修正后另用 Go 生成 120 万个浮点（随机位模式、二进制有理数、短十进制、特殊值）比对 `FormatFloat 'g' -1`，全部一致。
**教训**：向量里浮点格式化的平局只有 5 条，而且只出现在测试框架自己的输出编码上；`FromFloat64` 用的是同一个函数，向量却没有覆盖到它的平局。依赖标准库行为的地方，需要单独做大样本的差分测试（建议 govec 增加 `go-std` 领域：`FormatFloat`、`ParseFloat`、`TrimSpace`）。

### 收敛检查

`scripts/converge.py --block D01.2 --func-blocks <tracker>/data/func_blocks.tsv crates/types/src/mydecimal.rs crates/types/src/helper.rs`：

```
block D01.2: 58 Go functions in 1 file(s)
annotations: glue=2, go=61, go-part=2
go: levels: L1=59, L2=2
matched 58/58, missing 0, extra 0, duplicated 0
info: helper.rs isSpace / isDigit / strToInt belong to block D01.1
converge: OK
```

脚本还检查：等级语法、`fn` 没有任何标注、重复标注、Rust 中的顺序与 Go 行号顺序不一致（本块 0 处）。故障注入（删一条、改名、`L2` 不带理由、重复）都能报出。

### 基准（Apple M4 Pro；Go 1.25.12 `-count 5` 取中位，Rust criterion 中位）

| 基准 | Go ns/op | Rust ns/op | Rust/Go |
|---|--:|--:|--:|
| BenchmarkRound（45 次 Round） | 289.6 | 286.2 | 0.99 |
| BenchmarkToFloat64New（1000 个） | 3,283 | 2,871 | 0.87 |
| BenchmarkToFloat64Old（1000 个，ParseFloat(String())） | 47,108 | 33,931 | 0.72 |
| BenchmarkMyDecimalToBin（12 个） | 260.6 | 280.1 | 1.07 |
| BenchmarkMyDecimalToHashKey（12 个） | 277.7 | 299.8 | 1.08 |

ToBin/ToHashKey 比 Go 慢 7–8%，原因是分配：Go 每次调用 1 次分配（12 allocs/op），Rust 用的是 macOS 系统 malloc（工作区没有设置全局分配器）。临时测了一下复用缓冲区的 `WriteBin`：149 ns，即 12 次 malloc/free 约占 130 ns；纯计算部分比 Go 快。`ToHashKey` 还多一次 `push` 引起的 realloc（Go 的 size class 有余量，Rust 的 `Vec` 是精确容量）。这不是移植逻辑的问题，属于全局决定：
- 建议在基础设计中定下全局分配器（jemalloc/mimalloc），基准与服务端使用同一个；
- spec 的性能验收"不低于 Go"应写明分配器条件，或者对含分配的基准另定口径。

### 偏离 L1 的地方

| Rust | 等级 | 原因 |
|---|---|---|
| `round`（`MyDecimal.Round`） | L2 | Go 允许 `to == d`，并用 `to != d` 决定是否复制 `wordBuf` 并重设 `digitsInt`（这正是 round 与 round_inplace 内部状态不同的原因）。Rust 中核心函数接收 `d` 的快照和 `to_is_d` 标志；已逐行核对：Go 中对 `d` 的所有读取都发生在对 `to` 的第一次"之后不返回"的写入之前，所以读快照等价于读别名指针。对外是 `round(&self, to)` 与 `round_in_place(&mut self)` 两个入口（标 `go-part`） |
| `validate_args`（`validateArgs`） | L2 | `to == nil` 与 `f1 == to`、`f2 == to` 三个分支在 Rust 类型下不可能出现（`to: &mut`，`f1/f2: &`），所以没有移植。需要原地运算的调用方先复制（`MyDecimal: Copy`） |
| `gostd.rs` 中的标准库移植 | `go-std:` L1/L2 | `FormatFloat` 的最短位数取自 Rust `{:e}`，只移植 Go 的 `formatDigits/fmtE/fmtF` 和平局取偶规则；`ParseFloat` 用 Rust `parse::<f64>()`（同样正确舍入）；`DecodeRune` 的校验用 `from_utf8`；`json.Unmarshal` 用 serde_json 做语法、Go 的结构体解码规则自己写 |

其余 56 个函数都是 L1。

`encoding/json` 未覆盖的 Go 行为（向量里没有，记录在案）：键的 Unicode 大小写折叠（只做了 ASCII 不区分大小写）；字面量 `-0` 写入整数字段（serde_json 把它当作浮点，Rust 报类型错误、Go 接受）；字符串中的非法 UTF-8；错误文案。`WriteBin` 在 Go 中可以在已有容量内重切片，未写到的字节保留旧内容；Rust 的 `resize` 填 0。按代码路径看所有字节都会被写到，所以没有可见差异，但没有向量能证明这一点。

### 回绕与截断（Go 定宽运算在 Rust 中的显式写法）

Rust debug 构建溢出会 panic，所以凡是 Go 可能回绕的地方都写成 `wrapping_*`；`as` 转换与 Go 的 `int8()`/`int32()`/`uint64()` 同为截断或符号扩展，不另列。

| 函数 | 位置 | 说明 |
|---|---|---|
| `add`、`sub`、`sub2` | 字的加减 | 有效字不会溢出；`UnmarshalJSON` 可以写入任意字，照 Go 回绕 |
| `MyDecimal.stringSize` | `digitsInt + digitsFrac + 3` | **int8 加法**，可以回绕成负数，Go 随后 `make` panic（向量中 `mul` 的 makeslice panic 就是这条路径） |
| `MyDecimal.ToString` | `byte(y)+'0'`、`x -= y*digMask`、`x *= 10` | 非法字时回绕 |
| `MyDecimal.FromString` | `str[i]-'0'` | byte 减法 |
| `round`（`Round`） | `to.wordBuf[toIdx]++`、`shiftedNumber += 10`、`powers10[pos]*(…)`、末尾 `to.digitsInt++` | 非法字/非法 digitsInt 时回绕 |
| `MyDecimal.FromInt` | `uint64(-val)` | `MinInt64` 取负回绕 |
| `MyDecimal.ToInt` / `ToUint` | `x*wordBase - w` / `x*wordBase + w` | **溢出检测依赖回绕**（先算再比较 `x > y` / `x < y`） |
| `MyDecimal.FromBin` | `d.digitsInt -= …`（int8）、`(x^mask) * powers10[…]` | 后者对构造的非法输入会回绕，随后的 `> wordMax` 检查依赖它 |
| `doAdd` | `from1.wordBuf[0] + from2.wordBuf[0]` | 非法字时回绕 |
| `DecimalMul` | `from1.resultFrac + from2.resultFrac`、`from1.digitsFrac + from2.digitsFrac`（int8）、`carry += hi` | int8 加法在两边 frac 较大时真实回绕，结果影响 `notFixedDec` 截断 |
| `DecimalDiv` | `from1.resultFrac + int8(fracIncr)` | govec README 已记录的 int8 回绕 |
| `doDivMod` | `from2.wordBuf[start2]+1`、`norm2 += …`、`carry += hi` | 同上 |
| `strToInt` | `r + digit`（uint64）、`-r` | 溢出检测依赖回绕 |

数量（约 20 处）已经超过"逐处说明"能承受的程度。**建议新增映射规则 `M-INT`**：Go `int` → `isize`；定宽整数运算凡可能回绕处一律用 `wrapping_*`，`intN()` 转换用 `as`；Go 的负下标与越界 panic 由 `as usize` 后的越界 panic 承担。有了规则就可以写 `L1 [M-INT]` 而不必逐处登记。

### 别名（`to` 与 `from` 是同一个指针）

`mydecimal.go` 内部：
- `String()`：`tmp.Round(&tmp, …)` → `round_in_place`；
- `Shift()`：`d.Round(d, …)` → `round_in_place`；
- `Compare()`：`doSub(d, to, nil)`，`to` 为 nil → `Option<&mut MyDecimal>`；
- `DecimalDiv/DecimalMod` → `doDivMod(…, to, nil, …)` / `doDivMod(…, nil, to, …)`，Go 中 `to = mod` 的写法 → 两个 `Option`，加 `is_mod` 代替后面的 `mod != nil`；
- 文件内没有对 `DecimalAdd/Sub/Mul/Div/Mod` 的别名调用（`validateArgs` 的别名分支只服务于外部调用方）。

外部（其他块的移植者需要知道）：v8.5.8 `pkg/` 非测试代码中有 9 处 `x.Round(x, …)`（`types/datum.go` 3 处、`executor/aggfuncs` 5 处、`expression/builtin_time.go` 1 处），都对应 `round_in_place`；简单模式下没有找到 `DecimalAdd(a, b, a)` 式的调用，但调用方若要原地运算必须先复制。

### 时间（粗略）

| 阶段 | 用时 | 备注 |
|---|---|---|
| 读规则、spec、Go 源码、govec README/生成器 | ~25% | govec 的 `decimal.go`（参数解码、`decState` 的取法）是写回放测试必须读的，spec 没有指向它 |
| T1（骨架、错误桩、helper、gostd） | ~10% | |
| T2–T6（mydecimal.rs 一次写完） | ~30% | 按 Go 顺序逐函数翻译比按 T2–T6 分组更顺；任务分组对单文件块没有意义 |
| T7（回放测试 + 浮点平局 + 120 万浮点差分） | ~15% | 首次运行即 27,636/27,641 |
| T8（converge.py） | ~5% | |
| T9（基准 + 分配诊断） | ~10% | Go 基准 37 秒 |
| 记录 | ~5% | |

## 流程发现（供修订模板与规范）

4. **plan 缺少"多返回值与 error"的映射**。Go `(T, error)` 里 T 在出错时常常仍有意义（`ToInt` 返回 `MaxInt64` + `ErrOverflow`，`ToBin` 返回字节 + `ErrTruncated`，`FromBin` 返回 binSize + `ErrBadNumber`）。本试点的规则：Go 的 `error` 值 → `Result<(), Error>`（`Ok(())` 即 nil，可以直接写 `err == Err(ERR_OVERFLOW)`）；返回 `(T, error)` 时，若所有出错路径的 T 都是零值则用 `Result<T, Error>`，否则用 `(T, Result<(), Error>)`。建议写进 mappings.md（`M-ERRVAL`）。
5. **需要 `M-INT`**（见上节）。不然每个块都要重新讨论 `int`→`isize` 还是 `i64`、下标怎么转、哪里要 `wrapping`。
6. **需要 `M-ALIAS`**。Go 中 `to` 可以与输入相同是常见写法（本块 `Round`；全仓 9 处原地 `Round`）。建议规则：Go 允许别名的函数在 Rust 中提供"写入另一个对象"和"原地"两个入口；核心函数接收输入快照（`Copy` 类型）和 `to_is_d` 标志，前提是逐行确认 Go 对输入的读取都在对输出的写入之前，并在函数注释中写明。有了规则，`Round` 可以标 `L1 [M-ALIAS]` 而不是 L2。
7. **标准库移植没有标注语法**。provenance.md 只有 TiDB 符号（`go_funcs.tsv`）。本试点用了 `//! go-file: $GOROOT/src/strconv/ftoa.go` 和 `// go-std: strconv.FormatFloat L2 (…)`。建议正式加入 `go-std:`，符号写 `包.函数`，等级规则相同；converge 和覆盖率统计忽略它，但 lint 检查语法。
8. **Go 的 `fmt.Stringer`/`String()` 与 Rust 的 `Display`/`to_string`**：Go 有 `String()`（四舍五入到 resultFrac）和 `ToString()`（`[]byte`，不舍入）两个方法。本试点：`String()` → 固有方法 `string()`，`Display` 委托给它（标 `glue:`）；`ToString()` → `to_string() -> Vec<u8>`，与 `ToString` trait 同名但返回类型不同，容易误用。建议命名规则：Go 方法名与 Rust 标准 trait 方法冲突时，保留 Go 名的 snake_case（L1 判定要求），另在 mappings 中列出已知冲突（`String`、`ToString`、`Clone`、`Hash`、`Eq` 等）。
9. **clippy 风格 lint 与"同分支、同顺序"冲突**：`int_plus_one`（`end+1 <= len` 被要求改成 `end < len`）、`needless_range_loop`、`manual_range_contains`、`needless_late_init`、`field_reassign_with_default`、`manual_slice_fill`。本试点在 `mydecimal.rs` 模块级 `#![allow]` 这 6 个，正确性 lint 全部保留。建议在 workspace 定义"移植模块 lint 配置"（一份统一的 allow 清单），不要每个文件各写一份。
10. **Go 注释原样复制会变成 doctest**：Go 注释里缩进 4 格的段落在 `///` 中被当作 Rust 代码块，`cargo test` 编译失败（本块 5 处）。做法：用 ```` ```text ```` 包起来。应写进移植指南；converge/lint 也可以检查。
11. **向量 README 与 plan/spec 对 panic 的规则已统一为"Rust 也 panic"**（发现 3），tracker 已在 `334bd88` 修改。spec §4.1 仍写着"Go 会 panic 的记录 Rust 返回错误"，需要同步。
12. **spec §3 中 `mysql.MaxDecimalScale` 的处理**：PoC 的 `crates/parser` 没有这个常量可以复用（且依赖它要带上 build.rs 的语法表生成），本试点在 `mydecimal.rs` 内定义 `mod mysql`（`glue: stub blocked-by D00.2`）。建议 D00.2 把 `parser/mysql` 的常量放进一个不依赖语法生成的小 crate。
13. **tasks 的分组对单文件块用处不大**：T2–T6 标了 [P]，但它们都在同一个文件、互相调用（`FromString` 调 `Shift`，`Shift` 调 `Round`，`ToFloat64` 调 `String`），实际只能按 Go 顺序一次写完，再统一回放。建议模板：单文件块的 tasks 写成"按 Go 顺序移植全文件"一条，分组只在多文件块使用；完成条件统一用"该 op 的向量全部通过"。
14. **spec 模板应包含的东西**（本次需要但 spec/plan 没有提供的）：
    - govec 生成器源码的位置（参数解码、内部状态的取法决定了回放测试怎么写）；
    - 块用到的 Go 标准库函数清单，以及每个是否需要按 Go 语义移植（本块：`strconv.FormatFloat/ParseFloat`、`strings.TrimSpace`、`encoding/json`、`unicode.IsDigit/IsSpace`）；
    - 别名用法清单（块内和外部调用方）；
    - 有意义的 `(T, error)` 返回值清单；
    - 性能验收的环境（CPU、分配器、Go 版本、Go 基准的 `-count`）。
15. **converge 需要的输入**：`func_blocks.tsv` 足够做 1:1 检查。建议脚本进 tracker 时同时输出 `rs_provenance.tsv`（provenance.md 已定义列），并把"Rust 顺序与 Go 不一致"作为警告。另外 `go:` 后面可省略 `L1` 的规定会让 lint 难以区分"漏写等级"和"默认 L1"，建议要求显式写等级。
16. **浮点 FMA**：Go 规范允许把 `x*y + z` 融合成一条 FMA 指令（gc 在 arm64 等平台上会这样做），Rust 不会自动融合。`ToFloat64` 的 `f += float64(x) * pow10off81[…]` 因此可能在最后一位上与 Rust 不同；但随后 `math.Round(f*unit)/unit` 舍入到 resultFrac 位，通常会掩盖这种差别。本次 ≤12 位路径的向量全部一致。govec 文件头没有记录 GOARCH，建议加上，并在 amd64 和 arm64 各生成一次比对；凡是 Go 代码里有乘加的浮点逻辑，spec 都应提示这一点。
