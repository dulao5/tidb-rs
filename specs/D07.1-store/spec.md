# D07.1 存储驱动：作为 kv.Storage 的 TiKV 存储与只读事务（VS1-20 的部分）

状态：VS1-20（点查纵向集成）需要的部分。第 1 部分（VS1-18：snapshot 的点读、错误转换、backoffer）没有单独的 spec，记在 VS1 plan §5 和 `client-go-parity.md`。

## 1. 为什么需要

新栈的会话一直用一个替身存储（`UnwiredStore`，每次读都返回 `blocked-by:D07.1`）。预处理点查的计划和执行器接通之后（D05.3 spec §10），执行器第一次读数据时，会话要按 Go 的事务流程开启一个事务（`LazyTxn` 等到 start ts 后调用 `kv.Storage.Begin`），于是停在替身存储上（`active transaction fail: blocked-by:D07.1`）。D05.3 spec §7 第 1 条早已把"新会话接上真正的存储与 oracle（`Begin` / `GetOracle`）"记给 VS1-20。

## 2. 范围

| Go | Rust | 级别 | 说明 |
|---|---|---|---|
| `tikvStore.Begin`（`tikv_driver.go:365`） | `TikvStore::begin`（`crates/store/src/driver/mod.rs`） | L1 | 把 `TxnOption`（start ts、txn scope）交给 client-go 边界的 `KVStore.Begin`，结果包成 `tikvTxn` |
| `tikvStore.Close`（`:347`） | `Storage::close` | L2 | client-rust 的客户端与 PoC 的读共用，最后一个句柄释放时关闭；没有 etcd、GC worker（驱动的 `Open` 属于第 2 部分）。新栈不关存储 |
| `KVStore.GetOracle`（client-go，经嵌入提升） | `TikvStore::get_oracle` → `PdOracle` | 胶水 + L3 边界 | `PdOracle.GetTimestamp` 从 PD 取 TSO；`GetLowResolutionTimestamp` 见 §4 |
| `KVStore.Begin`、`transaction.NewTiKVTxn`、`KVTxn` 的读与结束（client-go） | `crates/store/src/driver/tikv.rs` 的 `KVStore::begin`、`new_tikv_txn`、`KVTxn` | L3（ADR 0008） | 没有 start ts 时取 TSO；事务读走同一个 `KVSnapshot`；`Commit` / `Rollback` 见 §3 |
| `NewTiKVTxn`（`txn_driver.go:57`） | `txn_driver::new_tikv_txn` | L2 | `SetKVFilter` 和 MemDB 的大小限制只作用于写事务的 MemDB，不设 |
| `tikvTxn.Commit` / `GetSnapshot` / `Get` / `BatchGet` / `SetOption` / `GetOption` / `extractKeyErr` | `txn_driver.rs` | L1 / L2 | 见 §3 |
| `tikvTxn.Iter` / `IterReverse` | 桩 | — | `blocked-by:D07.1`：union store 的迭代器在 scanner 之上，属于第 2 部分 |
| `CopClient` 作为 `kv.Client`（Go 的 `CopClient` 嵌入 `kv.RequestTypeSupportedChecker`） | `impl kv::Client for CopClient` | 胶水 | 规划上下文的下推检查要 `GetClient()`；`Send` 交给已移植的 `CopClient.Send` |

`TikvStore` 实现 `kv::Storage`（`Begin`、`GetSnapshot`、`GetClient`、`Close`、`UUID`、`CurrentVersion`、`Name`、`Describe`、`GetMinSafeTS`），除下面 §4 的两个外都转到已移植的方法。

## 3. 只读事务（与 Go 的差异）

tidb-rs 是只读节点（宪章 §4.3），Rust 的 `kv.Transaction` 没有 `Set` / `Delete` / `LockKeys`。所以事务的 MemDB **永远是空的**：

