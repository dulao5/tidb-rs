#!/usr/bin/env python3
"""Generate the list of scalar function signatures Go TiDB v8.5.8 pushes down to TiKV
(the denominator of the coprocessor evaluation oracle, crates/query-smoke/tests/copr_eval_oracle.rs).

    tools/expr-gen/gen_tikv_pushable_sigs.py <tidb-checkout>

Reads, from TiDB v8.5.8:

  pkg/expression/infer_pushdown.go   the function names of `scalarExprSupportedByTiKV`, grouped
                                     by the comment above each group ("op functions", ...);
  pkg/parser/ast/functions.go        the names behind the `ast.X` constants;
  pkg/expression/builtin.go          `funcs`: function name -> function class;
  pkg/expression/builtin_*.go        the pb codes (`tipb.ScalarFuncSig_X`) each function class's
                                     methods set (getFunction and its helpers);

and writes crates/query-smoke/tests/data/tikv_pushable_sigs.tsv: one row per (function name,
pb code) Go may push to TiKV. CAST is not in `funcs`: its rows are the pb codes of the
`castAs*FunctionClass` classes. The name-level conditions of `scalarExprSupportedByTiKV` are
applied where they depend only on the pb code (ROUND, RAND); CONV and the REGEXP functions keep
all their codes and are marked `conditional` (the condition depends on the arguments).

A class that sets no pb code is listed with `Unspecified` (never pushed: canScalarFuncPushDown
rejects it). A class shared by several names (e.g. `compareFunctionClass`) lists all its codes under each
name; the oracle counts distinct pb codes. Rerun when the baseline changes.
"""

import os
import re
import sys

tidb = sys.argv[1]
here = os.path.dirname(os.path.abspath(__file__))
root = os.path.join(here, "..", "..")
out = os.path.join(root, "crates", "query-smoke", "tests", "data", "tikv_pushable_sigs.tsv")
expr_dir = os.path.join(tidb, "pkg", "expression")


def read(*p):
    return open(os.path.join(tidb, *p), encoding="utf-8").read()


# ast.X -> "name"
ast_names = dict(re.findall(r"^\s*(\w+)\s*=\s*\"([^\"]+)\"", read("pkg/parser/ast/functions.go"), re.M))
ast_names["InternalFuncFromBinary"] = "from_binary"
ast_names["InternalFuncToBinary"] = "to_binary"

# scalarExprSupportedByTiKV: the unconditional case list, grouped by comment, then the
# conditional cases.
src = read("pkg/expression/infer_pushdown.go")
body = src[src.index("func scalarExprSupportedByTiKV(") :]
body = body[: body.index("\n}\n")]
uncond = body[body.index("case\n") : body.index("return true")]
families = []  # (family, name, condition)
family = None
for line in uncond.splitlines():
    s = line.strip()
    m = re.match(r"//\s*(\w[\w ]*) functions\.", s)
    if m:
        family = m.group(1)
        continue
    if s.startswith("//") or not s:
        continue
    s = re.sub(r"/\*.*?\*/", "", s)
    for ident in re.findall(r"(?:ast\.)?(\w+)", s):
        if ident in ("case",):
            continue
        name = ast_names[ident]
        fam = "cast" if name == "cast" else family
        families.append((fam, name, ""))
cond = body[body.index("return true") :]
for m in re.finditer(r"case ((?:ast\.\w+(?:, )?)+):", cond):
    for ident in re.findall(r"ast\.(\w+)", m.group(1)):
        name = ast_names[ident]
        fam = "math" if name in ("conv", "round", "rand") else "string"
        families.append((fam, name, "conditional"))

# Family names as the oracle reports them.
rename = {"op": "logic", "compare": "comparison", "arithmetical": "arithmetic", "control flow": "control", "date": "time"}

# funcs: name -> class
bsrc = read("pkg/expression/builtin.go")
funcs_body = bsrc[bsrc.index("var funcs = map[string]functionClass{") :]
funcs_body = funcs_body[: funcs_body.index("\n}\n")]
name_class = {}
for m in re.finditer(r"^\s*(?:ast\.)?(\w+):\s*&(\w+)\{", funcs_body, re.M):
    ident, cls = m.groups()
    if ident in ast_names:
        name_class[ast_names[ident]] = cls
# The internal charset functions are built outside `funcs` (builtin_convert_charset.go).
name_class["to_binary"] = "tidbToBinaryFunctionClass"
name_class["from_binary"] = "tidbFromBinaryFunctionClass"

# class -> pb codes, from every method with that receiver.
class_codes = {}
for fn in sorted(os.listdir(expr_dir)):
    if not (fn.startswith("builtin") and fn.endswith(".go")) or fn.endswith("_test.go"):
        continue
    text = open(os.path.join(expr_dir, fn), encoding="utf-8").read()
    text = re.sub(r"//[^\n]*", "", text)
    parts = re.split(r"\n(?=func )", text)
    for part in parts:
        m = re.match(r"func \(\w+ \*(\w+FunctionClass)\)", part)
        if not m:
            continue
        codes = re.findall(r"tipb\.ScalarFuncSig_(\w+)", part)
        class_codes.setdefault(m.group(1), set()).update(codes)

cast_classes = sorted(c for c in class_codes if c.startswith("castAs") and c.endswith("FunctionClass"))

rows = set()
for fam, name, cnd in families:
    if name == "cast":
        codes = set()
        for c in cast_classes:
            codes |= class_codes[c]
    else:
        cls = name_class.get(name)
        if cls is None:
            sys.exit(f"no function class for {name}")
        codes = class_codes.get(cls, set())
    if name == "round":
        codes = codes & {"RoundReal", "RoundInt", "RoundDec"}
    if name == "rand":
        codes = codes & {"RandWithSeedFirstGen"}
    if not codes:
        # The class sets no pb code (e.g. PERIOD_ADD): canScalarFuncPushDown rejects
        # ScalarFuncSig_Unspecified, so the name is listed but never pushed.
        codes = {"Unspecified"}
    for code in codes:
        rows.add((rename.get(fam, fam), name, code, cnd))

with open(out, "w", encoding="utf-8") as f:
    f.write("# Generated by tools/expr-gen/gen_tikv_pushable_sigs.py from TiDB v8.5.8 (8b857efa2). DO NOT EDIT.\n")
    f.write("# The scalar function signatures scalarExprSupportedByTiKV lets Go push down to TiKV.\n")
    f.write("family\tfunc\tsig\tcondition\n")
    for r in sorted(rows):
        f.write("\t".join(r) + "\n")
print(f"{len(rows)} rows, {len({r[2] for r in rows})} distinct pb codes -> {out}")
