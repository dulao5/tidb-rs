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
| `crates/types/src/gostd.rs`、`gostd/eisel_lemire.rs` | Go 标准库行为的按需移植：`strings.TrimSpace`、`utf8.DecodeRune/DecodeLastRune`、`strconv.FormatFloat(f,'g',-1,64)`（最短数字取自 `ryu` crate）、`strconv.ParseFloat`（Go 形状，见"评审跟进"）、`encoding/json` 解码 `jsonMyDecimal` 的语义 |
| `crates/types/src/mydecimal_test.rs` | `TestReset` |
| `crates/types/tests/govec_decimal.rs` | 向量回放；`tests/vectors/v8.5.8/{amd64,arm64}/decimal.jsonl.gz` + README（tracker `5087818`，每份 33,831 条，按 Rust 的 target_arch 选一份；之前为 `6366acd` 的 33,149 条单份、`e9da322` 的 31,620 条明文、初版 `334bd88`） |
| `crates/types/tests/fuzz_decimal.rs` | 无 Go 对照的随机测试，只查整数溢出 panic（评审跟进 3） |
| `crates/types/benches/mydecimal.rs` | Go 的 5 个基准 + FromFloat64 两个（criterion 0.5，jemalloc） |
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

### 向量更新（tracker PR #1 评审之后，31,620 条全部通过）

tracker `e9da322` 按评审追加了两类记录，原有 27,641 条不变：

- 别名调用 `add/sub/mul/div/mod_alias`（1,795 条，`f(x,y,x)`、`f(x,y,y)`、`f(x,x,x)`）：Rust 按 `M-ALIAS` 先复制被别名的输入再调用，`round_inplace` 走 `round_in_place`。全部一致，不需要改 Rust。
- 链式调用 `<producer>_then_<consumer>`（2,184 条）：`FromInt`/`FromUint`/`Round`/加/乘的结果对象（`digitsInt` 按字取整、溢出后的 `-0`、截断后 `digitsFrac=31` 等）直接交给 `to_bin`、`round`、`shift`、`to_hash_key`、`compare` 等。Rust 实现不需要改。第一次回放有 3 条不一致（`mul_then_to_bin/props/compare`，Go 记为 panic、Rust 正常返回），原因在 govec：链式 op 对中间对象调用了 `ToString`/`String`，而截断后的乘法结果在 `ToString` 中越界 panic，consumer 本身并不 panic。govec 已修正为不编码中间对象（tracker `e9da322`）。

错误比对一直包括 `msg`（未填充的模板），tracker README 已把这一条写入回放规则。

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
| `round`（`MyDecimal.Round`） | L1 [M-ALIAS]（原为 L2，`M-ALIAS` 合入 main 后改） | Go 允许 `to == d`，并用 `to != d` 决定是否复制 `wordBuf` 并重设 `digitsInt`（这正是 round 与 round_inplace 内部状态不同的原因）。Rust 中核心函数接收 `d` 的快照和 `to_is_d` 标志；已逐行核对：Go 中对 `d` 的所有读取都发生在对 `to` 的第一次"之后不返回"的写入之前，所以读快照等价于读别名指针。对外是 `round(&self, to)` 与 `round_in_place(&mut self)` 两个入口（标 `go-part`） |
| `validate_args`（`validateArgs`） | L1 [M-ALIAS]（原为 L2） | `f1 == to`、`f2 == to` 的复制由调用方完成（`M-ALIAS`，`MyDecimal: Copy`）。`to == nil` 分支没有对应：`&mut` 不能为空，v8.5.8 中没有调用方传 nil，而且 Go 的 DecimalAdd 等在 validateArgs 之后马上写 `to.resultFrac`，传 nil 本来就会 panic |
| `gostd.rs` 中的标准库移植 | `go-std:` L1/L2 | `FormatFloat` 的最短位数取自 `ryu` crate（与 Go `ftoaryu.go` 同一算法），移植 Go 的 `formatDigits/fmtE/fmtF`；`ParseFloat` 移植 Go 的语法、`atof64exact`、`eiselLemire64`、`atofHex`，只有慢路径的十进制转换用 Rust `parse::<f64>()`（同样正确舍入）；`DecodeRune` 的校验用 `from_utf8`；`json.Unmarshal` 用 tidb-base 的 `gostd::json`（Go `encoding/json` 的移植，D01.1 起；原为 serde_json） |

