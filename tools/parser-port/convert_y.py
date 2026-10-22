#!/usr/bin/env python3
"""Convert TiDB's goyacc grammar (parser.y) into a grmtools-compatible .y.

Usage: python3 -I tools/parser-port/convert_y.py <parser.y> <out.y> <report.json>
(convert_typed.py imports its grammar tokenizer; the untyped output is only a debugging aid.)

What it does (mechanically):
  * drops the Go prologue (%{ ... %}) and %union { ... }
  * %token <type> name "alias" ...   -> %token name ...   (alias table kept for rules)
  * %type <type> ...                 -> dropped (NoAction grammar has no value types)
  * %precedence                      -> %nonassoc (grmtools has no %precedence; compare_tables.py checks the result)
  * %left/%right/%nonassoc           -> kept, one directive per line, aliases resolved
  * rules: Go actions stripped, "ALIAS" literals in rules resolved to token names,
           every rule terminated with ';', %prec moved before (stripped) actions
  * goyacc-only comments (/*yy:token ...*/) dropped
Anything that cannot be translated is recorded in the JSON report.
"""
import json
import re
import sys
from collections import Counter, OrderedDict

IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_.]*")


class Tok:
    __slots__ = ("kind", "val", "line")

    def __init__(self, kind, val, line):
        self.kind, self.val, self.line = kind, val, line

    def __repr__(self):
        return f"{self.kind}:{self.val}@{self.line}"


def skip_go_string(src, i):
    """src[i] is a quote char of a Go literal; return index after it."""
    q = src[i]
    j = i + 1
    if q == "`":
        return src.index("`", j) + 1
    while True:
        c = src[j]
        if c == "\\":
            j += 2
            continue
        if c == q:
            return j + 1
        if c == "\n":
            raise ValueError(f"unterminated Go literal at {i}")
        j += 1


def scan_action(src, i):
    """src[i] == '{'. Return (end_index, text) of a balanced Go action."""
    depth = 0
    j = i
    n = len(src)
    while j < n:
        c = src[j]
        if c in "\"'`":
            j = skip_go_string(src, j)
            continue
        if src.startswith("//", j):
            j = src.index("\n", j)
            continue
        if src.startswith("/*", j):
            j = src.index("*/", j) + 2
            continue
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return j + 1, src[i : j + 1]
        j += 1
    raise ValueError("unterminated action")


def tokenize(src, base_line=1):
    toks = []
    i, n = 0, len(src)
    line = base_line
    while i < n:
        c = src[i]
        if c == "\n":
            line += 1
            i += 1
            continue
        if c.isspace():
            i += 1
            continue
        if src.startswith("//", i):
            i = src.find("\n", i)
            i = n if i < 0 else i
            continue
        if src.startswith("/*", i):
            j = src.index("*/", i) + 2
            line += src.count("\n", i, j)
            i = j
            continue
        if c == "{":
            j, text = scan_action(src, i)
            toks.append(Tok("action", text, line))
            line += text.count("\n")
            i = j
            continue
        if c == '"':
            j = skip_go_string(src, i)
            toks.append(Tok("alias", src[i + 1 : j - 1], line))
            i = j
            continue
        if c == "'":
            # yacc char literal, possibly escaped: '\\' or '\''
            j = i + 1
            if src[j] == "\\":
                j += 2
            else:
                j += 1
            if src[j] != "'":
                raise ValueError(f"bad char literal at line {line}: {src[i:i+6]!r}")
            toks.append(Tok("char", src[i : j + 1], line))
            i = j + 1
            continue
        if c == "<":
            j = src.index(">", i)
            toks.append(Tok("type", src[i + 1 : j], line))
            i = j + 1
            continue
        if c == "%":
            m = re.compile(r"%(%|[A-Za-z_-]+)").match(src, i)
            toks.append(Tok("directive", m.group(0), line))
            i = m.end()
            continue
        if c in ":|;":
            toks.append(Tok(c, c, line))
            i += 1
            continue
        m = IDENT_RE.match(src, i)
        if m:
            toks.append(Tok("ident", m.group(0), line))
            i = m.end()
            continue
        raise ValueError(f"unexpected char {c!r} at line {line}")
    return toks


def strip_block(src, opener):
    """Remove `opener { ... }` (balanced) from src; return (src, removed_text)."""
    k = src.find(opener)
    if k < 0:
        return src, None
    b = src.index("{", k)
    end, text = scan_action(src, b)
    return src[:k] + "\n" * src.count("\n", k, end) + src[end:], text


