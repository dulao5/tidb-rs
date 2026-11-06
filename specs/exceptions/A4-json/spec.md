# A4：JSON（二进制格式兼容，实现按 Rust 重新设计）（例外 spec，草案）

状态：草案（2026-11-04 按 PR #2 评审修订：待确认问题已有结论，`jsonvec` 并入通用的 `govec`，单行/向量化的写法以 N4 为准）。类型：实现例外。计划定稿：P1 开工前（ROADMAP §3 第 4 条、§5 第 1 条）。
依据：宪章第三条 1(b)（"JSON 性能"）；例外清单 A4；methodology Q3（编码格式复用 TiKV `tidb_query_datatype`，L3；值语义从 Go 移植；JSON 作为例外 A4）。

## 1. 替换的 Go 范围

### 1.1 本例外直接替换的代码（tracker 中标记为"例外"）

| 块 | Go 文件 | 文件行数 | 函数体行数（tracker） | 内容 |
|---|---|--:|--:|---|
| D01.5 | `pkg/types/json_binary.go` | 897 | 673 | `BinaryJSON{TypeCode, Value}` 定义与格式注释；访问器 `GetInt64/GetString/GetOpaque/GetTime/GetDuration/ArrayGetElem/objectGetKey/objectGetVal/valEntryGet`；文本输出 `MarshalJSON/marshalTo/marshalFloat64To/marshalArrayTo/marshalObjTo/jsonMarshalStringTo/jsonMarshalOpaqueTo/jsonMarshalTimeTo/jsonMarshalDurationTo`；文本解析 `ParseBinaryJSONFromString/UnmarshalJSON`；构造 `CreateBinaryJSON(WithCheck)/appendBinaryJSON/appendBinaryNumber/appendBinaryArray/appendBinaryObject/appendBinaryValElem`；`HashValue`、`IsZero`、`GetValue` |
| D01.5 | `pkg/types/json_binary_functions.go` | 1,387 | 1,231 | `Type`、`Unquote/UnquoteString/unquoteJSONString/quoteJSONString`；`Extract/extractTo/extractOne`；`buildBinaryJSONArray/Object`；`Modify/ArrayInsert/Remove` 与 `binaryModifier.{set,replace,insert,doInsert,remove,doRemove,rebuild,rebuildTo}`；`CompareBinaryJSON` 及 `compare*`；`MergeBinaryJSON/MergePatchBinaryJSON`；`ContainsBinaryJSON/OverlapsBinaryJSON`；`GetElemDepth`；`Search`；`Walk/extractToCallback`；`PeekBytesAsJSON` |
| D01.5 | `pkg/types/json_path_expr.go` | 620 | 445 | `JSONPathExpression{legs, flags}`、`parseJSONPathExpr` 及 `parseJSONPathMember/Array/Wildcard`、`tryParseArrayIndex`、`isEcmascriptIdentifier`、全局 LRU `peCache`（`ParseJSONPathExpr`，容量 1000，`init()`） |
| D01.5 | `pkg/types/json_constants.go` | 256 | —（只有常量） | 类型码 0x01…0x11、字面量 0x00/0x01/0x02、`jsonSafeSet`、布局常量（`headerSize=8, keyEntrySize=6, valEntrySize=5`）、`jsonTypePrecedences`、错误变量 |
| D08.10 | `pkg/expression/builtin_json.go` | 1,940 | 1,500 | 28 个 function class：TYPE、EXTRACT、UNQUOTE、SET/INSERT/REPLACE、REMOVE、MERGE、OBJECT、ARRAY、CONTAINS_PATH、MEMBER OF、CONTAINS、OVERLAPS、VALID、ARRAY_APPEND/INSERT、MERGE_PATCH/PRESERVE、PRETTY、QUOTE、SEARCH、STORAGE_FREE/SIZE、DEPTH、KEYS、LENGTH、SCHEMA_VALID（`builtin.go:931-958` 注册） |
| D08.10 | `pkg/expression/builtin_json_vec.go` | 1,392 | 1,312 | 上述函数的向量化版本 |
| 合计 | | 6,492 | 5,161 | 另有测试 2,605 行（types 1,014，expression 1,591） |

### 1.2 调用 JSON 的周边代码（不在本例外内，按所属块 L1 移植，只调用 A4 提供的 API）