现在 58 个函数全部是 L1（规则 ID 的使用：`M-ALIAS` 9、`M-INT` 25、`M-ERRVAL` 25、`M-NAME` 1、`M-FMA` 2，见第二轮评审跟进）。

`encoding/json` 的差异（键的 Unicode 大小写折叠、字面量 `-0`、字符串中的非法 UTF-8、语法错误文案）在 D01.1 中消除：`MyDecimal.UnmarshalJSON` 改用 `tidb_base::gostd::json`，govec `decimal` 领域的 `unmarshal_json_gs` 记录（`sys/json_edge`）覆盖这些情况，回放逐字比较错误消息。`WriteBin` 在 Go 中可以在已有容量内重切片，未写到的字节保留旧内容；Rust 的 `resize` 填 0。按代码路径看所有字节都会被写到，所以没有可见差异，但没有向量能证明这一点。

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
16. **浮点 FMA**（第二轮评审更正）：Go 规范允许把 `x*y + z` 融合成一条 FMA 指令，Rust 不会自动融合。原先这里认为 `math.Round(f*unit)/unit` "通常会掩盖"差别、并以"≤12 位路径的向量全部一致"为证据，这个判断**不成立**：向量里没有 `resultFrac < digitsFrac` 的对象，而只有这类对象（`DecimalDiv` 的结果）会让舍入位落在 .5 边界上。`DecimalDiv(50.77, 32, 4)` 后 `ToFloat64`，Go arm64 得 1.586563，amd64 得 1.586562，原来的 Rust 在 arm64 上得 1.586562。而且是否融合取决于平台和具体位置，只能看汇编（例如 amd64 v3 融合 `vector_functions.go` 的几处，却不融合 `mydecimal.go` 的两处）。处理见"第二轮评审跟进"：新规则 `M-FMA`、tracker 登记表 `data/go_fma_sites.tsv`、按目标分开的向量。

## 评审跟进（PR #4，Fable 评审，2026-11-06）

逐条处理结果。数字均为 Apple M4 Pro；Go 为 go1.25.12 `-count 5` 中位数，Rust 为 criterion 0.5 中位数，同一天重跑。

### 1. `FromFloat64` 的最短数字改用 `ryu` crate

`ryu_tie_to_even`（对 ≥15 位的结果做 800 位精确展开）删除，`gostd::ryu_ftoa_shortest` 从 `ryu::Buffer` 的输出读回数字串和小数点位置，接已移植的 `formatDigits/fmtE/fmtF`（`go-std: strconv.ryuFtoaShortest L2 (ryu crate)`）。`ryu` 已在 lock 中（serde_json 的依赖）。验收：原来那 120 万个 Go `FormatFloat(f,'g',-1,64)` 输出（同一生成器、同一种子重新生成，与 11-04 的文件逐字节相同）全部一致；979980073090831.25 等平局在单测里。

| 基准（每次 1000 个值，ns/值） | 改前 | 改后 | Go |
|---|--:|--:|--:|
| `FromFloat64`，随机位型 double | 15,508 | 97 | 93 |
| `FromFloat64`，`k*0.1+0.2` | 751 | 63 | 59 |

剩下约 5% 是 `String` 分配（Go 的 `FormatFloat` 也返回新串，再 `[]byte(s)`）。

### 2. 线上格式入口

`MyDecimal::from_wire(&[u8; 40]) -> Result<MyDecimal, Error>` 与 `as_wire(&self) -> [u8; 40]`：字节 0–2 为三个 int8，字节 3 为 `negative`，其后 9 个小端 int32（N5a §3.3）。`from_wire` 只检查字节 3 ∈ {0, 1}，否则返回 `ErrBadNumber`（Go 不检查；Rust `bool` 的其他位型是 UB）；其余字段照 Go 不检查。没有 unsafe，没有引入 zerocopy。向量回放对每个十进制结果（22,747 个对象）检查 `as_wire` 等于由 Go 的 `st` 拼出的 40 字节，且 `from_wire` 还原出同一对象；单测覆盖布局和非法 bool。标注是 `glue:`（Go 中对应的是 `util/chunk/column.go` 的 `unsafe.Pointer` 转换，不是 `mydecimal.go` 的函数）。

