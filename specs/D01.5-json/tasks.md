# D01.5 JSON 类型：tasks

| # | 内容 | 验收 | 状态 |
|---|---|---|---|
| T1 | govec `json` 领域（tracker PR #14） | 两个目标的记录相同，`targetsPending` 为空 | 完成 |
| T2 | 三个 Go 文件 L1 移植，128 个函数 | converge `matched 128/128` | 完成 |
| T3 | `tests/govec_json.rs` 回放 94,691 条 | debug 与 release-checked 都通过 | 完成 |
| T4 | 基准（Go 与 Rust 同输入）与局部优化 | spec §6 的表 | 完成（倍数目标未达，见 spec §6） |
| T5 | 整体重写（A4 §5.3–5.4，`JsonRef`、单遍解析） | 达到 A4 §6.4 的倍数目标 | 推迟：单独的性能块，排在 D01.7 / D08.10 之后，目标按 1024 行基准重定（issue #23） |
| T6 | Lucky 评审 #21：`Extract` 的 dup 判断按 legs；深嵌套去掉大栈线程，改迭代路径 | `sys/dup`、`sys/deep` 向量通过；`tests/json_deep.rs` | 完成 |
| T7 | Go 测试字面量接入 `cmd/minelits` / `minedGoTests`，过滤非 JSON 字面量；govec 表达不了的断言移植为单元测试 | CI 检查每个测试函数有记录；`src/json/json_test.rs` | 完成 |
| T8 | fuzz（A4 §6.3）：解析器字节、二进制访问器、往返 | `tests/fuzz_json.rs` | 完成 |
| T9 | 性能块第一项：`Extract` 快路径 | 不慢于 Go | 待做（性能块） |
