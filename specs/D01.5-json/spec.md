# D01.5 JSON 类型：块 spec

状态：评审中（2026-11-08，tidb-rs #21）。阶段：P1，A 线。基线：Go TiDB v8.5.8（`8b857efa2`）。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[例外 A4](../exceptions/A4-json/spec.md)、[mappings.md](../../docs/porting/mappings.md)。本块落在 `tidb-types`（rank 2）。

## 1. Go 范围

tracker `data/func_blocks.tsv` 中 block = D01.5 的 128 个函数：

| Go 文件 | 函数数 | Rust 文件 |
|---|--:|---|
| `pkg/types/json_binary.go` | 48 | `crates/types/src/json/json_binary.rs` |
| `pkg/types/json_binary_functions.go` | 46 | `crates/types/src/json/json_binary_functions.rs` |
| `pkg/types/json_path_expr.go` | 34 | `crates/types/src/json/json_path_expr.rs` |
| `pkg/types/json_constants.go` | （常量、错误变量） | `crates/types/src/json/json_constants.rs` |

A4 §7 把 P1 定为"最小子集"、路径与修改函数放到函数阶段。实际移植时三个文件一起按 L1 移植完了（路径、Extract/Modify/Merge 等与核心代码交织，拆开反而要写桩），所以本块覆盖 A4 §1.1 中 D01.5 的全部函数；D08.10（SQL 层 JSON 函数）仍在函数阶段。

## 2. 实现方式：L1 移植 + 有测量依据的局部 L2

A4 允许"按 Rust 重新设计实现"。本块**没有**做 A4 §5 的整体重写（单遍解析器直接写二进制、`JsonRef` 视图类型等），而是先按 L1 移植 Go 的结构，再根据基准测量在热点上做局部 L2 改动（§4）。理由：

1. 逐字节一致是硬性契约（A4 §8 第 1 条）。L1 移植加 govec 向量，最容易证明这一点。
2. 测量结果（§5）显示，局部改动后除 `Extract` 外都已不慢于 Go；整体重写的收益（解析 3 倍目标）需要单独评估，见 §6。

## 3. 设计决定

- **`BinaryJSON<'a>`** 持有 `Cow<'a, [u8]>`：子值借用文档字节（Go 的切片共享），构造函数返回 `'static`。去重和 `binaryModifier` 用的 `&Value[0]` 身份对应为字节地址（`ptr()`）。
- **文本解析**用 `tidb-base` 的 `gostd::json` 有序解码器（与 `encoding/json` 的接受集合、数字字面量、重复 key"后者生效"、非法 UTF-8 替换一致），A4 §5.4 的理由不用 serde_json。
- **深嵌套**：Go 中嵌套超过 101 层的文本一定转换失败（`CreateBinaryJSONWithCheck` 在 `appendBinaryJSON` 之后检查 `GetElemDepth()-1 > maxJSONDepth`；文本嵌套 n 层时 `GetElemDepth() >= n`，最内层容器非空时为 n+1）。文本先用 `gostd::json::unmarshal_raw` 解码（tidb-rs #25 起它是迭代的，`Raw` 的 `Drop` 也不递归，最多 10000 层）。嵌套 ≤ 101 层的文本照常转换（`raw_to_value`、`appendBinaryJSON` 的递归深度有界）；更深的文本走 `deep_text_error`：在 `Raw` 树上用显式栈按 `appendBinaryJSON` 的遍历顺序做它会报的检查（对象先检查全部 key 的长度，再按 key 顺序处理值，重复 key 后者生效；数字转换失败），都通过时报 `ErrJSONDocumentTooDeep`，不建 `JSONValue` 树和二进制。不起线程、不递归，小栈上和并发时都安全（`tests/json_deep.rs`：16 个 256 KiB 栈的线程同时解析 300 层和 10000 层的文本）。`json_valid` 就是 `unmarshal_raw(..).is_ok()`。
- **路径缓存**：去掉全局 `peCache`（A4 §5.6），`ParseJSONPathExpr` 每次都解析；常量路径由表达式层缓存（D08.10）。`init`、`jsonPathExpressionKey.Hash` 标为 L2（不保留）。