### 3. 回绕点

- (a) `doDivMod` 的 normFactor、D3、D4 和 `DecimalMul` 的乘积处写了 int64 界（有效字：D3 的 `x < (w+1)·wb`、`normFactor·x ≤ wb²`、`(x − guess·w)·wb + y ∈ (−2e18, 2e18)`；`DecimalMul` 的 int32×int32 对任何字都不越界）。**顺带发现**：`UnmarshalJSON` 和 `from_wire` 不检查字的范围（Go 也不查），非法字可以让 D3/D4 的 int64 中间量越界，Go 在那里静默回绕，Rust 原来会在开溢出检查的构建里 panic。这几处改为 `wrapping_*`（`M-INT`），有效字的结果不变。
- (b) 新 profile `release-checked`（release + `overflow-checks = true`），CI 增加一步在它下面跑 `tidb-types` 的全部测试（向量回放 + 5 亿步 fuzz，本机 5 千万步 10 秒）。
- (c) `tests/fuzz_decimal.rs`：随机 `FromString` 串（含 0/9 连串、指数、尾部垃圾、全角数字）、边界整数、浮点，所有 op 的结果回填进对象池，接收者一半是脏的；只把 `attempt to … with overflow` 当失败，Go 式 panic（越界、makeslice）计数。默认 3 万步（debug 约 2 秒），`TIDB_TYPES_FUZZ_ITERS` / `TIDB_TYPES_FUZZ_SEED` 可调。本机跑过 debug 300 万步、release-checked 5 千万步，无溢出。故障注入（把 `ToInt` 的 `wrapping_mul` 改成 `*`）第 0 步即报出；把 `stringSize` 的 int8 加法改回普通加法则报不出，因为从合法输入出发这条路径只会得到负数而不会越界（向量中 `mul` 的 makeslice panic 来自 `digitsFrac` 的回绕，不来自这里）。

### 4. `parse_float` 按 Go 签名

`gostd::parse_float(&str) -> (f64, Result<(), NumError>)`（`M-ERRVAL`）。`NumError { func, num, err: NumErrKind::{Syntax, Range} }`，文案同 Go。溢出返回 ±Inf + `Range`，语法错误返回 0 + `Syntax`，下溢返回 ±0 不报错。移植了 `special`、`readFloat`、`underscoreOK`、`atof64exact`、`eiselLemire64`（连同 696 项的表）、`atofHex`；只有慢路径用 Rust `parse::<f64>()`。

- **十六进制**：移植了。TiDB 中 `strconv.ParseFloat` 有 25 处非测试调用，`types.StrToFloat` 先经 `getValidFloatPrefix` 只留下十进制，但系统变量（`sysvar.go` 等）直接解析用户输入，`SET ... = '0x1p-2'` 在 Go 中会被接受。
- **Go 的意外行为**（照样复现）：慢路径 `decimal.set` 在放小数点时只数前 800 位数字，所以整数部分超过 800 位时多出的位数丢失，`ParseFloat("1" + 1000 个 0 + "e-500")` 在 Go 中是 `1e299`，不是 +Inf；`"1_000"` 是 1000（`underscoreOK` 允许十进制里的下划线）；`"+nan"`、`"-nan"` 是语法错误而 `"-inf"` 可以；Go 的 NaN 位型是 `0x7FF8000000000001`。
- 差分：92 万个输入（随机记号串、十进制与十六进制字面量、`FormatFloat 'g'/'x'` 的往返、700–1200 位长数字串）与 Go 比对值的位型、错误种类和文案，全部一致。
- 性能：`BenchmarkToFloat64Old`（`ParseFloat(String())`）42.4 µs，Go 47.1 µs（原来直接 `str::parse` 是 33.9 µs）。
- **`ToFloat64` 出错时的值**：改为保留 `ParseFloat` 的返回值，与 Go 相同。但用 Go 验证后，这里出错时的值是 0 而不是 ±Inf：`String()` 最多 81 位整数，不可能溢出；可达的只有非法状态（`UnmarshalJSON` 写入负字等）导致 `String()` 出现非数字字符，ParseFloat 报语法错误、值为 0。单测用 Go 的结果覆盖了两个这样的状态。
- spec 模板第 14 条（标准库清单）应把 `strconv.ParseFloat` 标为"需要按 Go 语义移植"：溢出要 `ErrRange`、接受十六进制和下划线、800 位规则。D01.1 的 `StrToFloat` 直接用 `gostd::parse_float`。

