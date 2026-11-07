# D01.9 Collation 与字符集：块 spec

状态：草案（2026-11-07）。阶段：P1，A 线第 2 块的另一部分（ROADMAP §4 L1）。基线：Go TiDB v8.5.8（`8b857efa2`）。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[mappings.md](../../docs/porting/mappings.md)、[N0 crate 分层](../exceptions/N0-crate-layering/spec.md)。本块落在 `tidb-types`（rank 2），`charset.go` 落在 `tidb-parser`（rank 1）。

## 1. Go 范围

函数清单来自 tracker `data/func_blocks.tsv` 中 block = D01.9 的行，共 95 个函数，12 个文件。另外有两个按宪章 4.4 整文件移植的块外文件。

| Go 文件 | 函数数 | Rust 文件 |
|---|--:|---|
| `pkg/util/collate/collate.go` | 28 | `crates/types/src/collate/mod.rs` |
| `pkg/util/collate/bin.go` | 15 | `collate/bin.rs` |
| `pkg/util/collate/charset.go` | 1 | `collate/charset.rs` |
| `pkg/util/collate/gbk_bin.go` | 5 | `collate/gbk_bin.rs` |
| `pkg/util/collate/gbk_chinese_ci.go` | 8 | `collate/gbk_chinese_ci.rs`（排序键表 `gbk_chinese_ci_data.rs`，生成） |
| `pkg/util/collate/general_ci.go` | 8 | `collate/general_ci.rs`（plane 表 `general_ci_data.rs`，生成） |
| `pkg/util/collate/pinyin_tidb_as_cs.go` | 5 | `collate/pinyin_tidb_as_cs.rs` |
| `pkg/util/collate/unicode_0400_ci_impl.go` | 6 | `collate/unicode_0400_ci_impl.rs` |
| `pkg/util/collate/unicode_0900_ai_ci_impl.go` | 7 | `collate/unicode_0900_ai_ci_impl.rs` |
| `pkg/util/collate/unicode_0400_ci_generated.go`、`unicode_0900_ai_ci_generated.go`、`ucaimpl/main.go` | 5 + 5 + 2 | `collate/unicode_ci.rs`（泛型，见 2.3） |
| `pkg/util/collate/ucadata/data.go` | — | `collate/ucadata/`（DUCET 0400 / 0900 表，生成） |

**整文件移植的块外文件**

| Go 文件 | 所属块 | 函数数 | Rust | 理由 |
|---|---|--:|---|---|
| `pkg/parser/charset/charset.go` | D02.2 | 16 | `crates/parser/src/gocharset/mod.rs`（`collations` / `charsets` 表在 `charset_data.rs`，生成） | collate 引用它 27 处（tracker `block_edges`），且是字符集与排序规则的注册表 |
| `pkg/parser/charset/encoding_gbk.go` 的编码器部分 | D02.2 | 4（另有 10 个未移植，见 §3） | `gocharset/encoding_gbk.rs`（编码表 `gbk_encode_data.rs`，生成） | gbk_bin 的 `Key` 用它 |
| `golang.org/x/text@v0.39.0` 的 GBK 编码器、`Encoder.Bytes` | — | — | 同上 | `go-dep:` |
| `pkg/util/stringutil/string_util.go` | D00.3 | 31 | `crates/types/src/stringutil.rs` | LIKE 模式的编译和匹配是 collate `Pattern` 的实现 |

## 2. 设计决定

### 2.1 `charset.go` 放在 tidb-parser

`charset.go` 属于 D02.2（parser），在 N0 中归 `tidb-parser`。所以它移植到 `crates/parser/src/gocharset/`，tidb-types 新增对 tidb-parser 的依赖（rank 2 → 1，`layers.toml` 允许）。

**代价**：tidb-types 的冷编译要先编译 tidb-parser，本机约多 14 秒。

**另一个办法**：把 charset.go 下沉到 tidb-types 或 tidb-base，可以省掉这个依赖。但这样 parser 自己用到字符集时就要反过来依赖它们，违反"按 Go 包归属"的原则，所以不采用。

