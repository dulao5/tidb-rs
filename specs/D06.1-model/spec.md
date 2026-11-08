# D06.1 元数据模型（meta/model）：块 spec

状态：Part 1 已合并（PR #20，2026-11-07）；Part 2 待评审（2026-11-08）。阶段 P1。Part 0（parser 的 model、types、auth、duration，块 D02.3）已合并（PR #17）。Part 1：Go-JSON 层、schema 结构体和它们的方法（139 个函数）。Part 2：`job.go`、`reorg.go` 中读路径保留的 13 个函数、`ColumnNeedRestoredData`，以及 job 相关结构体的 JSON（3.7 节）。

## 1. 为什么重要

`TableInfo`、`DBInfo`、`PolicyInfo`、`ResourceGroupInfo` 等结构体以 `encoding/json` 的形式存在 TiKV 的 meta key 下，Go TiDB 和 tidb-rs 读写的是同一份数据。混合部署、滚动升级、回退都要求：

- Rust 读出的每个值与 Go 读出的相同，包括 Go 对"坏"数据的处理（缺字段、多字段、大小写不同的键、错误类型、重复键、`null`）；
- Rust 写出的字节与 Go 写出的相同，Go 重新读出后不变。

因此验收的核心是：同一段 JSON，Go `Unmarshal → Marshal` 与 Rust 解码 → 编码**逐字节相同**，并且错误相同。

## 2. Go 范围（153 个函数）

| Go 文件 | 函数 | Rust | PR |
|---|--:|---|---|
| `pkg/meta/model/table.go` | 79 | `table.rs` | Part 1 |
| `pkg/meta/model/column.go` | 35 | `column.rs` | Part 1 |
| `pkg/meta/model/index.go` | 24 | `index.rs` | Part 1 |
| `pkg/meta/model/placement.go` | 7 | `placement.rs` | Part 1 |
| `pkg/meta/model/resource_group.go` | 5 | `resource_group.rs` | Part 1 |
| `pkg/meta/model/db.go` | 3 | `db.rs` | Part 1 |
| `pkg/meta/model/bdr.go` | 1 | `bdr.rs` | Part 1 |
| `pkg/meta/model/job.go` | 9 | `job.rs` | Part 2 |
| `pkg/meta/model/reorg.go` | 4 | `reorg.rs` | Part 2 |
| `pkg/types/etc.go`（`ColumnNeedRestoredData`） | 1 | `types_etc.rs` | Part 2 |

Part 1 移植 139 个，Part 2 移植余下 14 个：`job.go` 和 `reorg.go` 的函数（`Job.Decode`、`Job.String`、`ActionType.String` 等）要先有 `errors.Error` 的 JSON、`json.RawMessage` 和 `Job` 的并发字段（3.7 节）；`ColumnNeedRestoredData` 用 `types.NeedRestoredData` 和 collate 包（D01.9）。`types.NeedRestoredData` 及其辅助函数（`IsNonBinaryStr`、`IsString`、`IsTypeVarchar`、`IsTypeUnspecified`，块 D01.1）先移植到 `tidb-types` 的 `etc.rs`；`ColumnNeedRestoredData` 接收 model 的结构体，所以放在 tidb-model（Go 的 `pkg/types` 依赖 `pkg/meta/model`，Rust 的分层方向相反）。

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

**出错路径逐处对照**（Go 1.25 decode.go；每处在 govec 中有一条 `site/...` 记录，基值的每个字段都有值）：

