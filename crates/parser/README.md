# tidb-parser

TiDB's SQL grammar (`pkg/parser/parser.y` at TiDB `4cee7f2084`), compiled at build time into
LR tables and driven by a small hand-written LR driver. Promoted from `spike-parser-ast`
(custom backend only; see that spike's `REPORT.md` for the measurements behind the design).

```rust
use tidb_parser::{ParseOptions, parse};

let out = parse("SELECT a FROM t WHERE b > 1", &ParseOptions::default())?;
for stmt in &out.stmts { /* ast::Stmt */ }
```

API: `parse(sql, &ParseOptions) -> Result<ParseOutput { stmts, warnings }, ParseError>`, plus
`Parser` for explicit reuse of the parse stacks. `ParseError` is `Lex`, `Syntax { pos, near }`
(first error, no recovery) or `Semantic` (first `AppendError` of an action, as in TiDB's
`Parser.Parse`). The `ast` module is unstable.

## Architecture

```
TiDB parser.y ──tools/parser-port/convert_typed.py + handport.py──► grammar/tidb_typed.y (committed)

build.rs ─► cfgrammar (grammar) ─► lrtable (Pager LR(1), 5,683 states)
         ─► build/codegen.rs:
              OUT_DIR/tables.bin   comb-packed action/goto tables + default reductions (4.4 MB)
              OUT_DIR/actions.rs   value enum V, fn a_<p> (action) + fn r_<p> (reducer) per
                                   production, static REDUCERS table (1.2 MB of Rust)
              OUT_DIR/token_map.rs terminal name -> id

runtime: src/lexer.rs (hand-written, TiDB token classification, keyword tables from
         src/keywords_gen.rs) ─► src/driver.rs (LR loop over include_bytes! tables, reusable
         stacks) ─► actions with &Ctx (src/ctx.rs: mode flags, errors, warnings, abort)
```

- No parser-generator runtime: cfgrammar/lrtable are build-dependencies only.
- Each nonterminal has a Rust type, inferred from goyacc `%type` tags and `$n.(T)` assertions
  in the Go actions (`grammar/typed_report.json` has the statistics). Same-type passthroughs
  (`$$ = $1`) compile to a no-op reducer.
- goyacc semantics kept: `AppendError; return 1` → `ctx.fail()` stops the driver right after
  that reduction; `AppendError` alone is reported after the parse; `$span` gives the source
  range of a production (empty productions get the previous symbol's span, as in lrpar).
- The generated automaton is checked against goyacc's: `tools/parser-port/regen.sh <tidb>
  --check-tables` (isomorphic, 0 differences at `4cee7f2084`).

## Porting status

`grammar/tidb_typed.y`: 760 nonterminals, 3,235 productions (incl. the start production),
259 value types. Every production has a typed action:

| Action kind | Productions |
|---|---:|
| Hand-ported from Go (`tools/parser-port/handport.py`) | 110 |
| Passthrough `$$ = $k` | 1,078 |
| Constants (`true`/`false`, `nil` → `None`/default, empty list, unit) | 204 |
| **Placeholder** `ph(...)`: builds a generic `Node` holding its children | 1,842 |

Hand-ported (with `Restore()` in `src/ast.rs`): the statement list; SELECT and its variants,
select options (incl. the ALL+DISTINCT error), priority; field list; table references, table
names (incl. the bad-schema error), aliases, partition lists, joins (faithful `NewCrossJoin`
re-association); expressions: OR/XOR/AND/NOT, comparisons, IN, BETWEEN, IS NULL, bit and
arithmetic operators, unary operators, parentheses, column names, literals, generic function
calls; GROUP BY, HAVING, ORDER BY, LIMIT.

Placeholder (grammar accepted, AST is a `Node`/stub type from `src/gen_types.rs`): everything
else, notably DDL, DML other than SELECT, SET/SHOW/ADMIN, keyword functions
(`FunctionCallKeyword`/`NonKeyword`), aggregates (`SumExpr`), window specs, TABLESAMPLE, user
variables, INTO, hints (the nested hint grammar is not ported). 27 nonterminals still use the
catch-all type `Item = Box<dyn Any>` and need manual typing. Only the default parser mode is
supported (`ParseOptions::window_functions`; no MariaDB / REAL_AS_FLOAT / ANSI_QUOTES /
charset options yet).

Semantic actions that reject input in Go but are placeholders here make us accept some invalid
statements: 613 of 706 invalid corpus statements are rejected today.

### Tests (`cargo test -p tidb-parser`, < 1 s after build)

- `tests/acceptance.rs`: all 4,140 default-mode cases of TiDB's `parser_test.go`
  (`tests/data/all_cases.jsonl`): 3,434/3,434 valid accepted; invalid rejected 613/706
  (asserted ≥ 613).
- `tests/restore_golden.rs`: of 863 SELECT cases with an expected `Restore()` text, the 296
  that are fully inside the ported subset restore byte-identically, except one allowlisted
  case (raw latin1 bytes in the Go source that a `&str` cannot carry).
- `tests/driver.rs` + unit tests in `src/driver.rs`: error positions, end-of-input errors,
  action aborts, stack reuse after errors, table consistency.

## Performance

`cargo run --release -p tidb-parser --example bench` (3,434 accepted statements, 183 KB;
Apple M4 Pro, rustc 1.89, 2026-10-23; three runs):

| | stmts/s |
|---|---:|
| `parse` per statement (lex + parse + AST) | 787K – 819K |
| batched, 50 statements per call | 842K – 870K |
| Go TiDB parser, same corpus (`tools/parser-port/go-baseline/bench.sh`), same session | 491K – 533K |

Go runs real actions everywhere; ours allocate placeholder nodes for unported productions, so
expect the full port to land lower (spike estimate: 600–900K stmts/s).

Build cost (release, `cargo build -p tidb-parser`): from clean package 17.5 s (6.4 s of it table
generation in build.rs), crate-only rebuild 10.5 s, peak RSS 0.89 GB. Binary of the bench
example 7.2 MB. Build dependencies are compiled with `opt-level = 3` (workspace
`build-override`), otherwise table generation is ~10x slower.

## Regenerating

```
tools/parser-port/regen.sh <tidb-checkout> [--check-tables]
```

rewrites `grammar/tidb_typed.y`, `grammar/typed_report.json`, `src/gen_types.rs`,
`src/keywords_gen.rs` and `tests/data/all_cases.jsonl` (each records the TiDB commit in its
header). Porting an action = add it to `tools/parser-port/handport.py` (plus AST/Restore in
`src/ast.rs`), regenerate, and extend the restore-golden coverage.

## Open design points

- Spans of empty productions (see above) must be decided before porting the `setNodeText` /
  `yyS[yypt-N].offset` users; `$spanN` (per-symbol spans) is not implemented yet.
- Syntax errors carry position and token text only; expected-token lists are not generated.
- The value-stack `pop!` in generated code uses `unreachable_unchecked` in release builds (the
  LR automaton fixes the stack shape); debug builds check it.
