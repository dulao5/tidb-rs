# D14.0 执行统计数据结构：spec

状态：PR 评审中。

收敛检查：D14.0 matched 106/117，其余 11 个是 `tracing/util.go` 的豁免（§4 第 1 条），另有 1 个 `MergeTiFlashRUConsumption` 豁免（§4 第 2 条）。govec `execdetails` 领域见 plan.md。

## 1. 范围

tracker 中 D14.0 共 117 个函数（Go 函数体 1,747 行），3 个 Go 文件：

| Go 文件 | 函数数 | Rust | 内容 |
|---|--:|---|---|
| `pkg/util/execdetails/execdetails.go` | 85 | `tidb-execdetails::execdetails` | `ExecDetails` 的 `String` / `ToZapFields`（慢日志），`SyncExecDetails` 与 P90 汇总（语句摘要），`Percentile`，`RuntimeStatsColl` 与各种 `RuntimeStats`（`EXPLAIN ANALYZE`），TiFlash 扫描与等待统计，`FormatDuration` |
| `pkg/util/tracing/opt_trace.go` | 21 | `tidb-execdetails::tracing::opt_trace` | 优化器 trace 的结构与 JSON |
| `pkg/util/tracing/util.go` | 11 | — | 豁免（§4 第 1 条） |

依赖的外部代码，按 L1 一并移植（`// go-dep:` 标注）：

| 依赖 | Rust | 内容 |
|---|---|---|
| client-go `util/execdetails.go`、`util/misc.go`（`v2.0.8-0.20260803075849-c3b50791b9fb`） | `tidb-execdetails::clientgo` | `CommitDetails`、`LockKeysDetails`、`ScanDetail`、`TimeDetail`、`WriteDetail`、`TiKVExecDetails`、`ReqDetailInfo`、`RUDetails` 的 `String` / `Merge` / `Clone`，`FormatBytes`、`BytesToString` |
| `github.com/influxdata/tdigest@v0.0.1` | `tidb-execdetails::tdigest` | `Percentile` 超过 1000 个值后改用的 t-digest |
| Go 1.25.12 `math`：`Sin`、`Asin`（及 `trigReduce`、`satan`、`xatan`）、`Min`、`Max` | `tidb-base::gostd::math` | t-digest 的 scale 函数。Rust 的 libm 末位不同；Go 的 `Min`/`Max` 对 NaN 返回 NaN |

crate `tidb-execdetails`：N0 rank 4（`layers.toml` 去掉 `planned`），依赖 `tidb-base` 和 `tipb`（`ExecutorExecutionSummary`）。

## 2. 依赖

- `tidb-base`：`gostd::time`（`Duration.String`）、`gostd::strconv`、`gostd::sort`（Go 的 pdqsort）、`gostd::conv`（M-F2I）、`gostd::json`、`kv::StoreType`。
- `tipb`：`ExecutorExecutionSummary`、`TiFlashScanContext`、`TiFlashWaitSummary`。Go 的 tipb（`f9f651ef5fbc`）中向量索引字段叫 `total_vector_idx_*`，Rust 的 tipb 叫 `vector_idx_*`，字段号相同（100–108）。

## 3. 设计

### 3.1 浮点

- **M-FMA**：tracker `scripts/go_fma_sites.py` 扫描 `pkg/util/execdetails`、`pkg/util/tracing`：所有目标都没有融合的位置。依赖代码不在登记表的扫描范围内，融合位置读自 go1.25.12 linux/arm64 二进制的 `go tool objdump`（每条 `FMADDD` / `FMSUBD` / `FNMSUBD` 对应到表达式），amd64（GOAMD64=v1）不融合：
  - tdigest：`tdigest.go:154、173、181、182、186、187、260`。gc 把 `x/2.0` 写成 `x*0.5`、`2.0*x` 写成 `x+x`（都精确），并在每个融合表达式里重新计算 `index = q*processedWeight`，所以 Rust 的融合式按指令实际的操作数写出。
  - `math`：`sin.go:227、236、238`，`asin.go:40`，`atan.go:69、70`。
  - 其中 `tdigest.go:154、186、187` 的乘法是 `w*0.5`（精确），融合与否结果相同，反证测不出来；其余位置在两个目标上确实给出不同的结果：两个目标的向量有 152 条不同（tracker `targetDiffs`），也就是说 Go TiDB 在 amd64 与 arm64 上，超过 1000 个 cop task 时 `EXPLAIN ANALYZE`、慢日志和语句摘要中的 P80/P90/P95 末位不同。
