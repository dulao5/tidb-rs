# 围栏条目（模板）

状态：模板（#262，2026-11-21）。凡是例外（`specs/exceptions/`）或"不移植"（X）决定让用户在 SQL
或协议上看到"不支持"的地方，都按这五段写一条围栏，登记在 [fences.md](../exceptions/fences.md)
（或该例外 spec 的"围栏"一节，fences.md 里留一行索引）。临时的 `blocked-by:<块>` 边界桩不写围栏：
它们由 tracker 的 stubs.tsv 和 `scripts/check_stub_ratchet.py` 管。

```markdown
### F<n> <短名>

- **边界**：哪些语句 / 取值 / 协议命令被挡住；哪些相近的仍然放行（写出例子）。
- **理由**：对应的例外或 X 决定（链接到 spec / ROADMAP 行 / 负责人决定），为什么不移植或不同。
- **围栏位置**：`<file>:<line>`（每个返回错误的地方一行；改动代码后同步更新）。
- **用户看到的确切错误**：错误码、SQLSTATE（如有）、逐字的消息，例如
  `ERROR 1235 (42000): This version of TiDB doesn't yet support '…'`。
- **重开条件**：什么情况下重新评估（例如例外 spec 定稿、上游变化、用户需求、某块移植完成）。
- **测试**：`<path>::<函数>`，断言返回上面的错误码和消息、不 panic。
```

要求：

1. 每个围栏都有测试，返回 MySQL 错误码，**不能 panic**，也不能接受后静默忽略（宪章第五条）。
2. 错误消息逐字写出；改动消息时同步改测试和本条目，并检查
   `tools/ab/prototype-not-supported.txt`（容差 T1 只认登记过的固定文案）。
3. 用户可见、但不是"不支持"的差异（行为不同而不是拒绝）不写围栏，写进偏差台账
   `docs/testing/rulings.toml`。
