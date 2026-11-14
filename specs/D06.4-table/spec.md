# D06.4 table 抽象（spec）

状态：第 1 部分（读取路径中 VS1 需要的部分）进行中（2026-11-14）。块：D06.4（ROADMAP P2，#49），crate `tidb-infoschema`（N0：D06.2–D06.4、D06.6 同一 crate）。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（VS1-12）、§9；[D06.3 spec](../D06.3-infoschema/spec.md)（InfoSchema 中存放的 `table.Table`）。

## 1. 范围

Go 范围：`pkg/table`（`table.go`、`column.go`、`index.go`、`constraint.go`）、`pkg/table/tables`（`tables.go`、`index.go`、`partition.go`、`cache.go`、`state_remote.go`、`mutation_checker.go`）、`pkg/table/tblctx`、`pkg/table/tblsession`、`pkg/table/temptable`。tracker 中 416 个函数。

### 1.1 第 1 部分（本 PR，VS1-12 的 table 读取部分）

| Go | Rust | 内容 |
|---|---|---|
| `table/table.go` | `table/mod.rs` | `Type`、错误变量、`Table` / `PhysicalTable` 接口的读取部分；`PartitionedTable`、`CachedTable` 只是标记（§3） |
| `table/column.go` | `table/column.rs` | `Column`、`ClonableExprNode`、`FindCol(s)` / `FindColumns`、列值转换（`CastColumnValue`、`CastValue`、`castColumnValue`、`handleZeroDatetime`）、列默认值（`GetColDefaultValue`、`GetColOriginDefaultValue*`、`getColDefaultValue*`）、`GetZeroValue`、`OptionalFsp`、`FillVirtualColumnValue` |
| `table/index.go` | `table/index.rs` | `Index` 接口的读取部分（`Meta`、`TableMeta`） |
| `tables/tables.go` | `tables/mod.rs` | `TableCommon`、`TableFromMeta` / `MockTableFromMeta`、列缓存、`DecodeRawRowData`、`GetChangingColVal`、`GetColDefaultValue`、主键 / 公共 handle 辅助函数、`CanSkip`、`FindIndexByColName`、restored data、`BuildTableScanFromInfos`、`SetPBColumnsDefaultValue` |
| `tables/index.go` | `tables/index.rs` | `NewIndex`、`index.Meta` / `TableMeta` |
| `expression/helper.go` `GetTimeValue`（D08.12，归本块，tasks P1-1） | `tidb-expression` `helper.rs` | 字符串、`ValueExpr`、`FuncCallExpr` 分支；`UnaryOperationExpr` 分支是边界桩（需要 `EvalSimpleAst`，D09.2） |
| `util/generatedexpr`（D00.3） | `util/generatedexpr.rs` | `ParseExpression`、`SimpleResolveName`（宪章第四条 4：首次需要时整文件移植） |
| `util/misc.go` `ColumnsToProto` / `ColumnToProto`（D00.3） | `util/misc.rs` | 表扫描的列（宪章第四条 4、5） |

converge：D06.4 0/416 → 76/416。

### 1.2 永久豁免：写路径

tidb-rs 是只读节点。以下函数不移植，tracker 中豁免理由为 "write path"：

- `table.go` 的 mutate 选项（`AddRecordOpt`、`UpdateRecordOpt`、`RemoveRecordOpt`、`DupKeyCheckMode` 等）、`AllocAutoIncrementValue*`、`CheckRowConstraint*`；
- `index.go` 的 `CreateIdxOpt`、`IndexKVGenerator`；
- `column.go` 的 `CheckOnce`、`CheckNotNull`、`HandleBadNull`、`CheckNoDefaultValueForInsert`、`FindOnUpdateCols`、`CastColumnValueWithStrictMode`；
- `tables.go` 的 `AddRecord` / `UpdateRecord` / `RemoveRecord` 及其辅助函数、handle 分配、序列、重复键错误、`WritableConstraint`、`GetWritableIndexByName`、临时表大小检查；
- `tables/index.go` 的 `Create`、`Delete`、`Exist`、`GenIndexKVIter`、`GenIndexValue`、临时索引与重复键、部分索引条件求值；
- 整个 `constraint.go`（检查约束只在写入时生效）、`mutation_checker.go`、`tblctx`、`tblsession`。

### 1.3 第 2 部分（块内其余，`blocked-by:D06.4`）

见 [tasks.md](tasks.md) "第 2 部分"。

## 2. 承载方式

