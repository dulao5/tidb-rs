# CLAUDE.md — working rules for agents in tidb-rs

tidb-rs is a Rust port of the Go TiDB v8.5.8 SQL layer, starting as a read-only node that runs
next to Go TiDB in the same cluster. **Behaviour follows Go v8.5.8 exactly**; results are compared
with Go statement by statement. Read [CONSTITUTION.md](CONSTITUTION.md) first: it wins over this
file and over any task description.

## 1. Before you start

- **Go source**: only the pinned v8.5.8 tree. `go_tree=$(scripts/tidb-reference.sh)`, then read
  `$go_tree/pkg/...`. Never read master or another branch
  ([docs/porting/reference-tree.md](docs/porting/reference-tree.md)).
- **Process**: porting work follows the skill [`.claude/skills/port-block`](.claude/skills/port-block/SKILL.md);
  check your work with [`audit-block`](.claude/skills/audit-block/SKILL.md) before opening a PR.
  Background: [docs/porting/](docs/porting/) (methodology, provenance markers, Go → Rust mappings),
  the shared-metadata rule [specs/exceptions/N13-shared-metadata/spec.md](specs/exceptions/N13-shared-metadata/spec.md).
- **What to port**: the functions of a task are listed in the tracker repo
  (`dulao5/tidb-rs-tracker`, `data/part_funcs.tsv`, rows whose `part` is your task, e.g. `VS1-35`).
  Task rows and dependencies: [specs/VS1-vertical-slice/tasks.md](specs/VS1-vertical-slice/tasks.md) and `plan.md`.
- **Copy conventions from merged work** of the same kind: look at a recent merged PR touching the
  same crate (`gh pr list --state merged --search <crate>`) and its tracker PR.

## 2. Rules that reviews enforce

1. **Spec first**, in `specs/<Dxx.y-name>/spec.md`, in plain Chinese (the owner reviews specs, not
   code). Copy the shape of an existing spec of the same kind.
2. **Provenance**: every ported Go function carries `// go: <Func> L1|L2` (L1 = line-by-line,
   L2 = restructured, say why). One Rust translation per Go symbol (`scripts/check_one_translation.py`).
3. **Boundaries**: anything not ported yet is a stub `// glue: stub of <Sym>, blocked-by:<block>`
   or returns `tidb_base::blocked(...)`, with a plain-language reason. Every stub is registered in
   the tracker (`python3 scripts/blocks/parts.py scan --write <tidb-rs checkout>` in the tracker
   repo). The stub count may not rise without the justification described in port-block.
4. **Differential vectors (govec)**: behaviour is pinned by vectors generated from Go v8.5.8 in the
   tracker. amd64 vectors come from the tracker CI artifact — never relabel arm64 output.
5. **Naming**: project IDs (VS1-xx, Dxx.y, Nxx, A3, L1…) never appear in identifiers, config
   values, env vars, test names or file names — name things by what they mean. In comments,
   specs, PRs and issues an ID always comes with its meaning, e.g. "the range builder (D10.4)".
6. **No upstream links**: never write github.com/pingcap/… or pingcap/tidb#N in code, specs,
   commits or PRs; upstream numbers only as plain text in backticks. A suspected Go bug goes into
   the spec, not into an upstream issue.
7. **Do not touch** `third_party/` (vendored TiKV code) and never run `cargo fmt --all` (it reformats
   third_party) — use `cargo fmt -p <crate>`.
8. **Shared plans are immutable**: executors never write a plan (`crates/executor/clippy.toml`
   forbids `make_mut_plan` / `map_children`); keep per-execution state in the executor.

## 3. Verify before opening a PR

```sh
cargo check --workspace --locked
cargo test -p <every crate you touched>          # incl. its govec tests
# CI's clippy command (see .github/workflows) with -D warnings
python3 scripts/check_layers.py
python3 scripts/check_model_mutability.py
python3 scripts/check_one_translation.py
python3 scripts/converge.py --block <Dxx.y> --part <functions.tsv> --item <task> \
    --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv <your Rust files>   # exact form: port-block §5
```

No local TiDB cluster is needed: these checks plus CI are the acceptance for `duet-ready` tasks; the
maintainer runs the end-to-end comparison against Go (`tools/ab`) after merge.

Build in your worktree's own `target/` (a target shared between worktrees can pick up another
worktree's sources). Check free disk before a full build.

## 4. Pull requests

- One tracker PR + one tidb-rs PR per task; the body says: what, which spec sections, converge
  numbers, govec counts, known differences from Go, stubs added/removed, and the merge order
  "tracker#N → #M". Commits end with a `Co-Authored-By:` line for the model that wrote them.
- Wait for CI and fix failures. **Never merge** — the maintainer (Maya) merges after review
  (Lucky). Answer review comments on the PR; push fixes to the same branch.
- If something in the task is unclear, or the work needs a decision (scope, a deviation from Go,
  a design choice), **stop and ask in the issue** instead of choosing.

## 5. Task issues for autonomous agents (`duet-ready`)

Issues labelled `duet-ready` are self-contained tasks. To take one:
1. Pick the oldest `duet-ready` issue whose "Depends on" issues are closed; relabel it
   `duet-claimed` and comment that you started. Work on at most 2 at a time.
2. Branch `duet/<short-name>` in both repos; follow the issue's scope, done criteria and
   verification exactly; do not change files outside the listed areas.
3. Open the PRs with `Closes #<issue>` and the issue's checklist ticked. Before asking for review,
   add to the PR body a section **"Not checked"**: what you did not verify or were unsure about.
4. If blocked, comment on the issue with what you need and relabel it `duet-blocked`.
