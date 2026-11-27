//! SQL normalization for log redaction (Go `pkg/parser/digester.go` `Normalize`).

use crate::lexer::{Ids, Lexer, TokenTable};

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokKind {
    GenericSymbol,
    GenericSymbolList,
    SingleAt,
    UnderscoreCs,
    Identifier,
    IntLit,
    OtherNumLit,
    StringLit,
    BitLit,
    ParamMarker,
    Null,
    HintComment,
    Other,
}

struct NormTok {
    kind: TokKind,
    lit: String,
}

/// Normalize a SQL string for redacted logging (matching Go `parser.Normalize(sql, redact)`).
///
/// - When `redact` is `""` or `"OFF"`, returns `sql` verbatim.
/// - When `redact` is `"ON"`, lowercases keywords/identifiers, quotes identifiers in backticks,
///   replaces literals with `?` (and collapses lists to `...`), strips optimizer hints.
/// - When `redact` is `"MARKER"`, wraps literals in `‹...›` (doubling inner `‹`/`›`) instead of
///   replacing them with `?`.
// go: pkg/parser/digester.go Normalize L2 (uses tidb-parser Lexer::tokenize_raw_lossy)
pub fn normalize(sql: &str, redact: &str, tt: &TokenTable) -> String {
    if redact.is_empty() || redact == "OFF" {
        return sql.to_owned();
    }
    let ids = tt.ids();
    let raw = Lexer::new(tt, sql, true).tokenize_raw_lossy();
    let mut tokens: Vec<NormTok> = Vec::with_capacity(raw.len());

    let mut idx = 0;
    while idx < raw.len() {
        let tok = raw[idx];
        // Ignore a single trailing semicolon at the very end of the SQL (digester.go:230).
        if tok.start + 1 == sql.len() && &sql[tok.start..tok.end] == ";" {
            break;
        }
        let raw_slice = &sql[tok.start..tok.end];
        let kind = classify(tok.id, ids);
        let lit = if kind == TokKind::Identifier
            && raw_slice.starts_with('`')
            && raw_slice.ends_with('`')
            && raw_slice.len() >= 2
        {
            raw_slice[1..raw_slice.len() - 1]
                .replace("``", "`")
                .to_lowercase()
        } else if kind == TokKind::StringLit {
            unquote_sql_string(raw_slice).to_lowercase()
        } else {
            raw_slice.to_lowercase()
        };

        let mut curr = NormTok { kind, lit };

        // Strip optimizer hints: /*+ ... */, force/use/ignore index(...), straight_join -> join.
        if curr.kind == TokKind::HintComment {
            idx += 1;
            continue;
        }
        if curr.lit == "index"
            && let Some(prev) = tokens.last()
            && matches!(prev.lit.as_str(), "force" | "use" | "ignore")
        {
            let mut j = idx + 1;
            let mut found = false;
            while j < raw.len() {
                if &sql[raw[j].start..raw[j].end] == ")" {
                    found = true;
                    break;
                }
                j += 1;
            }
            if found {
                tokens.pop();
                idx = j + 1;
                continue;
            }
        }
        if curr.lit == "straight_join" {
            curr.lit = "join".to_owned();
        }

        reduce_lit(&mut tokens, &mut curr, redact);
        tokens.push(curr);
        idx += 1;
    }

    let mut out = String::with_capacity(sql.len() + 16);
    for (i, t) in tokens.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match t.kind {
            TokKind::SingleAt => {
                if !t.lit.starts_with('@') {
                    out.push('@');
                }
                out.push_str(&t.lit);
            }
            TokKind::UnderscoreCs => out.push_str("(_charset)"),
            TokKind::Identifier => {
                out.push('`');
                out.push_str(&t.lit);
                out.push('`');
            }
            _ => out.push_str(&t.lit),
        }
    }
    out
}

fn classify(id: u32, ids: Ids) -> TokKind {
    if id == ids.identifier {
        TokKind::Identifier
    } else if id == ids.single_at {
        TokKind::SingleAt
    } else if id == ids.underscore_cs {
        TokKind::UnderscoreCs
    } else if id == ids.int_lit {
        TokKind::IntLit
    } else if id == ids.dec_lit || id == ids.float_lit || id == ids.hex_lit {
        TokKind::OtherNumLit
    } else if id == ids.string_lit {
        TokKind::StringLit
    } else if id == ids.bit_lit {
        TokKind::BitLit
    } else if id == ids.param_marker {
        TokKind::ParamMarker
    } else if id == ids.null {
        TokKind::Null
    } else if id == ids.hint_comment {
        TokKind::HintComment
    } else {
        TokKind::Other
    }
}