- `table.Table` / `table.Index` 是 trait（`Send + Sync`），值为 `Arc<dyn Table>` / `Arc<dyn Index>`；Go 的类型断言（`t.(table.PhysicalTable)`、`t.(table.CachedTable)`、`t.(*tables.TableCommon)`）是 `as_physical_table` / `as_cached_table` / `as_any`（M-IFACE）。
- `table.Column` 嵌入 `*model.ColumnInfo`：Rust 中 `Column { info: Arc<ColumnInfo>, .. }` 加 `Deref`。按 [N13](../exceptions/N13-shared-metadata/spec.md)（`M-SHARE`，#229），`TableFromMeta` / `MockTableFromMeta` 直接克隆 `TableInfo.Columns` 中的 `Arc`，`NewIndex` 克隆 `TableInfo.Indices` 中的 `Arc`，与 Go 指向 `tblInfo.Columns[i]` / `tblInfo.Indices[i]` 相同，不复制。`Index::meta` 返回 `&Arc<IndexInfo>`（Go 的 `*IndexInfo`）。表对象和 `TableInfo` 都只读；要改先 `make_mut`（N13 §3）。
- 分区（第 2 部分）：Go 的每个分区与父表共享同一份列列表（`table/tables/partition.go:184` 传入 `tbl.Columns`），Rust 也共享（如 `Arc<[ColumnRef]>`），不为每个分区各建一份。Go 为每个分区把索引列表临时换进 `tblInfo.Indices`（`partition.go:164`–`:182`），Rust 不写元数据，把索引列表作为参数传给 `initTableCommonWithIndices`（N13 §4 第 4 行）。
- 生成列和默认值表达式的 AST：parser 的 AST 目前不是 `Send`（ADR 0004，VS1-04 修改），而表在会话之间共享，所以列中不保存 AST。`ClonableExprNode` 保存 Go 的构造函数，`Clone` 与 Go 相同地重新构造；`Internal` 也重新构造（L2）。`TableFromMeta` 照 Go 先构造一次，以便在同一时刻报告解析错误。
- `Table.Allocators` 不带 `AllocatorContext`：Go 只用它为临时表取替代分配器（第 2 部分）。
- protobuf：Go 的 gogoproto 非 nullable 字段总会写出，`tipb.ColumnInfo` / `tipb.TableScan` 中显式设置这些字段（与 `expr_to_pb.rs` 相同），向量逐字节比较。

## 3. 分区表、缓存表、临时表（第 2 部分的边界）

InfoSchema 为每张表调用 `TableFromMeta`，所以第 1 部分不能对这些表报错（否则 InfoSchema 加载失败）：

- `newPartitionedTable`：照 Go 检查分区信息（`ErrUnknownPartition`）并初始化索引，**不构造分区表达式和分区**；返回的表在 `GetPartitionedTable()` 中给出 `PartitionedTable` 标记（没有方法）。调用方据此停在自己的边界桩（VS1 plan §3.2：分区表 `blocked-by:D09.12` / `D06.4`），不会把分区表当普通表读。分区表达式的构造错误在第 1 部分不报告。
- `newCachedTable`：返回 `CachedTable` 标记，没有缓存。InfoSchema `tableFromMeta` 中 Go 对缓存表调用的 `Init`（经内部会话加载缓存）属于第 2 部分，跳过。
- `TempTableFromMeta`：`blocked-by:D06.4` 的桩。

## 4. 与 Go 的差异（均已在代码中标注）

1. `ParseExpression` 的解析错误：错误码同为 1064（`ErrParse`），文本是 Rust parser 的（Go 是 "line 1 column N near ..."）。只出现在损坏的元数据上；govec 只比较错误码。
2. `SimpleResolveName`：Rust AST 没有 `Accept`，改为遍历生成列可能出现的表达式种类，顺序与 Go 的 `Accept` 相同（L2）。
3. `NewIndex` 不解析部分索引条件（`ConditionExprString`）：Go 中它只被写路径 `MeetPartialCondition` 使用；无法解析的条件在 Rust 中不报错。
4. `TableFromMeta` / `MockTableFromMeta` 不加载检查约束（写路径），所以约束表达式的解析错误不报告。
5. `EvalColDefaultExpr`、`getColDefaultExprValue`（默认值是表达式，如 `DEFAULT (rand())`）、`GetTimeValue` 的一元运算分支需要 `expression.EvalSimpleAst`（规划器的表达式改写，D09.2）：返回 `blocked-by:D09.2` 错误（1235）。
6. 列值转换错误：照 M-ERR 只保留 cause（Go 的 `errors.Wrap` 前缀不移植）。

## 5. 测试

- govec 领域 `table`（tracker `govec/domains/table.go`）：`TableFromMeta` 的各列表与索引、列转换、默认值（含时区、严格模式、`NOT NULL`、枚举、自增）、`GetTimeValue`、`DecodeRawRowData`（int handle、公共 handle、前缀主键、默认值、DDL 中的列）、`FindCols`、`CanSkip`、restored data、`BuildTableScanFromInfos`（逐字节）与 `SetPBColumnsDefaultValue`。tidb-rs `crates/infoschema/tests/govec_table.rs` 回放。
- 共享（#229，N13 §5）：`tests/arc_sharing.rs` 检查表对象的每列、每个索引与 `TableInfo` 中的是同一个 `Arc`，DDL 轮次之后旧版本的值全部释放；`tests/arc_sharing_mem.rs` 测 10,000 张表 × 30 列 × 3 个索引：表对象 38.5 MiB（Go 30.1 MiB），去掉的副本 151.4 MiB。
- 单元测试：`table::column::tests::fill_virtual_column_value_cases`；`fill_virtual_column_value_shares_without_aliasing_writes`（照 Go `SetCol` 与临时 chunk 共享算出的列，不复制；返回后 `req` 独占该列，再次填充不改动先前取得的列）；`govec_table` 的 `decode_row` 回放（`DecodeRawRowData` 只在一次解码内借用列的 `FieldType`，照 Go `&col.FieldType` 不复制；同一行解码两次结果相同）。
- Go 单测：`column_test.go` 的 `TestGetZeroValue`、`TestCastValue`、`TestGetDefaultValue` 的用例形状进入向量；其余测试依赖 store / 写路径。