- **M-F2I**：`time.Duration(f)`、`uint64(x*(4/Pi))` 等用 `gostd::conv`。

### 3.2 同步与所有权

- `SyncExecDetails`、`RuntimeStatsColl` 内部是 `Mutex`（M-SYNC）。`RootRuntimeStats` 在 Go 中只在集合的锁下修改、不加锁读取，Rust 中有自己的锁。`CopRuntimeStats` 以 `Arc<Mutex<..>>` 交出：它的 `String` 会就地排序 / 处理百分位数（Go 通过结构体副本共享的切片和 t-digest 指针做到同样的事）。
- client-go 类型中的 `Mu struct { sync.Mutex; ... }` 是普通字段：共享的持有者加锁（M-SYNC）。
- `RuntimeStats` 接口是 trait；`Merge` 中 Go 的类型断言是 `as_any().downcast_ref`。
- `[]zap.Field` 是 `Vec<LogField>`。

### 3.3 优化器 trace

- `LogicalOptimizeTracer.curRuleTracer` 指向的步骤可能被 `removeUselessStep` 从 `Steps` 中删掉，Go 之后仍向这个（看不见的）对象追加。Rust 把步骤放在 arena 中，`Steps` 和 `curRuleTracer` 都是下标，行为与 Go 相同。
- JSON：结构体的 tag（字段顺序、`json:"-"`、map 按 key 排序，int key 按字符串排序、HTML 转义、nil 为 `null`、NaN/±Inf 报 `UnsupportedValueError`）写成 `marshal_json`。
- `map[string]any` 的 cost 参数是 `CostParam`（planner 记录 float64、int、string、bool）。

## 4. 偏离与豁免

1. **`tracing/util.go`（11 个函数）豁免**：opentracing span、`runtime/trace` region、`TRACE` 语句的 span 记录器。承载方式随 ExecCtx / M-GOCTX（D05.3）与 `TRACE` 执行器一起决定。
2. **`MergeTiFlashRUConsumption` 豁免**（以及 client-go `RUDetails.Update`）：TiFlash 的 RU 消耗（DEF.MPP），要解码 kvproto `resource_manager.Consumption`，而 Rust 的 kvproto crate 依赖 grpcio。随 MPP / TiFlash 客户端一起做。
3. **按值保存，不保存调用方的指针**（L2）：`SyncExecDetails.MergeExecDetails` / `MergeLockKeysExecDetails` / `MergeSharedLockKeysExecDetails`、`RuntimeStatsWithCommit.MergeCommitDetails` 第一次收到的 detail，Go 保存调用方的指针，之后的 `Merge` 写回调用方的对象；`RuntimeStatsColl.RegisterStats` 同样保存并在之后合并进调用方注册的 stats。Rust 保存副本 / 取走所有权，之后的合并不影响调用方。TiDB 的调用方在传入之后不再读这些对象。
4. **`Percentile` 的 `Clone` 是深拷贝**：Go 的结构体复制共享 `values` 的底层数组和 t-digest 指针（例如 `basicCopRuntimeStats.Clone`），之后在副本上 `Merge` 会改动原对象的 t-digest。Rust 不共享。govec 的 `bcop` 记录因此放在每条记录的最后（之后不再读集合）。`basicCopRuntimeStats` 的 `String` 只读 `sumVal`，不受影响。
5. **`*util.CommitDetails` 等的 `nil`**：`Option`；Go 解引用 nil 的地方（`MergeLockKeysExecDetails(nil)` 合并进已有 detail、`RURuntimeStats` 的 nil `RUDetails`、`mergeExecSummary` 缺必填字段、无当前规则时 `AppendRuleTracerStepToCurrent`、nil map 的 `AppendCandidate`）Rust 同样 panic，govec 记录这些 panic。
6. **依赖中的上游问题照搬**：tdigest `AddCentroidList` 内层循环结束后外层 `i++` 会跳过一个 centroid（`room == 0` 时跳过当前的）；`ExecDetails.ToZapFields` 的 `Slowest_prewrite_rpc_detail` 少一个 `{`。
7. **tidb-base 的修正**：`gostd::strconv::parse_float("NaN")` 原来返回 Rust 的 `f64::NAN`，改为 Go 的 `math.NaN()`（payload 1），带符号的 `"+NaN"` 是语法错误（与 Go 的 `special` 相同）。govec 的 `gomath` 记录发现了这个差异。
