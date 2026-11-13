# D06.2 meta 读取：spec

块 D06.2（ROADMAP P2 L1）：`pkg/structure`、`pkg/meta`（含 `meta_autoid.go`、`reader.go`）、`pkg/meta/metadef`、`pkg/meta/autoid`、`pkg/meta/metabuild`。crate `tidb-infoschema`（N0 rank 9，与 D06.3、D06.4、D06.6 同一个 crate），模块 `structure`、`meta`、`metadef`。

## 1. 范围

tidb-rs 只读：DDL 和元数据写入由 Go TiDB 承担。2026-11-11 按读路径划分（tracker `scripts/blocks/rules.py`），316 → 240 个函数：

- **归 X 的 76 个**（"DDL 与元数据写入由 Go TiDB 承担"）：
  - structure 的写入：`Set`、`Inc`、`Clear`，`HSet`、`HInc`、`HDel`、`HClear`、`updateHash`、`loadHashValue`（只被这些写入函数调用）、`hashFieldIntegerVal`、`EncodeHashAutoIDKeyValue`，`LPush`、`RPush`、`listPush`、`LPop`、`RPop`、`listPop`、`LSet`、`LClear`、`listMeta.Value`。
  - `meta.Mutator` 的写入：`Create*`、`Update*`、`Drop*`、`Add*`、`Gen*`、`Set*`、`ClearBDRRole`、`FinishBootstrap`、DDL 队列和历史的写入、`RestartSequenceValue`、写入前的存在性检查（`check*NotExists`、`checkPolicyExists`、`checkResourceGroupExists`、`checkTableExists`）、`attachMagicByte`、`DDLTableVersion.Bytes`，autoid accessor 的 `Put`、`Inc`、`Del`、`CopyTo`。
  - 测试辅助 `DefaultGroupMeta4Test`；v8.5.8 中没有调用方的 `getDDLJob`。
- **第 1 部分（本 PR）**：structure 的读取和 key 编码，`meta.Reader` 的全部读取方法（`GetRUStats` 除外）及其辅助函数，meta key 的构造和解析，`IsTableInfoMustLoad`、`Unescape`、`Element`，meta autoid accessor 的读取，`metadef`。converge 130/240。
- **第 2 部分**（plan.md）：`FastUnmarshalTableNameInfo` / `ListSimpleTables`、`ExtractSchemaAndTableNameFromJob` / `IsJobMatch` / `GetLastHistoryDDLJobsIteratorWithFilter`（需要 `pkg/util/partialjson`，即 `json.Decoder.Token`），`IterAllTables` / `splitRangeInt64Max`，`GetRUStats`（kvproto `resource_manager.Consumption` 的 JSON），`GetOldestSchemaVersion`（经 `store/helper` 读 MVCC，D07.1），`NewMutator`（`kv.Transaction`），`pkg/meta/autoid`（76 个），`pkg/meta/metabuild`（24 个）。

## 2. 依赖

`tidb-base`（错误、gofmt、`gostd::strconv`）、`tidb-parser`（`CIStr`）、`tidb-types`（`gostd::parse_float`）、`tidb-model`（D06.1 的模型及其 Go JSON）、`tidb-kv`（`Retriever`、`Snapshot`、`ErrNotExist`）、`tidb-codec`（`EncodeBytes` 等），以及 `regex`。

## 3. 设计

### 3.1 读取接口

- `TxStructure` 只有 reader（`Arc<dyn Retriever>`）；Go 的 `readWriter` 只给写入函数用，都归 X，所以 `ErrWriteOnSnapshot` 不会出现。
- 读取是 `async`（tidb-kv 的 `Retriever` 返回 future）；迭代回调是同步闭包，与 Go 相同。
- `meta.NewReader(snapshot)` 返回 `Mutator` 本身（Go 中 `Reader` 是 `*Mutator` 实现的接口）。和 Go 一样，它的 `jobListKey` 为空，所以不带参数的 `GetAllDDLJobsInQueue` 读的是空 key 的列表。

### 3.2 Go 形态的返回值（M-ERRVAL）

