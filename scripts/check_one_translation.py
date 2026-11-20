#!/usr/bin/env python3
"""One translation per Go symbol: every Go function and every Go type has one Rust home.

Two agents porting the same Go code independently produce two Rust versions that drift apart
(the FieldType / ColumnInfo case of #229). This check reads the provenance markers of
docs/porting/provenance.md in crates/ (without the design-pilot crate crates/spike) and reports

  func       a Go function or method (`// go: [<path>] <Symbol> <level>`) annotated above more
             than one Rust definition site. One site = one Rust item (file, enclosing impl,
             name): `#[cfg]` variants of one function (e.g. per target for M-FMA) are one site.
  std, dep   the same for ported Go standard library / module functions (`// go-std:`,
             `// go-dep:`).
  type-split the methods of one Go type annotated inside `impl <Type>` blocks (same name) in
             more than one crate: inherent impls cannot cross crates, so the type has a
             Rust definition in each.
  type-def   a struct / enum / type alias / trait of the same name defined in more than one
             crate, in files that port (`//! go-file:`) the same Go package; or a type whose
             comments name a ported Go type of its name (`/// ... (Go: `types.FieldType`)`),
             i.e. a hand-written stand-in next to the faithful port.

The Go package of a `// go:` symbol is the directory of the marker's explicit path, otherwise
of the file's `//! go-file:` headers; two sites conflict when they share a package.

A site that is a deliberate second translation carries, in the comments above it,

    // go-dup: <reason>

and is not counted. Findings that existed when the check was introduced are listed with their
number of sites and a reason in scripts/one_translation_allow.tsv (a ratchet): the check fails
on a finding not in the list or with more sites than listed, and on a list entry that is no
longer found or has fewer sites (delete the line / lower the count: the improvement is kept).

    scripts/check_one_translation.py                 check (exit 1 on new or stale entries)
    scripts/check_one_translation.py --list          print every finding with its sites
    scripts/check_one_translation.py --write-allowlist
                                                     rewrite the list from the current findings
                                                     (keeps existing reasons; review the diff)
    scripts/check_one_translation.py --changed <rev> [--pr-body FILE] [--stubs stubs.tsv]
        PR hygiene, report only: the Go files ported by the Rust files changed since <rev>
        (the PR should cite them; with --pr-body, the ones it does not are listed) and the
        `blocked-by:` markers the diff adds (with --stubs, the tracker's data/stubs.tsv, the
        files without a registered stub are listed).
"""

import argparse
import csv
import os
import re
import subprocess
import sys
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ALLOW = os.path.join(ROOT, "scripts", "one_translation_allow.tsv")
SKIP_DIRS = {"spike", "target"}

MARK_RE = re.compile(r"^\s*//\s*(go|go-std|go-dep):\s*(\S.*)$")
FILE_RE = re.compile(r"^\s*//!\s*go-file:\s*(\S+)")
DUP_RE = re.compile(r"^\s*//\s*go-dup:\s*\S")
ITEM_RE = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:default\s+)?(?:const\s+|async\s+|unsafe\s+|extern\s+\"[^\"]*\"\s+)*"
    r"(fn|struct|enum|union|type|trait|const|static|mod)\s+([A-Za-z_][A-Za-z0-9_]*)"
)
MACRO_ITEM_RE = re.compile(r"^\s*(?:macro_rules!\s*([A-Za-z_]\w*)|([A-Za-z_][\w:]*)!\s*[({\[])")
IMPL_RE = re.compile(r"^(\s*)(?:unsafe\s+)?impl\b(.*)")
SKIP_LINE_RE = re.compile(r"^\s*(//|#\[|#!\[|\)\]|$)")
TYPE_KINDS = {"struct", "enum", "union", "type", "trait"}
BLOCKED_RE = re.compile(r"blocked-by:\s*([A-Z][A-Za-z0-9.]*)|blocked\(\s*\"([^\"]+)\"")


