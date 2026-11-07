# D02.3 parser 的 model、types、auth、duration：块 spec

状态：待评审（2026-11-07）。阶段 P1，D06.1（meta/model）的 Part 0。tracker 块定义见 tidb-rs-tracker PR #10。

## 1. 为什么单列一块

tracker 把 `pkg/parser/` 整个算在 D02.2 里，并标为 DONE。语法表的移植确实完成了，但 `parser/model`、`parser/types`、`auth.UserIdentity`、`parser/duration` 在 tidb-rs 中**只有 PoC 手写替身**：`ast::FieldType` 只有 7 个字段，`ast::CIStr` 没有 `L`，没有来源标注，也没有 Go 的 JSON 行为。D02.2 的 DONE 对这些文件是高估的。

D06.1 的表结构（`TableInfo`、`ColumnInfo` 等）存在 TiKV 里，与 Go TiDB 共用，`FieldType` 和 `CIStr` 的 JSON 必须与 Go 逐字节一致，所以先把它们忠实移植过来。

## 2. Go 范围（86 个函数）

| Go 文件 | 函数 | Rust |
|---|--:|---|
| `pkg/parser/model/model.go` | 16 | `crates/parser/src/model.rs` |
| `pkg/parser/types/field_type.go` | 52 | `crates/parser/src/types/field_type.rs` |
| `pkg/parser/types/etc.go` | 6 | `crates/parser/src/types/etc.rs` |
| `pkg/parser/types/eval_type.go` | 3 | `crates/parser/src/types/eval_type.rs` |
| `pkg/parser/auth/auth.go` | 6 | `crates/parser/src/auth.rs` |
| `pkg/parser/duration/duration.go` | 2 | `crates/parser/src/duration.rs` |
| `pkg/parser/ast/ddl.go`、`functions.go` 中 meta/model 用到的部分 | 1 | `crates/parser/src/ast_ddl.rs` |

`parser/auth` 的密码插件（`caching_sha2.go`、`mysql_native_password.go`、`tidb_sm3.go`）不在本块，属于权限移植（D04.1 或 D02.2 的后续）；它们同样只有 PoC 状态。

## 3. 设计决定

### 3.1 Go JSON：在 tidb-base 中按 `encoding/json` 的规则解码

`FieldType.UnmarshalJSON` 是 `json.Unmarshal(data, &jsonFieldType)`。要与 Go 结果一致的不只是成功时的值，还有：
- 字段匹配：先精确，再按 `foldName`（ASCII 转大写，非 ASCII 取 fold 集合中最小的 rune）不区分大小写匹配，所以 `{"ſ…"}` 能匹配到 `S…`。
- `null` 对基本类型不做任何修改；对切片置为 nil。
- 数字按字面量解析：`strconv.ParseInt/ParseUint(s, 10, 64)` 加位宽检查，`1e2`、`1.5`、`-0`（无符号）都是类型错误，消息里带字面量。
- 类型错误不中断解码：记录第一处错误（带结构体名和字段路径），其余字段照样解码。`jsonFieldType` 解码出错时 `FieldType` 整体不变；`CIStr` 的第一次解码直接写入接收者，出错后再按字符串解码。
- 字符串里的无效 UTF-8 每个字节换成一个 U+FFFD（`from_utf8_lossy` 会把截断的多字节序列只换成一个，不一致）。编码时每个无效字节写成 `�` 转义。

这些实现在 `tidb_base::gostd::json` 中（`Raw`、`unmarshal_raw`、`Decoder`、`find_field`、`append_go_string`、`go_lossy`），D06.1 Part 1 生成的 JSON 层也建在它上面。同时修正了 config 已在用的 `unmarshal` 对无效 UTF-8 的处理（同一个 `string()` 函数）。

语法错误的消息文本不移植（`*json.SyntaxError`，与 D00.2、D01.3 相同）：语法错误时 Rust 只保证报错。

### 3.2 字符串类型

- `FieldType` 的 `elems` 是字节串（`Vec<u8>`）：ENUM / SET 的元素可以是二进制字面量（`elemsIsBinaryLit` 就是为此存在的），不一定是 UTF-8。
- `charset`、`collate`、`CIStr`、`UserIdentity` 的字段用 `String`：它们是字符集、排序规则名或 SQL 标识符，来自 lexer（合法 UTF-8）或 schema JSON（解码器已替换无效 UTF-8）。
- `CompactStr`、`InfoSchemaStr`、`String` 返回 `String`：`format.OutputFormat` 按 rune 遍历，Go 的输出本来就是合法 UTF-8。`Restore` 原样写出元素，输出到 `Vec<u8>`。

### 3.3 nil 切片

Go 的 nil 切片和空切片在 `json.Marshal` 中不同（`null` 与 `[]`），schema JSON 依赖这一点，所以 `elems`、`elems_is_binary_lit` 用 `Option<Vec<_>>`，`None` 即 nil。比较（`Equal`、`Equals`）按 `slices.Equal` 把两者视为相等，与 Go 一致。

### 3.4 命名与常量

枚举类型是 newtype（`PartitionType(isize)`），常量沿用 Go 名（M-GONAME）。Go 的 `String()` 方法按 M-NAME 写成 `to_string`。`FieldType` 的字段保持私有，只通过 getter/setter 访问，与 Go 相同。

### 3.5 PoC 替身

`ast::CIStr`、`ast::FieldType` 暂时保留，改标为 `glue: poc`：PoC 的规划器、执行器和 meta 都用它们，替换要等对应的块移植。本块不改 PoC 的行为。

## 4. 已知差异

1. `FieldType.Clone`：Go 复制结构体、共享元素切片，修改克隆体的元素会影响原对象；Rust 复制切片。没有调用方依赖这种别名。
2. `FieldType.MemoryUsage`：Go 用切片的 `cap()`；Rust 用 `Vec::capacity()`，两者的增长策略不同。构造时长度等于容量的情况（向量中的全部情况）结果一致。
3. 一个 JSON 对象中同一个切片字段出现三次以上、且中间一次更短时，Go 会在已有底层数组的容量内"复活"旧元素；Rust 截断后重新填充。只有构造的 JSON 才会这样，Go 写出的 JSON 不会。
4. JSON 语法错误的消息文本（见 3.1）。
5. `ParseDuration` 的输入是 `&str`；Go 的 `string` 可能不是 UTF-8，但 TTL 间隔来自 SQL 或 schema JSON，都是 UTF-8。
6. nil `*UserIdentity` 的 `String`/`LoginString`（Go 返回 `""`）：Rust 调用方用 `Option` 处理；`GetUserAndHostName` 接收 `Option`。

## 5. 验收

- converge：`matched 86/86`。
- govec `parser_types` 18,099 条，arm64 和 amd64 都回放通过（两个目标只在 3 条 `ParseDuration` 溢出记录上不同，M-F2I）。
- 故意改坏向量里的两条，测试报 `2 of 18099 records differ`。
- clippy `-D warnings`、fmt、`check_layers`、`cargo test --workspace`（PoC 不回退）。
