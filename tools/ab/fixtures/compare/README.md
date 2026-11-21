Fixtures of the result comparator (`tools/ab/src/compare.rs`, spec
`docs/testing/result-comparison.md`). `cargo test -p tidb-ab compare::tests::fixtures` runs
every `*.case`; each class needs at least one case judged equal and one judged different.

Format: `key: value` lines, then sections. `#` lines are comments.

    sql: <statement>              classified by its text
    cols: a b t2.c x=a            A's (and B's) result columns: [table.]name[=origin column]
    b_cols: ...                   B's columns when they differ
    sorted: yes                   --sorted_result before it (T2)
    result_log: off               --disable_result_log (T4)
    tolerance: T2                 the tolerance the case exercises, if any
    class: ordered 1              expected class (exact | unordered | ordered K | limited <inner>)
    verdict: match                match | mismatch | both_error | unsupported
    diff: <text>                  (repeatable) a diff line must contain it
    [a] / [b] / [a_full]          rows, cells separated by `|`; an empty section = no rows
    [a_error] / [b_error]         code|SQLSTATE|message
    [a_ok] / [b_ok]               affected rows
    [a_warnings] / [b_warnings]   SHOW WARNINGS rows (their number is the warning count)

Cases named `neg-*` MUST be judged different: they exist to prove the comparator is not too
lenient.