| 块 | 位置 | 用到的 JSON API |
|---|---|---|
| D01.1 | `types/datum.go`：`Datum.ToMysqlJSON`、`convertToMysqlJSON`、`compareMysqlJSON` | `CreateBinaryJSON`（int/uint/float/string/Time/Duration/nil）、`CompareBinaryJSON` |
| D01.4 | `types/convert.go:577-700`：`ConvertJSONToInt/Float/Decimal` | 访问器、`String()`（用于 truncate 报错文案） |
| D01.7 | `util/chunk/column.go`：`AppendJSON`（:54）、`GetJSON`（:671） | 列内存放 `TypeCode` + `Value`，读取为切片视图 |
| D01.8 | `util/codec/codec.go`：`jsonFlag=10`，`encode`（:129）、`HashChunkRow`（:598、:1554 调 `HashValue`）、解码（:1095、:1406 调 `PeekBytesAsJSON`）；`util/rowcodec/common.go:352` 行校验和用 `String()` | 原始字节、`HashValue`、`PeekBytesAsJSON`、`String()` |
| D08.4 | `builtin_compare.go`：`builtin{LT,LE,GT,GE,EQ,NE,NullEQ}JSONSig`、`CompareJSON`（:3278） | `CompareBinaryJSON` |
| D08.8 | `builtin_cast.go`：`castXAsJSON`（:1147-1290）、`castJSONAsX`（:2160-2360）、`castJSONAsArray`（:666） | `ParseBinaryJSONFromString`、`CreateBinaryJSON(Opaque)`、`String()`、`Unquote()` |
| D12.4 | `executor/aggfuncs/func_json_arrayagg.go`（112）、`func_json_objectagg.go`（212） | `CreateBinaryJSONWithCheck([]any / map[string]any)` |

## 2. Go 实现慢在哪里

以下"事实"均可在代码中直接看到；"影响"一栏未经实测的标为**假设**，在 §6.4 的基准中验证。

| # | 事实（file:function） | 影响 |
|---|---|---|
| 1 | `ParseBinaryJSONFromString` 先 `json.Valid` 扫一遍，再 `UnmarshalJSON` 用 `json.NewDecoder(...).UseNumber()` 解码到 `any`（每个对象一个 `map[string]any`、每个数组一个 `[]any`、每个数字一个 `json.Number` 字符串、每个标量装箱成 interface），再 `appendBinaryJSON` 生成二进制，`appendBinaryObject` 另建 `fields` 切片并排序，最后 `CreateBinaryJSONWithCheck` 调 `GetElemDepth` 递归走整棵树 | 至少 3 次全文遍历 + 1 次树遍历；堆分配数与节点数成正比（**假设**：`-benchmem` 可证实） |
| 2 | `appendBinaryNumber` 对每个数字依次尝试 `x.Int64()`、`strconv.ParseUint`、`x.Float64()`，失败路径构造 error 对象 | 浮点数每个都要走两次失败的整数解析 |
| 3 | `builtinJSONExtractSig.vecEvalJSON`（builtin_json_vec.go:954-963）每行 `make([]JSONPathExpression)`，每行每个路径参数调 `ParseJSONPathExpr`：全局 `peCache.mu` 加锁查 LRU，命中后 `clone()` 再分配 legs 切片 | 常量路径也按行重复"加锁 + 分配"；并发查询在同一把全局锁上竞争（**假设**：高并发下可见） |
| 4 | `Extract` 每个路径 `make(map[*byte]struct{})` 去重（json_binary_functions.go:236），结果多值时 `buildBinaryJSONArray` 再复制 | 每次调用至少 2 次分配 |
| 5 | `Modify` 对每个路径各做一次 `binaryModifier.rebuild`，整篇重建；最后再 `GetElemDepth` 全树遍历 | O(路径数 × 文档大小) |
| 6 | `CompareBinaryJSON` 每次比较（含递归到每个元素）调两次 `Type()` 得到字符串，再查 `map[string]int`（`jsonTypePrecedences`） | ORDER BY / 比较 JSON 列时每次比较都有 2 次字符串哈希 |
| 7 | `HashValue` 对象分支对每个 key 调 `CreateBinaryJSON(string(key))`：字符串复制 + `any` 装箱 + 新 buffer | GROUP BY / hash join JSON 列时每行每 key 多次分配 |
| 8 | 文本输出：`String()` 再把 `[]byte` 复制成 `string`；opaque 用 `fmt.Sprintf` + `base64.EncodeToString`；时间用 `quoteJSONString(time.String())` | 每个值多 1–3 次分配 |
| 9 | `JSON_OBJECT`（builtin_json.go:586-609）、`JSON_ARRAYAGG/OBJECTAGG` 先攒 `map[string]any` / `[]any`，再经 `appendBinaryJSON` 复制每个子文档 | 中间结构 + 子文档复制 |
| 10 | `JSON_PRETTY` 先 `MarshalJSON` 再 `encoding/json.Indent`；`JSON_QUOTE` 用 `json.NewEncoder`；`JSON_SCHEMA_VALID` 先 `MarshalJSON` 再 `encoding/json.Unmarshal`（反射）进 qri-io 的 schema | 两遍处理 + 反射 |

