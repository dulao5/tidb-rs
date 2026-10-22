#!/usr/bin/env python3
"""Convert TiDB parser.y into a *typed* grmtools grammar (YaccKind::Grmtools).

Usage: python3 -I tools/parser-port/convert_typed.py <parser.y> <out.y> <out_types.rs> <report.json>
(normally run through tools/parser-port/regen.sh)

Per nonterminal a Rust type is inferred from (in order):
  1. goyacc %type tag: <expr> -> Expr, <statement> -> Stmt, <ident> -> String
  2. for <item>: consumer assertions `$n.(T)` (majority vote), then the nonterminal's own
     `$$ = &ast.X{...}` / `[]*ast.X{` / literal shapes, then passthrough propagation,
     else the catch-all `Item` (= Box<dyn Any>)
  3. nullability (`$$ = nil` or consumers testing `$n != nil`) wraps the type in Option<>
Actions emitted:
  * hand-ported Rust (handport.py)                                    -> "ported"
  * no action / `{ $$ = $k }` with compatible types                    -> passthrough
  * `{ $$ = nil }`                                                     -> None / Vec::new() / default
  * everything else -> placeholder `ph("Rule#alt", $span, vec![children...])` -> "placeholder"
Also writes Rust stubs for every AST type referenced but not hand-written in
crates/parser/src/ast.rs.
"""
import json
import os
import re
import sys
from collections import Counter, OrderedDict, defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from convert_y import tokenize, strip_block  # noqa: E402
import handport  # noqa: E402

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CRATE = os.path.join(REPO, "crates", "parser")
# Provenance line written into every generated file (set by regen.sh).
SOURCE = os.environ.get("PARSER_PORT_SOURCE", "TiDB (unknown commit)")
REGEN = "Regenerate with: tools/parser-port/regen.sh <tidb-checkout>"

PRIMS = {"bool": "bool", "int": "i64", "int64": "i64", "int32": "i32", "uint64": "u64",
         "uint": "u64", "uint32": "u32", "uint16": "u16", "uint8": "u8", "byte": "u8",
         "float64": "f64", "string": "String", "rune": "char"}
INTERFACES = {"ast.ExprNode": "Expr", "ast.StmtNode": "Stmt", "ast.ResultSetNode": "ResultSetNode"}


def hand_written_types():
    src = open(os.path.join(CRATE, "src", "ast.rs")).read()
    return set(re.findall(r"pub (?:struct|enum|type) (\w+)", src))


class TypeMapper:
    def __init__(self, hand):
        self.hand = hand
        self.stubs = OrderedDict()  # rust name -> go name

    def stub(self, go):
        pkg, _, name = go.rpartition(".")
        rust = name if pkg in ("ast", "") else pkg.capitalize() + name
        rust = re.sub(r"\W", "", rust)
        rust = rust[0].upper() + rust[1:]
        if rust in ("Node", "Item", "Tok", "Span", "Any", "Box", "Vec", "Option", "String", "Ctx", "R"):
            rust = "Ast" + rust
        if rust not in self.hand and rust not in self.stubs.values():
            self.stubs[rust] = go
        return rust

    def map(self, go):
        """Go type -> Rust type, or None if it should go to the catch-all."""
        go = go.strip()
        if go in INTERFACES:
            return INTERFACES[go]
        if go in PRIMS:
            return PRIMS[go]
        if go.startswith("[]"):
            inner = self.map_elem(go[2:])
            return f"Vec<{inner}>" if inner else None
        if go.startswith("*"):
            inner = go[1:]
            if inner in INTERFACES or inner in PRIMS:
                return None
            if re.fullmatch(r"[\w.]+", inner):
                return f"Box<{self.stub(inner)}>"
            return None
        if go == "opcode.Op":
            return "Op"
        if re.fullmatch(r"[\w]+\.[\w]+", go) or re.fullmatch(r"[a-z]\w*", go):
            return self.stub(go)
        return None  # map[...], func(...), interface{}, etc.

    def map_elem(self, go):
        go = go.strip()
        if go.startswith("*") and re.fullmatch(r"[\w.]+", go[1:]) and go[1:] not in INTERFACES:
            return self.stub(go[1:])
        return self.map(go)


