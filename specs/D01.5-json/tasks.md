# D01.5 JSON 类型：tasks

| # | 内容 | 验收 | 状态 |
|---|---|---|---|
| T1 | govec `json` 领域（tracker PR #14） | 两个目标的记录相同，`targetsPending` 为空 | 完成 |
| T2 | 三个 Go 文件 L1 移植，128 个函数 | converge `matched 128/128` | 完成 |
| T3 | `tests/govec_json.rs` 回放 76,262 条 | debug 与 release-checked 都通过 | 完成 |
| T4 | 基准（Go 与 Rust 同输入）与局部优化 | spec §6 的表 | 完成（倍数目标未达，见 spec §6） |
| T5 | 整体重写（A4 §5.3–5.4，`JsonRef`、单遍解析） | 达到 A4 §6.4 的倍数目标 | 待评审决定 |
