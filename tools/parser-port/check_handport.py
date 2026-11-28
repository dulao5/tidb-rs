#!/usr/bin/env python3
"""Check that the committed grammar carries every hand-ported action of handport.py verbatim.

crates/parser/grammar/tidb_typed.y is generated (tools/parser-port/regen.sh) from a TiDB
parser.y and the hand-ported actions of handport.py. A regen needs the TiDB checkout the
grammar was generated from, which CI does not have; this check needs only the two files and
catches the two ways they drift apart: an action edited in the grammar alone (a regen would
undo it) and an action edited in handport.py without a regen.

    python3 tools/parser-port/check_handport.py        exit 1 and list the actions that differ
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import handport  # noqa: E402

GRAMMAR = os.path.join(HERE, "..", "..", "crates", "parser", "grammar", "tidb_typed.y")


def main():
    with open(GRAMMAR, encoding="utf-8") as f:
        grammar = f.read()
    missing = [k for k, code in handport.ACTIONS.items() if code.strip() not in grammar]
    if handport.PROGRAMS.strip() not in grammar:
        missing.append(("<programs>", ""))
    for rule, syms in missing:
        print(f"check_handport: the action of {rule}: {syms!r} in handport.py is not in "
              f"tidb_typed.y verbatim (edit handport.py and regenerate, or make the same edit "
              f"in both)")
    print(f"check_handport: {len(handport.ACTIONS)} actions, {len(missing)} differ")
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