def rust_files():
    crates = os.path.join(ROOT, "crates")
    for dirpath, dirnames, filenames in os.walk(crates):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for fn in sorted(filenames):
            if fn.endswith(".rs"):
                yield os.path.relpath(os.path.join(dirpath, fn), ROOT)


def crate_of(rel):
    parts = rel.split(os.sep)
    # crates/<name>/... ; crates/poc/<name>/...
    return "/".join(parts[1:3]) if parts[1] == "poc" else parts[1]


def impl_name(rest):
    """`impl<T> Foo for Bar<T> {` -> `Foo for Bar`, `impl Bar {` -> `Bar`."""
    rest = re.sub(r"^<[^>]*>", "", rest.strip()).split("{")[0].split(" where ")[0]
    return re.sub(r"<[^<>]*>", "", re.sub(r"<[^<>]*>", "", rest)).strip()


def next_item(lines, i):
    """(kind, name) of the item at or after line i, skipping comments, attributes and blank
    lines (up to 12 lines); None when the marker is not above an item (e.g. inside a body)."""
    for j in range(i, min(i + 12, len(lines))):
        line = lines[j]
        if SKIP_LINE_RE.match(line):
            continue
        m = ITEM_RE.match(line)
        if m:
            return m.group(1), m.group(2), j
        m = MACRO_ITEM_RE.match(line)
        if m:
            return "macro", m.group(1) or m.group(2), j
        return None
    return None


class Site:
    __slots__ = ("file", "line", "crate", "impl", "kind", "name")

    def __init__(self, file, line, impl, kind, name):
        self.file, self.line, self.impl, self.kind, self.name = file, line, impl, kind, name
        self.crate = crate_of(file)

    def ident(self):
        if self.kind is None:
            return (self.file, "line", self.line)
        return (self.file, self.impl, self.kind, self.name)

    def show(self):
        what = f"{self.kind} {self.name}" if self.kind else "(not above an item)"
        ctx = f" in impl {self.impl}" if self.impl else ""
        return f"{self.file}:{self.line} {what}{ctx}"


def parse_go_marker(text):
    """`[<path>] <Symbol> ...` -> (path or None, symbol)."""
    toks = text.split()
    if toks and toks[0].endswith(".go") and "/" in toks[0]:
        return toks[0], (toks[1] if len(toks) > 1 else "")
    return None, toks[0] if toks else ""


GO_REF_RE = re.compile(r"`\*?([a-z][a-z0-9_]*)\.([A-Za-z_][A-Za-z0-9_]*)`")


def go_type_refs(lines, i, name):
    """Go types `pkg.Name` (same name as the Rust item) named in the comments above line i,
    e.g. `/// A column type (Go: `types.FieldType`)`: a hand-written stand-in of that type."""
    refs = set()
    j = i - 1
    while j >= 0 and re.match(r"^\s*(//|#\[)", lines[j]):
        for m in GO_REF_RE.finditer(lines[j]):
            if m.group(2) == name:
                refs.add(m.group(1))
        j -= 1
    return frozenset(refs)


def std_symbol(text):
    """`cmd/compile float64->int32 conversion L1 [M-F2I]` -> the words before the level."""
    words = []
    for w in text.split():
        if re.match(r"^(L[123]\b|\(|\[)", w):
            break
        words.append(w)
    return " ".join(words)


