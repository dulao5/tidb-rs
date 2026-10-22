#!/usr/bin/env python3
"""Extract table-driven cases {src, ok, restore} from TiDB parser_test.go.

Usage: python3 -I tools/parser-port/extract_tests.py <parser_test.go> <all.jsonl> [<sample.jsonl> [N]]
The first line is a provenance record {"_generated": "..."}; consumers skip lines without "src".
Each other line: {"func", "line", "src", "ok", "window", "mode", "restore"}.
  window = the enclosing test calls RunTest(..., enableWindowFunc=true, ...)
  mode   = "default" | "mariadb" | "real_as_float" | "other" (custom parser setup; excluded from sample)
The sample is N valid ("ok": true) default-mode cases picked at an even stride.
"""
import json
import os
import re
import sys

SOURCE = os.environ.get("PARSER_PORT_SOURCE", "TiDB (unknown commit)")

STR = r'"(?:[^"\\\n]|\\.)*"|`[^`]*`'
CASE_RE = re.compile(r'^\s*\{\s*(%s)\s*,\s*(true|false)\s*,\s*(%s)?' % (STR, STR), re.M)
FUNC_RE = re.compile(r'^func (Test\w+)\(', re.M)


def go_unquote(lit):
    if lit[0] == "`":
        return lit[1:-1]
    # Go interpreted string -> Python via JSON-ish decoding of common escapes
    body = lit[1:-1]
    out, i = [], 0
    while i < len(body):
        c = body[i]
        if c != "\\":
            out.append(c)
            i += 1
            continue
        e = body[i + 1]
        simple = {"n": "\n", "t": "\t", "r": "\r", "\\": "\\", '"': '"', "'": "'", "a": "\a",
                  "b": "\b", "f": "\f", "v": "\v", "0": "\0"}
        if e == "x":
            out.append(chr(int(body[i + 2:i + 4], 16)))
            i += 4
        elif e == "u":
            out.append(chr(int(body[i + 2:i + 6], 16)))
            i += 6
        elif e in "01234567" and i + 3 < len(body) + 1 and re.match(r"[0-7]{3}", body[i + 1:i + 4]):
            out.append(chr(int(body[i + 1:i + 4], 8)))
            i += 4
        else:
            out.append(simple[e])
            i += 2
    return "".join(out)


def main():
    src = open(sys.argv[1], encoding="utf-8").read()
    sample_path = sys.argv[3] if len(sys.argv) > 3 else None
    n_sample = int(sys.argv[4]) if len(sys.argv) > 4 else 200
    funcs = [(m.start(), m.group(1)) for m in FUNC_RE.finditer(src)]
    bodies = {}
    for k, (pos, name) in enumerate(funcs):
        end = funcs[k + 1][0] if k + 1 < len(funcs) else len(src)
        bodies[name] = (pos, end, src[pos:end])
    cases = []
    for m in CASE_RE.finditer(src):
        pos = m.start()
        func = max((f for f in funcs if f[0] <= pos), default=(0, "?"))[1]
        body = bodies.get(func, (0, 0, ""))[2]
        fpos = bodies.get(func, (0, 0, ""))[0]
        decls = list(re.finditer(r"(\w+)\s*:?=\s*\[\]testCase\{", src[fpos:pos]))
        var = re.escape(decls[-1].group(1)) if decls else r"\w+"
        window = bool(re.search(r"RunTest\(t, %s, true," % var, body))
        if re.search(r"RunTest\(t, %s, \w+, true\)" % var, body):
            mode = "mariadb"
        elif re.search(r"RunTestInRealAsFloatMode\(t, %s," % var, body):
            mode = "real_as_float"
        elif re.search(r"RunTest\(t, %s," % var, body):
            mode = "default"
        else:
            mode = "other"
        cases.append({"func": func, "line": src.count("\n", 0, pos) + 1,
                      "src": go_unquote(m.group(1)), "ok": m.group(2) == "true",
                      "window": window, "mode": mode,
                      "restore": go_unquote(m.group(3)) if m.group(3) else None})
    with open(sys.argv[2], "w", encoding="utf-8") as f:
        f.write(json.dumps({"_generated": f"by tools/parser-port/extract_tests.py from pkg/parser/parser_test.go "
                                          f"of {SOURCE}. Regenerate with: tools/parser-port/regen.sh <tidb-checkout>"})
                + "\n")
        for c in cases:
            f.write(json.dumps(c, ensure_ascii=False) + "\n")
    valid = [c for c in cases if c["ok"] and c["mode"] == "default" and c["src"].strip()]
    stride = max(1, len(valid) // n_sample)
    sample = valid[::stride][:n_sample]
    if sample_path:
        with open(sample_path, "w", encoding="utf-8") as f:
            for c in sample:
                f.write(json.dumps(c, ensure_ascii=False) + "\n")
    from collections import Counter
    print(f"cases={len(cases)} by mode={dict(Counter(c['mode'] for c in cases))} "
          f"ok={sum(c['ok'] for c in cases)} valid_default={len(valid)} sample={len(sample)}",
          file=sys.stderr)


if __name__ == "__main__":
    main()