def parse(src):
    pro_s = src.index("%{")
    pro_e = src.index("%}", pro_s)
    src = src[:pro_s] + "\n" * src.count("\n", pro_s, pro_e + 2) + src[pro_e + 2:]
    src, _ = strip_block(src, "%union")
    parts = re.split(r"^%%[ \t]*$", src, flags=re.M)
    decl_src, rules_src = parts[0], parts[1]
    dtoks = tokenize(decl_src)
    tokens = OrderedDict()
    token_tag = {}
    alias_to_tok = {}
    nt_tag = {}
    prec_lines = []
    start = None
    cur = cur_type = None
    i = 0
    while i < len(dtoks):
        t = dtoks[i]
        if t.kind == "directive":
            cur, cur_type = t.val, None
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
                if i + 1 < len(dtoks) and dtoks[i + 1].kind == "alias":
                    alias_to_tok[dtoks[i + 1].val] = t.val
                    i += 1
                tokens[t.val] = True
                token_tag[t.val] = cur_type
            elif t.kind == "char":
                tokens[t.val] = True
        elif cur == "%type":
            if t.kind == "ident":
                nt_tag[t.val] = cur_type
                if i + 1 < len(dtoks) and dtoks[i + 1].kind == "alias":
                    i += 1
        elif cur in ("%left", "%right", "%nonassoc", "%precedence"):
            prec_lines[-1][1].append(alias_to_tok[t.val] if t.kind == "alias" else t.val)
        elif cur == "%start":
            start = t.val
        i += 1

    rtoks = tokenize(rules_src, decl_src.count("\n") + 2)
    rules = OrderedDict()
    lhs = alt = None

    def close():
        if lhs is not None and alt is not None:
            rules[lhs].append(alt)

    i = 0
    while i < len(rtoks):
        t = rtoks[i]
        nxt = rtoks[i + 1] if i + 1 < len(rtoks) else None
        if t.kind == "ident" and nxt is not None and nxt.kind == ":":
            close()
            lhs = t.val
            rules.setdefault(lhs, [])
            alt = {"syms": [], "prec": None, "action": None, "line": t.line}
            i += 2
            continue
        if t.kind == "|":
            close()
            alt = {"syms": [], "prec": None, "action": None, "line": t.line}
        elif t.kind == ";":
            close()
            alt = lhs = None
        elif t.kind == "action":
            alt["action"] = t.val
        elif t.kind == "directive" and t.val == "%prec":
            p = rtoks[i + 1]
            alt["prec"] = alias_to_tok.get(p.val, p.val) if p.kind == "alias" else p.val
            i += 2
            continue
        elif t.kind == "alias":
            alt["syms"].append(alias_to_tok[t.val])
        elif t.kind in ("ident", "char"):
            alt["syms"].append(t.val)
        i += 1
    close()
    prec_names = [s for _, syms in prec_lines for s in syms]
    prec_only = [s for s in prec_names if s not in tokens and s not in rules]
    return dict(tokens=tokens, token_tag=token_tag, nt_tag=nt_tag, prec_lines=prec_lines,
                prec_only=prec_only, start=start, rules=rules)


ASSERT_RE = re.compile(r"\$(\d+)\.\(([^()]*?(?:\{\})?)\)")
NILCHK_RE = re.compile(r"\$(\d+)\s*[!=]=\s*nil")
PASS_RE = re.compile(r"\{\s*\$\$\s*=\s*\$(\d+)\s*\}")
NIL_RE = re.compile(r"\{\s*\$\$\s*=\s*nil\s*\}")
EMPTY_LIST_RE = re.compile(r"\{\s*\$\$\s*=\s*\[\][\w.*]+\{\}\s*\}|\{\s*var (\w+) \[\][\w.*]+\s*\$\$\s*=\s*\1\s*\}")
BOOL_RE = re.compile(r"\{\s*\$\$\s*=\s*(true|false)\s*\}")


