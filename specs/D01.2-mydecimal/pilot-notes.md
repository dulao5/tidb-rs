# 流程试点记录（D01.2）

记录流程本身的发现，试点结束后据此修订宪章、方法论、来源标注规范和 spec 模板。

## 已发现

1. **块外的叶子函数**：`mydecimal.go` 用到 `types/helper.go`（D01.1）中的 `strToInt`、`isSpace`、`isDigit`。宪章第四条只允许"边界桩"或"`util/*` 叶子代码整文件移植"，没有覆盖"其他块里的、没有进一步依赖的小函数"。本试点的做法：按 L1 移植到 Rust 中对应的文件（`types/helper.rs`），由 D01.1 直接计为已完成。建议把这条写进宪章第四条 4：引用图中没有出边的函数可以提前移植到对应位置，不算"只拉进单个上游函数"。
2. **错误类型先于 D00.2**：几乎每个块都会用到 `terror`/`dbterror` 和各包的 `errors.go`。如果每个块都写错误桩，会重复很多次。建议把 D00.2（错误码与 terror）排到 P1 最前面，作为所有块的前置。
3. **govec 的 panic 记录**：tracker `govec/README.md` 写的是"Rust 必须返回错误"。plan 中改为"保持 panic，由 task 边界转换"（因为这些 panic 是 Go 的 bug 引起的，`M-PANIC` 只把控制流用途的 panic 改为 `Result`）。比对规则改为"Go panic 时 Rust 也 panic"，tracker README 需要同步修改。