def scan():
    """Returns (marks, typedefs): marks = [(kind, pkgs, symbol, Site)], typedefs =
    [(pkgs, name, Site)] for type items in files with go-file headers."""
    marks, typedefs = [], []
    for rel in rust_files():
        with open(os.path.join(ROOT, rel), encoding="utf-8") as f:
            lines = f.read().splitlines()
        go_files = [m.group(1) for line in lines if (m := FILE_RE.match(line))]
        header_pkgs = frozenset(os.path.dirname(g) for g in go_files) or frozenset({"?"})
        impl_stack = []  # (indent, name)
        dup_pending = False
        for i, line in enumerate(lines):
            im = IMPL_RE.match(line)
            if im and not line.rstrip().endswith(";"):
                indent = len(im.group(1))
                while impl_stack and impl_stack[-1][0] >= indent:
                    impl_stack.pop()
                impl_stack.append((indent, impl_name(im.group(2))))
            elif impl_stack and line.strip() == "}" and len(line) - len(line.lstrip()) == impl_stack[-1][0]:
                impl_stack.pop()
            if DUP_RE.match(line):
                dup_pending = True
            m = MARK_RE.match(line)
            item_here = None
            if not SKIP_LINE_RE.match(line):
                item_here = ITEM_RE.match(line) or MACRO_ITEM_RE.match(line)
            if m:
                found = next_item(lines, i + 1)
                impl = impl_stack[-1][1] if impl_stack else ""
                if found:
                    kind, name, _ = found
                    site = Site(rel, i + 1, impl, kind, name)
                else:
                    site = Site(rel, i + 1, impl, None, None)
                dup = dup_pending or any(DUP_RE.match(x) for x in lines[i + 1:(found[2] if found else i + 1)])
                if not dup:
                    kind = m.group(1)
                    if kind == "go":
                        path, sym = parse_go_marker(m.group(2))
                        if sym == "init":
                            # Go allows one init per file: key it by file, not package.
                            pkgs = frozenset({path}) if path else (frozenset(go_files) or header_pkgs)
                        else:
                            pkgs = frozenset({os.path.dirname(path)}) if path else header_pkgs
                        marks.append(("func", pkgs, sym, site))
                    else:
                        sym = std_symbol(m.group(2))
                        marks.append(("std" if kind == "go-std" else "dep", frozenset({"-"}), sym, site))
            if item_here and not m:
                im2 = ITEM_RE.match(line)
                if im2 and im2.group(1) in TYPE_KINDS and not dup_pending:
                    # associated types (in impls and traits) are not type definitions
                    if im2.group(1) != "type" or (not impl_stack and "=" in line):
                        name = im2.group(2)
                        refs = go_type_refs(lines, i, name)
                        if go_files or refs:
                            site = Site(rel, i + 1, "", im2.group(1), name)
                            typedefs.append((header_pkgs if go_files else frozenset(), refs, name, site))
                dup_pending = False
    return marks, typedefs


