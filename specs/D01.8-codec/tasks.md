# D01.8 编码：任务

- [x] T0 块 spec 草案，§2 移植方式待评审。
- [ ] T1 §2 定案（必要时更新 ADR 0007，或开 `block` issue）。
- [ ] T2 `crates/codec`：number / float / bytes（37 个函数），Go 测试表格作为单元测试。
- [ ] T3 decimal（3 个）。
- [ ] T4 codec.go 的 Datum 层。
- [ ] T5 codec.go 的 chunk 与 hash 部分；`base.Hasher` 的归属。
- [ ] T6 govec `codec` 领域（minelits、边界、随机、损坏输入、hash 写入字节），两个平台、多种子。
- [ ] T7 TiKV 对照测试。
- [ ] T8 反证、基准（与 Go `bench_test.go` 对比）。
