# D07.0 kv：计划与记录

## 1. 交接状态

- tidb-rs 分支 `block/D07.0-kv`（基于 `poc/rename-crates`，即 #36）；tracker 分支 `govec/kv-D07.0`。
- 本块完成。后续工作见 tasks.md 中未勾选的项。

## 2. 实现顺序（已完成）

1. PoC crate 改名（#36，N0 §5），腾出 `crates/kv` 和包名 `tidb-kv`。
2. tracker：读路径划分（`rules.py`，§1 of spec），重新生成 `func_blocks.tsv`、`block_edges.tsv`。
3. `gostd::runtime`：`growslice`/`roundupsize` 从 `tidb-chunk` 移到 `tidb-base`。
4. `tidb-kv`：key / hack / kv / option / error / txn / txn_scope_var / checker / iter / version / mpp / context。
5. govec `kv` 领域与 Rust 回放（mock 的 Storage、Transaction、Snapshot、Iterator 两边各写一份）。

## 3. 反证（故意改坏，回放报不一致的条数；在 cacheDB 归 X 前的 3,188 条上测得。去掉 cache_db 之后共 3,128 条，重测"稳定排序"为 49 条、"保留分区 ID"为 129 条）

| 改动 | 不一致 |
|---|--:|
| `approxSize` 的系数 204 改为 200 | 12 |
| `PrefixNext` 的 append 不按 Go 的增长规则 | 30 |
| `calcStrsMemUsage` 少算 `SizeofStrHandleVal` | 111 |
| `IsRequestTypeSupported` 不做 int32 截断 | 4 |
| `PartitionHandle.Next` 保留分区 ID | 125 |
| `SortFunc` 改用稳定排序 | 49 |
| `RunInNewTxn` 在 f 返回可重试错误时不重试 | 32 |
| `CommonHandle` 偏移切片的容量取长度 | 143 |

"append 不按 Go 规则"和"稳定排序"两项在第一版生成器下没被发现：前者只在长度为 9 以上的全 0xFF key 上不同（size class 取整），后者只在一个分区超过 12 个 range 时不同（12 个以内是插入排序，稳定）。生成器补上了长度 1 到 40 的全 0xFF key，以及大分区、多分区的用例。

## 4. 验证命令

```sh
# Rust（tidb-rs）
cargo test -p tidb-kv
cargo test --profile release-checked -p tidb-kv
GOVEC_KV_FILES=a.jsonl.gz,b.jsonl.gz cargo test -p tidb-kv --test govec_kv   # 其他种子
python3 scripts/converge.py --block D07.0 --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv crates/kv/src/*.rs
# Go 侧（tracker govec 模块下）
GOFLAGS=$(../scripts/govec-fixes.sh) go test -run 'TestDomains/kv|TestBlockCoverage|TestTargetsDiffer' .
bin/govec kv -seed N -o /tmp/kv_seedN.jsonl.gz
```
