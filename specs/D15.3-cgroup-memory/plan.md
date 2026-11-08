# D15.3 计划

1. tidb-base `gostd`：`path::rel_bytes` / `clean_bytes` / `join_bytes`、`bytes`、`bufio::scan_lines`、`strconv::parse_uint10`、`os`。
2. `tidb-memory::cgroup`：替换简化版，23 个函数 L1；`meminfo` 接口不变。
3. tracker：`cmd/minelits -decls`；govec `cgroup` 领域；arm64 本地生成，amd64 取自 CI artifact。
4. `tests/govec_cgroup.rs` 回放（debug 和 release-checked），`GOVEC_CGROUP_FILES` 回放额外种子；`in_container_checks` 单元测试；tidb-base `tests/gostd_path_bytes.rs`。
5. `Grown::Clear` 从 parent 开始。

状态：全部完成。