不慢的部分：Go 读取已经是零拷贝的——`Column.GetJSON` 返回指向列数据的切片，访问器按偏移随机访问，不反序列化。Rust 版在这一点上只需持平，收益集中在解析、构造、路径、比较、哈希。

## 3. 行为契约（必须与 Go 完全一致）

### 3.1 二进制格式（逐字节）

- 布局见 `json_binary.go` 文件头注释：`type(1) + value`。TiDB **只用**大对象/大数组（0x01/0x03，uint32 偏移），不用 MySQL 的小对象（0x00/0x02）和 int16/int32 类型码，因此 TiDB 的 JSON 二进制与 MySQL 的并不相同。
- 只有字面量内联在 value-entry 中，其余 3 字节为 0（`appendBinaryValElem`、`buildBinaryJSONElements`、`rebuildTo` 都如此）；int64/uint64/float64 不内联。
- 对象 key 按字节序排序（`appendBinaryObject` 用 `cmp.Compare`，`mergeBinaryObject` 用 `bytes.Compare`），重复 key 后者覆盖（`map` 赋值语义）；key 长度 > 65535 报 `ErrJSONObjectKeyTooLong`（8129）。
- 字符串长度用 uvarint；时间存 `CoreTime` 的 uint64（TiDB 自有格式），Duration 存 `int64 纳秒 + uint32 fsp`；opaque 为 `类型字节 + uvarint 长度 + 数据`。
- 子文档按 Go 的做法**原样复制**（`appendBinaryJSON` 的 `BinaryJSON` 分支、`rebuildTo` 的 key 区段整体复制），不做规范化。
- **为什么要求逐字节**：`JSON_STORAGE_SIZE` 返回 `len(Value)+1`（builtin_json.go:1605），字节长度对用户可见；`codec` 把原始字节写进 key（codec.go:129）；也便于用向量直接比对。存储侧与 TiKV coprocessor 只要求"合法布局"即可读。

### 3.2 文本输出

| 项 | Go 行为（`marshalTo` 一族） |
|---|---|
| 分隔 | 数组 `", "`，对象 `", "` 和 `": "`；无换行 |
| 对象 key 顺序 | 二进制中的顺序（即字节序） |
| 浮点 | 最短往返位数；`1e-6 ≤ |f| < 1e21` 用 `'f'` 格式，否则 `'e'`，并把 `e-09` 改成 `e-9`。**`3.0` 输出 `3`**，`1e20` 输出 `100000000000000000000`，`1e21` 输出 `1e+21`；Inf/NaN 报 `json.UnsupportedValueError` |
| 字符串转义 | `jsonMarshalStringTo`：`\"`、`\\`、`\n`、`\r`、`\t`；其余 < 0x20 输出 `\u00xx`（小写十六进制，故 `\b` 输出为 `\u0008`）；非法 UTF-8 每字节输出 `�`；U+2028/2029 输出 ` `/` `；不转义 HTML 字符 |
| opaque | `"base64:type<类型码十进制>:<标准 base64>"` |
| 时间 | 强制 fsp=6 后 `Time.String()` 加引号（DATE 不带时间部分）；TIMESTAMP 按 DATETIME 输出；Duration 同样 fsp=6 |
| 用途 | 结果集、`CAST(json AS CHAR)`、truncate 报错文案、行校验和（rowcodec/common.go:352） |

### 3.3 文本解析（`ParseBinaryJSONFromString`）