- `Get`：Go 先查 MemBuffer，`ErrNotExist` 时读 snapshot；空 MemBuffer 一定是 `ErrNotExist`，于是读 snapshot，空值同样转成 `ErrNotExist`。照这个顺序写（L2：MemBuffer 是一个空的替身）。
- `BatchGet`：照 Go 用 `NewBufferBatchGetter(MemBuffer, nil, snapshot)`，MemBuffer 为空的替身。
- `Len` / `Size` 为 0，`IsReadOnly` 为 true。
- `Commit`：client-go 先检查 `valid`（否则 `ErrInvalidTxn`），关闭事务，然后 committer 发现没有 mutation（`mutations.Len() == 0`）直接返回。照这个顺序。没有悲观锁要回滚。
- `Rollback`：同样先检查 `valid`，然后关闭；没有 committer 要清理。
- `SetOption`：snapshot 读的选项照 Go 交给 `KVSnapshot`（隔离级别、优先级、not fill cache、snapshot ts、replica read、task id、runtime stats、sample step、staleness、store labels、resource group tag / name、request source、load-based replica read 阈值、读超时），事务自己的（`Pessimistic`、`GuaranteeLinearizability`、`TxnScope`、`SnapInterceptor`）记在事务里。只给 committer 和 MemDB 用的选项（schema checker、InfoSchema、commit hook、async commit、1PC、KV filter、commit ts 上界检查、assertion level、column maps、txn source、size limits、session id、生命周期钩子、prewrite 锁策略）接受但不保存：没有写事务的提交。选项值的 Rust 类型与 snapshot 的 `SetOption` 相同。
- `GetOption`：`GuaranteeLinearizability`、`TxnScope`、`RequestSourceInternal`、`RequestSourceType` 照 Go 回答；`TableToColumnMaps`（写路径的缓存）为 nil。
- 事务的 `KVTxn` 只保存读用到的字段（start ts、valid、scope、causal consistency、pessimistic）；committer 才读的字段不保存。

## 4. 边界桩

| 位置 | 原因 |
|---|---|
| `tikvTxn.Iter` / `IterReverse` | union store 的迭代器（scanner，T11），第 2 部分 |
| `UUID`（client-go `KVStore.UUID`，`tikv-<cluster id>`） | client-rust 不暴露 PD 的 cluster ID；读路径不调用。trait 返回 `String`，桩以带 `blocked` 文本的 panic 表示 |
| `GetMinSafeTS`（client-go `KVStore.GetMinSafeTS`） | client-go 在后台按 store 维护 safe ts，client-rust 没有；只有 stale read 用，而 stale read 不进新栈（VS1 plan §4.3 第 0 步）。同样以 panic 表示 |
| `PdOracle.GetLowResolutionTimestamp` | client-go 保存最近一次的 TSO，后台按 `tidb_low_resolution_tso_update_interval` 刷新；client-rust 没有这个缓存。`tidb_low_resolution_tso` 不在路由器同步的变量里，开着它的会话不进新栈；万一走到，返回 `blocked`，不会拿一个更新的 TSO 冒充 |

## 5. 测试

- 这些函数都要连真实的 PD / TiKV 才能构造（`KVStore` 包着 client-rust 的 `TransactionClient`），没有单测替身；由 VS1-20 的端到端验证覆盖：ab `strict` 的 sysbench 点查全部在新栈上执行且与 Go 一致，sysbench 点查压测（`specs/VS1-vertical-slice/c2a-integration.md`）。会话层的事务流程（`Begin` 的参数、自动提交时的提交）由 `tidb-session` 的替身存储测试覆盖。
- govec：不适用（存储的调用，没有可按输入输出对照的计算）。

## 本文所用 ID

- D07.1：本块，存储驱动；D05.3：会话；D10.1：物理算子
- VS1-18：存储驱动第 1 部分（snapshot 点读）；VS1-20：点查纵向集成
- ADR 0008：client-go 由 client-rust 替代（L3）；T11：scanner
