# D06.2：plan

## 第 1 部分（本 PR）

- tracker：`rules.py` 把写入函数划到 X（spec §1），重新生成 `func_blocks.tsv`、`block_edges.tsv`。
- crate `tidb-infoschema`：`structure`（`structure.go`、`type.go`、`string.go`、`hash.go`、`list.go` 的读取）、`meta`（`meta.go` 的读取、`meta_autoid.go`、`reader.go`）、`metadef`。`tidb-base::gostd::strconv` 新增 `parse_int`、`parse_uint`、`atoi`。
- converge：D06.2 130/240；缺的 110 个是第 2 部分（下面），没有多余、没有重复。

### govec `meta` 领域

- `struct`：一个前缀、一份存储状态（Go 的 `TxStructure` 写入函数在 mockstore 事务上写出，再注入不属于 hash 的 key、类型标志错误的 key、坏的 list meta、缺失的 list 元素、不是整数的值）、一串读取调用。
- `meta`：一份存储状态（Go 的 `meta.Mutator` 写入：数据库、表（各种 must-load 属性）、autoid、sequence、placement policy、resource group、schema version 与 diff、DDL 历史和队列、BDR role、DDL table version、MDL、schema cache size、bootstrap version、ingest 参数，再注入非法 JSON、错误的 magic byte、坏的 list meta、越界的数字等）、一串 `meta.Reader` 调用。可能 panic 的 `GetAllNameToIDAndTheMustLoadedTableInfo` 单独成一条记录。
- `keys`：meta key 的构造和解析、`IsTableInfoMustLoad`、`Unescape`、`Element`。
- `metadef`：系统库名的判断。
- Go 与 Rust 都在同一种内存 snapshot 上读（有序 map，`Iter` / `IterReverse` 的语义与 client-go 的 scanner 相同，迭代器用完后 `Key()` 为 nil）。

## 第 2 部分

1. `pkg/util/partialjson`（`json.Decoder.Token` 的流式解码，含错误文字），然后 `FastUnmarshalTableNameInfo`、`ListSimpleTables`、`ExtractSchemaAndTableNameFromJob`、`IsJobMatch`、`GetLastHistoryDDLJobsIteratorWithFilter`（`set.StringSet`）。
2. `IterAllTables`、`splitRangeInt64Max`：多个 snapshot 并发扫描，需要 `kv.Storage`。
3. `GetRUStats`：`RUStats` 的 JSON，`rmpb.Consumption` 来自 kvproto。
4. `GetOldestSchemaVersion`：经 `store/helper` 读 MVCC（D07.1）。
5. `NewMutator`：在 `kv.Transaction` 上读（D03.2 的 HTTP handler、`SHOW BDR ROLE`），等事务接口落地。
6. `pkg/meta/autoid`（76 个）：allocator 的读取部分（`Base`、`End`、`NextGlobalAutoID`、`Allocators`、`ShardIDFormat`、runtime stats），写入部分划到 X；`alloc4Signed` 等已在 D06.7。
7. `pkg/meta/metabuild`（24 个）：需要 D06.3 的 exprctx / infoschema context。
