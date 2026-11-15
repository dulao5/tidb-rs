# D03.1 连接与协议（tasks）

第 1 部分（VS1-16，结果编码与预处理语句协议，[spec](spec.md)）待评审。下面是从其他块移交来的条目。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [x] P1-1 （D08.12）二进制协议参数（COM_STMT_EXECUTE，`conn_stmt.go` 的 `parseBinaryParams`）：`ExecBinaryParam`、`binaryDate`、`binaryDateTime`、`binaryDuration`、`binaryDurationWithMS`、`binaryTimestamp`、`binaryTimestampWithTZ`（第 1 部分完成：`tidb-expression` 的 `util.rs`，govec `protocol` 领域覆盖，tracker 豁免已删除）
