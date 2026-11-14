#!/usr/bin/env python3
"""Interior-mutability check of the shared metadata model (N13 §3 rule 3).

Shared metadata (`Arc<TableInfo>`, `Arc<ColumnInfo>`, ...) is changed only through
copy on write (`make_mut`). A field that can be changed through `&T` (`Cell`, `RefCell`,
`Mutex`, atomics, ...) would bypass that and bring back the #148 class of bugs, so the
model crate (crates/model/src, generated and hand-written types) must not contain one.

Fails on any of these names outside comments and `use` declarations:

    Cell RefCell OnceCell LazyCell UnsafeCell SyncUnsafeCell
    Mutex RwLock Condvar OnceLock LazyLock Once Atomic* ArcSwap* thread_local! `static mut`

Allowed without a marker: a `static` initialised once (`static X: LazyLock<T> =
LazyLock::new(..)` / `OnceLock`), which is a Go package-level value, not a field of a shared
type.

Allowlist: anything else that is legitimately needed gets an inline marker on the same line,
with the reason, and is reviewed like any N13 exception:

    foo: SomeCache<..>, // N13-allow: <why this cannot change shared metadata>

    scripts/check_model_mutability.py [paths...]   # default: crates/model/src
"""

import re
import sys
from pathlib import Path

NAMES = re.compile(
    r"\b(Cell|RefCell|OnceCell|LazyCell|UnsafeCell|SyncUnsafeCell|Mutex|RwLock|Condvar"
    r"|OnceLock|LazyLock|Once|Atomic[A-Za-z0-9]*|ArcSwap[A-Za-z]*|thread_local)\b"
    r"|\bstatic\s+mut\b"
)
MARKER = "N13-allow:"
STATIC_INIT_ONCE = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?static\s+\w+\s*:\s*(LazyLock|OnceLock)\b")
INIT_ONCE_CONT = re.compile(r"^\s*(LazyLock|OnceLock)::new\b")


def strip_comment(line: str) -> str:
    """The code part of `line` (no `//` comment; string literals are not special-cased)."""
    i = line.find("//")
    return line if i < 0 else line[:i]


def check_file(path: Path) -> list[str]:
    errors = []
    prev_static = False
    in_block_comment = False
    for no, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        code = line
        if in_block_comment:
            end = code.find("*/")
            if end < 0:
                continue
            code = code[end + 2 :]
            in_block_comment = False
        start = code.find("/*")
        if start >= 0 and code.find("*/", start) < 0:
            in_block_comment = True
            code = code[:start]
        code = strip_comment(code)
        stripped = code.strip()
        is_static = bool(STATIC_INIT_ONCE.match(code))
        continuation = prev_static and bool(INIT_ONCE_CONT.match(code))
        prev_static = is_static and not stripped.endswith(";")
        if not stripped or stripped.startswith("use ") or stripped.startswith("pub use "):
            continue
        if not NAMES.search(code) or MARKER in line:
            continue
        if is_static or continuation:
            # Re-check the rest of the line: only the init-once wrapper is allowed.
            rest = re.sub(r"\b(LazyLock|OnceLock)\b", "", code)
            if not NAMES.search(rest):
                continue
        errors.append(f"{path}:{no}: interior mutability in shared metadata: {line.strip()}")
    return errors


def main(argv: list[str]) -> int:
    roots = [Path(a) for a in argv] or [Path(__file__).resolve().parent.parent / "crates/model/src"]
    errors = []
    for root in roots:
        files = [root] if root.is_file() else sorted(root.rglob("*.rs"))
        for f in files:
            errors.extend(check_file(f))
    for e in errors:
        print(e)
    if errors:
        print(
            f"\n{len(errors)} site(s). Shared metadata is changed only by copy on write "
            "(N13 §3 rule 3); add `// N13-allow: <reason>` only for a reviewed exception.",
            file=sys.stderr,
        )
        return 1
    print("model interior-mutability check: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