def infer(g, tm):
    rules, nt_tag = g["rules"], g["nt_tag"]
    is_nt = lambda s: s in rules  # noqa: E731
    votes = defaultdict(Counter)
    nullable = set()
    for lhs, alts in rules.items():
        for a in alts:
            act = a["action"] or ""
            for k, ty in ASSERT_RE.findall(act):
                k = int(k)
                if ty == "type" or k > len(a["syms"]):
                    continue
                s = a["syms"][k - 1]
                if is_nt(s):
                    votes[s][ty] += 1
            for k in NILCHK_RE.findall(act):
                k = int(k)
                if k <= len(a["syms"]) and is_nt(a["syms"][k - 1]):
                    nullable.add(a["syms"][k - 1])
            if re.search(r"\$\$\s*=\s*nil\b", act):
                nullable.add(lhs)
    go_type, source = {}, {}
    for nt in rules:
        tag = nt_tag.get(nt)
        if tag == "expr":
            go_type[nt], source[nt] = "ast.ExprNode", "tag"
        elif tag == "statement":
            go_type[nt], source[nt] = "ast.StmtNode", "tag"
        elif tag == "ident":
            go_type[nt], source[nt] = "string", "tag"
        elif tag is None:
            go_type[nt], source[nt] = None, "untyped"
        else:
            if votes[nt]:
                ty, _ = votes[nt].most_common(1)[0]
                go_type[nt] = ty
                source[nt] = "assert" if len(votes[nt]) == 1 else "assert_majority"
                continue
            own = Counter()
            for a in rules[nt]:
                act = a["action"] or ""
                for m in re.finditer(r"\$\$\s*=\s*(.*)", act):
                    rhs = m.group(1).strip()
                    mm = re.match(r"&ast\.(\w+)\{", rhs)
                    if mm:
                        own["*ast." + mm.group(1)] += 1
                    elif re.match(r"\[\](\*?[\w.]+)\{", rhs):
                        own[re.match(r"(\[\]\*?[\w.]+)\{", rhs).group(1)] += 1
                    elif re.match(r"(true|false)\b", rhs):
                        own["bool"] += 1
                    elif re.match(r'"', rhs):
                        own["string"] += 1
                    elif re.match(r"ast\.NewValueExpr|ast\.NewParamMarkerExpr", rhs):
                        own["ast.ExprNode"] += 1
                    elif re.match(r"ast\.NewCIStr\(", rhs) or re.match(r"ast\.CIStr\{", rhs):
                        own["ast.CIStr"] += 1
                    elif re.match(r"\d+$", rhs):
                        own["int"] += 1
            if own:
                go_type[nt], source[nt] = own.most_common(1)[0][0], "own_action"
            else:
                go_type[nt], source[nt] = "?", "unknown"
    # passthrough propagation for unknown item types
    changed = True
    while changed:
        changed = False
        for nt in rules:
            if go_type[nt] != "?":
                continue
            srcs = set()
            for a in rules[nt]:
                k = None
                if a["action"] is None and a["syms"]:
                    k = 1
                elif a["action"] and PASS_RE.fullmatch(a["action"].strip()):
                    k = int(PASS_RE.fullmatch(a["action"].strip()).group(1))
                if k and is_nt(a["syms"][k - 1]) and go_type[a["syms"][k - 1]] not in ("?", None):
                    srcs.add(go_type[a["syms"][k - 1]])
            if len(srcs) == 1:
                go_type[nt], source[nt] = srcs.pop(), "passthrough"
                changed = True
    rtype = {}
    for nt in rules:
        gt = go_type[nt]
        if nt in handport.TYPES:
            rtype[nt] = handport.TYPES[nt]
            source[nt] = "handport"
            continue
        if gt is None:
            rtype[nt] = "()"
            continue
        rt = tm.map(gt) if gt != "?" else None
        if rt is None:
            rt = "Item"
            if source[nt] != "unknown":
                source[nt] = "catch_all(" + source[nt] + ")"
            else:
                source[nt] = "catch_all(unknown)"
        if nt in nullable and not rt.startswith("Vec<") and rt != "Item":
            rt = f"Option<{rt}>"
        rtype[nt] = rt
    return rtype, source, go_type