## 4. L2 位置

| 函数 | 改动 | 为什么结果相同 |
|---|---|---|
| `ParseBinaryJSONFromString` | Go 先 `json.Valid` 再 `UnmarshalJSON`，各扫一遍；这里只解码一次，解码失败就是 Go 的 `json.Valid` 失败 | 解码器接受的集合与 `json.Valid` 相同（嵌套上限 10000 先单独检查），错误映射不变 |
| `UnmarshalJSON` | 用有序解码器代替 `json.Decoder`；第一个值后面还有数据时报错 | 调用方传入的都是 `json.Valid` 通过的文本 |
| `Extract` | legs 中没有 `*`、`[*]`、范围、`**` 的路径不建 `dup` 集合 | 这样的路径最多命中一个值，不可能有重复。按 legs 判断而不是按 `flags`：`popOneLastLeg` 返回的父路径 `flags` 为 0（与 Go 相同），legs 却可能含 `**` 或范围（`ArrayInsert` 不拒绝通配符）。是否把结果包成数组仍按 `flags`，与 Go 相同 |
| `ParseBinaryJSONFromString`、`UnmarshalJSON`（嵌套超过 101 层） | 不建 `JSONValue` 树、不调用 `appendBinaryJSON`，在 `Raw` 树上按其顺序做出错检查（§3） | 这样的文本在 Go 中一定失败，失败原因就是 `appendBinaryJSON` 按遍历顺序遇到的第一个错误或 TooDeep；govec `sys/deep` 覆盖了各种先后顺序 |
| `json.Valid` | 用 `gostd::json::unmarshal_raw`（迭代）解码，成功即合法 | 解码器接受的集合就是 `checkValid` 的集合（嵌套上限 10000 相同）；多建一棵树，只在 `JSON_VALID` 一类调用中用到 |
| `json.Valid`（路径解析中检查带引号的 key） | 只实现扫描器的字符串状态 | 输入总是以 `"` 开头 |

另外两处不改变结构的优化：浮点输出用新的 `gostd::strconv::append_float`（最短 `'e'`/`'f'` 直接写进缓冲区，不经过临时 `String`；其他格式回退到 `format_float`，`tidb-base` 中有 20 万个随机值的对照测试），对象 key 的二分查找用内联的逐字节比较（`bytes.Compare`），不调用 `memcmp`。

## 5. 验证

- **govec `json` 领域**：94,691 条，25 个 op。输入分几类：
  - **Go 测试字面量**（宪章第七条第 1 款）：`cmd/minelits` 从三个测试文件抽出的字面量（`testdata/json_literals.tsv`，在 `minedGoTests["json"]` 登记，CI 检查每个测试函数都有记录），去掉不是 JSON 也不是路径的字面量（结构体字段名、消息）。每个字面量做 `parse` 或 `path_parse`；同一测试函数内，文档两两做 `compare`/`contains`/`merge`，文档与路径两两做 `extract`/`remove`/`array_insert`/`modify`，期望值取 Go 的实际输出。共 14,772 条，`src` 为测试函数名。
  - **边界**：数字、转义、非法 UTF-8、孤立代理项、深度 99–10001、键长 65535/65536；`sys/deep` 是 100–10001 层的文本，包括出错先后顺序（前面元素、较小 key 的错误先报；对象的 key 先于值检查；重复 key 后者生效）和非法的深文本，各做 `parse`、`valid`、`unmarshal`；`sys/dup` 是父路径含 `**` 或范围、同一个值被命中两次的 `array_insert`/`remove`/`modify`/`extract`（Lucky 在 #21 中发现的问题）。
  - **随机**：随机文档与变异、随机路径；Lucky 的大对象生成器（1–400 个 key，公共前缀、多字节、200 字节长 key、重复 key），为控制向量体积缩到 60 个对象、值只取标量。
  - amd64 与 arm64 的记录完全相同（没有 FMA 位置）。
