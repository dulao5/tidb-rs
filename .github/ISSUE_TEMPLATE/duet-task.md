---
name: Autonomous task (duet-ready)
about: A self-contained porting task for an autonomous agent (see CLAUDE.md §5)
labels: duet-ready
---

## Goal
<!-- one paragraph, plain words: what works after this task -->

## Scope
- Go functions: tracker `data/part_funcs.tsv`, part `<task>` (N functions) — list the Go files
- Rust crate(s) / files you may change:
- Spec to write or extend: `specs/<Dxx.y-name>/spec.md`

## Out of scope (do not touch)
-

## Depends on
- #

## Done when
- [ ] spec written (plain Chinese), sections listed in the PR
- [ ] every function of the part has `// go:` or a registered stub (converge 100%)
- [ ] govec vectors from Go v8.5.8 for:
- [ ] tests of touched crates, CI clippy, layer / mutability / one-translation checks pass
- [ ] tracker PR + tidb-rs PR open, CI green, "Not checked" section in the PR body

## Verify
```sh
```

## Notes for the implementer
<!-- known pitfalls, Go behaviours to watch, related merged PRs to copy conventions from -->
