# D06.1 元数据模型（meta/model）：plan

## 1. Rust 模块布局

新 crate `crates/model`（`tidb-model`，N0 rank 3），依赖 `tidb-base`（rank 0）和 `tidb-parser`（rank 1）。

| 模块 | 内容 |
|---|---|
| `types_gen.rs`、`consts_gen.rs` | 生成（tracker `govec/cmd/rsgen -model`，之后 `cargo fmt`）：36 个结构体、`encode_json`/`decode_json`、`GoCopy`、常量与命名类型 |
| `gocopy.rs` | `GoCopy` trait：Go 的结构体赋值（spec 3.5） |
| `any.rs` | `AnyValue`（Go `interface{}`） |
| `gotime.rs` | `time.Time` 的 `MarshalJSON`/`UnmarshalJSON` |
| `table.rs`、`column.rs`、`index.rs`、`placement.rs`、`resource_group.rs`、`db.rs`、`bdr.rs` | 手写的方法，按 Go 文件分 |

`tidb-base` 的改动：`gostd::json`（`DecodeError`、`Decoder::object_promoted`、`bytes` 带 Go 类型名、`any`/`map`/`int_key`/`float64`、base64、`encode_value`/`encode_float`），`gostd::time`（`Time` 的 `Default`/`PartialEq`、`append_strict_rfc3339`、`parse_strict_rfc3339` 及其 `time.Parse(RFC3339)` 回退）。`tidb-parser`：`FieldType`/`CIStr` 的 `decode_json`。

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

## 4. 交接（2026-11-07 检查点）

### 已完成

1. tidb-model crate：生成层 + 139 个函数，converge `matched 139/153`（缺的 14 个见 spec §2，Part 2）。clippy `-D warnings` 干净，生成文件不再有模块级 `allow`。
2. tracker：`rsgen -model` 生成器（含提升字段的错误路径、`[]model.ActionType` 的类型名、`GoCopy`）；govec `model` 领域（集群语料 + 随机 + 变异 + into + 方法 op），`TestBlockCoverage` 通过。集群语料 `govec/domains/testdata/model_cluster.jsonl.gz` 由自建 v8.5.8 playground 导出，playground 已销毁。
3. arm64：8,990 条记录全部通过（加入 `sys/time` 之前的向量）。
4. `time.Parse(RFC3339)` 回退已移植（`parse_layout_rfc3339`、`time_quote`），编译、clippy 通过。

### 进行中 / 剩余（按顺序）

1. 新加的 `sys/time`（57 条 roundtrip）和 `marshal_time` op：tracker 端已生成向量；Rust 端要在 `govec_model.rs` 加 `marshal_time`（`unix(sec, nsec).in_loc(&fixed_zone("", off))` 放进 `StatsOptions.stats_window_settings.window_start`），复制新向量，跑通。
2. tracker：`go test ./...`（`TestDomains` 确定性、`TestStorage`），提交向量；开 tracker PR，取 CI 的 amd64 artifact，复制到 tidb-rs，写 `crates/model/tests/vectors/v8.5.8/README.md`（tracker commit + sha256）。
3. tidb-rs：CI arm64 job 加 `-p tidb-model`；`docs/porting/mappings.md` 加 M-BYTES（JSON 来的值用 `String`，可能是用户原始字节的用 `Vec<u8>`）；ROADMAP D06.1 行更新（153 个函数，Part 1 状态）；D04.1 行加注：mysql-protocol 中 PoC 手写的 scramble 要换成 `parser/auth` 认证插件的忠实移植；`tasks.md`。
4. fmt / clippy（`tools/first-party.sh`）、`cargo test --workspace`、`check_layers --blocks`；删除 `target/`；开 tidb-rs PR（中文）。
5. Part 2（另一个 PR）：`job.go`、`reorg.go` 的 13 个函数，`ColumnNeedRestoredData`。

### 决定

- 拆分：本 PR（a）= 生成的 Go-JSON 层 + schema 结构体 + 它们的方法（139 个函数）；PR（b）= Part 2。
- `[]*T` 的 `null` 元素按零值解码（spec §4.1），生成器的随机值不产生 `null` 元素。
- `time.Time` 解码用解码后的字符串，Go 用引号间的原始字节（go.dev/issue/47353）；向量中的时间不含 JSON 转义。
- `,string` tag：生成器遇到即报错（meta/model 中没有）。

### 验证

```sh
cd tidb-rs && cargo test -p tidb-model                     # govec_model 回放
python3 scripts/converge.py --block D06.1 --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv crates/model/src/*.rs
cd tidb-rs-tracker/govec && CC=/usr/bin/clang go test ./...  # 覆盖、确定性
```
