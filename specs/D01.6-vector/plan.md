# D01.6 Vector 类型：plan

## 1. Rust 模块布局

| Rust | 内容 |
|---|---|
| `crates/types/src/vector.rs` | `VectorFloat32<'a>`、`ZeroVectorFloat32`、`Elements` / `ElementsMut` 视图、构造、解析、序列化与反序列化 |
| `crates/types/src/vector_functions.rs` | 距离、范数、`Add` / `Sub` / `Mul`、`Compare`，`madd32` / `madd64`（M-FMA） |
| `crates/types/src/jsoniter.rs` | jsoniter v1.1.12 中 `ParseVectorFloat32` 用到的部分 |
| `crates/base/src/gostd/strconv.rs` | 公开 `format_float`（Go `strconv.FormatFloat`） |
| `crates/base/src/gostd/math.rs` | `nan()`（Go `math.NaN`） |

## 2. 写法映射

| Go | Rust |
|---|---|
| `VectorFloat32{data []byte}` | `VectorFloat32<'a> { data: Cow<'a, [u8]> }` |
| `v.Elements()[i]`（读） | `v.elements().get(i)` / `iter()` |
| `copy(vec.Elements(), xs)`、`vr[i] = ...` | `vec.elements_mut().copy_from(xs)`、`set(i, ...)` |
| `(VectorFloat32, error)` | `Result<VectorFloat32<'static>, Error>` |
| `ZeroCopyDeserializeVectorFloat32(b) (v, rest, err)` | `Result<(VectorFloat32<'_>, &[u8]), Error>` |
| `strconv.AppendFloat(buf, f, 'g', 2, 32)` | `format_float(f, b'g', 2, 32)` |
| `distance += x * y`（M-FMA 位置） | `distance = madd32(distance, x, y); // [M-FMA] vector_functions.go:<行>` |
| `int(unsafe.Sizeof(v))` | 常量 24（Go 的 slice 头大小，内存统计与 Go 一致） |
| `Add`/`Sub`/`Mul` 三份相同的循环 | 一个 `elementwise` 辅助函数（`go-part`），三个公开函数各自标 `go:` |

## 3. 测试

- `tests/govec_vector.rs` 回放 `tests/vectors/v8.5.8/<目标>/vector.jsonl.gz`，`out` 按 JSON 相等，错误按消息比对，`panic` 记录要求 Rust 也 panic。
- arm64 本地生成；amd64 取 tracker PR 的 CI artifact `govec-vectors-ubuntu-latest`。

## 4. 反向引用与处理（N0）

tracker `data/block_edges.tsv` 中引用 D01.6 的块（D01.1 Datum、D01.7 Chunk、D01.8 codec、D01.11 rowcodec、D08.x 表达式等）都在 tidb-types 之上或同 crate，按 rank 方向依赖即可，无需例外。D01.6 自身只引用 tidb-base。

## 5. 遗留到其他块

- `Datum.SetVectorFloat32` / `GetVectorFloat32`（D01.1）：Datum 持有序列化字节，取值时用 `zero_copy_deserialize_vector_float32` 借用它。
- 表达式层的 `VEC_*` 函数（D08.x）直接调用本块的方法。
