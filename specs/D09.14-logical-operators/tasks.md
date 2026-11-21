# D09.14 逻辑算子与基本逻辑规则（tasks）

进度记录（VS1-23）：

- [x] 第 1 部分：七种算子的公共结构和方法；列裁剪、投影消除（逻辑部分）、谓词下推、谓词简化、TopN / Limit 下推；govec `logicalrules`（1,692 条记录，6,630 步与 Go 一致，16 步 IN 边界）；M-FT 审计表（VS1 plan §11）。
- [ ] 第 2 部分：spec §2 的边界桩随各自的块解除（HashCode 随 `util/plancodec`，ExtractFD 随 `funcdep`，BuildKeyInfo 的两个桩随 key-info 规则，物理投影消除随 `postOptimize`）。
