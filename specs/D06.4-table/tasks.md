# D06.4 table 抽象（tasks）

spec：[spec.md](spec.md)。状态截至 2026-11-14：第 1 部分进行中（本 PR）。

## 第 1 部分：VS1 需要的读取路径（VS1-12）

- [x] T1 `pkg/table`：`Table` / `Index` 接口的读取部分、错误变量、`Type`
- [x] T2 `table/column.go`：列、查找、列值转换、默认值、零值、`FillVirtualColumnValue`
- [x] T3 `tables/tables.go`：`TableCommon`、`TableFromMeta`、`DecodeRawRowData`、`GetChangingColVal`、主键辅助函数、`CanSkip`、restored data、表扫描、`SetPBColumnsDefaultValue`
- [x] T4 `tables/index.go`：`NewIndex`、`Meta` / `TableMeta`
- [x] T5 `util/generatedexpr`、`util.ColumnsToProto`（D00.3，首次需要）
- [x] T6 govec `table` 领域与回放；InfoSchema 改用真正的 `TableFromMeta`
- [x] T7 amd64 向量（tracker#77 CI artifact）

## 第 2 部分（`blocked-by:D06.4`，块完成前移植或改为永久豁免）

tracker 豁免理由写 "D06.4 part 2"。

- 分区表：`tables/partition.go` 全部（`newPartitionedTable` 在第 1 部分是只做检查的桩，见 spec §3）、`BuildPartitionTableScanFromInfos`
- 缓存表：`tables/cache.go`、`tables/state_remote.go`（`newCachedTable` 是标记桩；InfoSchema 中 `cachedTable.Init` 跳过）
- 临时表：`temptable/*`、`TempTableFromMeta`（桩）、`TemporaryTable.*`、`Allocators` 的 `AllocatorContext`
- 索引读取辅助函数（索引读取路径，D10.2 / D12.1 用）：`index.GenIndexKey`、`getIndexedValue`、`FetchValues`、`fetchIndexRow`、`NeedRestoredData`、`BuildRowcodecColInfoForIndexColumns`、`BuildFieldTypesForIndexColumns`、`TryAppendCommonHandleRowcodecColInfos`、`GenIndexValueFromIndex`
- 部分索引条件（规划器用）：`ExtractColumnsFromCondition`、`DedupIndexColumns`、`extractColumnsFromExpr`
- `RowWithCols`（需要会话的 `Txn`，D05.3）、`IterRecords`、`tryDecodeColumnFromCommonHandle`（只被 `IterRecords` 调用）
- SHOW COLUMNS：`NewColDesc`、`ColDescFieldNames`
- 默认值表达式：`EvalColDefaultExpr`、`getColDefaultExprValue`（桩，等 `EvalSimpleAst`，D09.2）

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.12）`GetTimeValue`：列默认值的时间值（`table/column.go` `getColDefaultValue`；改写器的 DEFAULT 也用），需要 parser driver 的 `ValueExpr` 和 `EvalSimpleAst`。它调用的 `getTimeCurrentTimeStamp` 已在 `crates/expression/src/helper.rs` 移植。
  - 第 1 部分：已移植（字符串、`ValueExpr`、`FuncCallExpr` 分支），字符串分支由 govec `table` 的 `time_value` / `default_value` 覆盖；`UnaryOperationExpr` 分支是 `blocked-by:D09.2` 的桩。cast 领域中的 "owned by D06.4" 豁免在块完成时删除。