Go 中调用方会读"值 + error"的地方，Rust 返回 `(值, Result<(), Error>)`：`GetInt64`（范围错误时返回截断后的值）、`HGetLen`、`HKeys` / `HGetAll` / `HGetLastN`（出错前读到的部分）、`LLen`、解析 key 的函数（`Atoi` 的值）、`GetDatabase` / `GetPolicy` / `GetResourceGroup` / `GetSchemaDiff` / `GetHistoryDDLJob` / `GetTable`（JSON 解码出错时返回部分解码的对象）、`GetAllNameToIDAndTheMustLoadedTableInfo` / `GetTableInfoWithAttributes`（出错前收集的部分）、`CheckDDLTableVersion`（-1）、`GetMetadataLock`、`GetSchemaCacheSize`、ingest 参数。

### 3.3 strconv 与正则

- `tidb-base::gostd::strconv` 新增 Go 形态的 `parse_int`、`parse_uint`、`atoi`（64 位；`Atoi` 1–18 字节的快速路径；`NumError` 的文字用 `%q`）。`ParseFloat` 用 `tidb-types::gostd::parse_float`。
- `GetAllNameToIDAndTheMustLoadedTableInfo` 的两个正则用 `regex::bytes` 加 `(?-u)`：Go 的 `\d` 只匹配 ASCII，写成 `[0-9]`；Go 的 `regexp` 把非法 UTF-8 的每个字节读成一个 U+FFFD，按字节匹配得到的范围相同（`.` 和 `[^"\\]` 不会停在多字节字符中间）。正则没匹配上时，Go 下标越界 panic，Rust 以相同文字 panic。Go 的 `NameExtractRegexp` 取的是第一个 `"O"`，即表名的原始大小写（变量名 `nameLRegex` 容易误解），Rust 照搬。

## 4. 偏离与豁免

1. ~~**`ReverseHashIterator` 每步重新定位**（L2）~~ **已消除（D07.0 T11）**：原先 tidb-kv 的 `Retriever::iter_reverse` 让迭代器借用 key 参数，迭代器不能比构造起点 key 的函数活得久，Rust 只好每次 `Next` 重新 `IterReverse`；在 TiKV 上这是每步一次 RPC（Lucky 评审 #85）。现在 trait 把迭代器的生命周期（`'a`，只借用 retriever）和 key 参数（`'f`，只被 future 借用）分开，需要保留边界的实现自己 clone。`ReverseHashIterator` 与 Go 相同：构造时 seek 一次，之后在底层迭代器上 `Next`（L1）。`tests/govec_meta.rs` 的 `reverse_hash_iterator_seeks_once` 验证只 seek 一次，回放结果不变。Go 的第一个 `Key()`（`Next` 之前）是空的 field，照搬。
2. **autoid accessor 不共享**（L2）：Go 的各个 picker（`RowID()`、`IncrementID()` 等）修改同一个 accessor 的 `idEncodeFn` 并返回同一个指针，先取到的 accessor 会跟着变成最后一次 pick 的 key。Rust 每次 pick 返回独立的 accessor。v8.5.8 的调用方都是取到后立刻使用，看不出差别；govec 也按这种方式调用。
3. **`ListTables` 没有 context 参数**：tidb-kv 的 `Context` 没有取消，Go 中检查 `ctx.Err()` 的分支走不到。
4. **meta 的 metrics 没有移植**：`GetHistoryDDLJob`、`GetSchemaDiff` 中观察 `MetaHistogram` 的部分。
5. **`LGetAll` 的预分配**：Go 的 `make([][]byte, 0, length)` 在 length 为负或超过 `maxAlloc/24` 时 panic（"makeslice: cap out of range"），Rust 以相同文字 panic；在此之下 Go 按 length 分配，Rust 最多预留 1024 个（第一个缺失的元素就会出错返回）。
6. **`HLastJobIterator` 没有名字过滤**：过滤属于第 2 部分；Go 的 `IsJobMatch` 在没有名字时直接返回 true，不读 job，Rust 第 1 部分同样不读。
7. **默认 resource group 每次新建**：Go 返回包级变量 `defaultRGroupMeta` 的指针，Rust 每次构造一个相同的值（`ResourceGroupInfo` 没有 `Clone`）。
8. `model.AutoIDGroup`（D06.1 的 job_args.go）在 `meta::AutoIDGroup` 中定义，等 model crate 需要时再移过去。
9. `ParseFloat` 的输入按 UTF-8 解释（`from_utf8_lossy`）：存的 ingest 参数是 Go 写入的 ASCII 数字，非法字节只影响错误文字。

没有面向用户的行为差异，`docs/compatibility.md` 不变。