### 5. 脏接收者

- (a) `from_string`、`from_int`、`from_uint`、`from_float64`、`from_bin` 的文档写明"不重置其他字段，按 Go"，并写出后果。
- (b) govec 新增 `dirty_<op>`（tracker `5a51dbe`）：接收者 / `to` 先 `FromString` 一个脏值（负数、带小数、9 字占满、解析失败的状态），再执行 `from_string from_int from_uint from_float64 from_bin unmarshal_json round add sub mul div mod`。1,529 条，追加在原记录之后，原 31,620 条不变；两次生成逐字节相同，`--check` 与 `go test` 通过。Go 的实际行为：接收者为 `-123.456` 时 `FromString("2")` 得 **-2**，`FromUint(5)` 得 **-5**（`FromString` 只在 9 个字全为 0 时清 `negative`，残留的高位字也算在内）。Rust 回放 1,529 条第一次就全部通过，移植不需要改。

### 6. 交给 D00.2：错误比较的两种语义

本块的 `Error` 桩按 class + code 比较（`PartialEq`），等于 Go 的 `terror.Error.Equal`。Go 代码里常见的 `err == ErrTruncated` 是**指针相等**：哨兵经 `GenWithStackByArgs` 生成的新错误与哨兵不相等，`errors.Cause`/`Equal` 才相等。本块所有比较对象都是裸哨兵（`fixWordCntError` 等直接返回 `ErrOverflow`），两种语义结果相同，所以桩可以只给一种。D00.2 需要：

1. 提供两种比较：按 code 的 `is()`（对应 `terror.ErrorEqual` / `Equal` / `errors.Is` 一类）和哨兵同一性（对应 `err == ErrXxx`）；后者在 Rust 里可以是"未带参数的哨兵"标志位，而不是地址；
2. 移植指南要求每个比较点按 Go 的写法选其一，并在需要时注明；Go 用 `==` 比较一个可能被 `GenWithStackByArgs` 包装过的错误时，Go 的结果就是 false，Rust 必须同样为 false；
3. govec 的 `errOf` 先按同一性、再按 `Equal` 命名错误，回放只比 code 和文案，所以向量无法区分这两种语义；D00.2 的向量需要单独的 op。

### 7. 分配器与基准

`tidb-server` 和本 crate 的基准都用 `tikv-jemallocator` 作 `#[global_allocator]`（A1 / P0 D7），criterion 0.3 → 0.5，`black_box` 用 `std::hint::black_box`。

| 基准 | Go ns/op | Rust（jemalloc） | Rust/Go | Rust（系统 malloc，同日） |
|---|--:|--:|--:|--:|
| BenchmarkRound | 297.6 | 294.5 | 0.99 | 281.2 |
| BenchmarkToFloat64New | 3,422 | 2,774 | 0.81 | 2,740 |
| BenchmarkToFloat64Old | 47,141 | 42,435 | 0.90 | — |
| BenchmarkMyDecimalToBin | 265.7 | 279.7 | 1.05 | 282.5 |
| BenchmarkMyDecimalToHashKey | 281.7 | 289.0 | 1.03 | 295.4 |

jemalloc 让 ToBin/ToHashKey 快了约 2%，但没有"转正"：仍比 Go 慢 3–5%，几次运行在 272–280 / 284–289 之间波动。差距是每次调用一次 malloc/free 对 Go 的 bump 分配 + GC 摊销，计算部分（复用缓冲时 149 ns）比 Go 快。spec §4.5 改为写明分配器，含分配的基准建议另定口径（差距 ≤ 5%，待确认）。

### 三、小项

