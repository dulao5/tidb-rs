# D15.3：cgroup 内存部分（pkg/util/cgroup）

块：D15.3，属于 P1，crate 为 `tidb-memory`（`layers.toml`，rank 1）。基线是 Go TiDB v8.5.8（`8b857efa2`）。

## 1. 范围

tracker `scripts/blocks/rules.py` 中 D15.3 共 23 个函数：

- `cgroup_memory.go` 全部：`GetMemoryLimit`、`GetCgroupMemLimit`、`GetMemoryUsage`、`GetMemoryInactiveFileUsage`、`getCgroupMem{Limit,Usage,InactiveFileUsage}`、`detectMem*` 共 16 个。
- `cgroup.go` 中路径探测的部分：`controllerMatch`、`detectControlPath`、`getCgroupDetails`、`detectCgroupVersion`、`readFile`、`readInt64Value`。
- `cgroup_cpu_linux.go` / `cgroup_cpu_unsupport.go` 中的 `InContainer`、`inContainer`。

CPU 部分（`GetCgroupCPU`、`CPUQuotaToGOMAXPROCS`、`SetGOMAXPROCS`、`detectCPU*`、`cgroupFileTo*`）仍为 X。

本块替换 `tidb-memory` 中原来简化的 `cgroup.rs`（glue）：原来的版本假定内存控制器挂载在 `/sys/fs/cgroup` 或 `/sys/fs/cgroup/memory`，不解析 `/proc/self/mountinfo`，在容器外、namespace 内或 hybrid 模式下会读错文件（D15.1 评审）。

## 2. 设计

- **全部 L1**。`root` 参数（生产为 `/`，测试为 fixture 目录）保留为 `*_at` 函数。
- **Go 字符串是字节**：路径和文件内容都是 `&[u8]`。用到的 Go 标准库行为放在 `tidb-base` 的 `gostd` 中：
  - `gostd::path::{clean_bytes, join_bytes, rel_bytes}`：`filepath.Clean`、`Join`、`Rel`（Unix）。`Rel` 用 Go 的 `reltests` 表单独测试，因为它的 `..` 分支在 cgroup 中走不到（mount root 含 `..` 的行先被跳过）。
  - `gostd::bytes::{fields, trim_space, is_space}`：`bytes.Fields`、`bytes.TrimSpace`、`unicode.IsSpace`（包括 U+0085、U+00A0 和 Latin-1 以上的 White_Space）。
  - `gostd::bufio::scan_lines`：默认 `bufio.Scanner` + `ScanLines`，去掉一个尾部 `\r`；一行达到 64 KiB（`MaxScanTokenSize`）时扫描停止，这一行不返回。
  - `gostd::strconv::parse_uint10`：`strconv.ParseUint(s, 10, 64)` 和 `*NumError` 的文字。溢出时立即返回 `ErrRange`，先于后面字符的语法错误，和 Go 一样。
  - `gostd::os`：`*os.PathError` 的文字；`os.Open` 能打开目录，读取失败时 Scanner 看不到数据。
- **错误**：`errors.Wrapf(err, f)` 对应消息 `f + ": " + err`。`errors.Wrapf(nil, ...)` 是 nil，所以 `readInt64Value` 遇到空文件（或目录）返回 `(0, nil)`，Rust 照样返回 `Ok(0)`。
- **`log.Warn`** 对应 `tracing::warn!`。
- **build tag**：只有 `InContainer` 有（linux / !linux）；其余在所有平台上编译，读 `/proc` 路径，和 Go 一样。

## 3. 验证

- **govec `cgroup` 领域**（tracker），每条记录带一个 fixture（文件和目录），Go 和 Rust 各自写到临时目录 `<tmp>/n/n/n/n/n/n` 下再调用（多出的层数保证 cgroup 路径中的 `..` 不会跑出临时目录）。错误消息中的根目录替换为 `<ROOT>`，临时目录替换为 `<TMP>`。
  - Go 测试 `cgroup_mock_test.go` 的 3 张用例表，以及 `TestCgroupsGetCPU` 的 (cgroup, mountinfo) 组合（两轮，包括 `cpuacct,cpu` 的写法），fixture 用 `cmd/minelits -decls` 抽取。
  - 测试 fixture 两两交叉，再配上生成的数值文件。
  - 生成的 `/proc/self/cgroup` 和 `/proc/self/mountinfo`：v1、v2、hybrid、嵌套和含 `:` 的路径、可选字段、缺少 `-`、Unicode 空白、CRLF、超过 64 KiB 的行、文件缺失或被目录或文件挡住；数值文件放在 Go 实际会读的位置（或缺失、错位、是目录），内容包括 `max`、溢出、符号、下划线、空白、非法 UTF-8 等。
  - 单独的 `detectCgroupVersion`、`controllerMatch`、`readInt64Value`、`detectMemStatValue`、`readFile` 记录。
- `InContainer` / `inContainer` 读固定的 `/proc/self` 路径，无法用 fixture，govec 中豁免；Rust 把 `inContainer` 的判断拆成 `in_container_content` 做单元测试。四个 `Get*` 包装函数以 `/` 为 root 调用已覆盖的 `*_at` 函数，同样豁免。

## 4. 偏离

1. `getCgroupMemLimit` 出错时，Go 同时返回已经走到的版本号；调用方只在 err 为 nil 时使用它，Rust 的 `Err` 不带版本号。govec 只在成功时记录版本号。
2. `in_container_content` 是从 `inContainer` 中拆出的 glue，用于测试；行为不变。

## 5. 顺带修改

- `tidb-memory` `Tracker::checkpoint`：清除增长标志时从 parent 开始，不再覆盖自己的标志。原来的写法会抹掉 `swap(false)` 之后并发记录的一次增长（D15.1 评审遗留）。

## 6. upstream-pitfall 候选

见 PR 描述。
