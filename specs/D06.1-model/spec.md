# D06.1 元数据模型（meta/model）：块 spec

状态：待评审（2026-11-07）。阶段 P1。Part 0（parser 的 model、types、auth、duration，块 D02.3）已合并（PR #17）。本文是 Part 1：Go-JSON 层、schema 结构体和它们的方法。Part 2 是 `job.go`、`reorg.go` 中保留的函数。

## 1. 为什么重要

`TableInfo`、`DBInfo`、`PolicyInfo`、`ResourceGroupInfo` 等结构体以 `encoding/json` 的形式存在 TiKV 的 meta key 下，Go TiDB 和 tidb-rs 读写的是同一份数据。混合部署、滚动升级、回退都要求：

- Rust 读出的每个值与 Go 读出的相同，包括 Go 对"坏"数据的处理（缺字段、多字段、大小写不同的键、错误类型、重复键、`null`）；
- Rust 写出的字节与 Go 写出的相同，Go 重新读出后不变。

因此验收的核心是：同一段 JSON，Go `Unmarshal → Marshal` 与 Rust 解码 → 编码**逐字节相同**，并且错误相同。

## 2. Go 范围（153 个函数）

| Go 文件 | 函数 | Rust | 本 PR |
|---|--:|---|---|
| `pkg/meta/model/table.go` | 79 | `table.rs` | 全部 |
| `pkg/meta/model/column.go` | 35 | `column.rs` | 全部 |
| `pkg/meta/model/index.go` | 24 | `index.rs` | 全部 |
| `pkg/meta/model/placement.go` | 7 | `placement.rs` | 全部 |
| `pkg/meta/model/resource_group.go` | 5 | `resource_group.rs` | 全部 |
| `pkg/meta/model/db.go` | 3 | `db.rs` | 全部 |
| `pkg/meta/model/bdr.go` | 1 | `bdr.rs` | 全部 |
| `pkg/meta/model/job.go` | 9 | — | Part 2 |
| `pkg/meta/model/reorg.go` | 4 | — | Part 2 |
| `pkg/types/etc.go`（`ColumnNeedRestoredData`） | 1 | — | Part 2 |

本 PR 移植 139 个，余下 14 个在 Part 2：`job.go` 和 `reorg.go` 的函数（`Job.Decode`、`Job.String`、`ActionType.String` 等）要先有 `errors.Error` 的 JSON、`json.RawMessage` 和 `Job` 的并发字段；`ColumnNeedRestoredData` 要用 `types.NeedRestoredData` 和 collate 包，与正在进行的 D01.9 重叠。

`flags.go` 没有函数，只有常量，随结构体一起生成。

## 3. 设计决定

### 3.1 结构体与 JSON 层由 Go 反射生成

tracker 的 `govec/cmd/rsgen -model` 从 Go 反射读出 36 个结构体（`reflect.Type` 的字段、tag、嵌入关系），生成：

- 结构体定义（`types_gen.rs`）和常量、命名类型（`consts_gen.rs`）；
- `encode_json` / `decode_json`：按 `encoding/json` 的 `typeFields` 计算字段表（嵌入字段的提升、同名字段的支配规则、同一深度同名字段**全部丢弃**、`json:"-"`、无效 tag、`omitempty`），按字段索引排序；
- `GoCopy`：Go 的结构体赋值 `nt := *t`（见 3.5）。

方法（139 个函数）手写在以 Go 文件命名的模块中。生成器不生成逻辑，只生成数据和机械的编解码。

`,string` tag 在 meta/model 中没有出现，生成器遇到时直接报错，而不是悄悄生成错误代码。

### 3.2 编码（`json.Marshal`）

- 字符串：HTML 转义（`<`、`>`、`&` 写成 `<` 等），U+2028/U+2029 转义，无效 UTF-8 每个字节写成 `�`。
- `float64`：Go 的格式（指数形式的阈值是 `< 1e-6` 或 `>= 1e21`，`e-09` 写成 `e-9`）；NaN、±Inf 是 `Marshal` 的错误（`json: unsupported value: NaN`），不是 panic。
- map：整数键先格式化成十进制字符串，再按字符串排序（所以 `10` 在 `9` 前面）。
- `[]byte`：标准 base64；nil 写 `null`。
- `time.Time`：`MarshalJSON`（RFC 3339，纳秒，年份超出 [0, 9999] 是错误）。
- nil 切片 / map / 指针写 `null`，空切片写 `[]`（3.4）。

### 3.3 解码（`json.Unmarshal`）

在 D02.3 的 `tidb_base::gostd::json::Decoder` 上扩展：

