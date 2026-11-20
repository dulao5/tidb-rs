---
name: port-block
description: Port one block (or one part of a block) of Go TiDB v8.5.8 to tidb-rs — spec first, provenance markers, boundary stubs, govec vectors, converge, tracker PR, done criteria, and the recurring review rules. Use when asked to port / implement a block, a part (Dxx.y, a VS task row) or a set of Go functions in tidb-rs. 移植一个功能块或块的一部分时使用。
---

# port-block：移植一个块（或块的一部分）

依据：[CONSTITUTION.md](../../../CONSTITUTION.md)（宪章）、[methodology.md](../../../docs/porting/methodology.md)、[provenance.md](../../../docs/porting/provenance.md)、[mappings.md](../../../docs/porting/mappings.md)、[specs/templates/plan.md](../../../specs/templates/plan.md)、共享元数据规则 [N13 spec](../../../specs/exceptions/N13-shared-metadata/spec.md)。本文件只汇总流程和评审中反复出现的要求；规则冲突时以上述文档为准。

块的选择用 `find-next-block`；提交前用 `audit-block` 自查一遍。

## 0. 开工前

1. **Go 源码只读参照树**：`go_tree=$(scripts/tidb-reference.sh)`，然后读 `$go_tree/pkg/...`。不要读 master 或其他分支的克隆（见 [reference-tree.md](../../../docs/porting/reference-tree.md)）。
2. **worktree 与构建目录**：在主 checkout 的同级目录建 worktree，`git worktree add ../tidb-rs-<名字> -b <分支> origin/main`；tracker 同理（`../tracker-<名字>`）。所有 worktree 共用一个 target（`scripts/shared-target.sh --check`）。构建前看 `df -h /`，剩余不到 10 GB 就停下报告，不要删别人的目录。
3. **读**：本块（或任务行）的 spec、plan、tasks；`specs/ROADMAP.md` 中的块定义；最近合并的同类块的 spec 和 PR（照它们的写法）。
4. **禁止**：`cargo fmt --all`（会改 third_party），用 `cargo fmt -p <crate>`；裸 `git stash`；在代码、spec、提交、PR 中写上游链接（上游编号只写成反引号里的纯文本）。

## 1. spec 先行

- 在 `specs/<块 ID>-<名称>/` 写 `spec.md`（中文、平实，负责人评审的是 spec 不是代码），格式照 `specs/D06.1-model/spec.md`；planner / expression 类的块用 `specs/templates/plan.md` 写 plan。
- spec 必须与 Go 源码逐条核对（评审多次发现正负号写反、声称 Go 做了实际没做的事）。引用 Go 代码写文件和行号（基于参照树）。
- **与 Go 的每一处差异**都写进 spec 的"已知差异"一节或代码的文档注释。没有登记的用户可见差异不允许以"写路径"之类的理由带过。
- Go 的 bug 照搬，登记为 `upstream-pitfall`（写进 spec，不去上游提 issue）。依赖 gc 实现细节的行为（如零大小对象的指针相等）写明并用单元测试固定。
- spec 标着"待确认"时不得合并依赖它的代码。

## 2. 写代码

- **结构对应**：模块对应 Go 包，文件对应文件，函数对应函数（snake_case，类型名不变），按 Go 的源码顺序。
- **来源标注**（provenance.md）：文件头 `//! go-file: pkg/...go`；每个函数 `// go: <Symbol> L1`（`L1 [M-xx]` 引用映射规则，`L2 (<理由>)`，L3 需要 ADR）；拆分出的辅助函数 `// go-part:`；标准库 `// go-std:`；第三方模块 `// go-dep:`；没有 Go 对应物的胶水 `// glue: <理由>`。
- **一型一译**：一个 Go 函数、一个 Go 类型在 Rust 中只有一个翻译。写之前先 grep：`grep -rn "// go: <Symbol> " crates/`、`grep -rn "struct <Type>\b" crates/`。已经有了就调用它（必要时移到更低层的 crate），不要另写一份。确实需要第二份（如性能原因）时，在第二处标注上方写 `// go-dup: <理由>`。`scripts/check_one_translation.py` 在 CI 中检查。
- **承载方式**按 mappings.md：共享元数据用 `Arc`、只读，修改只经 `make_mut`，只对真正要改的节点调用 `make_mut`，每条语句的路径上不做深拷贝，`Arc::clone(&x)`（`clippy::clone_on_ref_ptr`）；内部可变性不进共享元数据（`check_model_mutability.py`）；跨连接共享的状态用 `parking_lot` 等不会毒化的锁；不跨 `.await` 持锁；同步函数里不阻塞等待 future（改成返回 boxed future 的方法）。
- **顺序照抄 Go**：`defer`、事务结束、生命周期相关的先后顺序（如 PREPARE 结束自动提交事务、关闭的预处理语句在事务结束时才移除），以及报错的先后顺序（先做 Go 先做的权限检查、`sql_select_limit` 检查，再报"不支持"）。
- **取消安全**：Go 用 `defer` 收尾的地方用 drop guard；持有后台 worker 的结构实现 `Drop`，通知结束并中止 worker；配一个在中途 drop 的测试。
- **新代码不进默认路径**：新移植的代码在默认的 `--new-stack=off` 模式下不能运行，做成 `Option` 构建，并测试 `off` 模式下一个也没建。
- **命名**：标识符、环境变量、配置值、测试名、文件名里不出现项目 ID（VS1-xx、Dxx.y、Nxx、M-xx 等），按含义命名。注释、spec、PR 中出现 ID 时，句子本身用平实的话说清楚，ID 放在括号里。
- **注释**：Go 注释原样复制，缩进 4 格的段落用 ```` ```text ```` 包起来。
- 尺寸和内存布局的测试加 `#[cfg(target_pointer_width = "64")]`。关于 Go 类型的事实（如"在 Go 中大小为零"）写成 Rust 类型旁边的 trait 常量，不要在别处硬编码类型判断。