- 空串 → 3140 `Invalid JSON text: The document is empty`。
- 任何语法错误（`json.Valid` 为 false，包括嵌套超过 encoding/json 自身 10000 层上限）→ 统一 3140 `Invalid JSON text: The document root must not be followed by other values.`，**不区分具体错误**。
- 数字：依次 int64 → uint64 → float64（`appendBinaryNumber`；其中 `strings.Contains(x, "Ee.")` 永远为假，属 Go 的笔误，但不改变结果）。`-0` 得 int 0；`9223372036854775807` 得 INTEGER，`9223372036854775808` 得 UNSIGNED INTEGER；超出 float64 范围时把 strconv 的错误文本包进 3140（具体文案以向量为准）。
- 字符串：非法 UTF-8、孤立代理项 `\ud800` 按 encoding/json 的规则替换为 U+FFFD，不报错。
- 深度：构造完成后 `GetElemDepth()-1 > 100` → 3157 `The JSON document exceeds the maximum depth.`（`maxJSONDepth=100`）。
- 错误先后顺序：语法错误 > 构造期错误（按排序后的 key 顺序，先检查本层所有 key 长度，再递归各 value）> 深度。
- `JSON_VALID` 只调 `json.Valid`（builtin_json.go:1039），所以 `JSON_VALID('1e400')=1`，而 `CAST('1e400' AS JSON)` 报错。两者必须分开实现。
- `CAST(str AS JSON)`：二进制字符串变 opaque（`TypeString` 且 flen>0 时补零到 flen）；带 `ParseToJSONFlag` 时解析，否则作为 JSON 字符串（builtin_cast.go:1217-1243）。

### 3.4 比较、排序与哈希

- `CompareBinaryJSON`：先按 `jsonTypePrecedences`（MySQL 5.7 顺序：BLOB > BIT > OPAQUE > DATETIME > TIME > DATE > BOOLEAN > ARRAY > OBJECT > STRING > 数字 > NULL）；opaque 按字段类型分成 BLOB/BIT/OPAQUE，TIMESTAMP 归入 DATETIME。
- 同类内：int/uint 精确比较；float 与 int/uint 用 `floatEpsilon = 1e-8`（不满足传递性，照搬）；float 与 float 精确比较；字符串和 opaque 按字节（opaque 不比较类型码）；数组逐元素后返回**未截断的**长度差；对象先比元素个数，再逐对比 key 和 value；true > false。调用方只看符号。
- `HashValue`：int/uint 在不丢精度时（≤52 位）按 float64 编码，使 `3` 与 `3.0` 哈希相同；对象逐 key 写入。输出字节与 Go 一致（落盘与跨算子一致性所需，见 A1/N6）。
- `IsZero`：等价于 `CompareBinaryJSON(bj, 0) == 0`，不出 warning。

### 3.5 路径表达式

- 语法：`$`、`.key`、`."quoted key"`、`.*`、`[n]`、`[last]`、`[last-n]`、`[m to n]`、`[*]`、`**`；空白可出现在 leg 之间；未加引号的 key 必须满足 `isEcmascriptIdentifier`（允许 ≥0x80 的字节）；加引号的 key 先 `json.Valid` 再 `unquoteJSONString`；索引 > `math.MaxUint32` 视为非法；以 `**` 结尾非法。
- 错误：3143 `Invalid JSON path expression. The error is around character position %d.`，位置是出错时的**字节偏移** `s.pos`。
- 求值语义：在非数组上 `[0]`、`[0 to n]`、`[0 to last]` 选中自身，`[*]` 不选中（`extractTo`）；多路径结果按出现顺序、按文档内位置去重（Go 用 `&Value[0]` 指针）；单路径但含通配或范围时也包成数组（issue 30352 的修复）；修改类函数遇到通配或范围报 3149。
- 报错时机：Go 在每行遇到非 NULL 参数时才解析路径，所以 `JSON_EXTRACT(NULL, '$a')` 返回 NULL 而不报错。缓存路径时必须保持这一时机。

### 3.6 错误码与 warning

使用 `json_constants.go` 中的全部错误变量（3069、3140、3141、3143、3144、3149、3150、3157、3158、3165、3853、8129 等，文案见 `errno/errname.go:852-866, 930, 1131`），以及 Go 库错误透传的文本（如 `Modify` 参数个数不符时的 `"Incorrect parameter count"`、`unquoteJSONString` 的 `"Missing a closing quotation mark in string"` / `"Invalid unicode: %s"`）。周边块产生的 warning（如 `ConvertJSONToInt` 的 `ErrTruncatedWrongVal` 经 `HandleTruncate`）由周边块按 L1 实现，A4 只保证 `String()` 文案一致。

## 4. 方案比较

PoC 已经把 TiKV 的 `tidb_query_datatype` 原样放在 `projects/tidb-rs/third_party/tikv/`（`json/` 目录与 TiKV 源码 `diff -r` 一致，共 5,132 行），因此方案 a 的接入成本几乎为零。但对照代码，发现它与 Go 有以下差异：

