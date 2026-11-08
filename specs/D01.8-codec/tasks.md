# D01.8 编码：任务

- [x] T0 块 spec。
- [x] T1 §2 定案：L1 + TiKV 双向 oracle（负责人 #39）；methodology Q3、ADR 0007、ROADMAP 已更新。
- [x] T2 `crates/codec`：number / float / bytes（37 个函数）与 `gobinary`。
- [x] T3 decimal（3 个）。
- [x] T4 codec.go 的 Datum 层。
- [x] T5 codec.go 的 chunk 与 hash 部分；`Hash64` 用 `IHasher`。
- [x] T6 govec `codec` 领域（minelits、边界、随机、损坏输入、hash 写入字节），多种子。amd64 向量取自 tracker CI。
- [x] T7 TiKV 双向对照测试。
- [x] T8 反证、解码变异测试、基准。
- [ ] T9 性能：`DecodeOne`（Datum 拥有字节，随 D01.1 的 `b` 表示方式决定）；`SerializeKeys`（Lucky 在 Linux arm64 上测得快于 Go：75.1µs 对 79.8µs，先两个平台都测再查）；`EncodeIntWithSize`（测的是堆分配，Go 的缓冲区在栈上：加一个复用缓冲区的版本衡量编码本身）。x86 上两边都测一次。
- [ ] T10 启动时调用 `tidb_codec::init()`（D00.1 / 服务端）。
- [ ] T11 后续替换：D07.0 中 `codec.EncodeInt` 的单行 glue 和 CommonHandle 构造函数的 glue 改用 `tidb-codec`。