def findings():
    """{key: [Site, ...]} of every duplicate."""
    marks, typedefs = scan()
    out = {}

    # func / std / dep: same symbol, shared package, more than one site.
    by_sym = defaultdict(list)
    for kind, pkgs, sym, site in marks:
        by_sym[(kind, sym)].append((pkgs, site))
    for (kind, sym), entries in by_sym.items():
        for pkg in sorted({p for pkgs, _ in entries for p in pkgs}):
            sites = {}
            for pkgs, site in entries:
                if pkg in pkgs or "?" in pkgs or pkg == "?":
                    sites.setdefault(site.ident(), site)
            if len(sites) > 1:
                key = f"{kind}\t{sym}" if kind != "func" else f"func\t{pkg}\t{sym}"
                if not any(k.startswith(key) for k in out) and not _covered(out, kind, sym, sites):
                    out[key] = sorted(sites.values(), key=lambda s: (s.file, s.line))

    # type-split: methods of one Go type in more than one crate.
    by_type = defaultdict(lambda: defaultdict(list))
    for kind, pkgs, sym, site in marks:
        if kind != "func" or "." not in sym:
            continue
        typ = sym.split(".")[0].lstrip("(*").rstrip(")")
        # Only methods inside an inherent `impl <Type>` of the same name are evidence of a
        # Rust definition of the type (a Go method ported as a free function or onto
        # another type, e.g. for crate layering, is not).
        impl_type = site.impl.split(" for ")[-1].split("::")[-1].strip()
        if " for " in site.impl or impl_type.lower() != typ.lower():
            continue
        for pkg in pkgs:
            by_type[(pkg, typ)][site.crate].append(site)
    for (pkg, typ), crates in by_type.items():
        if len(crates) > 1 and pkg != "?":
            out[f"type-split\t{pkg}\t{typ}"] = [sorted(v, key=lambda s: s.line)[0] for _, v in sorted(crates.items())]

    # type-def: (a) the same type name in files porting the same Go package, in different
    # crates; (b) a type whose comments name a ported Go type `pkg.Name` (a hand-written
    # stand-in), in another file than the ported definition.
    by_def = defaultdict(lambda: defaultdict(list))
    for pkgs, _, name, site in typedefs:
        for pkg in pkgs:
            by_def[(pkg, name)][site.crate].append(site)
    for (pkg, name), crates in by_def.items():
        if len(crates) > 1 and pkg != "?":
            out[f"type-def\t{pkg}\t{name}"] = [v[0] for _, v in sorted(crates.items())]
    ported = defaultdict(list)  # (package base name, type name) -> [(pkg, site)]
    for pkgs, _, name, site in typedefs:
        for pkg in pkgs:
            if pkg != "?":
                ported[(os.path.basename(pkg), name)].append((pkg, site))
    for _, refs, name, site in typedefs:
        for base in refs:
            for pkg, psite in ported.get((base, name), []):
                if psite.file == site.file:
                    continue
                key = f"type-def\t{pkg}\t{name}"
                sites = out.setdefault(key, [psite])
                if all(x.ident() != site.ident() for x in sites):
                    sites.append(site)
    return out


def _covered(out, kind, sym, sites):
    """The same duplicate already reported under another package of the same symbol."""
    ids = set(sites)
    for key, s in out.items():
        parts = key.split("\t")
        if parts[0] == kind and parts[-1] == sym and {x.ident() for x in s} == ids:
            return True
    return False


def load_allow():
    """{key: (sites, reason)}"""
    allow = {}
    if os.path.exists(ALLOW):
        with open(ALLOW, encoding="utf-8") as f:
            for line in f:
                line = line.rstrip("\n")
                if not line or line.startswith("#"):
                    continue
                parts = line.split("\t")
                if len(parts) < 4 or not parts[-2].isdigit() or not parts[-1].strip():
                    sys.exit(f"check_one_translation: {ALLOW}: want <kind> <key...> <sites> <reason>: {line!r}")
                allow["\t".join(parts[:-2])] = (int(parts[-2]), parts[-1])
    return allow


HEADER = """# Known duplicate translations (scripts/check_one_translation.py), a ratchet. One line per
# finding: <kind> TAB <key fields> TAB <number of sites> TAB <reason>. Lines are only removed
# (duplicate resolved) or their count lowered; a new duplicate, or a new site of a known one,
# is fixed or marked `// go-dup: <reason>` at the site, not added here.
"""


def write_allow(found, allow):
    with open(ALLOW, "w", encoding="utf-8") as f:
        f.write(HEADER)
        for key in sorted(found):
            crates = ", ".join(sorted({s.crate for s in found[key]}))
            reason = allow[key][1] if key in allow else f"baseline 2026-11-21 ({crates}); to resolve"
            f.write(f"{key}\t{len(found[key])}\t{reason}\n")