## 3. 边界桩（宪章第四条 3）

- 需要未排期或未完成块的能力时写桩，不要只拉进单个上游函数。桩返回 `tidb_base::blocked::blocked("<块>", "<说明>")`（错误 1235，路由器据此回退），标注 `// glue: stub of <Symbol>, blocked-by:<块> (<缺什么>)`。不用其他错误代替。
- **不要静默**：Go 会走某个分支、而我们还做不了的地方，必须在 Go 分支的同一位置变成显式的 `blocked-by`，不能留空值、默认值或空列表。
- 每个桩在 tracker 登记：`python3 scripts/blocks/parts.py scan --write <tidb-rs worktree>`，然后给每一行补 `note`（缺什么、为什么）。tracker PR 必须在 rebase 之后的分支上扫描；如果另一个改桩的 PR 先合了，在最新的 main 上再扫一次。
- 推迟到后续任务的要求，写进那个任务的完成条件，并写明对应的测试名。

## 4. 测试与 govec

- 不依赖存储的 Go 单测随代码移植；能用向量回放的不手工翻译。
- **govec**（tracker 的 `govec/`）：每个移植的函数都要有向量覆盖（domain 的 `Coverage`），豁免必须属实。已经移植的函数不能以"尚未移植"为由豁免。对关键的边界形状加生成记录。
- 向量必须能由生成器逐字节重新生成（文件头除外）。**amd64 向量必须来自 tracker CI 的产物**（Go 在 amd64 上实际运行的结果），不能从 arm64 改名或转换。
- 回放中出现 `blocked` 的条数按块固定（如 `EXPECTED_BLOCKED`），以免本应通过的路径悄悄变成 `blocked`。Go 能解析而 Rust 不能解析的记录必须让测试失败，不能静默收集。
- 测试不依赖主机时区和当天日期：会读时区的回放在非 UTC 的 `TZ` 下也跑一遍（CI 已有 `TZ=America/Sao_Paulo` 的步骤，新增的这类测试加进去）。
- 疑似每条语句一次的额外分配或拷贝，先登记到性能剖析清单，不提前优化。

## 5. 收敛与提交

1. 本地检查，跟 CI 一样：
   - `cargo check --workspace --locked`
   - 改动 crate 的测试（包括 govec 回放）
   - `cargo clippy --no-deps --all-targets --locked $(tools/first-party.sh) -- -D warnings`
   - `cargo fmt -p <crate> --check`
   - `python3 scripts/check_layers.py`
   - `python3 scripts/check_model_mutability.py`
   - `python3 scripts/check_one_translation.py`
2. converge：`scripts/converge.py --block <块> [--part specs/<...>/functions.tsv --item <任务>] --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv <Rust 文件...>`。范围内的函数 100% 有 `// go:` 标注，或者登记为桩、不移植。
3. 更新 tasks.md 中本任务行的状态。tracker 中更新 `data/block_parts.tsv` / `blocks_done.tsv`。
4. **两个 PR**：先开 tracker PR（向量、桩登记），再开 tidb-rs PR。PR 正文写：做了什么、对应 spec 的章节、converge 数字、govec 条数、已知的 Go 差异、桩列表、本 PR 移植的 Go 文件（逐个列出，CI 会报告没有列出的）、合并顺序"tracker#N → #M"，末尾加 "🤖 Generated with [Claude Code](https://claude.com/claude-code)"。
5. 等 CI 通过，失败就修。不要自己合并；评审由 Lucky 做。
6. 和本块无关的改动单独提交，并在 PR 正文中说明。

## 6. 完成标准

- 范围内函数 converge 100%；
- 所有桩已在 `stubs.tsv` 登记，且都有 note；
- govec 向量覆盖全部移植函数，amd64 / arm64 两个目标都有，amd64 的来自 CI；
- `check_one_translation.py` 没有新的重复；
- 评审通过；每周构建的默认模式不退化（宪章第七条 4）。

## 7. 维护本文件

每次合并 PR 时看一下评审意见：同一类意见出现第二次，就把它写成上面的一条规则（写清楚怎么做，不写"注意质量"之类的话）。能用脚本检查的规则，改成 CI 检查（宪章第二条 5），本文件只保留指向脚本的一句话。