- `#[derive(PartialEq, Eq)]`：在 `MyDecimal` 文档写明 `==` 比较表示（`1.0 != 1.00`），值比较用 `compare`。`TestReset` 保持 Go 的深比较（Go 的 `require.Equal` 就是逐字段）。
- `to_string() -> Vec<u8>` 与 `ToString` 的冲突：main 上 mappings 的 `M-NAME` 已列出（`String`、`ToString`、`Clone`、`Hash`、`Equal`），不再另记。
- 向量 `.jsonl.gz` 入库（tracker `6366acd`：govec `Gzip = true`，README 定为所有领域的规矩），本仓库用 `flate2` 读，6.7 MB → 0.49 MB。
- lint allow 清单：放在根 `Cargo.toml` 的 `[workspace.metadata.porting] style-lint-allow`（pilot-notes 9 说的"workspace 统一一份"；Cargo 的 `[lints]` 不能按模块），`scripts/converge.py` 检查每个移植文件的 `#![allow]` ⊆ 清单，item 级 `#[allow]` 列出供评审（本块两处：`inherent_to_string`、`absurd_extreme_comparisons`）。main 上 mappings.md 的 "lint" 一段合入后应指向这里。
- 向量条数：33,149（27,641 基本、1,795 别名、2,184 链式、1,529 脏接收者）。（第二轮后为每个目标 33,831 条，见下。）

## 第二轮评审跟进（PR #4，Lucky 评审）

### 1. FMA（必须改）

- **规则**：main 的 mappings.md 新增 `M-FMA`：Go 浮点乘加位置按"每个位置 × 每个目标"移植，Go 在该目标上融合的用 `f64::mul_add`，不融合的分开乘和加；amd64 基线为 GOAMD64=v1（Go 默认值，v8.5.8 的 Makefile 没有设置），v1 不融合任何位置。原来"移植模块的统一约定"中的"浮点"一条改为指向本规则。
- **登记表**：tracker `scripts/go_fma_sites.py` 对每个目标交叉编译 `go build -gcflags=<包>=-S`，收集 `FMADDD`/`FMADDS`/`VFMADD231SD`/`FNMSUBD` 等指令并映射回源码行，写入 `data/go_fma_sites.tsv`（tracker.py 的表，CI `--check`）。`pkg/types` 共 8 行：

  | 位置 | amd64 v1 | amd64 v3 | arm64 | ppc64le / s390x / riscv64 / loong64 |
  |---|---|---|---|---|
  | mydecimal.go:1118、1125（`ToFloat64`） | 否 | 否 | `FMADDD` | 是 |
  | vector_functions.go:44、100、101、102（float32） | 否 | `VFMADD231SS` | `FMADDS` | 是 |
  | vector_functions.go:152（`L2Norm`） | 否 | `VFMADD231SD` | `FMADDD` | 是 |
  | vector_functions.go:71（`InnerProduct`，float32） | 否 | 否 | `FMADDS` | 是 |

  评审只查了 `FMADDD`，所以没看到 arm64 上 float32 的几处（`FMADDS`）。本块用到的 Go 标准库（`strconv`、`encoding/json`、`strings`、`unicode/utf8`）没有融合位置；`math` 包在 arm64 上有约 136 行融合（`Log`、`Exp`、`Pow`、三角函数等），以后移植 SQL 数学函数的块要用这张表（`-p math`）。