tidb-parser 原有的 `charset.rs` 是 PoC 为 lexer 写的子集，这次没有改。两者有重叠，应在 D02.2 的后续工作中合并，由 lexer 改用 `gocharset`。

### 2.2 可变的包级状态

Go 的 `charset` 包状态是可变的：`CharacterSetInfos`、`AddCollation`，`switchDefaultCollation` 会改 `IsDefault`。`*Collation` 指针在多张 map 中共享。Rust 的做法：
- 用 `RwLock` 保护状态，map 的值是 `Arc`；
- Go 在 init 之后会写的字段（`IsDefault`、`DefaultCollation`、`Collations`）用内部可变性实现；
- `CharacterSetInfos` 通过 `character_set_infos()` 访问，保证先执行 init。

collate 的 `newCollationEnabled` 是 `AtomicI32`。`newCollatorMap` / `newCollatorIDMap` 在第一次使用时由 `init` 填好，之后不可变。

### 2.3 Collator 是 trait，UCA 两份生成代码写成一个泛型

- **trait**：`Collator`、`WildcardPattern` 是 trait。`GetCollator` 返回 `&'static dyn Collator`，`Clone` 返回 `Box<dyn Collator>`。`CanUseRawMemAsKey` 是 Go 的类型断言，用 `as_any` 实现。
- **UCA 泛型**：`unicode_0400_ci_generated.go` 和 `unicode_0900_ai_ci_generated.go` 是同一个模板（`ucaimpl/unicode_ci.go.tpl`）生成的。Rust 写成泛型 `UnicodeCollator<I: UcaImpl>`，`UcaImpl` 对应模板参数 `ImplName`。两个类型别名对应 Go 的两份代码。`ucaimpl` 的 `generateFile` / `main` 标为 L2：Go 中生成代码的那一步，在 Rust 中由泛型实例化完成。

### 2.4 数据表全部生成

数据表由 tracker 的 `cmd/rsgen`（`-types`、`-parser` 两个输出目录）生成，模块声明处加 `#[rustfmt::skip]`：
- 从 Go AST 生成：general_ci 的 plane 表、gbk_chinese_ci 的排序键表、charset 的 `collations` / `charsets`。
- 从 Go 运行时值生成：ucadata 的 DUCET 0400 / 0900、GBK 编码表。GBK 编码表是 x/text 的 5 张编码表合成一张 BMP 表，逐个 rune 调用 Go 编码器得到。

每张表都由 govec 的 `key_digest` 做全码位校验（§4）。

### 2.5 照搬的 Go 行为

- **比较的回绕**：`sign(int(an) - int(bn))` 按回绕计算。
- **pinyin**：`utf8mb4_zh_pinyin_tidb_as_cs` 的所有方法都 panic（`implement me`）。
- **charset 的 init**：不调用 `switchDefaultCollation`，所以 gbk 的默认排序规则是 `gbk_bin`。

### 2.6 名字

排序规则名、字符集名在 Rust 中是 `&str` / `String`。Go 中它们来自 SQL 标识符或系统变量，都是 UTF-8。govec 只用 UTF-8 字面量作名字，非 UTF-8 的字面量只送进字符串 op。

## 3. 偏离与未完成

0. **移植了上游修复 pingcap/tidb#67149**（宪章第一条 2）：v8.5.8 的 `convertRuneUnicodeCI0900` 判断 `int(r) > len(MapTable4)` 差一，U+2CEA1（正好等于 len）会越界读表并 panic，用户输入就能触发（比较、排序或 LIKE 任何含 U+2CEA1 的字符串）。master 在 commit dfda1fd3e8（2026-10-19）改为 `>=`，没有回移到 release-8.5。tidb-rs 按修复后的行为实现：U+2CEA1 得到隐式权重（与表外的其他码点一样），不再 panic。函数标注 `// go-fix: pingcap/tidb@dfda1fd3e8 #67149`。**与 v8.5.8 的差异仅限于此**：v8.5.8 在这里 panic（连接报错），tidb-rs 返回结果。
   - govec 链接的是打过这个修复的 TiDB 副本（tracker `govec/fixes`、`scripts/govec-fixes.sh`），向量文件头有 `"fixes":["pingcap/tidb#67149"]`。重新生成后共有 31 条记录变化：原先 panic 的 30 条（`key` 14、`compare` 11、`pattern` 5），以及 1 条 `key_digest`（含 U+2CEA1 的区间，原先记 0xFF）。两条专门的 sys 记录的 `src` 为 `fix pingcap/tidb#67149`。