| # | TiKV 实现（`codec/mysql/json/`） | Go v8.5.8 | 影响 |
|---|---|---|---|
| 1 | `serde.rs`：浮点输出用 serde_json `CompactFormatter`（ryu），自带测试断言 `3.0 → "3.0"` | `3` | 文本输出、行校验和不同 |
| 2 | 字符串用 serde_json 转义：`\b`/`\f` 输出短格式，不转义 U+2028/2029；非 UTF-8 时报 `json contains invalid UTF-8 characters` | `\u0008`、转义 2028/2029、非法字节输出 `�` | 同上 |
| 3 | `JsonVisitor::visit_u64` 中 `v < i64::MAX` 才转 i64，所以 `9223372036854775807` 存成 U64（测试用语义相等比较，没发现）（按代码推断，待向量确认） | INTEGER | `JSON_TYPE` 结果不同 |
| 4 | 解析错误文案为 `Illegal Json text: ...`；没有 100 层深度检查（只有 serde_json 的 128 层递归上限）；没有 key 长度检查；孤立代理项报错 | 见 §3.3 | 错误码、错误文案不同 |
| 5 | `comparison.rs`：浮点容差用 `f64::EPSILON`，并且也用于 double 与 double 之间；对象按原始字节比较 | 1e-8，只用于 float 与整数之间；对象按个数、key、value 比较 | 比较、排序结果不同 |
| 6 | 缺少 `Search`、`OverlapsBinaryJSON`、`ArrayInsert`、`Walk`、`HashValue`；`tidb_query_expr` 中没有 SEARCH/OVERLAPS/ARRAY_INSERT/PRETTY/STORAGE_*/CONTAINS_PATH/SCHEMA_VALID | 有 | 覆盖缺口约占 Go 函数类的 1/4 |
| 7 | `impl Ord for JsonRef` 直接 `partial_cmp().unwrap()`（字符串非 UTF-8 时为 None，会 panic） | 不会 panic | 健壮性 |

（这些差异同时意味着：下推到 TiKV 的 JSON 表达式本来就可能与 TiDB 根节点上的求值结果不一致。tidb-rs 使用真实 TiKV，下推结果与 Go TiDB + TiKV 相同，不在本例外范围内；是否向上游报告见 §8。）

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| a. 复用并扩展 TiKV 实现（L3） | 直接用 `Json/JsonRef`，补缺失函数，修正差异 | 已接入；二进制读写与 `JsonRef` 借用视图现成 | 上表 1–5 都要改，等于 fork 一个**会被 TiKV 上游继续修改**的模块，每次同步都要重新对照；`Ord`/`PartialEq` 的语义与 Go 不同，留在类型上容易被误用；文本层建在 serde 之上，无法精确控制错误文案和先后顺序 |
| b. 全新 Rust 实现 | 按 Go 的文件和函数结构写新模块，数据结构和算法按 Rust 设计 | 完全可控；可以直接写成零分配；与 Go 的对应关系清楚 | 二进制布局读取这部分（约 400 行）与 TiKV 重复 |
| c. 混合 | 二进制布局的读取、偏移计算、类型码这一层（TiKV `binary.rs`、`constants.rs`、`jcodec.rs` 中解码部分）按 L3 引入（复制进本仓库并标注来源，**不依赖** TiKV 的 `Json` 类型和 trait 实现）；解析、输出、比较、哈希、路径、修改、函数全部按 Go 语义新写 | 与 Q3（"编码复用 TiKV，语义从 Go"）一致；格式层有 TiKV 生产验证；语义层没有 serde 的约束 | 需要维护一份复制来的格式代码（很小，且 MySQL/TiDB 格式多年不变） |

**建议：方案 c。** 实际上就是 b，只是把格式读取层作为 L3 复用，且只复制代码、不引用类型。PoC 中 coprocessor 侧对 vendored crate 的使用可以保留，两边在边界上都是 `(u8, &[u8])`，转换没有开销。

## 5. 设计

### 5.1 模块与对应关系

`crates/types/src/json/` 下文件与 Go 一一对应：`json_binary.rs`、`json_binary_functions.rs`、`json_path_expr.rs`、`json_constants.rs`，外加 `format.rs`（L3，来自 TiKV）。函数级来源标注沿用宪章第六条，等级写 `A4`（如 `// go: pkg/types/json_binary_functions.go CompareBinaryJSON A4`）。产生二进制字节的算法（`appendBinary*`、`buildBinaryJSON*`、`binaryModifier.rebuildTo`、`MergeBinaryJSON`、`MergePatchBinaryJSON`、`ArrayInsert`、`Remove`）在**布局决策**上逐行对应 Go，只改分配方式，以保证 §3.1 逐字节一致。

