# D07.0 kv 接口与 key 类型：块 spec

状态：草案（2026-11-08）。阶段：P1，B 线。基线：Go TiDB v8.5.8（`8b857efa2`）。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[mappings.md](../../docs/porting/mappings.md)、[N3+A1](../exceptions/N3-A1-concurrency-memory/spec.md)（KV 访问没有同步/异步边界、`M-GOCTX`、`M-BLOCK`、`M-LOCK`）、[N0 crate 分层](../exceptions/N0-crate-layering/spec.md)。本块建立正式的 `tidb-kv` crate（rank 5，依赖 `tidb-base`、`tidb-types`、`tidb-memory`）。同名的 PoC crate 已先由 #36 改名为 `poc-kv`（N0 §5）。

## 1. Go 范围与读路径划分

tidb-rs 是只读计算节点，写路径由 Go TiDB 承担。按 D06.1 处理 DDL job 的做法，在 tracker 的函数级引用图 `work/go_refs.tsv`（含接口分派边）上，从所有非 X 的块出发求 `pkg/kv` 函数的可达集，再逐个判断（tracker `scripts/blocks/rules.py`）：

| 归属 | 函数数 | 内容 |
|---|--:|---|
| D07.0 | 111 | key.go、kv.go、txn.go、option.go、error.go、checker.go、iter.go、txn_scope_var.go、version.go 的读路径部分，mpp.go 的 `GetNewestMppVersion` |
| X | 40 | 写路径：keyflags.go 全部（memdb 的 key 标志）、`FindKeysInStage`（语句缓冲暂存）、`WithReturnCommitTS`（提交选项）、`GenKeyExistsErr`（唯一键冲突）、TiCDC / lossy DDL 的写入来源位、utils.go（`IncInt64`、`WalkMemBuffer`）；测试辅助：fault_injection.go；读路径上没有调用方：`KeyRange.IsPoint`（只有 mockstore）、`KeyRanges.ForEachPartition`/`TotalRangeNum`（只有 lightning）、`GetInt64`、`NewVariables`、`Version.Cmp`；cachedb.go 全部（5 个）：v8.5.8 中没有调用方，缓存表实际经 `tables/cache.go` 的 MemBuffer 读取，`GetMemCache` 只被 schema_validator 用来 Delete（Lucky 在 v8.5.8 上验证：200 KB 的行缓存后全表扫描和点查都正常） |
| DEF.MPP | 7 | mpp.go 其余部分 |

D07.0 由 158 个函数变为 111 个。几处判断：
- `RunInNewTxn` 保留：`SHOW BDR ROLE`、`ADMIN` 检查等只读路径用它读 meta；它登记的内部事务 start ts 由 `GetMinInnerTxnStartTS` 上报给 GC safe point，tidb-rs 同样需要。
- `BatchGetValue` 保留：`ADMIN CHECK` 的读取。
- 接口中只声明读路径的方法（§3.2）。

## 2. Rust 布局

| Go 文件 | Rust 文件 | 状态 |
|---|---|---|
| key.go | `key.rs` | 52/55；`NewCommonHandle`、`CommonHandle.Data` 等 D01.8（§4.1） |
| kv.go | `kv.rs` | 23/30；`ResourceGroupTagBuilder` 7 个待定（§4.2） |
| txn.go、option.go、error.go、checker.go、iter.go、txn_scope_var.go、version.go、mpp.go | 同名 `.rs` | 全部 |
| `pkg/util/hack/map_abi.go`（D00.3）中 `MemAwareMap` 的一部分 | `hack.rs` | `MemAwareHandleMap` 用到的部分（§3.4） |
| client-go `util.RequestSource` / `WithInternalSourceType` | `context.rs` | `M-GOCTX`（§3.3） |

收敛检查：102/111。缺的 9 个即 §4.1、§4.2 的豁免，govec `kv` 领域的 `Exempt` 中逐个写了原因。

## 3. 设计决定

### 3.1 `Handle`

Go 的 `Handle` 是接口，v8.5.8 中只有 `IntHandle`、`*CommonHandle`、`PartitionHandle` 三个实现，移植为封闭的 enum（`M-IFACE`）。`PartitionHandle` 嵌入一个 `Handle`，没有重写的方法沿用被嵌入的 handle，与 Go 相同：例如 `PartitionHandle.Next()` 返回内层 handle 的 `Next()`，结果不是 `PartitionHandle`。`*CommonHandle` 共享，用 `Arc`；`Next` 与原 handle 共享 `colEndOffsets`，Go 也共享这个切片。跨类型的 `Compare`、对 `CommonHandle` 调 `IntValue` 等 Go 的 panic，Rust 同样 panic，消息相同（`M-PANIC`）。

### 3.2 接口

Go 接口移植为 object-safe 的 trait（`dyn Snapshot`、`dyn Storage`……）。store driver（D07.1）、临时表的拦截器、测试 mock 实现它们，调用方像 Go 持有接口值一样持有 trait 对象。

- 可能做 I/O 的方法是异步的（N3 3.1 第 6 条），返回装箱 future（`KvFuture`），这是异步方法的 object-safe 写法。`Iterator::next` 也是异步的，因为扫描器会去取下一批。
- 只声明读路径的方法。`Transaction` 的写方法（`Set`、`Delete`、`LockKeys`、fair locking、assertion、memdb checkpoint、pipelined flush），以及 `Mutator`、`MemBuffer`、`RetrieverMutator`、`AssertionProto`、`FairLockingController`、`SplittableStore`、`EtcdBackend` 都不声明。
- `Storage.GetMemCache` 不声明：它返回的 `cacheDB` 归 X（第 1 节）。
- `Storage` 中 `GetMPPClient`（MPP）、`GetOracle`、`GetLockWaits`、`GetCodec`、`ShowStatus`、`SupportDeleteRange` 随 D07.1 和调用它们的块补上。
- `SetOption(opt int, val any)` 的值是 `Box<dyn Any + Send + Sync>`。
- `map[string]ValueEntry` 的结果是 `HashMap<Vec<u8>, ValueEntry>`（`M-BYTES`）。
- client-go 的 `GetOption` / `BatchGetOption` 在 v8.5.8 中只有 `WithReturnCommitTS` 一种，移植为 enum。

