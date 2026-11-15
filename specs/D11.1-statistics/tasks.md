# D11.1 统计结构（tasks）

spec：[spec.md](spec.md)。状态截至 2026-11-15：第 1 部分评审中（VS1-29）。

## 第 1 部分：pseudo 统计与读取部分（VS1-29）

- [x] T1 建立 `tidb-statistics` crate（layers.toml 去掉 planned）
- [x] T2 `table.go`：`Table`、`HistColl`、`ColAndIdxExistenceMap`、`PseudoTable` 与读取方法（共享规则见 spec §3）
- [x] T3 `column.go`、`index.go` 的读取部分
- [x] T4 `histogram.go` 的结构与读取方法；`scalar.go`（`EnumRangeValues` 除外）
- [x] T5 `TopN` 的读取部分（总数在构造时计算）
- [x] T6 `check_model_mutability.py` 覆盖 `crates/statistics/src`
- [x] T7 govec（与 D10.6 共用 `cardinality` 领域）

## 第 2 部分（`blocked-by:D11.1`）

- 直方图构建与合并（`builder.go`、`MergeHistograms`、`MergePartitionHist2GlobalHist`、`SplitRange` 等）
- CMSketch、FMSketch（`cmsketch.go`、`fmsketch.go`、`cmsketch_util.go`）、TopN 的查询与构建
- 越界估算：`OutOfRange`、`OutOfRangeRowCount`；`EnumRangeValues`
- 采样（`sample.go`、`row_sampler.go`）、`analyze*.go`、`estimate.go`、扩展统计构建、`debugtrace.go`
- protobuf 与存储编码（`HistogramToProto` 等）、`CheckAnalyzeVerOnTable`

## 交给 D11.2

- 统计缓存（`Arc<Table>` 快照、整体替换，spec §3）、加载、淘汰（`DropEvicted`、`DropUnnecessaryData`）、`ColumnIsLoadNeeded` / `IndexIsLoadNeeded`、`SetAllIndexFullLoadForBootstrap`
- 异步加载集合 `asyncload`（桩 `neededStatsMap.Insert`）
- used-stats 记录（D10.6 的 `recordUsedItemStatsStatus` 桩，D11.2 tasks P1-1）
