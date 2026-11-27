#!/usr/bin/env python3
"""Check runtime logging parity across layered tidb-rs crates (specs/D00.3-logging/spec.md §6).

Three rules:
  Rule A (pure Rust, zero tolerance):
    Forbid `println!` / `eprintln!` / `print!` / `eprint!` and bare `catch_unwind` in
    layered production code (outside `#[cfg(test)]` blocks, `crates/base/src/panic_guard.rs`,
    and explicit `// log-skip:` lines).
  Rule B (pure Rust, ratcheted):
    Forbid interpolated `{...}` / `key=` / `format!` / `from_utf8_lossy` inside log message
    arguments of `log::*!`, `tracing::*!`, and `tidb_base::log_*!` calls.
  Rule C (Go reference tree comparison when `--go-tree <path>` is provided, ratcheted):
    Merge `// go-part:` fragments per `(go_file, go_symbol)` and compare Go vs Rust log
    call counts, levels, message literals, and `zap.Stack` presence.

Ratchet file: `scripts/log_parity_ratchet.tsv` (only decreases unless `--write`).
"""

from __future__ import annotations

import argparse
import dataclasses
import pathlib
import re
import sys
import tomllib
from collections import defaultdict

ROOT = pathlib.Path(__file__).resolve().parent.parent
DEFAULT_RATCHET = ROOT / "scripts" / "log_parity_ratchet.tsv"

# Dynamic field construction functions verified by byte-exact golden tests (spec §6.2 #3).
DYNAMIC_GO_SYMBOLS = {
    ("pkg/util/util.go", "GenLogFields"),
    ("pkg/util/execdetails/execdetails.go", "ExecDetails.ToZapFields"),
    ("pkg/util/execdetails/execdetails.go", "CopTasksDetails.ToZapFields"),
    ("pkg/util/logutil/consistency/reporter.go", "Reporter.ReportAdminCheckInconsistent"),
    ("pkg/util/logutil/consistency/reporter.go", "Reporter.ReportLookupInconsistent"),
}

LOG_MACRO_RE = re.compile(
    r"\b(?:(?:log|tracing)::(trace|debug|info|warn|error)|"
    r"(?:tidb_base::|crate::)?log_(debug|info|warn|error|fatal|target|err_verbose|general))!\s*\("
)
PRINT_MACRO_RE = re.compile(r"\b(e?print(?:ln)?)!\s*\(")
BARE_CATCH_UNWIND_RE = re.compile(r"(?:\bstd::panic::catch_unwind\s*\(|\.\s*catch_unwind\s*\(|\bcatch_unwind\s*\()")
INTERP_RE = re.compile(r"\{[^{}]*\}|\b(?:error|err|stack|sql|SQL|schemaVersion|class|code)\s*=")
GO_FILE_RE = re.compile(r"^\s*//!\s*go-file:\s*(\S+)")
GO_ANNOT_RE = re.compile(r"^\s*//\s*(go|go-part):\s*(.+)$")
GO_LOG_CALL_RE = re.compile(
    r"\b(?:logutil\.\w+\([^)]*\)|log)\.(Debug|Info|Warn|Error|DPanic|Panic|Fatal)\s*\("
)


@dataclasses.dataclass
class Violation:
    rule: str  # "A", "B", or "C"
    crate: str
    path: str
    line: int
    detail: str


def load_layered_crates(root: pathlib.Path) -> dict[str, pathlib.Path]:
    """Return mapping of layered crate name -> crate root directory from layers.toml."""
    layers_path = root / "layers.toml"
    with layers_path.open("rb") as f:
        data = tomllib.load(f)
    layered_names = {
        name
        for name, meta in data.get("crate", {}).items()
        if not meta.get("planned", False) and name != "tidb-planner-pilot"
    }
    crates: dict[str, pathlib.Path] = {}
    for cargo_toml in sorted((root / "crates").glob("*/Cargo.toml")):
        with cargo_toml.open("rb") as f:
            pkg = tomllib.load(f).get("package", {})
        name = pkg.get("name")
        if name in layered_names:
            crates[name] = cargo_toml.parent
    return crates


