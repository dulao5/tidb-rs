# CLAUDE.md — working rules for agents in tidb-rs

tidb-rs is a Rust port of the Go TiDB v8.5.8 SQL layer, starting as a read-only node that runs
next to Go TiDB in the same cluster. **Behaviour follows Go v8.5.8 exactly**; results are compared
with Go statement by statement. Read [CONSTITUTION.md](CONSTITUTION.md) first: it wins over this
file and over any task description.

## 1. Before you start

- **Go source**: only the pinned v8.5.8 tree. `go_tree=$(scripts/tidb-reference.sh)`, then read
  `$go_tree/pkg/...`. Never read master or another branch
  ([docs/porting/reference-tree.md](docs/porting/reference-tree.md)).
  If the script cannot download the tarball (an environment that only reaches this repository),
  run it as `TIDB_REFERENCE_MIRROR=release scripts/tidb-reference.sh`: it fetches the same archive
  from this repository's release `reference-tidb-v8.5.8` through the REST API (`gh api`) and
  verifies it identically.
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
   **Language** (owner decision 2026-11-26): issues, PR titles and bodies, review comments and
   commit messages are written in **English**; code comments are in **English**; specs and the
   other documents (`specs/`, `docs/`, CONSTITUTION) stay in **Chinese** where they are Chinese now.
   Existing Chinese issues, PRs and comments are not rewritten. When an English text uses a
   project term defined in Chinese (e.g. a constitution article), cite it with its number and
   meaning, e.g. "CONSTITUTION art. 9.3 (maintainer may fix a reproduced wrong-rows/panic bug)".
2. **Provenance**: every ported Go function carries `// go: <Func> L1|L2` (L1 = line-by-line,
   L2 = restructured, say why). One Rust translation per Go symbol (`scripts/check_one_translation.py`).
3. **Boundaries**: anything not ported yet is a stub `// glue: stub of <Sym>, blocked-by:<block>`
   or returns `tidb_base::blocked(...)`, with a plain-language reason. Every stub is registered in
   the tracker (`python3 scripts/blocks/parts.py scan --write <tidb-rs checkout>` in the tracker
   repo; run it **after** your last rebase). **No silent fallbacks**: a branch Go takes must either
   be ported or explicitly return `blocked(...)` — never skip it or return a default.
   The stub ratchet (`scripts/check_stub_ratchet.py`, baseline `scripts/stub_ratchet.tsv`) only
   goes down; lower it with `--write` after removing stubs. It may rise only in the two cases of
   port-block §2 "桩棘轮", with every net-new marker listed in the PR body.
4. **Differential vectors (govec)**: behaviour is pinned by vectors generated from Go v8.5.8 in the
   tracker. amd64 vectors come from the tracker CI artifact — never relabel arm64 output.
5. **Naming**: project IDs (VS1-xx, Dxx.y, Nxx, A3, L1…) never appear in identifiers, config
   values, env vars, test names or file names — name things by what they mean. In comments,
   specs, PRs and issues an ID always comes with its meaning, e.g. "the range builder (D10.4)".
6. **No upstream links**: never write github.com/pingcap/… or pingcap/tidb#N in code, specs,
   commits or PRs; upstream numbers only as plain text in backticks. A problem in Go itself is
   filed as an **English issue in this repo** (how to reproduce on v8.5.8), labelled
   `upstream-pitfall` (Go behaves wrongly) or `go-gap` (Go lacks a feature). Every
   `upstream-pitfall` issue also gets exactly one of `go-bug-kept` (tidb-rs copies Go: technical
   debt to fix later) or `go-bug-fixed` (tidb-rs is correct; a difference users can see — rows,
   errors, warnings, metadata — gets a ruling, an internal-only one is a known difference in the
   spec),
   and cited in the spec's known-differences section; never filed upstream. If copying Go would
   cause a security problem, wrong results or data loss, do not decide yourself: describe it in
   the issue and **stop and ask** (it needs an entry in `docs/testing/rulings.toml` approved by the
   owner). Exception, CONSTITUTION art. 9.3: a wrong-rows or panic bug you reproduced on v8.5.8,
   where another Go path, or MySQL outside TiDB's documented compatibility differences, is
   correct, is implemented correctly once the maintainer (`@du-maya`) confirms in the issue and
   the reviewer has no objection — ask them, not the owner. It does not apply (ask the owner) when
   only metadata, warnings or error text differ, when it is disputed whether Go is wrong, when the
   fix changes performance or resource behaviour, written data or encodings, or interaction with
   TiKV / TiFlash / Go TiDB in the same cluster, or when either of them has doubts.
