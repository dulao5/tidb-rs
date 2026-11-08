# D01.5 JSON 类型：块 spec

状态：草案（2026-11-08）。阶段：P1，A 线。基线：Go TiDB v8.5.8（`8b857efa2`）。
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
- **深嵌套**：嵌套超过 256 层的文档在 512 MB 栈的线程上解码（Go 的 goroutine 栈会增长，Rust 的递归不会），结果相同。
- **路径缓存**：去掉全局 `peCache`（A4 §5.6），`ParseJSONPathExpr` 每次都解析；常量路径由表达式层缓存（D08.10）。`init`、`jsonPathExpressionKey.Hash` 标为 L2（不保留）。

## 4. L2 位置

| 函数 | 改动 | 为什么结果相同 |
|---|---|---|
| `ParseBinaryJSONFromString` | Go 先 `json.Valid` 再 `UnmarshalJSON`，各扫一遍；这里只解码一次，解码失败就是 Go 的 `json.Valid` 失败 | 解码器接受的集合与 `json.Valid` 相同（嵌套上限 10000 先单独检查），错误映射不变 |
| `UnmarshalJSON` | 用有序解码器代替 `json.Decoder`；第一个值后面还有数据时报错 | 调用方传入的都是 `json.Valid` 通过的文本 |
| `Extract` | 不含通配符、范围、`**` 的路径不建 `dup` 集合 | 这样的路径最多命中一个值，不可能有重复 |
| `json.Valid`（路径解析中检查带引号的 key） | 只实现扫描器的字符串状态 | 输入总是以 `"` 开头 |

另外两处不改变结构的优化：浮点输出用新的 `gostd::strconv::append_float`（最短 `'e'`/`'f'` 直接写进缓冲区，不经过临时 `String`；其他格式回退到 `format_float`，`tidb-base` 中有 20 万个随机值的对照测试），对象 key 的二分查找用内联的逐字节比较（`bytes.Compare`），不调用 `memcmp`。

## 5. 验证

- **govec `json` 领域**：76,262 条（其中 19,260 条是错误），23 个 op：parse 23,336、extract 9,000、compare 6,000、modify 6,000、path_parse 5,183、create 3,022、hash、remove、contains、overlaps、search 各 2,500，array_insert、merge、merge_patch 各 1,800，walk 1,200，以及 unquote、type、depth、keys、peek、get_value 等。amd64 与 arm64 的记录完全相同（没有 FMA 位置）。
- 回放：`tests/govec_json.rs`，debug 和 release-checked 都通过；输出按 JSON 相等，错误按消息比对，Go panic 的记录要求 Rust 也 panic（本领域没有）。
- 收敛检查：`converge.py --block D01.5` 128/128。

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

结论：比较达标；解析、哈希快于 Go 但没到倍数目标；文本输出持平；`Extract` 慢 12%（9 ns）。剩余开销见 plan §3。A4 的倍数目标是"初值，基线实测后定稿"，是否为达到它做整体重写，交给评审决定（PR 描述中的问题 1）。