| Go 位置 | Go 的行为 | Rust |
|---|---|---|
| `literalStore` null | interface / 指针 / map / 切片置零；基本类型和 string 不变；有 UnmarshalJSON 的值类型调用 `UnmarshalJSON("null")`（`FieldType` 因此变成零值，`CIStr`、`time.Time` 不变） | 同 |
| `literalStore` bool / string / number 进错误的类型 | `saveError`，不写，继续 | 同 |
| `[]byte` 的 base64 错误 | `saveError`，不写；`\r`、`\n` 被跳过；`""` 得到空切片 | 同 |
| number 进 interface，超出 float64 | `saveError; break`，**不写**（保留原值）；嵌套在数组 / 对象里的则成为 `nil` 元素 | 同（`Decoder::any_opt`）。评审修正 |
| 整数 / 浮点超范围、非整数字面量 | `saveError`，不写 | 同 |
| `array` 进非切片 | `saveError` 并跳过 | 同 |
| 数组元素出错（`saveError`） | 元素保持原值，继续 | 同 |
| 数组元素的 `UnmarshalJSON` 出错 | 立即返回：已解码的元素保留，**不截断**，之后的字段不再解码 | 同 |
| 数组更短 / 为空 | 截断；`[]` 得到非 nil 的空切片 | 同 |
| `object` 进非 map 非结构体 | `saveError` 并跳过 | 同 |
| map | 先分配；每个值解码进新的零值；先解码值、再转换键，键的 `ParseInt` 失败时 `saveError`、不存这一项；值的 `UnmarshalJSON` 出错时立即返回、不存这一项 | 同。评审时修正了两处：值出错后不再存这一项；`ParseInt` 接受前导 `+` |
| 结构体字段 | 未知键跳过；重复键依次生效；提升字段先分配经过的嵌入指针（即使值是 `null` 或类型错误） | 同 |
| `*T` 字段遇到错误类型 | `indirect` 先分配，再 `saveError` | 同 |
| `UnmarshalJSON` 返回错误 | 解码终止，返回该错误（类型错误加上下文），覆盖之前存下的错误 | 同 |
| `time.Time.UnmarshalJSON` | 非字符串：返回错误，不写；解析失败：`*t, err = parseStrictRFC3339(data)` **先赋值**，字段成为零时间 | 同。评审修正 |
| `CIStr.UnmarshalJSON` | 先按结构体解码进接收者（部分写入保留），失败后再按字符串解码 | 同 |
| `FieldType.UnmarshalJSON` | 解码进临时值，失败时不变 | 同 |
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

### 3.7 Part 2：job 相关结构体

生成器的根类型加上 `Job`，由它带出 `SubJob`、`MultiSchemaInfo`、`DDLReorgMeta`、`HistoryInfo`、`TimeZoneLocation`、`TraceInfo`、`InvolvingSchemaInfo`、`AddForeignKeyInfo`，共 45 个结构体。三种 Go 类型有自己的 JSON 方法：

| Go | Rust | JSON |
|---|---|---|
| `*terror.Error`（= `errors.Error`） | `Option<tidb_base::Error>` | `MarshalJSON` / `UnmarshalJSON` 移植到 `tidb_base::errors`（go-dep `compatible_shim.go`）。编码：`{"class","code","message","rfccode"}`，`class` 由 RFC 码的组件名反查（未知为 0）。解码：先解码一个新的 `jsonError`；类型错误经 `errors.Trace` 返回，外层不加错误上下文（`addErrorContext` 只认 `*UnmarshalTypeError`）；`rfccode` 为空且 `class > 0` 时由旧的 class 拼出 `组件:code`。`null` 置 nil；其他值先分配（`indirect`），所以解码失败时字段是零值的 `errors.Error`。`errors.Error` 加了 `Default`（Go 的零值） |
| `go.uber.org/atomic.Int64`（`DDLReorgMeta` 的 `Concurrency`、`BatchSize`、`MaxWriteSpeed`） | `i64` | 编码 `json.Marshal(i.Load())`。解码 `json.Unmarshal(b, &v)` 到新的 int64 再 `Store`：`null` 存 0（不同于普通 int64 字段的"不变"）；错误由 `UnmarshalJSON` 返回，终止解码，类型错误带字段上下文 |
| `json.RawMessage`（`Job.RawArgs`、`SubJob.RawArgs`） | `Option<Vec<u8>>` | 解码保留输入中该值的原始字节（空白、转义原样；`null` 得到字节 `null` 而不是 nil）：`Raw` 的字符串、数组、对象带上输入中的范围（`Span`），生成的 `unmarshal_json` 对含 RawMessage 的类型用 `Decoder::with_source`。编码：`appendCompact(escapeHTML)`，去掉字符串外的空白，转义 `<`、`>`、`&`、U+2028、U+2029，其余原样 |

