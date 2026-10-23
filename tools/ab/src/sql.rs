//! Just enough SQL lexing for the harness: splitting a file into statements, finding the leading
//! keywords, and deciding whether a statement is safe to send to both servers. This is not a
//! parser; it only has to respect quotes and comments.

/// A word outside quotes and comments, upper-cased, with its parenthesis depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub text: String,
    pub depth: u32,
}

/// Upper-cased words (identifier-like runs, including `@`, `@@x.y` and `.`) outside string
/// literals, quoted identifiers and comments. Optimizer hints `/*+ ... */` and MySQL
/// executable comments `/*! ... */` are skipped too: they never change what kind of statement
/// it is.
pub fn words(sql: &str) -> Vec<Word> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut depth: u32 = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' | b'`' => i = skip_quoted(b, i),
            b'#' => i = skip_line(b, i),
            b'-' if is_dash_comment(b, i) => i = skip_line(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_block(b, i),
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if is_word_byte(c) => {
                let start = i;
                while i < b.len() && (is_word_byte(b[i]) || b[i] == b'.') {
                    i += 1;
                }
                out.push(Word {
                    text: sql[start..i].to_ascii_uppercase(),
                    depth,
                });
            }
            _ => i += 1,
        }
    }
    out
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'@' || c == b'$' || c >= 0x80
}

/// `--` starts a comment when followed by whitespace or the end of input (MySQL rule).
fn is_dash_comment(b: &[u8], i: usize) -> bool {
    b.get(i + 1) == Some(&b'-') && b.get(i + 2).is_none_or(|c| c.is_ascii_whitespace())
}

/// Index just past the quoted run starting at `i` (backslash escapes and doubled quotes).
fn skip_quoted(b: &[u8], i: usize) -> usize {
    let q = b[i];
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == b'\\' && q != b'`' {
            j += 2;
            continue;
        }
        if b[j] == q {
            if b.get(j + 1) == Some(&q) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

/// Index of the newline ending the comment at `i` (or the end).
fn skip_line(b: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < b.len() && b[j] != b'\n' {
        j += 1;
    }
    j
}

fn skip_block(b: &[u8], i: usize) -> usize {
    let mut j = i + 2;
    while j + 1 < b.len() {
        if b[j] == b'*' && b[j + 1] == b'/' {
            return j + 2;
        }
        j += 1;
    }
    b.len()
}

/// What the harness may do with a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Read-only query: sent to both servers and compared.
    Read,
    /// Session statement (session `SET`, `USE`): sent to both, compared, and replayed after a
    /// reconnect.
    Session,
    /// Anything else (writes, DDL, global settings, locking reads, EXPLAIN...): never sent to B.
    Other(String),
}

/// Classifies a statement. Only SELECT/TABLE/VALUES/WITH (without data-changing or locking
/// clauses), SHOW, session SET and USE are allowed.
pub fn classify(sql: &str) -> Kind {
    let w = words(sql);
    let Some(first) = w.first() else {
        return Kind::Other("empty".into());
    };
    let has = |seq: &[&str]| {
        w.windows(seq.len())
            .any(|win| win.iter().zip(seq).all(|(a, b)| a.text == *b))
    };
    let other = |why: &str| Kind::Other(format!("{} ({why})", first.text));
    match first.text.as_str() {
        "SELECT" | "TABLE" | "VALUES" | "WITH" => {
            for bad in [
                &["FOR", "UPDATE"][..],
                &["FOR", "SHARE"],
                &["LOCK", "IN", "SHARE", "MODE"],
                &["INTO", "OUTFILE"],
                &["INTO", "DUMPFILE"],
            ] {
                if has(bad) {
                    return other("locking or file-writing read");
                }
            }
            if first.text == "WITH"
                && w.iter().any(|x| {
                    x.depth == 0 && matches!(x.text.as_str(), "UPDATE" | "DELETE" | "INSERT")
                })
            {
                return other("data-changing statement");
            }
            Kind::Read
        }
        "SHOW" => Kind::Read,
        "USE" => Kind::Session,
        "SET" => {
            let second = w.get(1).map_or("", |x| x.text.as_str());
            if matches!(
                second,
                "PASSWORD" | "DEFAULT" | "ROLE" | "RESOURCE" | "CONFIG" | "BINDING"
            ) {
                return other("not a session variable assignment");
            }
            if w.iter().any(|x| {
                matches!(
                    x.text.as_str(),
                    "GLOBAL" | "PERSIST" | "PERSIST_ONLY" | "INSTANCE"
                ) || x.text.starts_with("@@GLOBAL.")
                    || x.text.starts_with("@@PERSIST")
                    || x.text.starts_with("@@INSTANCE.")
            }) {
                return other("global/persisted variable");
            }
            Kind::Session
        }
        _ => other("not read-only"),
    }
}

