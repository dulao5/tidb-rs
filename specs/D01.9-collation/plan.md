# D01.9 Collation 与字符集：plan

## 1. Rust 模块布局

| Rust | 内容 |
|---|---|
| `crates/parser/src/gocharset/mod.rs` | `pkg/parser/charset/charset.go` 整文件：`Charset`、`Collation`、注册表与查询函数 |
| `crates/parser/src/gocharset/charset_data.rs` | `collations` / `charsets` 表（rsgen，从 AST） |
| `crates/parser/src/gocharset/encoding_gbk.rs` | `encoding_gbk.go` 的编码器部分，加上 x/text 的 GBK 编码器和 `Encoder.Bytes`（`go-dep:`） |
| `crates/parser/src/gocharset/gbk_encode_data.rs` | GBK 编码表（rsgen，逐个 rune 调用 Go 编码器） |
| `crates/types/src/collate/mod.rs` | `collate.go`：`Collator` / `WildcardPattern` trait、注册表、开关、ID 与名字的转换 |
| `crates/types/src/collate/{bin,general_ci,gbk_bin,gbk_chinese_ci,pinyin_tidb_as_cs,charset}.rs` | 各排序规则，与 Go 文件一一对应 |
| `crates/types/src/collate/unicode_ci.rs` | `UnicodeCollator<I: UcaImpl>`：两份 UCA 生成代码共用的模板 |
| `crates/types/src/collate/unicode_0400_ci_impl.rs`、`unicode_0900_ai_ci_impl.rs` | 两个 `UcaImpl` |
| `crates/types/src/collate/ucadata/` | DUCET 0400 / 0900 表（rsgen，从运行时值） |
| `crates/types/src/collate/{general_ci,gbk_chinese_ci}_data.rs` | 权重表（rsgen，从 AST） |
| `crates/types/src/stringutil.rs` | `pkg/util/stringutil/string_util.go` 整文件 |

生成命令：`go run ./cmd/rsgen -types <tidb-rs>/crates/types/src -parser <tidb-rs>/crates/parser/src`（tracker `govec/`）。

## 2. 写法映射

| Go | Rust |
|---|---|
| `type Collator interface{...}` | `trait Collator: Send + Sync + Any`，`GetCollator` 返回 `&'static dyn Collator` |
| `c.(*binCollator)` 等类型断言（`CanUseRawMemAsKey`） | `c.as_any().is::<BinCollator>()` |
| `Clone() Collator` | `clone_collator() -> Box<dyn Collator>` |
| 模板生成的 `unicodeCICollator` / `unicode0900AICICollator` | `UnicodeCollator<I>` 的两个类型别名 |
| 字符串参数 `string` | `&[u8]`（Go 的 string 是字节串，可能是无效 UTF-8） |
| `[]rune`（`CompilePattern` 的权重） | `Vec<char>`（Go 把无效字节解码成 U+FFFD，`char` 能表示） |
| 包级 map（charset） | `LazyLock<RwLock<...>>`，值为 `Arc<Collation>` / `Arc<Charset>` |
| `newCollationEnabled int32`（atomic） | `AtomicI32` |

## 3. 测试

- `tests/govec_collate.rs`：回放 46,736 条记录，覆盖范围见 spec §4。
- `stringutil` 的单元测试：`MemoizeStr`（Go `TestMemoizeStr`，以及空结果不缓存）、`StringerFunc`。
- debug 和 `release-checked` 都要通过。

## 4. 反向引用与处理（N0）

- **D01.9 → D02.2（charset.go，27 处）**：按 Go 包归属放在 tidb-parser，tidb-types 依赖 tidb-parser（rank 2 → 1），不需要例外。
- **D01.9 → D00.3（stringutil）**：整文件移植到 tidb-types，即用到它的最底层 crate（N0 §3）。
- tracker `--block-edges` 中，D01.9 没有指向更高 rank 的引用。

## 5. 遗留到其他块

- **D02.2**：
  - `encoding_gbk.go` 中 `Encoding` 接口的部分（`encodingGBK`、解码器），与其他 `encoding_*.go` 一起移植；
  - tidb-parser 原有的 PoC `charset.rs`（lexer 用）改用 `gocharset` 后删除。
- **D01.1、D08.x**：`Datum` 的比较、`LIKE` 等表达式通过 `collate::get_collator` 取排序规则。
- **D05.x、D06.5**：启动时按 `mysql.tidb` 中的 `new_collation_enabled` 设置开关（Go 在 session 启动流程中设置），随这些块接上。
