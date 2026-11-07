# D02.3 parser 的 model、types、auth、duration：plan

## 1. Rust 模块布局

全部在 `crates/parser`（`tidb-parser`，N0 rank 1）中，新增对 `tidb-base` 的依赖（rank 0）。

| 模块 | Go |
|---|---|
| `model` | `parser/model/model.go` |
| `types`（`field_type`、`etc`、`eval_type`） | `parser/types/*.go` |
| `auth` | `parser/auth/auth.go` |
| `duration` | `parser/duration/duration.go` |
| `ast_ddl` | `parser/ast/ddl.go` 的 `NormalizeTableAffinityLevel`、`TableAffinityLevel*`；`functions.go` 的 `VecL2Distance`、`VecCosineDistance` |

`tidb-base` 新增 `gostd::json` 的类型化解码部分（spec 3.1）。

## 2. 写法映射

| Go | Rust | 规则 |
|---|---|---|
| `type PartitionType int` 及其常量 | `pub struct PartitionType(pub isize)`，常量 Go 名 | M-INT、M-GONAME |
| `func (t T) String() string` | `fn to_string(self) -> &'static str` 或 `String` | M-NAME |
| `[]string` 可能为 nil | `Option<Vec<Vec<u8>>>` | spec 3.2、3.3 |
| `error` 返回值 | `Result<_, tidb_base::Error>` | M-ERR |
| `time.Duration(i * float64(time.Hour))` | `f64_to_i64(...)` | M-F2I |
| `panic`（`EvalType.String` 无效值、`StorageLength` 负下标、`SetElem` 越界） | `panic!` / 下标越界 | M-PANIC |
| `format.RestoreCtx`（`DefaultRestoreFlags`） | 写入 `Vec<u8>` / `String` 的私有函数，标注 `go: pkg/parser/format/format.go …` | — |
| `types.TiDBStrictIntegerDisplayWidth` 包变量 | `static AtomicBool` | — |

## 3. 测试

- `tests/govec_parser_types.rs` 回放 govec `parser_types`（op 说明见 tracker `govec/domains/parsertypes.go`）。
- 比对规则：输出逐字段相等；错误消息逐字相等，JSON 语法错误除外（只要求报错）；Go panic 的记录 Rust 也必须 panic。

## 4. 反向引用与处理（N0）

D02.3 的出边只有 D00.2（136 处，rank 0，向下）和 D02.2（23 处，同一 crate：`charset` 常量、`format.RestoreCtx`）。没有反向引用。`--block-edges` 的总数不变（123）。

## 5. 遗留

- `parser/auth` 的密码插件仍是 PoC 状态，随权限块移植。
- `ast::CIStr`、`ast::FieldType` 的 PoC 替身：AST 节点移植时换成本块的类型。
- D06.1 Part 1：生成的 Go-JSON 层与 schema 结构体（`db.go`、`table.go`、`column.go`、`index.go`、`placement.go`、`flags.go`、`resource_group.go`）。`IndexInfo.ConditionExpr` 要调用 parser，Part 1 中处理。