- 回放：`tests/govec_json.rs`，debug 和 release-checked 都通过；输出按 JSON 相等，错误按消息比对，Go panic 的记录要求 Rust 也 panic。
- **govec 表达不了的 Go 断言**（调用未导出函数或检查内部状态）移植为 `src/json/json_test.rs` 的单元测试：`TestContainsAnyAsterisk`、`TestPushBackOneIndexLeg`、`TestPushBackOneKeyLeg`、`TestDecodeEscapedUnicode`、`TestFunctions`、`TestBinaryJSONOpaque`、`TestHashValue`。其余测试函数的断言都被对应的 govec 记录覆盖（期望值来自 Go 本身，比测试中的断言更全）；Benchmark 函数对应 `benches/json.rs`。
- **fuzz**（A4 §6.3）：`tests/fuzz_json.rs`，没有 Go oracle 的随机检查：解析器字节（不 panic；`json_valid` 拒绝的文本报 ErrInvalidJSONText）、深路径对照递归路径（把文本包进 120 层数组，深路径报的错误必须等于递归路径对原文本报的错误，原文本能转换时为 TooDeep）、往返（打印再解析后比较相等，第二轮起打印是不动点）、二进制访问器（随机和截断的二进制可以出现 Go 式的越界 panic，不能出现整数溢出 panic）。默认 2 万次，`TIDB_TYPES_FUZZ_ITERS` 可加大；release-checked 下跑过 30 万次。
- 收敛检查：`converge.py --block D01.5` 128/128。
- **反证**：分别把 `Extract` 的 dup 判断改回按 `flags`、去掉深路径的 key 长度检查、把数组元素的遍历顺序反过来、重复 key 改为前者生效、把深路径的阈值改为 100，回放分别报 26、91、36、36、5 条不一致。

## 6. 性能（A4 §6.4）

同一台机器（Apple M 系列，macOS），Go 1.25.12 `go test -bench`（5 次平均）对 Rust criterion（jemalloc）。文档与操作相同（tracker `govec/domains/json_bench_test.go`、`crates/types/benches/json.rs`）。

| 基准 | Go | Rust | Rust 相对 Go | A4 目标 |
|---|--:|--:|--:|---|
| 解析 100 B | 1,426 ns | 801 ns | 1.78× | ≥ 3× |
| 解析 4 KB | 62.9 µs | 36.2 µs | 1.74× | ≥ 3× |
| 解析 256 KB | 3.89 ms | 2.56 ms | 1.52× | ≥ 3× |
| 文本输出 4 KB | 6.50 µs | 6.48 µs | 1.00× | ≥ 1.5× |
| 常量路径 Extract | 63.7 ns | 72.5 ns | **0.88×** | ≥ 3×（向量化场景） |
| 路径解析（无缓存） | 333 ns | 108 ns | 3.1× | — |
| 路径解析（Go 缓存命中） | 39.6 ns | —（表达式层缓存，0） | — | 每行 0 次解析 |
| 比较 4 KB | 11.8 µs | 5.9 µs | 2.0× | ≥ 2× |
| 哈希 100 B | 299 ns | 268 ns | 1.11× | ≥ 2× |

深嵌套的文本（一定失败，A4 §6.4"任何基准不慢于 Go"）：

| 嵌套层数 | Go | Rust（迭代路径） |
|--:|--:|--:|
| 256 | 25.5 µs | 6.4 µs |
| 257 | 26.6 µs | 6.5 µs |
| 1000 | 101 µs | 27.2 µs |
| 10000 | 1.07 ms | 268 µs |

结论：比较达标；解析、哈希快于 Go 但没到倍数目标；文本输出持平；`Extract` 慢 12%（9 ns）。剩余开销见 plan §3。评审结论（tidb-rs #21）：先合并这一版，倍数目标等 D01.7 chunk 和 D08.10 的 1024 行基准再定；修改已批准例外的目标与排期由负责人决定，见 issue tidb-rs #23。`Extract` 的快路径（单条、无通配符的路径直接沿 legs 走，Lucky 试验从 72 ns 降到 66.7 ns）作为性能块的第一项。
