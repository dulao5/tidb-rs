# Go 参照树与本地构建目录

状态：2026-11-21 起生效（#265）。依据 [宪章](../../CONSTITUTION.md) 第一条：行为以 Go TiDB v8.5.8 为准，所以读到的 Go 代码必须确实是 v8.5.8。

## 1. Go 参照树

**规则**：移植、评审、脚本读的 Go 源码，一律来自 `tidb-reference.lock` 固定的版本，通过 `scripts/tidb-reference.sh` 取得。不要读 master 或别的分支的本地克隆（本机 `~/src/pingcap/tidb-all-source-code/tidb` 停在 master，和 v8.5.8 不同）。

`tidb-reference.lock` 记录四项：仓库、tag（`v8.5.8`）、完整 commit（`8b857efa20363d50a8fa2ea7dd9809a85a61b115`）、源码包的 sha256。源码包是 GitHub 按 tag 生成的 tar.gz；包头里带有 commit（`git get-tar-commit-id` 可读出），脚本两项都核对。2026-11-21 核对过：解开的树与完整克隆中 `v8.5.8` 的内容逐文件相同，两次下载的 sha256 相同。

用法：

| 命令 | 作用 |
|---|---|
| `scripts/tidb-reference.sh` | 没有就下载、校验、解开，打印树的路径（已经有且未过期时不联网） |
| `scripts/tidb-reference.sh --check` | 只检查，从不下载：树缺失或与 lock 不符时退出码 1 |
| `TIDB_REFERENCE_CACHE=<目录>` | 源码包和默认树的位置，默认 `~/.cache/tidb-rs`（树在 `<目录>/tidb-v8.5.8`，约 130 MB） |
| `TIDB_REFERENCE_DIR=<目录>` | 改用这个树。可以是已有的 git 克隆：HEAD 必须是固定的 commit，已跟踪文件不能有改动；脚本只检查，不会覆盖它 |

解开的树里有一个 `.tidb-reference-stamp`，记录它来自哪个 lock；lock 改了，旧树就是"过期"，`--check` 报错，不带参数运行会重新解开。

在 shell 里取路径：`go_tree=$(scripts/tidb-reference.sh)`，然后读 `$go_tree/pkg/...`。

已经改用它的地方：

- `scripts/eval-gate.sh`、`scripts/integrationtest.sh`：`TIDB_SRC` 不给时用参照树；给了就先用 `--check` 核对，不符直接退出。
- CI `ci.yml` 的 `reference tree lock` 任务：按 lock 的哈希缓存源码包，缓存为空时下载一次（脚本校验 sha256 和 commit），然后运行 `--check`，并核对 lock 与宪章第一条写的 tag、commit 一致。
- CI `eval-gate.yml`：Go 一侧的源码从参照树来，不再单独 checkout。
- tracker 的 govec 通过 Go 模块代理引用 TiDB（`govec/go.mod` 中的伪版本 `...-8b857efa2036`），tracker CI 核对这个伪版本与本 lock 的 commit 一致。

更换基线：改 lock 的四行，运行脚本，按上游同步流程（[sync-drill-8.5.x.md](sync-drill-8.5.x.md)）处理，同时更新宪章第一条和 tracker 的 `govec/go.mod`。

## 2. 共享的 cargo 构建目录

**规则**：同一台机器上的所有 worktree 共用一个 target 目录，不各自生成完整的 target（每个约 10 GB 以上）。

做法：worktree 都建在主 checkout 的同级目录（`../tidb-rs-<名字>`）。cargo 会读取当前目录各级父目录中的 `.cargo/config.toml`，所以在共同的父目录放一个

```toml
[build]
target-dir = "<主 checkout>/target"
```

就对主 checkout 和所有 worktree 生效，不改仓库自己的 `.cargo/config.toml`（CI 仍用默认的 `target/`），也不改用户全局的 cargo 配置。`scripts/shared-target.sh` 写这个文件（已存在且内容相同时不动），`scripts/shared-target.sh --check` 检查当前 worktree 是否用的是共享目录。环境变量 `CARGO_TARGET_DIR` 仍然优先。

注意：

- 多个 worktree 同时构建时，cargo 在目录锁上排队（"Blocking waiting for file lock"），这是预期的。
- 各 worktree 的路径不同，工作区内的 crate（包括 `third_party/` 下的路径依赖）在每个 worktree 第一次构建时仍会重编，crates.io 的依赖只编一次。

### 增量重建基线（2026-11-21，Apple M4 Pro 12 核，dev profile，`cargo build --locked -p poc-server`）

| 场景 | 时间 | 重编的单元 |
|---|---|---|
| 新 worktree 第一次构建（共享目录已有主 checkout 的产物） | 18 s | 67 / 392 |
| 没有改动 | 0.4 s | 0 |
| 改 `crates/poc/server/src/main.rs`（最上层） | 1 s | 1 |
| 改 `crates/executor/src/lib.rs` | 1 s | 5 |
| 改 `crates/expression/src/lib.rs` | 3 s | 13 |
| 改 `crates/types/src/lib.rs`（最底层之一） | 4 s | 27 |

"改"指在文件末尾加一个 `pub const`。各自独立 target 时，新 worktree 第一次构建要编全部 392 个单元，并多占一份目录。数字明显变差时（例如某次改动让底层 crate 的重建超过 30 s），在 PR 中说明原因。