def default_for(t):
    if t.startswith("Option<"):
        return "None"
    if t.startswith("Vec<"):
        return "Vec::new()"
    if t == "()":
        return "()"
    if t == "Item":
        return "Box::new(()) as Item"
    return "Default::default()"


def main():
    inp, outp, out_types, repp = sys.argv[1:5]
    g = parse(open(inp, encoding="utf-8").read())
    tm = TypeMapper(hand_written_types())
    rtype, source, go_type = infer(g, tm)
    rules = g["rules"]
    tokens = g["tokens"]
    stats = Counter()
    mism = Counter()

    def symtype(s):
        return rtype[s] if s in rules else "TOKEN"

    def placeholder(lhs, i, a):
        kids = [f"Box::new(${k + 1}) as Box<dyn Any>" for k, s in enumerate(a["syms"])
                if s in rules and rtype[s] != "()"]
        return f'ph("{lhs}#{i}", $span, vec![{", ".join(kids)}])'

    def conv(k, a, D):
        s = a["syms"][k - 1]
        S = symtype(s)
        if S == "TOKEN":
            if D == "String":
                return f"tok_text($lexer, &${k})"
            if D == "Option<String>":
                return f"Some(tok_text($lexer, &${k}))"
            return None
        if S == D:
            return f"${k}"
        if D == f"Option<{S}>":
            return f"Some(${k})"
        if D == "Item":
            return f"Box::new(${k}) as Item"
        return None

    out = ["%grmtools {yacckind: Grmtools, recoverer: RecoveryKind::None}",
           f"// GENERATED by tools/parser-port/convert_typed.py from pkg/parser/parser.y of {SOURCE}.",
           f"// Do not edit by hand. {REGEN}",
           f"%start {g['start']}",
           "%parse-param p: &'input crate::ctx::Ctx"]
    decl = [t for t in tokens if not t.startswith("'")] + [p for p in g["prec_only"] if not p.startswith("'")]
    for k in range(0, len(decl), 12):
        out.append("%token " + " ".join(decl[k:k + 12]))
    for kind, syms in g["prec_lines"]:
        out.append(("%nonassoc" if kind == "%precedence" else kind) + " " + " ".join(syms))
    out.append("%%")
    for lhs, alts in rules.items():
        D = rtype[lhs]
        out.append(f"{lhs} -> {D}:")
        for i, a in enumerate(alts):
            act = a["action"]
            key = (lhs, " ".join(a["syms"]))
            if key in handport.ACTIONS:
                code = handport.ACTIONS[key].strip()
                stats["ported"] += 1
            elif D == "()":
                code = "()"
                stats["unit"] += 1
            elif act is None and a["syms"]:
                code = conv(1, a, D)
                if code is None:
                    mism[(symtype(a["syms"][0]), D)] += 1
                    code = placeholder(lhs, i, a)
                    stats["implicit_passthrough_type_mismatch->placeholder"] += 1
                else:
                    stats["implicit_passthrough"] += 1
            elif act is None:
                code = default_for(D)
                stats["empty_no_action->default"] += 1
            elif PASS_RE.fullmatch(act.strip()):
                k = int(PASS_RE.fullmatch(act.strip()).group(1))
                code = conv(k, a, D)
                if code is None:
                    mism[(symtype(a["syms"][k - 1]), D)] += 1
                    code = placeholder(lhs, i, a)
                    stats["explicit_passthrough_type_mismatch->placeholder"] += 1
                else:
                    stats["explicit_passthrough"] += 1
            elif EMPTY_LIST_RE.fullmatch(act.strip()) and D.startswith("Vec<"):
                code = "Vec::new()"
                stats["empty_list"] += 1
            elif NIL_RE.fullmatch(act.strip()):
                code = default_for(D)
                stats["nil"] += 1
            elif BOOL_RE.fullmatch(act.strip()) and D in ("bool", "Option<bool>"):
                v = BOOL_RE.fullmatch(act.strip()).group(1)
                code = v if D == "bool" else f"Some({v})"
                stats["bool_const"] += 1
            else:
                code = placeholder(lhs, i, a)
                stats["placeholder"] += 1
            lead = "    " if i == 0 else "  | "
            body = " ".join(a["syms"])
            if a["prec"]:
                body += f" %prec {a['prec']}"
            out.append(f"{lead}{body} {{ {code} }}")
        out.append("  ;")
    used = {(l, " ".join(a["syms"])) for l, alts in rules.items() for a in alts}
    unknown = [k for k in handport.ACTIONS if k not in used]
    if unknown:
        raise SystemExit(f"handport keys not found in grammar: {unknown}")
    out.append("%%")
    out.append("#[allow(unused_imports)] use crate::ast::*;")
    out.append("#[allow(unused_imports)] use crate::gen_types::*;")
    out.append("#[allow(unused_imports)] use crate::actions_rt::*;")
    out.append("#[allow(unused_imports)] use std::any::Any;")
    out.append(handport.PROGRAMS)
    with open(outp, "w", encoding="utf-8") as f:
        f.write("\n".join(out) + "\n")

    ts = [f"// GENERATED by tools/parser-port/convert_typed.py from pkg/parser/parser.y of {SOURCE}.",
          f"// Do not edit by hand. {REGEN}",
          "//",
          "// Stub Rust types for Go AST types that are referenced by the grammar but not",
          "// hand-written in src/ast.rs. Each wraps the placeholder node of an unported action.",
          "use crate::actions_rt::{FromNode, Node};", ""]
    for rust, go in tm.stubs.items():
        ts.append(f"/// Go: {go}")
        ts.append(f"#[derive(Debug, Default)] pub struct {rust}(pub Option<Box<Node>>);")
        ts.append(f"impl FromNode for {rust} {{ fn from_node(n: Node) -> Self {{ {rust}(Some(Box::new(n))) }} }}")
    with open(out_types, "w", encoding="utf-8") as f:
        f.write("\n".join(ts) + "\n")

    def bucket(t):
        if t == "()":
            return "unit (untyped in goyacc)"
        if t == "Item":
            return "catch-all Item (Box<dyn Any>)"
        base = t[7:-1] if t.startswith("Option<") else t
        if base in ("Expr", "Stmt", "ResultSetNode"):
            return "AST enum (Expr/Stmt/ResultSetNode)"
        if base.startswith("Vec<"):
            return "Vec<_>"
        if base.startswith("Box<"):
            return "Box<concrete AST struct>"
        if base in ("bool", "i64", "u64", "i32", "u32", "u16", "u8", "f64", "char"):
            return "primitive"
        if base == "String":
            return "String"
        return "value struct / enum (CIStr, Op, JoinType, ...)"

    report = OrderedDict()
    report["_generated"] = f"by tools/parser-port/convert_typed.py from pkg/parser/parser.y of {SOURCE}. {REGEN}"
    report["nonterminals"] = len(rules)
    report["productions"] = sum(len(a) for a in rules.values())
    report["distinct_rust_types"] = len(set(rtype.values()))
    report["type_buckets"] = dict(Counter(bucket(t) for t in rtype.values()).most_common())
    report["type_source"] = dict(Counter(re.sub(r"\(.*", "", s) for s in source.values()).most_common())
    report["catch_all_reasons"] = dict(Counter(s for s in source.values() if s.startswith("catch_all")).most_common())
    report["option_wrapped"] = sum(1 for t in rtype.values() if t.startswith("Option<"))
    report["action_kinds"] = dict(stats.most_common())
    report["passthrough_mismatch_pairs"] = {f"{a} -> {b}": n for (a, b), n in mism.most_common(25)}
    report["stub_types_generated"] = len(tm.stubs)
    report["catch_all_nonterminals"] = sorted(nt for nt, t in rtype.items() if t == "Item")
    report["types"] = {nt: [rtype[nt], go_type[nt], source[nt]] for nt in rules}
    with open(repp, "w", encoding="utf-8") as f:
        json.dump(report, f, indent=1)
        f.write("\n")
    print(json.dumps({k: v for k, v in report.items() if k not in ("types", "catch_all_nonterminals")}, indent=1))


if __name__ == "__main__":
    main()