- **解码到已有值**，不清零：缺的字段保持原值；切片按元素逐个解码到已有元素上，多余的截掉；map 合并（新值解码到零值上）；指针为 nil 时新建，`null` 置 nil；嵌入的 nil 指针在解码提升字段时新建（Go 的 `subv.Set(reflect.New(...))`）。
- `interface{}`：数字一律成为 `float64`（不保留原始字面量），重新编码时按 3.2 的规则。超过 2^53 的整数会丢精度，与 Go 相同。原始字面量只用于 `json.RawMessage`（Part 2 的 `Job`）。
- 错误：类型错误记录第一个，其余字段继续解码；错误的上下文是最内层结构体名加字段路径，提升字段的路径中带上经过的嵌入字段的 Go 名（`PolicyInfo.PlacementSettings.followers`）。`UnmarshalJSON` 方法返回的错误（`CIStr`、`FieldType`、`time.Time`）终止解码，并且覆盖之前记录的错误。
- 字段匹配：先精确，再按 `foldName`（D02.3）。
- 整数：字面量按 `ParseInt`/`ParseUint` 加位宽检查；`1e2`、`1.5`、`-0`（无符号）、超范围都是类型错误。map 的整数键按 `ParseInt` 解析，失败是类型错误。

### 3.4 Go 类型到 Rust 类型

| Go | Rust | 说明 |
|---|---|---|
| `[]T`、`map[K]V` | `Option<Vec<T>>`、`Option<BTreeMap<K, V>>` | `None` 即 nil；`null` 与 `[]` 不同（D02.3 3.3） |
| `*T` | `Option<Box<T>>` | |
| `[]*T` | `Option<Vec<T>>` | 元素按值持有，见第 4 节第 1 条 |
| `interface{}` | `AnyValue` | `Value`（JSON 形状的值）或 `GoString(Vec<u8>)`（Go 代码放进去的任意字节，如 BIT 列默认值） |
| `string` | `String` | 来自 JSON 的值，见 mappings.md M-BYTES |
| `[]byte`、可能是用户原始字节的 `string` | `Vec<u8>` | 同上 |
| `type X int` 等 | `pub struct X(pub isize)` | M-INT、M-GONAME |
| `type X string` | `pub struct X(pub Cow<'static, str>)` | 常量可以是 `const` |
| `time.Time` | `tidb_base::gostd::time::Time` | 零值、`==` 与 Go 相同 |

### 3.5 `Clone` 与 Go 的结构体赋值

Go 的 `TableInfo.Clone` 先 `nt := *t`（浅拷贝，共享指针），再对部分字段调用各自的 `Clone`。这些 `Clone` 的结果与浅拷贝不同：`make([]T, len(nil))` 得到空切片，所以 nil 变 `[]`。如果 Rust 的结构体拷贝直接用 `Clone`，`Constraints` 这类 Go 只浅拷贝的字段就会被"Go 式克隆"，JSON 从 `null` 变成 `[]`。

所以结构体赋值用单独的 `GoCopy`（生成），手写的 `Clone` 实现 Go 的 `Clone` 方法，二者不混用。Rust 没有共享指针，"共享"一律是复制；Go 代码不依赖修改副本会影响原对象（见第 4 节第 3 条）。

### 3.6 `IndexInfo.ConditionExpr`

调用 parser 解析部分索引的条件。tidb-rs 的 parser 是 PoC 移植（D02.2），返回 PoC 的 AST；标为 L2。

## 4. 已知差异

1. `[]*T` 中的 `null` 元素：Go 得到 nil 指针，重新编码为 `null`；Rust 按值持有元素，得到零值。Go 写出的 meta 中不会有 `null` 元素（`TableInfo.Columns` 等由 DDL 代码填写）。生成器的随机值不产生 `null` 元素。
2. `time.Time` 的 `UnmarshalJSON` 错误消息按 Go 的 `parseStrictRFC3339` 生成，`time.ParseError` 的个别措辞与 Go 不完全相同时，以向量为准（当前向量全部一致）。
3. `Clone` / `Copy`：Go 浅拷贝共享指针，Rust 复制（3.5）。`DBInfo.Copy` 复制表信息而不是共享，标为 L2。
4. `TableInfo.GetNonTempColumns` 的结果顺序：Go 遍历 map，顺序随机；Rust 按列名排序（M-MAPORDER）。向量排序后比较。
5. JSON 语法错误的消息文本不移植（D02.3 3.1）。

## 5. 验收

- converge：`matched 139/153`，缺的 14 个是第 2 节列出的 Part 2 函数，tracker 的 `model` 领域把它们登记为豁免并写明理由。
- govec `model` 领域 8,990 条记录，arm64 和 amd64 都回放通过：
  - roundtrip 3,866 条：从 v8.5.8 集群导出的 866 份 meta（857 张表、9 个库；DDL 见 tracker `govec/domains/testdata/model_cluster_ddl.sql`，覆盖全部列类型、分区、placement、TTL、视图、序列、外键、check 约束、资源组），每个结构体的随机值 1,500 份，二者的变异 1,500 份（删键、重复键、改大小写、未知键、错误类型、超范围数字、`null`）；
  - into 300 条：解码到已有值；
  - 方法 3,800 余条：表、列、索引、分区、placement、资源组、外键和其他函数；
  - 其中 603 条 Go 返回错误。
- clippy `-D warnings`、fmt、`check_layers --blocks`、`cargo test --workspace`。