def convert(src):
    report = OrderedDict()
    issues = []  # non-mechanical / lossy constructs
    counts = Counter()

    # ---- split sections ---------------------------------------------------
    # prologue %{ ... %}
    pro_s = src.index("%{")
    pro_e = src.index("%}", pro_s)
    counts["go_prologue_lines"] = src.count("\n", pro_s, pro_e)
    src_no_pro = src[:pro_s] + "\n" * src.count("\n", pro_s, pro_e + 2) + src[pro_e + 2 :]
    src_no_pro, union_text = strip_block(src_no_pro, "%union")
    report["union_fields"] = [l.strip() for l in (union_text or "").strip("{}").splitlines() if l.strip()]
    parts = re.split(r"^%%[ \t]*$", src_no_pro, flags=re.M)
    if len(parts) < 2:
        raise SystemExit("no %% separator")
    decl_src, rules_src = parts[0], parts[1]
    epilogue = parts[2] if len(parts) > 2 else ""
    counts["epilogue_nonblank_lines"] = sum(1 for l in epilogue.splitlines() if l.strip())
    rules_base_line = decl_src.count("\n") + 2
    counts["goyacc_yy_comment_annotations"] = len(re.findall(r"/\*yy:", src))

    # ---- declarations --------------------------------------------------------
    dtoks = tokenize(decl_src)
    tokens = OrderedDict()  # name -> alias or None
    token_types = Counter()
    alias_to_tok = {}
    nonterm_types = {}
    nonterm_alias = {}
    prec_lines = []  # (kind, [syms])
    start = None
    i = 0
    cur = None
    cur_type = None
    while i < len(dtoks):
        t = dtoks[i]
        if t.kind == "directive":
            cur = t.val
            cur_type = None
            counts["directive " + cur] += 1
            if cur in ("%left", "%right", "%nonassoc", "%precedence"):
                prec_lines.append([cur, []])
            i += 1
            continue
        if t.kind == "type":
            cur_type = t.val
            i += 1
            continue
        if cur == "%token":
            if t.kind == "ident":
                alias = None
                if i + 1 < len(dtoks) and dtoks[i + 1].kind == "alias":
                    alias = dtoks[i + 1].val
                    i += 1
                tokens[t.val] = alias
                token_types[cur_type or "<none>"] += 1
                if alias is not None:
                    if alias in alias_to_tok:
                        issues.append({"kind": "duplicate_token_alias", "alias": alias,
                                       "tokens": [alias_to_tok[alias], t.val]})
                    alias_to_tok[alias] = t.val
            elif t.kind == "char":
                tokens[t.val] = None
            else:
                raise ValueError(f"unexpected {t} in %token")
        elif cur == "%type":
            if t.kind == "ident":
                nonterm_types[t.val] = cur_type
                if i + 1 < len(dtoks) and dtoks[i + 1].kind == "alias":
                    nonterm_alias[t.val] = dtoks[i + 1].val
                    i += 1
            else:
                raise ValueError(f"unexpected {t} in %type")
        elif cur in ("%left", "%right", "%nonassoc", "%precedence"):
            if t.kind == "alias":
                prec_lines[-1][1].append(alias_to_tok[t.val])
            else:
                prec_lines[-1][1].append(t.val)
        elif cur == "%start":
            start = t.val
        else:
            issues.append({"kind": "unknown_declaration", "directive": cur, "tok": repr(t)})
        i += 1

    # ---- rules -----------------------------------------------------------------
    rtoks = tokenize(rules_src, rules_base_line)
    rules = OrderedDict()  # lhs -> list of alts; alt = dict(syms, prec, n_actions)
    i = 0
    lhs = None
    alt = None

    def new_alt():
        return {"syms": [], "prec": None, "actions": 0, "line": None, "mid_action": False,
                "prec_after_action": False}

    def close_alt():
        if lhs is not None and alt is not None:
            rules[lhs].append(alt)

    while i < len(rtoks):
        t = rtoks[i]
        nxt = rtoks[i + 1] if i + 1 < len(rtoks) else None
        if t.kind == "ident" and nxt is not None and nxt.kind == ":":
            close_alt()
            lhs = t.val
            if lhs in rules:
                counts["rule_lhs_split_across_definitions"] += 1
            rules.setdefault(lhs, [])
            alt = new_alt()
            alt["line"] = t.line
            i += 2
            continue
        if t.kind == "|":
            close_alt()
            alt = new_alt()
            alt["line"] = t.line
        elif t.kind == ";":
            close_alt()
            alt = None
            lhs = None
            counts["explicit_semicolons"] += 1
        elif t.kind == "action":
            alt["actions"] += 1
            counts["actions_stripped"] += 1
        elif t.kind == "directive" and t.val == "%prec":
            p = rtoks[i + 1]
            pname = alias_to_tok.get(p.val, p.val) if p.kind == "alias" else p.val
            alt["prec"] = pname
            if alt["actions"]:
                alt["prec_after_action"] = True
            i += 2
            continue
        elif t.kind == "directive":
            issues.append({"kind": "unknown_rule_directive", "tok": repr(t)})
        elif t.kind in ("ident", "char", "alias"):
            if alt["actions"]:
                alt["mid_action"] = True
            if t.kind == "alias":
                name = alias_to_tok.get(t.val)
                if name is None:
                    issues.append({"kind": "unknown_alias_in_rule", "alias": t.val, "line": t.line})
                    name = t.val
                counts["alias_literals_resolved"] += 1
                alt["syms"].append(name)
            else:
                alt["syms"].append(t.val)
        else:
            issues.append({"kind": "unexpected_rule_token", "tok": repr(t)})
        i += 1
    close_alt()

    # ---- analysis ------------------------------------------------------------
    used_syms = set()
    for alts in rules.values():
        for a in alts:
            used_syms.update(a["syms"])
    prec_names = [s for _, syms in prec_lines for s in syms]
    prec_only = [s for s in prec_names if s not in tokens and s not in rules]
    undefined = sorted(s for s in used_syms
                       if s not in tokens and s not in rules and not s.startswith("'"))
    if undefined:
        issues.append({"kind": "undefined_symbols", "syms": undefined})
    mid_actions = [(l, a["line"]) for l, alts in rules.items() for a in alts if a["mid_action"]]
    prec_after = [(l, a["line"]) for l, alts in rules.items() for a in alts if a["prec_after_action"]]
    if mid_actions:
        issues.append({"kind": "mid_rule_actions", "count": len(mid_actions), "where": mid_actions[:20]})
    unused_tokens = [t for t in tokens if t not in used_syms and t not in prec_names]
    unreachable = sorted(set(rules) - used_syms - {start})
    nonterm_alias_used_in_rules = sorted(set(nonterm_alias.values()) & {a for a in alias_to_tok})

    # ---- emit ----------------------------------------------------------------
    out = []
    out.append("%grmtools {yacckind: Original(NoAction), recoverer: RecoveryKind::None}")
    out.append("// AUTO-GENERATED by tools/parser-port/convert_y.py from TiDB parser.y. Do not edit.")
    out.append(f"%start {start}")
    # declare every terminal (real + precedence-only pseudo tokens) so unquoted names
    # are classified as tokens by cfgrammar.
    decl = [t for t in tokens if not t.startswith("'")] + [p for p in prec_only if not p.startswith("'")]
    for k in range(0, len(decl), 12):
        out.append("%token " + " ".join(decl[k : k + 12]))
    for kind, syms in prec_lines:
        k2 = "%nonassoc" if kind == "%precedence" else kind
        if kind == "%precedence":
            counts["precedence_to_nonassoc"] += 1
        out.append(f"{k2} " + " ".join(syms))
    out.append("%%")
    for lhs_name, alts in rules.items():
        out.append(f"{lhs_name}:")
        for n_alt, a in enumerate(alts):
            lead = "    " if n_alt == 0 else "  | "
            body = " ".join(a["syms"]) if a["syms"] else "/* empty */"
            if a["prec"]:
                body += f" %prec {a['prec']}"
            out.append(lead + body)
        out.append("  ;")
    out.append("")

    report["counts"] = dict(sorted(counts.items()))
    report["terminals_declared"] = len(tokens)
    report["terminal_type_tags"] = dict(token_types)
    report["precedence_only_pseudo_tokens"] = prec_only
    report["nonterminals"] = len(rules)
    report["productions"] = sum(len(a) for a in rules.values())
    report["nonterminal_type_tags"] = dict(Counter(nonterm_types.values()))
    report["precedence_levels"] = len(prec_lines)
    report["prec_after_action_moved"] = prec_after
    report["unused_tokens"] = unused_tokens
    report["unreachable_nonterminals"] = unreachable
    report["nonterminal_aliases_colliding_with_token_aliases"] = nonterm_alias_used_in_rules
    report["issues"] = issues
    keyword_alias = {name: alias for name, alias in tokens.items() if alias}
    return "\n".join(out), report, keyword_alias


def main():
    inp, outp, repp = sys.argv[1:4]
    src = open(inp, encoding="utf-8").read()
    out, report, alias_map = convert(src)
    with open(outp, "w", encoding="utf-8") as f:
        f.write(out)
    with open(repp, "w", encoding="utf-8") as f:
        json.dump(report, f, indent=1)
    print(json.dumps({k: v for k, v in report.items()
                      if k not in ("unused_tokens", "precedence_only_pseudo_tokens",
                                   "unreachable_nonterminals", "union_fields")}, indent=1))


if __name__ == "__main__":
    main()