7. **Do not touch** `third_party/` (vendored TiKV code) and never run `cargo fmt --all` (it reformats
   third_party) — use `cargo fmt -p <crate>`.
8. **Every difference from Go is written down**: the spec's "known differences" section names the
   Go file and line for each one. Copy Go's error order and its `defer` order exactly; cleanup
   that Go does in `defer` becomes a drop guard in Rust, with a test that it runs (also on error
   and early return).
9. **Shared plans are immutable**: executors never write a plan (`crates/executor/clippy.toml`
   forbids `make_mut_plan` / `map_children`); keep per-execution state in the executor.

## 3. Verify before opening a PR

These are the CI checks (`.github/workflows/ci.yml`); run all of them — for a task without a
local cluster they are the whole acceptance:

```sh
cargo check --workspace --locked
cargo fmt --check $(tools/first-party.sh)
cargo clippy --no-deps --all-targets --locked $(tools/first-party.sh) -- -D warnings
cargo test --workspace --locked                   # at least every crate you touched, incl. govec tests
python3 scripts/check_layers.py
python3 scripts/check_model_mutability.py
python3 scripts/check_one_translation.py
python3 scripts/check_stub_ratchet.py
python3 scripts/check_rulings.py
python3 scripts/converge.py --block <Dxx.y> --part <functions.tsv> --item <task> \
    --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv <your Rust files>   # exact form: port-block §5
```

No local TiDB cluster is needed for these checks. But a PR that makes statements which used to fall
back now **execute** on the new stack (a new executor, builder dispatch, routing) also needs the
end-to-end comparison against Go (`tools/ab`) **before merge**; the maintainer runs it (see
docs/process/agent-workflow.md). Session-level tests must check result rows **and** column metadata
(names and types), not only values.

Build in your worktree's own `target/` (a target shared between worktrees can pick up another
worktree's sources). Check free disk before a full build: with less than 10 GB free, stop and
report in the issue. Delete your worktree's `target/` after the PR is merged.

## 4. Pull requests

- One tracker PR + one tidb-rs PR per task. The body says: what, which spec sections, **each Go
  file ported** (listed one by one), converge numbers, govec counts, known differences from Go,
  stubs added/removed (and the ratchet change), the merge order "tracker#N → #M", and at the end
  a list defining every project ID used in it. Update your task's row in
  `specs/VS1-vertical-slice/tasks.md`. Commits end with a `Co-Authored-By:` line for the model
  that wrote them; PR bodies end with the attribution line your tool adds.
- Wait for CI and fix failures. **Never merge** — the maintainer (Maya) merges after review
  (Lucky). Answer review comments on the PR; push fixes to the same branch.
- Decision rights: CONSTITUTION article 9. Only four kinds of decision go to the owner (a
  user-visible difference from Go, exceptions or changing a confirmed decision, scope and gates,
  resources). Everything else — task split, boundary stubs for unported dependencies, internal
  representation, internal parameters, test approach — is decided by the maintainers: write it in
  the spec's "维护者已定" section, or ask the maintainer (`@du-maya`) in the issue if unsure.
  Stop and ask in the issue only when the task itself is unclear or a decision may be of the four
  owner kinds.

## 5. Task issues for autonomous agents (`duet-ready`)

Who reviews, who merges and in which order: [docs/process/agent-workflow.md](docs/process/agent-workflow.md).

Issues labelled `duet-ready` are self-contained tasks. To take one:
1. Pick the oldest `duet-ready` issue whose "Depends on" issues are closed; relabel it
   `duet-claimed` and comment that you started. Work on at most 2 at a time.
2. Use any branch name (your environment may pick it); if the two repos' branch names differ,
   the "tracker#N → #M" line in the PR body is what pairs them. Follow the issue's scope, done
   criteria and verification exactly; do not change files outside the listed areas.
3. Open the PRs with `Closes #<issue>` and the issue's checklist ticked. Before asking for review,
   add to the PR body a section **"Not checked"**: what you did not verify or were unsure about.
4. If blocked, comment on the issue with what you need and relabel it `duet-blocked`.
5. Every round, before taking new work: read review comments and CI results on your open PRs and
   address them first.
6. "Depends on" means: those issues are **closed**. If the maintainer's end-to-end comparison
   finds a difference after merge, it becomes a new `duet-ready` issue citing the original PR.