- **Rust**：`to_float64` 的两处改为调用 `to_float64_madd`：aarch64（以及登记表中融合的 ppc64le、riscv64、loong64，未测试）用 `mul_add`，其他（x86_64）分开计算。s390x 是大端，被线上格式的小端断言排除。标注 `// go: MyDecimal.ToFloat64 L1 [M-ERRVAL, M-FMA]`，两处各有 `// [M-FMA] mydecimal.go:<行>` 注释；`converge.py --fma-sites <tracker>/data/go_fma_sites.tsv` 检查登记表中每个在已移植 Go 文件里的位置都有这样的注释、函数的 `go:` 标注带 `M-FMA`，并且注释不指向登记表外的行（故障注入：删注释、去掉规则 ID、写错行号都能报出）。单测 `to_float64_fma_per_target` 用评审的例子按架构断言。
- **向量**：govec 文件头加 `goamd64`，每个目标一份（`amd64` = v1、`arm64`），`--check` 只与同一目标的文件比较；`div` 加为链式 producer；新增 `sys/fma`（评审的 4 个例子）、300 条随机 `x/j`、以及只保留结果依赖融合的随机 `x/j` 共 40 条（按 Go 源码重算的 ToFloat64 模型分别用 `math.FMA` 和禁止融合的写法计算，结果不同即保留；与生成平台无关）。两份文件恰好有 44 条不同。govec 的 `TestFMA` 用登记表的结论和模型核对 Go 的实际输出，`TestTargetsDiffer` 要求两份文件只在 `*to_float64` 上不同。tracker CI 改为 `ubuntu-latest` 与 `ubuntu-24.04-arm` 的 matrix；本仓库 CI 新增 aarch64 job（`tidb-types`，debug 和 release-checked）。
- **验证**：本机 aarch64 回放 `arm64` 33,831/33,831；把 aarch64 也改成不融合时恰好 44 条失败（反证）。不融合的代码路径（即 x86_64 路径）回放 `amd64` 33,831/33,831（在 aarch64 上模拟，本机没有 Rosetta，`--target x86_64-apple-darwin` 只做了编译和 clippy）。**`amd64` 向量不是在 amd64 上生成的**：本机没有 Rosetta，Docker Desktop 启动失败，所以用 `-gcflags=all=-d=fmahash=n`（gc 不融合任何乘加，等同 amd64 v1 的行为）在 darwin/arm64 上生成，文件头写 amd64/v1；它与 arm64 文件的差别正好是模型预测的 44 条。真正在 amd64 上的验证是 tracker CI 的 linux/amd64 `govec.sh --check` 和本仓库 x86_64 CI 的回放。

### 2. 映射规则 ID

所有适用的位置都改写了规则 ID（converge 统计：`M-ALIAS` 9、`M-INT` 25、`M-ERRVAL` 25、`M-NAME` 1、`M-FMA` 2），converge 检查规则 ID 都在 mappings.md 中。取法：Go 签名返回 `error` 的函数标 `M-ERRVAL`；函数体有 `wrapping_*` 或缩窄 `as` 转换的标 `M-INT`；`Round`（及 `round`/`round_in_place` 两个 `go-part`）、`validateArgs`、`DecimalAdd/Sub/Mul/Div/Mod` 标 `M-ALIAS`；`ToString` 标 `M-NAME`。`Round`、`validateArgs` 由 L2 改为 L1，本块不再有 L2。

### 3. 小项

- `gostd.rs` 的 15 个函数补了标注（`go-std: strconv.syntaxError`、`encoding/json.decodeState.literalStore/array/object`、`UnmarshalTypeError.Error` 的 L2，`glue:` 用于 serde 的 `expecting` 和读 ryu 指数）；converge 的文件列表加入 `gostd.rs`、`gostd/eisel_lemire.rs`。
- 线上格式：`from_wire`/`as_wire` 写明假定小端序，并加 `const _: () = assert!(cfg!(target_endian = "little"))`。
- Go panic 记录：309 条（PR 描述原写 286，是只算基本操作的数；别名和链式记录也会 panic）。
- **gostd 以后单独成 crate**：D01.1 等后续块都会用到 gostd，建议以后拆成 `tidb-gostd`，`tidb-types` 的公开 API 不再带 strconv 的仿制品。本 PR 不做。

converge 命令（本轮起包括 gostd 和 FMA 登记表）：

```
scripts/converge.py --block D01.2 --func-blocks <tracker>/data/func_blocks.tsv \
    --fma-sites <tracker>/data/go_fma_sites.tsv \
    crates/types/src/mydecimal.rs crates/types/src/helper.rs \
    crates/types/src/gostd.rs crates/types/src/gostd/eisel_lemire.rs

annotations: glue=6, go=61, go-part=4, go-std=34
go: levels: L1=61
mapping rules: M-ALIAS=9, M-ERRVAL=25, M-FMA=2, M-INT=25, M-NAME=1
M-FMA: 2 registered site(s) in the ported Go files
matched 58/58, missing 0, extra 0, duplicated 0
converge: OK
```