def strip_comments_and_strings_mask(src: str) -> list[bool]:
    """Return a boolean mask of length len(src): True where character is active Rust code
    (outside `//` line comments, nested `/* */` block comments, normal/raw string literals)."""
    n = len(src)
    code = [True] * n
    i = 0
    while i < n:
        c = src[i]
        # Line comment
        if c == "/" and i + 1 < n and src[i + 1] == "/":
            start = i
            while i < n and src[i] != "\n":
                i += 1
            for k in range(start, i):
                code[k] = False
            continue
        # Block comment (nestable in Rust)
        if c == "/" and i + 1 < n and src[i + 1] == "*":
            start = i
            depth = 1
            i += 2
            while i < n and depth > 0:
                if src[i] == "/" and i + 1 < n and src[i + 1] == "*":
                    depth += 1
                    i += 2
                elif src[i] == "*" and i + 1 < n and src[i + 1] == "/":
                    depth -= 1
                    i += 2
                else:
                    i += 1
            for k in range(start, i):
                code[k] = False
            continue
        # Raw string literal r#"..."# or br#"..."#
        if (c == "r" or (c == "b" and i + 1 < n and src[i + 1] == "r")) and i + 1 < n:
            j = i + (2 if c == "b" else 1)
            hashes = 0
            while j < n and src[j] == "#":
                hashes += 1
                j += 1
            if j < n and src[j] == '"':
                start = i
                i = j + 1
                end_seq = '"' + ("#" * hashes)
                idx = src.find(end_seq, i)
                i = n if idx == -1 else idx + len(end_seq)
                for k in range(start, i):
                    code[k] = False
                continue
        # Normal / byte string literal
        if c == '"' or (c == "b" and i + 1 < n and src[i + 1] == '"'):
            start = i
            i += 2 if c == "b" else 1
            while i < n:
                if src[i] == "\\":
                    i += 2
                elif src[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
            for k in range(start, i):
                code[k] = False
            continue
        # Char literal (avoid confusing lifetime `'a`)
        if c == "'" and i + 2 < n:
            if src[i + 1] == "\\" and i + 3 < n:
                j = i + 2
                while j < n and src[j] != "'" and src[j] != "\n":
                    j += 1
                if j < n and src[j] == "'":
                    for k in range(i, j + 1):
                        code[k] = False
                    i = j + 1
                    continue
            elif src[i + 2] == "'":
                for k in range(i, i + 3):
                    code[k] = False
                i += 3
                continue
        i += 1
    return code


def compute_test_ranges(src: str, code_mask: list[bool]) -> list[tuple[int, int]]:
    """Find character ranges of `#[cfg(test)]` items (modules or functions) using brace-depth
    matching so production code AFTER a `#[cfg(test)] mod tests { ... }` is still checked."""
    ranges: list[tuple[int, int]] = []
    n = len(src)
    for m in re.finditer(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]", src):
        if not code_mask[m.start()]:
            continue
        # Scan forward for `{` or `;` at top level of the attributed item.
        i = m.end()
        while i < n:
            if not code_mask[i]:
                i += 1
                continue
            ch = src[i]
            if ch == ";":
                ranges.append((m.start(), i + 1))
                break
            if ch == "{":
                depth = 1
                j = i + 1
                while j < n and depth > 0:
                    if code_mask[j]:
                        if src[j] == "{":
                            depth += 1
                        elif src[j] == "}":
                            depth -= 1
                    j += 1
                ranges.append((m.start(), j))
                break
            i += 1
    return ranges


def in_ranges(pos: int, ranges: list[tuple[int, int]]) -> bool:
    return any(a <= pos < b for a, b in ranges)


def line_number_at(src: str, pos: int) -> int:
    return src.count("\n", 0, pos) + 1


def has_log_skip(lines: list[str], line_no: int) -> bool:
    """Return True if `// log-skip:` appears on `line_no` or the immediately preceding line."""
    for idx in range(max(0, line_no - 2), min(len(lines), line_no)):
        if "log-skip:" in lines[idx]:
            return True
    return False


def extract_balanced_parens(src: str, open_paren_pos: int, code_mask: list[bool]) -> str:
    """Given index of `(` in `src`, return inner text up to matching `)`."""
    n = len(src)
    depth = 1
    i = open_paren_pos + 1
    while i < n and depth > 0:
        if code_mask[i]:
            if src[i] == "(":
                depth += 1
            elif src[i] == ")":
                depth -= 1
                if depth == 0:
                    return src[open_paren_pos + 1 : i]
        i += 1
    return src[open_paren_pos + 1 :]


def split_top_level_args(arg_str: str) -> list[str]:
    """Split comma-separated macro arguments respecting quotes and nested delimiters."""
    mask = strip_comments_and_strings_mask(arg_str)
    parts: list[str] = []
    start = 0
    depth = 0
    for i, ch in enumerate(arg_str):
        if not mask[i]:
            continue
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth = max(0, depth - 1)
        elif ch == "," and depth == 0:
            parts.append(arg_str[start:i].strip())
            start = i + 1
    tail = arg_str[start:].strip()
    if tail:
        parts.append(tail)
    return parts


def unescape_rust_str_literal(token: str) -> str | None:
    token = token.strip()
    if token.startswith('"') and token.endswith('"') and len(token) >= 2:
        return token[1:-1]
    m = re.match(r'^r(#*)"(.*)"\1$', token, re.DOTALL)
    if m:
        return m.group(2)
    return None


def scan_rust_file(
    crate: str,
    rel_path: str,
    src: str,
) -> tuple[list[Violation], dict[tuple[str, str], list[tuple[str, str | None, bool]]]]:
    """Scan one Rust file for Rules A and B, and collect per-(go_file, go_sym) calls for Rule C."""
    violations: list[Violation] = []
    symbol_calls: dict[tuple[str, str], list[tuple[str, str | None, bool]]] = defaultdict(list)

    code_mask = strip_comments_and_strings_mask(src)
    test_ranges = compute_test_ranges(src, code_mask)
    lines = src.splitlines()
    is_panic_guard = rel_path.endswith("crates/base/src/panic_guard.rs")

    # Rule A: print!/println!/eprint!/eprintln!
    for m in PRINT_MACRO_RE.finditer(src):
        if not code_mask[m.start()] or in_ranges(m.start(), test_ranges):
            continue
        line_no = line_number_at(src, m.start())
        if has_log_skip(lines, line_no):
            continue
        violations.append(
            Violation("A", crate, rel_path, line_no, f"raw {m.group(1)}! in layered production code")
        )

    # Rule A: bare catch_unwind
    if not is_panic_guard:
        for m in BARE_CATCH_UNWIND_RE.finditer(src):
            if not code_mask[m.start()] or in_ranges(m.start(), test_ranges):
                continue
            # Ignore `panic_guard::catch_unwind_{silent,capture,future}` (different identifier)
            before = src[max(0, m.start() - 16) : m.start()]
            if before.endswith("panic_guard::") or before.endswith("catch_unwind_"):
                continue
            line_no = line_number_at(src, m.start())
            if has_log_skip(lines, line_no):
                continue
            violations.append(
                Violation(
                    "A",
                    crate,
                    rel_path,
                    line_no,
                    "bare catch_unwind (use tidb_base::panic_guard::catch_unwind_{silent,capture,future})",
                )
            )

    # Rule B: interpolated messages in log/tracing/structured_log macros
    for m in LOG_MACRO_RE.finditer(src):
        if not code_mask[m.start()] or in_ranges(m.start(), test_ranges):
            continue
        line_no = line_number_at(src, m.start())
        if has_log_skip(lines, line_no):
            continue
        open_pos = m.end() - 1
        inner = extract_balanced_parens(src, open_pos, code_mask)
        args = split_top_level_args(inner)
        if not args:
            continue
        is_tracing = src[m.start() : m.end()].startswith("tracing::")
        is_log_target = "log_target!" in src[m.start() : m.end()]
        is_log_err_verbose = "log_err_verbose!" in src[m.start() : m.end()]
        is_tidb_log = "log_" in src[m.start() : m.end()]

        msg_arg: str | None = None
        if is_log_target:
            msg_arg = args[2] if len(args) >= 3 else None
        elif is_log_err_verbose:
            msg_arg = args[1] if len(args) >= 2 else None
        elif is_tidb_log:
            msg_arg = args[0]
        elif is_tracing:
            # Skip `target: ...` and `k = v` / `%k` / `?k` fields to locate format string.
            for a in args:
                if a.startswith("target:") or a.startswith("parent:"):
                    continue
                if re.match(r"^(?:[A-Za-z_][A-Za-z0-9_.]*\s*=|[%?][A-Za-z_])", a):
                    continue
                if "=" in a and not a.startswith('"'):
                    continue
                msg_arg = a
                break
        else:
            # std `log::*!`: skip optional `target: ...` and `k:v = expr` kv prefix args
            for a in args:
                if a.startswith("target:"):
                    continue
                if "=" in a and not a.startswith('"') and not a.startswith("format!"):
                    continue
                msg_arg = a
                break

        if msg_arg is None:
            continue
        lit = unescape_rust_str_literal(msg_arg)
        if lit is not None:
            if INTERP_RE.search(lit):
                violations.append(
                    Violation(
                        "B",
                        crate,
                        rel_path,
                        line_no,
                        f"interpolated/mixed log message: {msg_arg[:80]}",
                    )
                )
        elif "format!(" in msg_arg or "from_utf8_lossy(" in msg_arg:
            violations.append(
                Violation(
                    "B",
                    crate,
                    rel_path,
                    line_no,
                    f"dynamic format!/from_utf8_lossy in log message: {msg_arg[:80]}",
                )
            )

    # Rule C collection: map each `// go:` / `// go-part:` function to its log calls.
    go_files: list[str] = []
    for ln in lines:
        gm = GO_FILE_RE.match(ln)
        if gm:
            go_files.append(gm.group(1))

    line_starts: list[int] = [0]
    for ln in lines:
        line_starts.append(line_starts[-1] + len(ln) + 1)

    for idx, ln in enumerate(lines):
        am = GO_ANNOT_RE.match(ln)
        if not am:
            continue
        rest = am.group(2).strip().split()
        if not rest:
            continue
        if rest[0].endswith(".go") and len(rest) >= 2:
            candidate_files, gsym = [rest[0]], rest[1]
        elif go_files:
            candidate_files, gsym = [g for g in go_files if g.startswith("pkg/")], rest[0]
        else:
            continue
        if not candidate_files:
            continue
        # Find the opening `{` of the annotated fn.
        pos = line_starts[idx]
        if in_ranges(pos, test_ranges):
            continue
        brace_pos = -1
        i = pos
        n = len(src)
        while i < n:
            if code_mask[i] and src[i] == "{":
                brace_pos = i
                break
            if code_mask[i] and src[i] == ";" and i > line_starts[min(idx + 1, len(lines))]:
                break
            i += 1
        if brace_pos == -1:
            continue
        depth = 1
        j = brace_pos + 1
        while j < n and depth > 0:
            if code_mask[j]:
                if src[j] == "{":
                    depth += 1
                elif src[j] == "}":
                    depth -= 1
            j += 1
        body = src[brace_pos:j]
        body_mask = code_mask[brace_pos:j]
        fn_calls: list[tuple[str, str | None, bool]] = []
        for lm in LOG_MACRO_RE.finditer(body):
            if not body_mask[lm.start()]:
                continue
            macro_head = body[lm.start() : lm.end()]
            lvl = (lm.group(1) or lm.group(2) or "info").lower()
            inner = extract_balanced_parens(body, lm.end() - 1, body_mask)
            args = split_top_level_args(inner)
            if "log_target!" in macro_head and len(args) >= 3:
                msg_tok = args[2]
                if "Debug" in args[1]:
                    lvl = "debug"
                elif "Warn" in args[1]:
                    lvl = "warn"
                elif "Error" in args[1]:
                    lvl = "error"
                elif "Fatal" in args[1]:
                    lvl = "fatal"
                else:
                    lvl = "info"
            elif "log_err_verbose!" in macro_head and len(args) >= 2:
                msg_tok = args[1]
                if "Debug" in args[0]:
                    lvl = "debug"
                elif "Warn" in args[0]:
                    lvl = "warn"
                elif "Error" in args[0]:
                    lvl = "error"
                elif "Fatal" in args[0]:
                    lvl = "fatal"
                else:
                    lvl = "info"
            else:
                msg_tok = args[0] if args else ""
            lit = unescape_rust_str_literal(msg_tok) if msg_tok else None
            has_stack = bool(re.search(r'"stack"\s*=>|\bstack\s*[=:]', inner))
            fn_calls.append((lvl, lit, has_stack))
        # Also count `// log-skip:` markers inside the function body as intentionally accounted.
        skip_count = len(re.findall(r"//\s*log-skip:", body))
        for _ in range(skip_count):
            fn_calls.append(("skip", None, False))
        # Register under first candidate file (multi-go-file files resolve via explicit file prefix
        # or fallback to all listed pkg/ files during Rule C lookup).
        symbol_calls[(candidate_files[0], gsym)].extend(fn_calls)

    return violations, symbol_calls


def extract_go_function_logs(go_src: str, go_sym: str) -> list[tuple[str, str | None, bool]] | None:
    """Locate `go_sym` (e.g. `Func` or `Type.Method`) in `go_src` and return its log calls."""
    if "." in go_sym:
        recv, method = go_sym.rsplit(".", 1)
        pattern = re.compile(
            rf"^func\s+\([^)]*\*?{re.escape(recv)}(?:\[[^\]]*\])?\)\s+{re.escape(method)}\s*\(",
            re.MULTILINE,
        )
    else:
        pattern = re.compile(
            rf"^func\s+{re.escape(go_sym)}(?:\[[^\]]*\])?\s*\(",
            re.MULTILINE,
        )
    m = pattern.search(go_src)
    if not m:
        return None
    n = len(go_src)
    i = m.end()
    while i < n and go_src[i] != "{":
        i += 1
    if i >= n:
        return None
    depth = 1
    j = i + 1
    in_str = False
    in_raw = False
    while j < n and depth > 0:
        ch = go_src[j]
        if in_raw:
            if ch == "`":
                in_raw = False
            j += 1
            continue
        if in_str:
            if ch == "\\":
                j += 2
                continue
            if ch == '"':
                in_str = False
            j += 1
            continue
        if ch == "/" and j + 1 < n and go_src[j + 1] == "/":
            while j < n and go_src[j] != "\n":
                j += 1
            continue
        if ch == "/" and j + 1 < n and go_src[j + 1] == "*":
            end = go_src.find("*/", j + 2)
            j = n if end == -1 else end + 2
            continue
        if ch == "`":
            in_raw = True
            j += 1
            continue
        if ch == '"':
            in_str = True
            j += 1
            continue
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
        j += 1
    body = go_src[i:j]
    calls: list[tuple[str, str | None, bool]] = []
    for lm in GO_LOG_CALL_RE.finditer(body):
        lvl = lm.group(1).lower()
        tail = body[lm.end() : lm.end() + 240]
        lit_m = re.match(r'\s*"([^"\\]*(?:\\.[^"\\]*)*)"', tail)
        lit = lit_m.group(1) if lit_m else None
        has_stack = "zap.Stack(" in tail
        calls.append((lvl, lit, has_stack))
    return calls


def load_ratchet(path: pathlib.Path) -> dict[tuple[str, str], int]:
    """Read `(rule, crate) -> max_count` from TSV."""
    if not path.exists():
        return {}
    out: dict[tuple[str, str], int] = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or line.startswith("rule\t"):
            continue
        parts = line.split("\t")
        if len(parts) >= 3:
            out[(parts[0], parts[1])] = int(parts[2])
    return out


def write_ratchet(path: pathlib.Path, counts: dict[tuple[str, str], int]) -> None:
    lines = [
        "# Runtime logging parity ratchet (specs/D00.3-logging/spec.md §6).",
        "# Rule A must stay 0. Rules B and C only decrease.",
        "rule\tcrate\tcount",
    ]
    for (rule, crate), cnt in sorted(counts.items()):
        if cnt > 0:
            lines.append(f"{rule}\t{crate}\t{cnt}")
    path.write_text("\n".join(lines) + "\n")


def run_check(
    root: pathlib.Path,
    go_tree: pathlib.Path | None,
    ratchet_path: pathlib.Path,
    list_all: bool,
    write: bool,
) -> int:
    crates = load_layered_crates(root)
    violations: list[Violation] = []
    # (crate, go_file, go_sym) -> merged calls across go-part: fragments
    merged_symbols: dict[tuple[str, str, str], list[tuple[str, str | None, bool]]] = defaultdict(list)

    for crate, cdir in sorted(crates.items()):
        src_dir = cdir / "src"
        if not src_dir.exists():
            continue
        for rs in sorted(src_dir.rglob("*.rs")):
            name = rs.name
            if name.endswith("_test.rs") or name.endswith("_tests.rs") or name == "tests.rs":
                continue
            rel = rs.relative_to(root).as_posix()
            text = rs.read_text(encoding="utf-8")
            v, syms = scan_rust_file(crate, rel, text)
            violations.extend(v)
            for (gfile, gsym), calls in syms.items():
                merged_symbols[(crate, gfile, gsym)].extend(calls)

    # Rule C: Go reference comparison when --go-tree is supplied.
    if go_tree is not None:
        go_cache: dict[str, str] = {}
        for (crate, gfile, gsym), rust_calls in sorted(merged_symbols.items()):
            if (gfile, gsym) in DYNAMIC_GO_SYMBOLS:
                continue
            if gfile not in go_cache:
                p = go_tree / gfile
                go_cache[gfile] = p.read_text(encoding="utf-8") if p.exists() else ""
            gsrc = go_cache[gfile]
            if not gsrc:
                continue
            go_calls = extract_go_function_logs(gsrc, gsym)
            if not go_calls:
                continue
            if len(rust_calls) < len(go_calls):
                violations.append(
                    Violation(
                        "C",
                        crate,
                        gfile,
                        0,
                        f"{gsym}: Rust has {len(rust_calls)} log/skip site(s) vs Go {len(go_calls)}",
                    )
                )
            else:
                # Also verify zap.Stack presence when Go explicitly attaches a stack trace and
                # Rust has no `// log-skip:` covering it.
                go_stacks = sum(1 for _, _, st in go_calls if st)
                rust_stacks = sum(1 for lvl, _, st in rust_calls if st or lvl == "skip")
                if rust_stacks < go_stacks:
                    violations.append(
                        Violation(
                            "C",
                            crate,
                            gfile,
                            0,
                            f"{gsym}: Rust missing stack field ({rust_stacks} vs Go {go_stacks})",
                        )
                    )

    counts: dict[tuple[str, str], int] = defaultdict(int)
    for v in violations:
        counts[(v.rule, v.crate)] += 1

    if list_all:
        for v in violations:
            loc = f"{v.path}:{v.line}" if v.line else v.path
            print(f"[Rule {v.rule}] {v.crate} {loc}: {v.detail}")

    existing = load_ratchet(ratchet_path)
    if write:
        # Preserve Rule C entries if running without --go-tree so `--write` never wipes them.
        merged_counts = dict(counts)
        if go_tree is None:
            for (r, c), val in existing.items():
                if r == "C" and (r, c) not in merged_counts:
                    merged_counts[(r, c)] = val
        write_ratchet(ratchet_path, merged_counts)
        print(f"wrote ratchet {ratchet_path} ({len(merged_counts)} entries)")
        return 0

    failed = False
    # Rule A must always be 0.
    rule_a_total = sum(cnt for (r, _), cnt in counts.items() if r == "A")
    if rule_a_total > 0:
        failed = True
        print(f"FAIL: Rule A has {rule_a_total} violation(s) (must be 0):")
        for v in violations:
            if v.rule == "A":
                print(f"  {v.crate} {v.path}:{v.line}: {v.detail}")

    # Rules B & C checked against ratchet (only decrease).
    checked_rules = {"B", "C"} if go_tree is not None else {"B"}
    all_keys = {k for k in set(counts) | set(existing) if k[0] in checked_rules}
    for rule, crate in sorted(all_keys):
        cur = counts.get((rule, crate), 0)
        cap = existing.get((rule, crate), 0)
        if cur > cap:
            failed = True
            print(f"FAIL: Rule {rule} regression in {crate}: {cur} > ratchet {cap}")
            for v in violations:
                if v.rule == rule and v.crate == crate:
                    loc = f"{v.path}:{v.line}" if v.line else v.path
                    print(f"  {loc}: {v.detail}")

    rule_b_total = sum(cnt for (r, _), cnt in counts.items() if r == "B")
    rule_c_total = sum(cnt for (r, _), cnt in counts.items() if r == "C")
    if failed:
        return 1
    if go_tree is not None:
        print(f"log parity: Rule A=0, Rule B={rule_b_total}, Rule C={rule_c_total}: OK")
    else:
        print(f"log parity: Rule A=0, Rule B={rule_b_total}: OK")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="Check tidb-rs runtime logging parity.")
    ap.add_argument("--go-tree", type=pathlib.Path, default=None, help="Path to Go TiDB v8.5.8 tree")
    ap.add_argument("--ratchet", type=pathlib.Path, default=DEFAULT_RATCHET, help="Ratchet TSV path")
    ap.add_argument("--list", action="store_true", help="Print every violation")
    ap.add_argument("--write", action="store_true", help="Write current counts to ratchet TSV")
    args = ap.parse_args()
    return run_check(ROOT, args.go_tree, args.ratchet, args.list, args.write)


if __name__ == "__main__":
    sys.exit(main())
