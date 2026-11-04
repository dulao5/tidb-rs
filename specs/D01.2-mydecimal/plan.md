# D01.2 MyDecimal：plan

## 1. Rust 模块布局

| Go | Rust | 说明 |
|---|---|---|
| 包 `pkg/types` | crate `crates/types`（包名 `tidb-types`） | 规则：一个 Go 包对应一个 crate（Go 禁止包循环，所以 crate 依赖也无环）；体量小的子包可以作为模块。本试点验证这条规则 |
| `pkg/types/mydecimal.go` | `crates/types/src/mydecimal.rs` | 函数顺序与 Go 相同 |
| `pkg/types/errors.go`（部分） | `crates/types/src/errors.rs` | 临时边界桩，只有 5 个错误 |
| `pkg/types/helper.go`（部分） | `crates/types/src/helper.rs` | 只有 `strToInt`、`isSpace`、`isDigit` 三个叶子函数 |
| `pkg/parser/terror` 的 `Log` | 桩，放在 `errors.rs`，标 `glue: stub blocked-by D00.2` | |

## 2. 类型与写法映射

| Go | Rust | 等级 |
|---|---|---|
| `type MyDecimal struct{ digitsInt, digitsFrac, resultFrac int8; negative bool; wordBuf [9]int32 }` | `#[repr(C)] pub struct MyDecimal { digits_int: i8, digits_frac: i8, result_frac: i8, negative: bool, word_buf: [i32; 9] }`，加 `size_of == 40` 的静态断言 | L1 |
| 方法 `func (d *MyDecimal) X(...) error`：修改接收者，同时返回错误；出错时接收者里往往仍有有效值（如截断） | `fn x(&mut self, ...) -> Result<(), Error>`；出错时 `self` 的内容与 Go 相同 | L1 |
| `func DecimalAdd(from1, from2, to *MyDecimal) error` | `fn decimal_add(from1: &MyDecimal, from2: &MyDecimal, to: &mut MyDecimal) -> Result<(), Error>`。Go 允许 `to` 与 `from1` 是同一个指针；Rust 借用规则不允许，调用方需要原地运算时先复制 `from1`（`MyDecimal: Copy`，40 字节） | L1。若 Go 中存在依赖别名的写法，在 tasks 中逐处登记 |
| Go 定宽整数运算的溢出回绕（如 `DecimalDiv` 中 `int8(fracIncr)`） | 显式 `wrapping_*` / `as` 截断。Rust debug 构建溢出会 panic，必须逐处写明 | L1。若这类写法很多，提议新增映射规则 `M-INT` |
| Go 运行时 panic（越界、makeslice，见 govec README） | 保持 panic（越界索引在 Rust 中同样 panic），由 task 边界的 `catch_unwind` 转成错误（`M-PANIC`：只有控制流用途的 panic 才改为 `Result`，bug 引起的 panic 保持） | L1 |
| `[]byte` 输入与输出 | `&[u8]` 输入，`Vec<u8>` 输出；`ToString() []byte` 返回 `Vec<u8>` | L1 |
| `strconv.ParseFloat` / `FormatFloat`（`FromFloat64`、`ToFloat64`） | 需要与 Go 逐位一致的浮点解析和格式化。先查 Rust 标准库的 `parse::<f64>()`（正确舍入，与 Go 一致）和 `ryu` / `{}` 格式化与 Go `'g' -1` 的差异 | 若需要移植 Go `strconv` 的部分，作为 ON-DEMAND 工具代码登记 |
| `errors.Trace(err)` | 直接返回（`M-ERR`） | L1 [M-ERR] |

## 3. 测试

- **向量回放**：`crates/types/tests/govec_decimal.rs` 读取 `tests/vectors/v8.5.8/decimal.jsonl`（从 tracker 复制，文件头记录 tracker commit 和 sha256），按 tracker `govec/README.md` 的规则逐条比对；失败时打印 `src` 和差异。
- **单测**：`TestReset` 翻译为 Rust 单测。
- **基准**：`benches/mydecimal.rs` 对应 Go 的 5 个基准（criterion）。

## 4. 标注与收敛检查

- 每个函数一条 `// go: <Symbol> L1`（文件头 `//! go-file: pkg/types/mydecimal.go`）。
- 收敛检查脚本（本试点写第一版，以后进 tracker）：抽取 `// go:` 标注，与 `func_blocks.tsv` 中 block = D01.2 的 58 个符号比对，报告缺失、多余、拼写不符。