### 5.2 值表示（与 N5、A1 的关系）

- `JsonRef<'a> { tc: JsonTypeCode, val: &'a [u8] }`：`Copy`，对应 Go 的 `BinaryJSON` 值语义（Go 的 `BinaryJSON` 本身就是借用切片）。chunk 列、coprocessor 响应缓冲区（A6）、N5 的紧凑值都直接借出 `JsonRef`，不复制。
- 拥有型 `Json { tc, buf }`：只在构造新文档时出现；`buf` 的分配走 A1 的语句级内存上下文并计账。N5 的 `Value` 枚举中 JSON 变体就是 `JsonRef<'a>` 或拥有型。
- 不为 `JsonRef` 实现 `Ord/PartialEq/Hash`，比较和哈希只能显式调用 `compare_binary_json`、`hash_value`，避免误用。
- 访问器面对可能损坏的字节：Go 中越界会 panic（在执行层被 recover 成错误）。Rust 访问器全部做边界检查并返回 `Result`，热路径在顶层校验一次后用不检查边界的内部版本。

### 5.3 构造器

`JsonWriter<'b>` 直接写入调用方给出的 `Vec<u8>`（可以是 chunk 列的数据缓冲区）：`array(n)` / `object(n)` 先预留头部和 entry 区，写完子值后回填偏移和大小，与 `appendBinaryArray/Object` 的写法相同。对象 key 由调用方给出已排序、已去重的序列。`JSON_OBJECT`、`JSON_ARRAYAGG/OBJECTAGG`、`buildBinaryJSONArray/Object` 都基于它，不再经过 `map[string]any`。深度在构造过程中累计，不再在构造完成后整树遍历；但报错的**先后顺序**仍按 §3.3。

### 5.4 文本解析器

手写单遍解析器，文本直接生成二进制，不建 DOM：
- 语法规则与 Go `encoding/json` 的 scanner 一致（RFC 8259 严格模式，空白只有 4 种，嵌套上限 10000）。
- 对象：成员先写入一个可复用的 scratch 区（key 区间 + 已编码的 value），对象结束时按 key 字节序排序、相同 key 保留最后一个，再整体输出。scratch 在解析器内复用，单次解析的分配次数与文档大小无关（输出 buffer 除外）。
- 数字：按 int64 → uint64 → f64 的顺序；f64 用标准库 `str::parse::<f64>`（正确舍入，与 Go `strconv.ParseFloat` 相同），结果为 ±Inf 时按 Go 的 range error 报错。
- 字符串：按 encoding/json 的规则把非法 UTF-8 和孤立代理项替换为 U+FFFD。
- 错误先后顺序：快路径遇到第一个非语法错误时，改走一个"慢路径"（解析成临时树，再按 Go 的构造顺序检查），保证报出的是 Go 会报的那一个错误。错误很少出现，慢路径的成本可以接受。
- 不用 serde_json：它的整数分类（问题 3）、错误文案、递归上限、代理项处理都与 Go 不同，绕开它们的代码量不比手写少。不用 simd-json：需要可变输入缓冲区，语义差异同上。
- `json_valid(s)` 单独实现为只做语法检查的 scanner，与 `json.Valid` 等价。

### 5.5 输出

逐项对应 `marshalTo` 一族，直接写入调用方的 buffer（结果集编码 N7 可以直接写进网络缓冲区）。浮点：用 `ryu` 取最短位数和指数，再按 Go `strconv.AppendFloat(f, 'f'|'e', -1, 64)` 的排版输出（'e' 格式下指数至少两位，然后按 Go 的规则去掉 `e-0x` 中的 0）。两边"最短往返位数"的结果应相同，由 §6.3 的差分模糊测试验证。`JSON_PRETTY` 直接输出 `encoding/json.Indent` 的格式，不走两遍。

### 5.6 路径与求值

- `JsonPath { legs: SmallVec<[Leg; 4]>, flags }`，解析器按 L1 移植（错误位置必须一致）。
- 去掉全局 `peCache`：常量路径参数在函数签名内**懒缓存**——第一次遇到非 NULL 行时解析，把 `Ok` 或 `Err` 一起缓存，保证 §3.5 的报错时机。非常量路径每行解析，无锁、无克隆。（全局缓存对用户不可见，去掉它不改变行为。）
- `extract` 输出到 `SmallVec<[JsonRef; 1]>`；去重用"值在文档内的字节偏移"代替 Go 的 `&Value[0]`，二者等价。
- `binaryModifier` 的修改目标同样用偏移标识；多个路径时仍按 Go 逐个重建（字节结果要一致），但重建写入复用的 buffer。
- `CompareBinaryJSON`：类型优先级用 `match` 返回常量，不查 map。