def changed(rev, pr_body, stubs):
    diff_files = subprocess.run(
        ["git", "-C", ROOT, "diff", "--name-only", f"{rev}...HEAD", "--", "crates"],
        check=True, capture_output=True, text=True).stdout.split()
    go_files = set()
    for rel in diff_files:
        p = os.path.join(ROOT, rel)
        if rel.endswith(".rs") and os.path.exists(p):
            with open(p, encoding="utf-8") as f:
                go_files.update(m.group(1) for line in f if (m := FILE_RE.match(line)))
    print(f"Go files ported by the changed Rust files ({len(go_files)}):")
    body = open(pr_body, encoding="utf-8").read() if pr_body else None
    for g in sorted(go_files):
        cited = "" if body is None else ("  cited" if (g in body or os.path.basename(g) in body) else "  NOT CITED in the PR body")
        print(f"  {g}{cited}")
    added = subprocess.run(["git", "-C", ROOT, "diff", "-U0", f"{rev}...HEAD", "--", "crates"],
                           check=True, capture_output=True, text=True).stdout
    registered = set()
    if stubs:
        with open(stubs, newline="", encoding="utf-8") as f:
            registered = {(r["file"], r["block"]) for r in csv.DictReader(f, delimiter="\t")}
    cur, new = None, []
    for line in added.splitlines():
        if line.startswith("+++ b/"):
            cur = line[6:]
            # as the tracker's stub scan: no tests (tests/, tests.rs, *_tests.rs)
            base = os.path.basename(cur)
            if "/tests/" in cur or base == "tests.rs" or base.endswith("_tests.rs"):
                cur = None
        elif cur and line.startswith("+") and not line.startswith("+++"):
            for m in BLOCKED_RE.finditer(line):
                new.append((cur, m.group(1) or m.group(2)))
    print(f"blocked-by markers added ({len(new)}):")
    for f_, b in new:
        status = "" if not stubs else ("  registered" if (f_, b) in registered else "  NOT in stubs.tsv")
        print(f"  {f_} blocked-by:{b}{status}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--write-allowlist", action="store_true")
    ap.add_argument("--changed", metavar="REV")
    ap.add_argument("--pr-body")
    ap.add_argument("--stubs")
    a = ap.parse_args()
    if a.changed:
        changed(a.changed, a.pr_body, a.stubs)
        return 0
    found = findings()
    allow = load_allow()
    if a.write_allowlist:
        write_allow(found, allow)
        print(f"wrote {len(found)} entries to {os.path.relpath(ALLOW, ROOT)}")
        return 0
    counts = defaultdict(int)
    for key in found:
        counts[key.split("\t")[0]] += 1
    summary = ", ".join(f"{k} {counts[k]}" for k in ("func", "std", "dep", "type-split", "type-def"))
    new = sorted(k for k in found if k not in allow)
    grown = sorted(k for k in found if k in allow and len(found[k]) > allow[k][0])
    shrunk = sorted(k for k in found if k in allow and len(found[k]) < allow[k][0])
    stale = sorted(k for k in allow if k not in found)
    if a.list:
        for key in sorted(found):
            tag = "" if key in allow and key not in grown else "  [NEW]"
            print(key.replace("\t", " ") + tag)
            for s in found[key]:
                print(f"    {s.show()}")
    print(f"one translation: {len(found)} duplicates ({summary}); {len(found) - len(new)} known, "
          f"{len(new)} new, {len(grown)} with new sites, {len(stale) + len(shrunk)} allowlist entries to update")
    for key in new + grown:
        what = "NEW duplicate" if key in new else f"NEW site of a known duplicate (allowlist: {allow[key][0]})"
        print(f"{what}: {key.replace(chr(9), ' ')}")
        for s in found[key]:
            print(f"    {s.show()}")
    for key in stale:
        print(f"RESOLVED, delete the allowlist line: {key.replace(chr(9), ' ')}")
    for key in shrunk:
        print(f"FEWER sites ({len(found[key])}), lower the allowlist count: {key.replace(chr(9), ' ')}")
    if new or grown:
        print("Fix: keep one Rust translation and call it from the other crate, or mark a deliberate "
              "second site `// go-dup: <reason>` (see .claude/skills/port-block/SKILL.md).")
    stale = stale + shrunk
    new = new + grown
    return 1 if new or stale else 0


if __name__ == "__main__":
    sys.exit(main())