不进 JSON、在 Rust 中没有对应字段的：`Job.Mu`（`sync.Mutex`）、`SubJob.JobArgs`（非空接口，`json:"-"`）、`Job.args` / `SubJob.args`（未导出，只由 X 的 `FillArgs` / `decodeArgs` 填写）。`Job.Clone`、`SubJob.Clone` 属于 X，两者派生 `Clone`。

### 3.6 `IndexInfo.ConditionExpr`

调用 parser 解析部分索引的条件。tidb-rs 的 parser 是 PoC 移植（D02.2），返回 PoC 的 AST；标为 L2。

## 4. 已知差异

1. `[]*T` 中的 `null` 元素：Go 得到 nil 指针，重新编码为 `null`；Rust 按值持有元素，得到零值。这个偏离成立的**前提**是 Go 写出的 meta 中不会有 `null` 元素：`TableInfo.Columns`、`Indices` 等由 DDL 代码填写，从不放 nil。如果真有，Go 的读路径在遍历时就会对 nil 解引用而 panic（例如 `TableInfo.Cols()`、`FindColumnByID`），而 Rust 得到一个零值的列继续运行，所以两边本来就不可能一致。govec 在这类记录的 `out` 上标记 `"known_deviation": "nil-ptr-elem"`；回放把它们单独计数（当前 5 条，都来自变异），不当作不一致，但如果某条标记的记录不再不同，测试就失败（偏离被修好，或者标记过期了）。
   - 这类记录同时带有 `"ported"`：把 nil 元素换成 `new(T)` 后 Go 的 `Marshal` 输出，即移植版应有的输出；回放要求 Rust 的输出与它严格相等（Part 1 评审建议），所以同一条记录中的其他差异不会被掩盖。
   - 同类的另一处（D02.3 §4 第 3 条）：同一个切片字段在一个对象中出现三次以上、中间一次更短时，Go 在底层数组的容量内"复活"旧元素，Rust 截断后重新填充。当前向量中没有命中。
2. `time.Time.UnmarshalJSON`：Go 解析引号之间的**原始字节**，不处理 JSON 转义（go.dev/issue/47353，Go 自己的 TODO）；Rust 用解码后的字符串。只有时间串里写了 JSON 转义（如 `\u0032024-…`）时结果不同，Go 写出的 JSON 不会这样。其余与 Go 相同：Go 1.25 的 `parseStrictRFC3339` 在快速路径失败时就是 `time.Parse(RFC3339)`（严格复查被禁用，所以一位数的小时、逗号小数点、`+24:00` 都被接受），这条回退连同 `ParseError` 的消息一起移植了（`tidb_base::gostd::time`）。数字时区用固定时区而不是 `Local`，`MarshalJSON` 的输出相同。
3. `Clone` / `Copy`：Go 浅拷贝共享指针，Rust 复制（3.5）。`DBInfo.Copy` 复制表信息而不是共享，标为 L2。
4. `TableInfo.GetNonTempColumns` 的结果顺序：Go 遍历 map，顺序随机；Rust 按列名排序（M-MAPORDER）。向量排序后比较。
5. JSON 语法错误的消息文本不移植（D02.3 3.1）。
6. `Job.Mu`：Go 用它保护 `RowCount` 和 reorg 警告，防止 DDL worker 并发修改；Rust 不持有这个锁，`GetRowCount`、`GetWarnings` 借用 `&self`（L2）。tidb-rs 只读取自己解码的 job。
7. `Job.String` 的 `ArgLen` 恒为 0：Rust 不持有 `Job.args`（L2）。读路径上 Go 解码出的 job 也不填 `args`，所以输出相同。
8. `atomic.Int64` 在 Rust 中是普通 `i64`（L2，见 3.7）：`ADMIN ALTER DDL JOBS` 的并发修改发生在 Go TiDB 中；JSON 形式与 Go 相同。
9. `json.RawMessage` 的字符串里有无效 UTF-8 时，Go 的 `Marshal` 原样写出这些字节，Rust 的输出是 `String`，写成 U+FFFD。Go 自己写出的 job 不会有无效 UTF-8（`Marshal` 已替换）。