### 5.7 JSON 函数（D08.10）

函数类、签名、参数检查、NULL 处理、报错顺序按 L1 移植，只把内部调用换成 §5.3–5.6 的接口。Go 中单行（`evalXxx`）和向量化（`vecEvalXxx`）两个版本如何对应到 Rust，按 N4（表达式求值框架）的统一规则处理，本 spec 不单独规定。`JSON_SCHEMA_VALID` 依赖 Go 第三方库 `qri-io/jsonschema v0.2.1`，处理方式见 §8。

## 6. 验证

### 6.1 Go 差分向量

JSON 是通用向量工具 **govec** 的第一个用户（方法论 §5；Decimal、Time、convert、collation、ranger 也用它）。在 tracker 仓库的 govec 下实现 JSON 部分（下文仍称 `jsonvec`），`go.mod` 用 `replace` 指向本地 v8.5.8 源码，Go 工具链固定为 v8.5.8 `go.mod` 声明的 **go 1.25.12**（`strconv`、`encoding/json` 的行为随 Go 版本可能变化）。输出 JSONL，每行 `{op, args, out_bin_hex, out_text, err_code, err_msg, warnings}`：

| op | 调用的 Go 入口 |
|---|---|
| parse / valid | `types.ParseBinaryJSONFromString`、`json.Valid` |
| marshal / type / depth / unquote / quote | `BinaryJSON.String`、`Type`、`GetElemDepth`、`UnquoteString`、JSON_QUOTE 的签名 |
| compare / hash | `CompareBinaryJSON`（取符号）、`HashValue` |
| path_parse / extract / modify / array_insert / remove / merge / merge_patch / contains / overlaps / search / keys / length | `types` 包对应函数 |
| from_datum / to_int / to_float / to_decimal | `Datum.ToMysqlJSON`、`ConvertJSONTo*`（含 `HandleTruncate` 产生的 warning） |
| func | 用 `mock.NewContext()` 和常量参数构造 `expression.NewFunction`，同时跑 `evalXxx` 与 `vecEvalXxx`，覆盖全部 28 个函数类 |

输入来源：(1) 抽取 Go 单元测试中的用例表；(2) `tests/integrationtest/t/expression/json.test`；(3) 带种子的随机生成器，重点覆盖：深度 99/100/101/10000/10001、key 长度 65535/65536、重复 key、`-0`、`±2^63`、`2^64`、`1e-7`、`1e21`、`1e400`、`1e-400`、控制字符、非法 UTF-8、`\ud800`、U+2028、opaque 和时间类型、带空白的路径、`last-n`、范围越界、NULL 与非法路径的组合。向量文件入库，Rust 侧用 `#[test]` 逐条比对。

### 6.2 tracker 中要通过的测试

- 单元测试：`pkg/types/json_binary_test.go`（20）、`json_path_expr_test.go`（5）、`json_binary_functions_test.go`（2）、`pkg/expression/builtin_json_test.go`（27）、`builtin_json_vec_test.go`（1），以及与 JSON 有关的 `convert_test`（3）、`datum_test`（1）、`codec_test`（1）、`builtin_cast_test`（3）、`util_test`（`TestDisableParseJSONFlag4Expr`）。按宪章第七条随代码移植。
- 依赖存储的：`tests/integrationtest/t/expression/json.test`（300 条语句）、`TestBuiltinFuncJSONMergePatch_InColumn/_InExpression`、`TestCastJSONTimeDuration`，以 SQL 级对照测试覆盖；D12.4 中的 `TestJsonArrayagg/Objectagg` 等随 D12.4 移植。

### 6.3 模糊测试

- `cargo fuzz`：解析器（任意字节输入：不 panic；若 Go 判为合法则字节一致）、二进制访问器（任意字节：不 panic、无 UB）、往返不变量 `parse(marshal(x)) == x`。
- 差分模糊：`jsonvec` 提供 stdin/stdout 的 oracle 模式，nightly 用生成器和 fuzz 语料对 parse/marshal/compare/extract 跑差分。

### 6.4 性能目标