/// Strip outer SQL quotes `'...'` / `"..."` for MARKER mode (Go's scanner produces the unquoted
/// string literal text in `lit`).
fn unquote_sql_string(raw: &str) -> String {
    let bytes = raw.as_bytes();
    if bytes.len() < 2 {
        return raw.to_owned();
    }
    let quote = bytes[0];
    if (quote != b'\'' && quote != b'"') || bytes[bytes.len() - 1] != quote {
        return raw.to_owned();
    }
    let inner = &raw[1..raw.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == quote as char && chars.peek() == Some(&(quote as char)) {
            out.push(c);
            let _ = chars.next();
        } else if c == '\\' {
            if let Some(esc) = chars.next() {
                match esc {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '0' => out.push('\0'),
                    '\\' => out.push('\\'),
                    '\'' => out.push('\''),
                    '"' => out.push('"'),
                    other => out.push(other),
                }
            } else {
                out.push('\\');
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_num_lit(k: TokKind) -> bool {
    matches!(k, TokKind::IntLit | TokKind::OtherNumLit)
}

fn is_lit(t: &NormTok) -> bool {
    is_num_lit(t.kind)
        || matches!(
            t.kind,
            TokKind::StringLit | TokKind::BitLit | TokKind::ParamMarker | TokKind::Null
        )
        || t.lit == "*"
        || (t.kind == TokKind::Identifier && t.lit == "null")
}

fn is_generic_list(tokens: &[NormTok]) -> bool {
    let n = tokens.len();
    if n < 2 {
        return false;
    }
    tokens[n - 1].lit == ","
        && matches!(
            tokens[n - 2].kind,
            TokKind::GenericSymbol | TokKind::GenericSymbolList
        )
}

fn is_generic_list_with_charset(tokens: &[NormTok]) -> usize {
    let n = tokens.len();
    if n < 3 {
        return 0;
    }
    let slice = if n >= 4 {
        &tokens[n - 4..]
    } else {
        &tokens[n - 3..]
    };
    let (to_pop, tail) = if slice.len() == 4 {
        let extra = usize::from(slice[0].kind == TokKind::UnderscoreCs);
        (extra, &slice[1..])
    } else {
        (0, slice)
    };
    if tail[2].kind != TokKind::UnderscoreCs {
        return 0;
    }
    if tail[1].lit != ","
        || !matches!(
            tail[0].kind,
            TokKind::GenericSymbol | TokKind::GenericSymbolList
        )
    {
        return 0;
    }
    to_pop + 3
}

fn is_generic_lists(tokens: &[NormTok]) -> bool {
    let n = tokens.len();
    if n < 4 {
        return false;
    }
    let last4 = &tokens[n - 4..];
    matches!(
        last4[0].kind,
        TokKind::GenericSymbol | TokKind::GenericSymbolList
    ) && last4[1].lit == ")"
        && last4[2].lit == ","
        && last4[3].lit == "("
}

fn is_prefix_by_unary(tokens: &[NormTok], curr_kind: TokKind) -> bool {
    if !is_num_lit(curr_kind) {
        return false;
    }
    let n = tokens.len();
    if n == 0 {
        return false;
    }
    if tokens[n - 1].lit != "-" && tokens[n - 1].lit != "+" {
        return false;
    }
    if n == 1 {
        return true;
    }
    matches!(
        tokens[n - 2].lit.as_str(),
        "(" | "," | "+" | "-" | ">=" | "is" | "<=" | "=" | "<" | ">" | "select"
    )
}

fn is_order_or_group_by(tokens: &[NormTok]) -> bool {
    let mut back = 2usize;
    loop {
        if tokens.len() < back {
            return false;
        }
        let slice = &tokens[tokens.len() - back..];
        if slice[1].lit != "," {
            let pair = if slice[1].lit == "(" {
                if tokens.len() < back + 1 {
                    return false;
                }
                &tokens[tokens.len() - (back + 1)..]
            } else {
                slice
            };
            return (pair[0].lit == "order" || pair[0].lit == "group") && pair[1].lit == "by";
        }
        back += 2;
    }
}

fn reduce_lit(tokens: &mut Vec<NormTok>, curr: &mut NormTok, redact: &str) {
    if !is_lit(curr) {
        return;
    }
    if redact == "MARKER" {
        if curr.lit == "?" || curr.lit == "*" {
            return;
        }
        let mut marked = String::with_capacity(curr.lit.len() + 6);
        marked.push('‹');
        for ch in curr.lit.chars() {
            if ch == '‹' || ch == '›' {
                marked.push(ch);
                marked.push(ch);
            } else {
                marked.push(ch);
            }
        }
        marked.push('›');
        curr.lit = marked;
        return;
    }

    // count(*) => count(?)
    if curr.lit == "*" {
        if tokens.last().is_some_and(|t| t.lit == "(") {
            curr.kind = TokKind::GenericSymbol;
            curr.lit = "?".to_owned();
        }
        return;
    }

    // -x / +x => x
    if is_prefix_by_unary(tokens, curr.kind) {
        tokens.pop();
    }

    // ?, ? => ...
    if is_generic_list(tokens) {
        tokens.truncate(tokens.len() - 2);
        curr.kind = TokKind::GenericSymbolList;
        curr.lit = "...".to_owned();
        return;
    }

    let to_pop = is_generic_list_with_charset(tokens);
    if to_pop != 0 {
        tokens.truncate(tokens.len() - to_pop);
        curr.kind = TokKind::GenericSymbolList;
        curr.lit = "...".to_owned();
        return;
    }

    if is_generic_lists(tokens) {
        tokens.truncate(tokens.len() - 4);
        curr.kind = TokKind::GenericSymbolList;
        curr.lit = "...".to_owned();
        return;
    }

    // order by 1, 2 => keep integer
    if curr.kind == TokKind::IntLit && is_order_or_group_by(tokens) {
        return;
    }

    curr.kind = TokKind::GenericSymbol;
    curr.lit = "?".to_owned();
}