## 5. 验收

- converge：`matched 153/153`（Part 1 为 139/153，Part 2 去掉了 14 个豁免）。
- govec `model` 领域：Part 1 合并时 63,366 条；Part 2 加入 job 相关的 8 个结构体后为 62,826 条（随机值按类型轮转，表的方法记录相应减少），arm64 和 amd64 都回放通过（9 条已知偏离，见第 4 节第 1 条）。Part 1 的组成：
  - roundtrip：从 v8.5.8 集群导出的 866 份 meta（857 张表、9 个库；DDL 见 tracker `govec/domains/testdata/model_cluster_ddl.sql`，覆盖全部列类型、分区、placement、TTL、视图、序列、外键、check 约束、资源组），每个结构体的随机值共 15,000 份，对象层的变异 15,000 份（删键、重复键、改大小写、未知键、错误类型、超范围数字、`null`），Lucky 的 `luckyMutate` 20,000 份（任意节点包括数组元素的边界字面量、interface{} 字段、折叠键、重复键）；
  - 时间 68 条：`Time.UnmarshalJSON` 的各种合法与非法串（含 Go 宽松接受的形式）、`Time.MarshalJSON` 的年份与时区范围错误；
  - into 3,000 条：解码到已有值；另有 61 条出错路径记录（§3.3 的表）；
  - 方法 3,800 余条：表、列、索引、分区、placement、资源组、外键和其他函数，以及 Go 测试 `TestDefaultValue`、`TestPlacementSettingsString` 的用例；
  - NaN / ±Inf 默认值的 `Marshal` 错误；
  - 其中 8,537 条 Go 返回错误。
- Part 2 新增：
  - 结构体 `Job`、`SubJob`、`MultiSchemaInfo`、`DDLReorgMeta`、`HistoryInfo`、`TimeZoneLocation`、`TraceInfo`、`InvolvingSchemaInfo` 参与 roundtrip、变异与 into（随机值中有 `errors.Normalize` 生成的 terror、各种 atomic 值与 RawMessage）；
  - `job` 400 条（`Job.Decode` 与各方法；`Job.String` 中本地时区的开始时间换成 `<T>`，另给 UTC 形式）、`job_strings` 136 条、`reorg` 80 条、`need_restored` 249 条（新 collation 关与开）；
  - 32 条出错路径记录：`map[ErrorID]*terror.Error` 的值 `UnmarshalJSON` 出错（之前的项保留、该项与之后的成员丢弃）、`null` 值、旧 class、解码进已有的值；`atomic.Int64` 的 `null`、类型错误、范围；`terror.Error` 的各种非对象值、折叠键、重复键；`RawMessage` 的空白与转义、`null`、标量、子 job、重复键；`HistoryInfo` 的无 tag 键；`[]*SubJob` 的 null 元素。
  - 反证：分别让 RawMessage 忽略原始字节、terror 的错误带上外层上下文、map 在值出错后仍存入、atomic 的 `null` 保持原值，回放分别报 3、149、65、1 条不一致。
- `tidb-base` 单元测试 `tests/json_map_unmarshaler.rs`：通用 map 解码器在值的 `UnmarshalJSON` 致命出错时的状态，期望值取自 Go。
- clippy `-D warnings`、fmt、`check_layers --blocks`、`cargo test --workspace`。