/// Whether the statement has an ORDER BY at the top level (outside subqueries), i.e. whether
/// its row order is defined.
pub fn has_top_level_order_by(sql: &str) -> bool {
    let w = words(sql);
    w.windows(2)
        .any(|p| p[0].depth == 0 && p[0].text == "ORDER" && p[1].text == "BY")
}

/// Which section of a workload file a statement is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// `-- ab:setup`: everything runs on A (writes allowed); session statements also on B.
    Setup,
    /// Default, or after `-- ab:test`: compared.
    Test,
}

/// One statement of a workload file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub sql: String,
    /// 1-based line where the statement starts.
    pub line: usize,
    pub section: Section,
    /// mysqltest `--sorted_result` preceded it: compare rows as a multiset.
    pub sorted: bool,
}

/// Splits a workload file. Statements end at `;` outside quotes and comments; a file with no
/// such `;` at all has one statement per non-empty line. Lines starting with `--` or `#` between
/// statements are comments; among them `-- ab:setup` / `-- ab:test` switch sections and
/// `--sorted_result` (mysqltest) marks the next statement as unordered. Other mysqltest
/// directives (`--error`, `--enable_warnings`, ...) are ignored.
pub fn split_statements(src: &str) -> Vec<Statement> {
    let line_mode = !has_terminator(src);
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut section = Section::Test;
    let mut sorted = false;
    let mut start: Option<usize> = None;
    let mut i = 0;
    let line_of = |pos: usize| src[..pos].bytes().filter(|&c| c == b'\n').count() + 1;
    let mut finish = |from: usize, to: usize, section: Section, sorted: &mut bool| {
        let sql = src[from..to].trim();
        if !sql.is_empty() {
            out.push(Statement {
                sql: sql.to_string(),
                line: line_of(from),
                section,
                sorted: *sorted,
            });
        }
        *sorted = false;
    };
    while i < b.len() {
        let c = b[i];
        if start.is_none() {
            if c.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            // Between statements: whole-line comments and directives.
            if c == b'#' || (c == b'-' && b.get(i + 1) == Some(&b'-')) {
                let end = skip_line(b, i);
                let line = src[i..end].trim();
                let directive = line.trim_start_matches(['-', '#']).trim();
                match directive {
                    "ab:setup" => section = Section::Setup,
                    "ab:test" => section = Section::Test,
                    "sorted_result" => sorted = true,
                    _ => {}
                }
                i = end;
                continue;
            }
            start = Some(i);
        }
        match c {
            b'\'' | b'"' | b'`' => i = skip_quoted(b, i),
            b'#' => i = skip_line(b, i),
            b'-' if is_dash_comment(b, i) => i = skip_line(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_block(b, i),
            b';' if !line_mode => {
                finish(start.take().unwrap_or(i), i, section, &mut sorted);
                i += 1;
            }
            b'\n' if line_mode => {
                finish(start.take().unwrap_or(i), i, section, &mut sorted);
                i += 1;
            }
            _ => i += 1,
        }
    }
    if let Some(s) = start {
        finish(s, b.len(), section, &mut sorted);
    }
    out
}

/// Whether `src` has a `;` outside quotes and comments.
fn has_terminator(src: &str) -> bool {
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => i = skip_quoted(b, i),
            b'#' => i = skip_line(b, i),
            b'-' if is_dash_comment(b, i) => i = skip_line(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_block(b, i),
            b';' => return true,
            _ => i += 1,
        }
    }
    false
}

