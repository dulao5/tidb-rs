# D01.7 Chunk：计划与交接记录

## 交接状态（随进度更新）

- 分支：tidb-rs `block/D01.7-chunk`（worktree `projects/tidb-rs-d017`），tracker `govec/chunk-D01.7`（`projects/tracker-d017`）。
- 已完成：新 crate `crates/chunk`（`tidb-chunk`，N0 rank 3）：goslice（Go 切片的长度/容量语义）、column、chunk、row、codec、chunk_util、iterator、list、compare、alloc。`cargo build` 通过。
- 下一步：govec `chunk` 领域（脚本式 op）、Rust 回放测试、tracker 的 X 归属（M-POOL）、benchmark、spec/tasks、PR。

## 设计要点

见 spec.md。核心：
1. 列缓冲 `GoSlice<E>` 保留 Go 的 len/cap，按 `growslice` + size class 计算容量，分配恰好 `cap*E` 字节；
   reslice 向上会露出之前写过的字节（`ResizeX(n, true)`），与 Go 相同，`Codec.Encode` 逐字节一致。
2. `elemBuf` 按 Go 语义保留（定长列追加 NULL 时写入的是上一次写入的值）。
3. 共享缓冲 `Bytes` 零拷贝解码，写时复制。
4. `Arc<Column>`；写入口在 debug 下断言没有别名。
5. 等 D01.1 的函数（Enum/Set/Datum/ToString、MutRow 全部）登记为豁免。
