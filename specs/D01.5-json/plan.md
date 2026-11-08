# D01.5 JSON 类型：plan

## 1. 模块布局

`crates/types/src/json/`：`json_binary.rs`、`json_binary_functions.rs`、`json_path_expr.rs`、`json_constants.rs`，与 Go 文件一一对应；`mod.rs` 只做导出。`tidb-base` 新增 `gostd::strconv::append_float`。

## 2. 写法映射

| Go | Rust |
|---|---|
| `BinaryJSON{TypeCode, Value []byte}` | `BinaryJSON<'a> { type_code, value: Cow<'a, [u8]> }` |
| 子值共享父文档的切片 | `borrow_elem` / `borrow_opt` 把子值重新借用为文档的生命周期（`glue:`） |
| `&bj.Value[0]`（去重、修改目标） | `ptr()`：值字节的地址 |
| `func (bj BinaryJSON) marshalTo(buf []byte) ([]byte, error)` | `fn marshal_to(&self, buf: Vec<u8>) -> Result<Vec<u8>, Error>`（M-ERR） |
| `map[string]any` / `[]any` / `json.Number` | `JSONValue`（`Object(BTreeMap<Vec<u8>, _>)`、`Array`、`Number(String)` 等） |
| `extractTo(buf, pathExpr, dup, one)` 中的 `popOneLeg` | `extract_to(buf, legs: &[PathLeg], …)`，用切片代替复制路径 |

## 3. 已知的剩余开销（后续优化的起点）

- **Extract**：`sample` 显示时间主要在 key 的二分查找（每次探测的边界检查、`Cow` 解引用）和 `val_entry_get`。Go 的同一路径 63.7 ns，Rust 72.5 ns。`unsafe` 在 tidb-types 中禁止，进一步的办法是给 `BinaryJSON` 加只读视图类型（A4 §5.3 的 `JsonRef`），属于整体重写的范围。
- **解析**：仍是"有序解码器建树 → `JSONValue` 树（BTreeMap）→ 二进制"三步，外加一遍 `max_nesting`。单遍直接写二进制（A4 §5.4）可以去掉两层中间树。
- **哈希**：`hash_value(Vec::new())` 的缓冲区由调用方传入，复用缓冲区时每行 0 次分配（A4 目标），基准里每次新建。

## 4. 留给其他块

- `Datum` 中 JSON 的存取（D01.1）、`ConvertJSONTo*`（D01.4）、chunk 列（D01.7）、codec（D01.8）直接用本块的 `BinaryJSON`。
- SQL 层 JSON 函数（D08.10）在函数阶段，常量路径的缓存放在那里（A4 §5.6）。
- `JSON_SCHEMA_VALID` 依赖的 `qri-io/jsonschema` 见 A4 §8 第 2 条。