/// Replaces each `?` placeholder outside quotes and comments with the given literals.
pub fn substitute(sql: &str, literals: &[String]) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16 * literals.len());
    let mut next = literals.iter();
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => i = skip_quoted(b, i),
            b'#' => i = skip_line(b, i),
            b'-' if is_dash_comment(b, i) => i = skip_line(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => i = skip_block(b, i),
            b'?' => {
                out.push_str(&sql[last..i]);
                out.push_str(next.next().map_or("?", String::as_str));
                i += 1;
                last = i;
            }
            _ => i += 1,
        }
    }
    out.push_str(&sql[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        assert_eq!(classify("SELECT c FROM sbtest1 WHERE id=1"), Kind::Read);
        assert_eq!(classify(" /* x */ (select 1) union (select 2)"), Kind::Read);
        assert_eq!(classify("select /*+ USE_INDEX(t, k) */ 1"), Kind::Read);
        assert_eq!(classify("show tables"), Kind::Read);
        assert_eq!(classify("with t as (select 1) select * from t"), Kind::Read);
        assert_eq!(classify("use `test`"), Kind::Session);
        assert_eq!(classify("SET NAMES utf8mb4"), Kind::Session);
        assert_eq!(classify("set @@session.tidb_x = 1, @a = 2"), Kind::Session);
        assert_eq!(classify("SELECT 'FOR UPDATE'"), Kind::Read);
        for w in [
            "insert into t values (1)",
            "update t set a=1",
            "delete from t",
            "create table t (a int)",
            "select * from t for update",
            "select * from t lock in share mode",
            "select 1 into outfile '/tmp/x'",
            "set global tidb_x = 1",
            "set @@global.tidb_x = 1",
            "set session a = 1, global b = 2",
            "set password = 'x'",
            "set config tikv `x` = 1",
            "explain select 1",
            "analyze table t",
            "with t as (select 1) delete from u",
            "",
        ] {
            assert!(matches!(classify(w), Kind::Other(_)), "{w}");
        }
    }

    #[test]
    fn order_by() {
        assert!(has_top_level_order_by(
            "select c from t where id between 1 and 2 order by c"
        ));
        assert!(!has_top_level_order_by("select c from t"));
        assert!(!has_top_level_order_by(
            "select * from (select c from t order by c) x"
        ));
        assert!(!has_top_level_order_by("select 'order by'"));
    }

    #[test]
    fn splits() {
        let src = "-- comment\nselect 1;\n--sorted_result\nselect 'a;b'\n  from t; # trailing\n\
                   -- ab:setup\ncreate table t (a int);\n-- ab:test\nselect /* ; */ 2;";
        let s = split_statements(src);
        let sqls: Vec<_> = s.iter().map(|s| s.sql.as_str()).collect();
        assert_eq!(
            sqls,
            vec![
                "select 1",
                "select 'a;b'\n  from t",
                "create table t (a int)",
                "select /* ; */ 2"
            ]
        );
        assert_eq!(s[0].line, 2);
        assert!(!s[0].sorted && s[1].sorted && !s[2].sorted);
        assert_eq!(s[1].line, 4);
        assert_eq!(s[2].section, Section::Setup);
        assert_eq!(s[3].section, Section::Test);

        let lines = split_statements("select 1\n\n# c\nselect 2\n");
        let sqls: Vec<_> = lines.iter().map(|s| s.sql.as_str()).collect();
        assert_eq!(sqls, vec!["select 1", "select 2"]);
        assert_eq!(lines[1].line, 4);
    }

    #[test]
    fn substitutes() {
        let lits = vec!["1".to_string(), "'x'".to_string()];
        assert_eq!(
            substitute("select '?', ? from t where a = ? -- ?\n", &lits),
            "select '?', 1 from t where a = 'x' -- ?\n"
        );
    }
}