1. **`StringerStr`**：持有 Rust `String`，用于日志的 `Display`，不能像 Go 的 `StringerStr`（Go string）那样承载无效 UTF-8。govec 中输入是无效 UTF-8 的 64 条 `string_misc` 记录，不比对 `stringer` 这一项。目前没有调用方需要传字节；如果以后有，改成字节串。
2. **`encoding_gbk.go` 只移植了编码器**：`customGBK.NewEncoder` 等 4 个函数。另外 10 个（`encodingGBK` 的 `Name`、`Tp`、`Peek`、`MbLen`、`ToUpper`、`ToLower`、解码器 `NewDecoder` / `Transform` / `Reset`、`init`）属于 parser 的 `Encoding` 接口，随 D02.2 的 Encoding 部分一起移植。所以这个文件按 D02.2 跑 converge 会报这 10 个缺失；charset.go 的 16 个函数全部匹配。
3. **`MemoizeStr`、`StringerFunc.String`**：参数是函数值，govec 豁免，由 Rust 单元测试覆盖（移植了 Go 的 `TestMemoizeStr`）。

## 4. 验证

govec 领域 `collate`（tracker PR #12），共 46,736 条记录，两个目标相同（只有文件头不同）。Rust 回放：`crates/types/tests/govec_collate.rs`。

- **`key_digest`**：覆盖 binary、utf8mb4_bin、utf8mb4_0900_bin、utf8mb4_general_ci、utf8mb4_unicode_ci、utf8mb4_0900_ai_ci、gbk_bin、gbk_chinese_ci 共 8 个排序规则。在每 0x1000 个码位的区间上，计算 `Key`、`KeyWithoutTrimRightSpace` 以及与前一个 rune 的 `Compare`，取 SHA-256；panic 时记一个 0xFF 字节。这一类对生成的表做全码位校验。
- **字面量**：20 个 Go 测试函数中的字符串，覆盖 13 个排序规则（含 binary、ascii_bin、latin1_bin、utf8_*），新排序规则开和关两种状态，做 `key` / `compare` / `pattern`。
- **随机字符串**：
  - Unicode 各平面的字符；
  - 无效 UTF-8；
  - 末尾空格（PAD SPACE 与 NO PAD）；
  - 大小写折叠的特例：Kelvin K U+212A、Angstrom U+212B、长 s、ß 与大写 ß U+1E9E；
  - 第二个串多数取第一个的近似变体（大小写翻转、增删一个零件、末尾加空格）。
- **LIKE 模式**：包括非 ASCII 的转义字节，同时比对 `CompilePattern` / `CompilePatternBinary` 的权重和类型。
- **其余**：collator 的 Go 类型名与 Clone；排序规则 ID、名字的各个转换；`GetSupportedCollations`；charset 的查询与校验；GBK 编码；stringutil 的全部函数。

**比对方式**：`out` 按 JSON 逐项相等，错误按消息比对，Go panic 的记录在 Rust 中也必须 panic。单个 `#[test]` 顺序回放，因为排序规则开关是全局状态。回放开始前先断言 Go 的初始状态：新排序规则开启，gbk 的默认排序规则是 gbk_bin。

**反例验证**：故意改坏 3 条记录（一条 `key_digest`、一条 `compare`、一条 `charsets`），测试报 `3 of 46736 records differ`。

**converge**：
- D01.9：95/95 OK（collate 的 12 个 Rust 文件）。
- charset.go：16/16。
- stringutil：31/31。