基线基准：Go 已有的 `BenchmarkBinaryMarshal`（types/json_binary_test.go:390）、`BenchmarkMergeBinary`、`BenchmarkMergePatchBinary`（json_binary_functions_test.go:37/45）、`BenchmarkDecodeEscapedUnicode`、`BenchmarkVectorizedBuiltinJSONFunc`（builtin_json_vec_test.go:147）；在 `jsonvec` 中补充：解析（100B / 4KB / 256KB 三档文档）、常量路径 `JSON_EXTRACT` 处理 1024 行 chunk、`CompareBinaryJSON` 排序、`HashValue`。Rust 用 criterion 跑同样的输入。

初步目标（基线实测后定稿）：

| 项 | 目标 |
|---|---|
| 解析 | 吞吐 ≥ Go 3 倍；除输出 buffer 外，每篇文档的分配次数为常数 |
| 常量路径的向量化 EXTRACT | 每行 0 次路径解析、0 次加锁；吞吐 ≥ Go 3 倍 |
| 文本输出 | 吞吐 ≥ Go 1.5 倍；直接写入目标 buffer |
| 比较、哈希 | ≥ Go 2 倍；`HashValue` 每行 0 次分配 |
| 任一基准 | 不低于 Go |
| 内存 | 拥有型 JSON 全部计入 A1 的语句内存 |

## 7. 阶段与块

| 阶段 | 需要的 JSON 能力 | 使用方 |
|---|---|---|
| **P1（最小子集）** | 常量、错误变量；`JsonRef/Json`；访问器；`CreateBinaryJSON` 的标量/字符串/nil/bool/Time/Duration/Opaque 分支；`ParseBinaryJSONFromString`；`json_valid` scanner；文本输出（`String/MarshalJSON`）；`Type`、`Unquote/UnquoteString`；`CompareBinaryJSON`、`IsZero`；`HashValue`、`PeekBytesAsJSON`；`GetElemDepth`；`JsonWriter`（数组/对象） | D01.1 Datum、D01.4 convert、D01.7 chunk、D01.8 codec、D08.4 比较、D08.8 CAST（含 `castJSONAsArray`） |
| P1 规模 | `json_binary.go` 全部 + `json_binary_functions.go` 中约 1/4 + `json_constants.go`，约 1,000 行 Go 函数体 | |
| P4 | `JsonWriter` 的"从多个子文档构造"接口 | D12.4 `JSON_ARRAYAGG/OBJECTAGG` |
| 函数阶段（建议与 D08.6/D08.7/D08.9 一起放在 P5） | 路径表达式；Extract/Modify/ArrayInsert/Remove/Merge/MergePatch/Contains/Overlaps/Search/Walk；D08.10 全部函数 | 用户 SQL |

在函数阶段之前，所有 JSON 函数按宪章第五条返回统一的"不支持"错误，并在 tracker 中登记边界桩。下推判定（D08.3，P2）只需要函数签名枚举，不依赖函数实现。

## 8. 决定（2026-11-04，PR #2 评审）

1. **逐字节一致是硬性契约**：所有构造路径输出的二进制与 Go 逐字节一致（`JSON_STORAGE_SIZE`、codec key、行校验和都依赖它）。
2. **JSON_SCHEMA_VALID**：先作为边界桩报错，函数阶段再评估移植 `qri-io/jsonschema` 用到的部分还是用 Rust crate 加差分。
3. **D08.10 的排期**：放在 P5，与其他函数族一起；之前全部报"不支持"。
4. **TiKV 差异**：§4 的表整理后作为 issue 报给 TiKV 上游（它们也是 TiDB 与 TiKV 下推之间的不一致）。
5. **损坏的二进制**：统一返回内部错误码，差分向量不覆盖损坏输入。
6. **向量工具**：不做 A4 专属的工具，作为 govec 的一部分放在 tracker 仓库；向量文件随 tidb-rs 入库，上游同步时重新生成并 diff。

## 9. 不确定之处（写给评审）

- §4 问题 3（i64::MAX 的类型）和问题 1 中 `1e20` 的 TiKV 输出是读代码推断的，没有运行验证；`3.0 → "3.0"` 有 TiKV 自带测试为证。
- §3.3 中"超出范围时的错误文案"、"10000 层嵌套落到哪条错误"、"`1e-400` 是否得到 0"都依赖 Go 标准库细节，以向量为准。
- §2 的分配次数和锁竞争影响未实测，标为假设；§6.4 的倍数目标是初值。
- TiKV `jcodec.rs` 写出的字节是否与 Go 完全相同未逐字节验证（布局代码看起来一致）；方案 c 只复用读取层，不受影响。