### 3.3 `context.Context`（`M-GOCTX`）

`pkg/kv` 只从 context 读一个值：request source（`util.RequestSourceKey`，由 `util.WithInternalSourceType` 设置）。`Context` 只携带这个值，clone 的代价很小，相当于 Go 派生 context 的开销。取消不是 context 中的值：drop future 即取消请求（N3 3.1 第 6 条）。

### 3.4 Go 的容量与 map 记账

- `KeyRangeSliceMemUsage` 和 `CommonHandle.ExtraMemSize` 读 Go 切片的容量。移植用 Vec 的容量，并让 Go 中用 `make` 创建的缓冲区容量与 Go 相同；`append` 按 Go 的增长规则（`growslice`，按 size class 取整）。这条规则原在 `tidb-chunk` 的 `goslice` 中，本块把它移到 `tidb-base` 的 `gostd::runtime`，两个 crate 共用。
- `MemAwareHandleMap` 的字节数来自 `hack.MemAwareMap`。Go 读 swiss table 的表头（`Used`、目录长度），按 key 与元素类型的 group 大小估算字节数。这个结果只取决于元素个数、group 大小和 map 的初始状态，所以移植不需要 Go 的 map 也能算出同样的数：group 大小由 `GoLayout`（Go 类型的 size 与 align）按编译器的 group 布局计算（8 字节控制字加 8 个槽，超过 128 字节的 key 或元素间接存放）；`Init` 只支持 TiDB 实际使用的"空 map、无容量提示"（Go 大小为表头加一个 group）。
- Go 1.25 编译器会给不逃逸的 `append` 结果分配 32 字节的栈上底层数组，这时观察到的容量取决于调用点。TiDB 中 `PrefixNext` 等函数的结果都会逃逸（存入 range、handle），govec 生成器也让结果逃逸，记录的是堆上的容量。

### 3.5 `RunInNewTxn` 与 `BackOff`

按 L1 移植，`defer` 用 drop guard（`M-DEFER`）。`BackOff` 用异步定时器睡眠，不阻塞运行时线程（`M-BLOCK`，L2）。failpoint（`mockCommitErrorInNewTxn`、`TxnResourceGroupChecker`）只用于测试，不移植。日志通过 `log` 门面输出（项目约定，由二进制桥接到 tracing）。

### 3.6 `RequestTypeSupportedChecker`

Go 把 `subType` 转成 `tipb.ExprType`（int32，截断），与 ExprType 和请求子类型一起比较；移植对截断后的整数做 match。ExprType 的取值抄自 tipb 的 `expression.proto`（TiDB v8.5.8 使用的版本），本 crate 不依赖 `tipb`（其 Rust crate 要编译 gRPC 的 C++ 核心）。

## 4. 已知差异与豁免

1. **等 D01.8（`util/codec`）**：`NewCommonHandle` 用 `codec.CutOne` 切分列，`CommonHandle.Data` 用 `codec.DecodeOne` 解码。在此之前：`CommonHandle::new_with_offsets`（`glue:`）由调用方给出 Go 算出的列结束偏移，偏移切片的容量按 Go 逐个 append 的结果设置；`CommonHandle.Data` 返回错误，`CommonHandle.String` 已按 Go 移植，会照 Go 的方式输出这个错误。`IntHandle.Encoded` 需要的 `codec.EncodeInt` 只有一行，暂作 `glue:`，D01.8 时替换。
2. **`ResourceGroupTagBuilder`（7 个函数）**：需要 `parser.Digest`（D02.2 的 digester，Rust 中还没有）、`resourcegrouptag.GetResourceGroupLabelByKey`（需要 D01.11 的 rowindexcodec）、`tikvrpc.Request`（D07.1）。`Request.ResourceGroupTagger` 字段也随之后补。属于 Top SQL 的标签，不影响读取结果。
3. **`Request` 中尚未移植类型的字段**：`CoprRequestRateLimit`（client-go `util.RateLimit`，D07.2）、`MatchStoreLabels`（kvproto `metapb.StoreLabel`，D07.1）、`ResourceGroupTagger`（第 2 条）、`RunawayChecker`（`resourcegroup`）。`ClientSendOption` 的 `EventCb`、`TiFlashReplicaRead` 同样。
4. **map 的遍历顺序**：`HandleMap.Range` 等按 `HashMap` 的顺序遍历（`M-MAPORDER`）。Go 是随机顺序，中途停止时两边看到的都是不确定的子集。

## 5. 验收

- converge：`matched 102/111`，缺的 9 个即第 4 节第 1、2 条，govec `kv` 领域的 `Exempt` 中有登记。
- govec `kv` 领域：arm64 本地生成、amd64 取自 tracker CI，两个平台回放都通过（debug 和 release-checked）。另用种子 1 到 8 本地生成并回放，全部通过。
- Go 测试：key_test.go、checker_test.go、error_test.go、txn_test.go、version_test.go、option_test.go 的字面量由 `cmd/minelits` 抽取（`testdata/kv_literals.tsv`），只保留已移植函数的测试（`minedGoTests["kv"]`）。govec 表达不了的断言（`TestError`：每个 KV 错误转成 SQL 错误后码不变）移植为 `tests/unit.rs`。
- 反证：见 plan §3。
