# D06.1 元数据模型（meta/model）：plan

## 1. Rust 模块布局

新 crate `crates/model`（`tidb-model`，N0 rank 3），依赖 `tidb-base`（rank 0）、`tidb-parser`（rank 1）和 `tidb-types`（rank 2，Part 2 起：`NeedRestoredData` 与 collate）。

| 模块 | 内容 |
|---|---|
| `types_gen.rs`、`consts_gen.rs` | 生成（tracker `govec/cmd/rsgen -model`，之后 `cargo fmt`）：36 个结构体、`encode_json`/`decode_json`、`GoCopy`、常量与命名类型 |
| `gocopy.rs` | `GoCopy` trait：Go 的结构体赋值（spec 3.5） |
| `any.rs` | `AnyValue`（Go `interface{}`） |
| `gotime.rs` | `time.Time` 的 `MarshalJSON`/`UnmarshalJSON` |
| `table.rs`、`column.rs`、`index.rs`、`placement.rs`、`resource_group.rs`、`db.rs`、`bdr.rs` | 手写的方法，按 Go 文件分 |
| `job.rs`、`reorg.rs` | Part 2：`job.go`、`reorg.go` 中读路径保留的函数（`ActionMap` 是 Go 的 map 字面量，移植为按键查找的表） |
| `types_etc.rs` | Part 2：`pkg/types/etc.go` 的 `ColumnNeedRestoredData` |

`tidb-base` 的改动：`gostd::json`（`DecodeError`、`Decoder::object_promoted`、`bytes` 带 Go 类型名、`any`/`map`/`int_key`/`float64`、base64、`encode_value`/`encode_float`），`gostd::time`（`Time` 的 `Default`/`PartialEq`、`append_strict_rfc3339`、`parse_strict_rfc3339` 及其 `time.Parse(RFC3339)` 回退）。`tidb-parser`：`FieldType`/`CIStr` 的 `decode_json`。

Part 2 的 `tidb-base` 改动：`gostd::json` 的 `Raw` 字符串、数组、对象带输入范围（`Span`），`Decoder::with_source`、`raw_message`、`append_raw_message`（`json.RawMessage`）；`errors` 加 `Default`（Go 零值）和 `marshal_json` / `unmarshal_json_raw`（`compatible_shim.go`）。`tidb-types`：`etc.rs`（`NeedRestoredData` 等 5 个 D01.1 函数）。

## 2. 写法映射

| Go | Rust | 规则 |
|---|---|---|
| `func (t T) String() string` | `to_string`（`#[allow(clippy::inherent_to_string)]`） | M-NAME |
| `range` 遍历 map 得到的结果（`GetNonTempColumns`） | 按列名排序 | M-MAPORDER |
| 返回 `error` | `Result<_, Error>` | M-ERR |
| `PolicyInfo.Clone` 对 nil 设置解引用 | panic | M-PANIC |
| `[]*T`、`*T`、nil 切片 | `Option<Vec<T>>`、`Option<Box<T>>` | spec 3.4 |
| `nt := *t` | `struct_copy()` / `GoCopy` | spec 3.5 |

L2：`DBInfo.Copy`（复制而不是共享表）、`IndexInfo.ConditionExpr`（PoC parser）、`writeSetting*ToBuilder`（`separatorFns` 改为可选分隔串）。

## 3. 测试

- `crates/model/tests/govec_model.rs` 回放 govec `model`（op 说明见 tracker `govec/domains/model.go`）。比对：`out` 逐字段相等（JSON 文本逐字节）；错误消息逐字相等，JSON 语法错误只要求报错；Go panic 的记录 Rust 也必须 panic。`GOVEC_ALL=1` 打印全部不一致的记录（调试用）。
- 向量放在 `crates/model/tests/vectors/v8.5.8/<目标>/model.jsonl.gz`，amd64 取 tracker PR 的 CI artifact。

## 4. 交接（Part 1：2026-11-07；Part 2：2026-11-08）

### 已完成（PR (a)）

1. tidb-model crate：生成层 + 139 个函数，converge `matched 139/153`（缺的 14 个见 spec §2，属 Part 2）。clippy `-D warnings` 干净，生成文件没有模块级 `allow`。
2. tracker PR #13：`rsgen -model` 生成器；govec `model` 领域 63,366 条记录（集群语料、随机、两种变异、into、出错路径、时间、方法），`go test ./...` 通过。集群语料由自建 v8.5.8 playground 导出，playground 已销毁。
3. `time.Parse(RFC3339)` 回退与 `ParseError` 消息（Go 1.25 的 `parseStrictRFC3339`）。
4. arm64 与 amd64（tracker CI artifact）的向量回放全部通过，两个目标的记录相同。
5. mappings.md M-BYTES；ROADMAP 的 D06.1 行和 D04.1 的 scramble 注记；CI arm64 job 加 `tidb-model`。

### Part 2（PR (b)，tidb-rs `block/D06.1-model-b`，tracker PR #16 `govec/model-D06.1-b`）

已完成：上面"剩余"的 1～4 全部，converge `matched 153/153`；Part 1 评审的三条不阻塞建议（map 解码器单元测试、已知偏离带 `ported` 严格比较、向量体积写进 README）。设计见 spec 3.7，偏离见 spec §4 第 6～9 条。

### 剩余（写于 Part 1 交接时，已在 Part 2 完成）

1. `job.go`：`ActionType.String`、`JobState.String`、`SchemaState.String`、`JobVersion.String`、`Job.Decode`（`json.RawMessage` 保留原始字面量、`errors.Error` 的 JSON、`atomic.Int64` 等并发字段）、`Job.String`、`Job.GetRowCount`、`Job.GetWarnings`、`Job.MayNeedReorg`。生成器要扩展到 `Job`、`SubJob`、`DDLReorgMeta` 等类型。
2. `reorg.go`：`DDLReorgMeta.Get*OrDefault`（读 `vardef` 的全局变量）、`ReorgType.String`。
3. `ColumnNeedRestoredData`：等 D01.9（collate）合并后移植 `types.NeedRestoredData`。
4. govec `model` 领域加 job 的 op，去掉对应豁免；converge 到 `153/153`。

### 决定

- 拆分：PR (a) = 生成的 Go-JSON 层 + schema 结构体 + 它们的方法（139 个函数）；PR (b) = Part 2。
- `[]*T` 的 `null` 元素按零值解码（spec §4.1），生成器的随机值不产生 `null` 元素。
- `time.Time` 解码用解码后的字符串，Go 用引号间的原始字节（go.dev/issue/47353）；向量中的时间不含 JSON 转义。
- `,string` tag：生成器遇到即报错（meta/model 中没有）。
- govec 的 `table` op 中探测 id 单独作为 int64 参数：放在 `map[string]any` 里经过 float64，`MaxInt64` 的转换在 amd64 与 arm64 上不同（M-F2I），这是测试工具自身的问题，不是 TiDB 的。

### 验证

```sh
cd tidb-rs && cargo test -p tidb-model                     # govec_model 回放
python3 scripts/converge.py --block D06.1 --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv crates/model/src/*.rs
cd tidb-rs-tracker/govec && CC=/usr/bin/clang go test ./...  # 覆盖、确定性、两个目标
```
