# D01.6 Vector 类型：块 spec

状态：草案（2026-11-07）。阶段：P1，A 线第 2 块的一部分（ROADMAP §4 L1）。基线：Go TiDB v8.5.8（`8b857efa2`）。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[mappings.md](../../docs/porting/mappings.md)、[N0 crate 分层](../exceptions/N0-crate-layering/spec.md)。本块落在 `tidb-types`（rank 2）。

## 1. Go 范围

函数清单来自 tracker `data/func_blocks.tsv` 中 block = D01.6 的行，共 31 个函数。

| Go 文件 | 函数数 | Rust 文件 |
|---|--:|---|
| `pkg/types/vector.go` | 19 | `crates/types/src/vector.rs` |
| `pkg/types/vector_functions.go` | 12 | `crates/types/src/vector_functions.rs` |

**块外依赖**

| 来源 | 内容 | Rust | 标注 |
|---|---|---|---|
| `github.com/json-iterator/go@v1.1.12` | `ParseString`、`ReadArrayCB`、`ReadFloat64` 及其用到的内部函数（`nextToken`、`readPositiveFloat64`、`readFloat64SlowPath`、`readNumberAsString`、`validateFloat`、`ReportError`、`skipThreeBytes` 等），只覆盖从字节串读的情形 | `crates/types/src/jsoniter.rs` | `go-dep:` |
| Go `strconv.FormatFloat` | `'g'` 精度 2、`'f'` 最短，bitSize 32 | `tidb-base` `gostd::strconv::format_float`（公开已有的 `gofmt::append_float`） | `go-std:` |
| Go `math.NaN` | 位模式 `0x7FF8000000000001` | `tidb-base` `gostd::math::nan` | `go-std:` |
| `pkg/parser/types.UnspecifiedLength`（D02.2） | 常量 -1 | `vector.rs` 中的局部常量（tidb-types 不依赖 tidb-parser） | `glue:` |

## 2. 设计决定

### 2.1 元素视图，不用 `unsafe`

Go 的 `Elements()` 用 `unsafe.Slice` 把 `data[4:]` 当成 `[]float32`，不管对齐。tidb-types 是 `forbid(unsafe_code)`，而且从任意 `&[u8]` 反序列化出来的数据不保证 4 字节对齐，不能转成 `&[f32]`。所以 `elements()` 返回只读视图 `Elements`（`len`、`get(i)`、`iter()`、`prefix(n)`），`elements_mut()` 返回可写视图 `ElementsMut`（`set(i, v)`、`copy_from`）。每个元素是一次非对齐的小端读写，不复制整组元素。Go 中 `Elements()` 只在 `pkg/types` 内部使用（`vector.go`、`vector_functions.go` 和测试），其他包都通过序列化字节交换向量，所以这个改动不影响后续块。

### 2.2 零拷贝反序列化用生命周期表达

Go 的 `ZeroCopyDeserializeVectorFloat32` 返回的向量与输入共享内存（注释要求调用方不要修改输入）。Rust 中 `VectorFloat32<'a>` 持有 `Cow<'a, [u8]>`：反序列化借用输入，构造函数（`InitVectorFloat32`、`CreateVectorFloat32`、`ParseVectorFloat32`、`Clone`、`Add` 等）返回 `VectorFloat32<'static>`。`elements_mut()` 在数据是借用时先复制一份（Go 会直接写进调用方的字节，移植代码中没有这样用的地方）。

### 2.3 浮点

- **M-FMA**：登记表中本块有 6 个位置（vector_functions.go:44、71、100、101、102 的 float32 累加和 :152 的 float64 累加），arm64 融合，amd64 GOAMD64=v1 不融合。`madd32` / `madd64` 按目标平台选择 `mul_add` 或分开的乘加，写法与 D01.2 的 `to_float64_madd` 一致。距离记录因此在两个目标上不同，tracker 的 `targetDiffs` 按 op 登记。
- **NaN 的位**：`CosineDistance` 返回 Go 的 `math.NaN()`（`0x7FF8000000000001`），Rust 的 `f64::NAN` 位不同，改用 `gostd::math::nan()`。`NegativeInnerProduct` 的 `d * -1` 被 gc 改写成取负（NaN 的符号位翻转），Rust 写成 `-d`。
- **默认 NaN 的符号**：无效运算（Inf−Inf、0×Inf）得到硬件的默认 NaN，amd64 上带符号位（`0xFFF8…`），arm64 上不带。Go 和 Rust 在同一硬件上得到同样的位，不需要特别处理，但两个目标的向量在这些记录上不同（38 条）。
- float64 → float32 的转换（`float32(v)`）和 Rust 的 `as f32` 都是就近舍入、溢出为 ±Inf，所以 `[1e39]` 解析得到含 +Inf 的向量，与 Go 相同。

### 2.4 jsoniter 照搬

`ParseVectorFloat32` 的行为由 jsoniter 决定，与 JSON 规范和 `encoding/json` 都不同：
- 不超过 6 位小数、整数部分不超过 2^53 的数走快路径，算法是 `float64(整数) / 10^小数位数`，结果不一定是正确舍入的值；
- 其他情况取出 `+-.eE0-9` 组成的串交给 `strconv.ParseFloat`，所以 `[+5]`、`[+00.5]` 被接受；
- `null` 解析成零维向量，`nul` 报错；数组之后的字节不检查。

这些都照 Go 移植。错误对外只有一个结果，即 `Invalid vector text: <输入>`，但 `Iterator.Error` 仍按 Go 区分 `io.EOF` 和其他错误，因为 `ReportError`、`unreadByte` 和慢路径的行为取决于它。

### 2.5 错误（M-ERR）

本块的错误都是 `errors.Errorf`（不带错误码），实现为 `tidb_base::errors::errorf`，消息逐字节一致。Go 在出错时返回 `ZeroVectorFloat32`（反序列化还返回原输入），Rust 只返回 `Err`，调用方需要零向量时自己取 `ZeroVectorFloat32`。

### 2.6 `PeekBytesAsVectorFloat32` 的 uint32 回绕

Go 用 uint32 计算 `elements*4 + 4`，`elements >= 2^30` 时回绕。Rust 用 `wrapping_mul` / `wrapping_add` 照搬（M-INT）。回绕后的长度为 0 或 4 时，Go 在 `Len` / `Elements` 中越界 panic，Rust 也 panic（向量中有这样的记录）。回绕后不小于 8 时，Go 得到长度字大于数据的向量，`Elements` 读越界内存（未定义行为）；Rust 在 `elements()` 切片时 panic。govec 不生成后一类输入。

## 3. 已知差异（都不影响 TiDB 的结果）

| # | 内容 | 理由 |
|---|---|---|
| 1 | 长度字大于数据的向量：Go 读越界内存，Rust panic | Go 的行为未定义；这样的字节只能来自损坏的存储 |
| 2 | 大端机器：Go 在 init 时 panic，Rust 编译失败 | 编译期断言 |
| 3 | `elements_mut()` 对借用的数据先复制 | Go 写穿到调用方字节，移植代码中没有依赖这一点的地方 |

## 4. 验收

1. converge：`matched 31/31, missing 0`，`--fma-sites` 通过（6 个位置）。
2. govec `vector` 领域的向量在 aarch64 和 x86_64 上全部通过（`tests/govec_vector.rs`），debug 和 `release-checked` 两种构建都跑。
3. clippy `-D warnings`、fmt、`check_layers --blocks --block-edges` 通过。
