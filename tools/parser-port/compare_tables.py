#!/usr/bin/env python3
"""Check that goyacc's LALR automaton (y.output) and grmtools' lrtable automaton are isomorphic.

Usage: python3 -I tools/parser-port/compare_tables.py <goyacc y.output> <lrtable dump>
The lrtable dump is written by crates/parser/build.rs when TIDB_PARSER_DUMP_TABLE=<path> is set
(see regen.sh --check-tables). Exits non-zero on any difference.

Walks both automata in lock-step from the start state; for each paired state compares every
terminal action (shift target pairing / reduce production by text / accept) and every goto.
"""
import re
import sys
from collections import deque


def norm_tok(t):
    if len(t) >= 3 and t[0] == "'" and t[-1] == "'":
        return t[1:-1]
    return t


def load_goyacc(path):
    rules = {}
    states = {}
    cur = None
    item_re = re.compile(r"^\s+(\d+) (\S+): (.*?)(?:\s+\[.*\])?(?:\s+//.*)?$")
    act_re = re.compile(r"^\s{4}(\S+)\s+(shift, and goto state (\d+)|reduce using rule (\d+) \(\S+\)|accept|goto state (\d+))\s*$")
    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.startswith("state "):
                cur = int(line.split()[1])
                states[cur] = ({}, {})
                continue
            if cur is None:
                continue
            m = act_re.match(line)
            if m:
                sym = norm_tok(m.group(1))
                if m.group(3):
                    states[cur][0][sym] = ("shift", int(m.group(3)))
                elif m.group(4):
                    states[cur][0][sym] = ("reduce", int(m.group(4)))
                elif m.group(5):
                    states[cur][1][sym] = int(m.group(5))
                else:
                    states[cur][0][sym] = ("accept", None)
                continue
            m = item_re.match(line)
            if m and " . " in f" {m.group(3)} " or (m and m.group(3).rstrip().endswith(".")):
                n = int(m.group(1))
                rhs = [norm_tok(x) for x in m.group(3).split() if x != "."]
                rules[n] = f"{m.group(2)}: {' '.join(rhs)}".rstrip()
    return states, rules


def load_lrtable(path):
    states = {}
    start = 0
    cur = None
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            if line.startswith("START "):
                start = int(line.split()[1])
            elif line.startswith("S "):
                cur = int(line[2:])
                states[cur] = ({}, {})
            elif line.startswith("T "):
                name, act = line[2:].split("\t")
                if act.startswith("shift "):
                    states[cur][0][name] = ("shift", int(act[6:]))
                elif act.startswith("reduce "):
                    states[cur][0][name] = ("reduce", act[7:].rstrip())
                else:
                    states[cur][0][name] = ("accept", None)
            elif line.startswith("G "):
                name, tgt = line[2:].split("\t")
                states[cur][1][name] = int(tgt)
    return states, start


def main():
    g_states, g_rules = load_goyacc(sys.argv[1])
    r_states, r_start = load_lrtable(sys.argv[2])
    print(f"goyacc states={len(g_states)} rules-seen={len(g_rules)}; lrtable states={len(r_states)}")
    # goyacc state 0 has `$accept: . Start`; lrtable has `^: Start`. goyacc accepts in the state
    # reached by goto(0, Start) on $end; lrtable accepts likewise.
    pair = {0: r_start}
    rpair = {r_start: 0}
    q = deque([0])
    diffs = []
    while q:
        g = q.popleft()
        r = pair[g]
        ga, gg = g_states[g]
        ra, rg = r_states[r]
        for sym in set(ga) | set(ra):
            a, b = ga.get(sym), ra.get(sym)
            if a is None or b is None or a[0] != b[0]:
                diffs.append((g, r, sym, a, b))
                continue
            if a[0] == "reduce":
                if g_rules.get(a[1]) != b[1]:
                    diffs.append((g, r, sym, g_rules.get(a[1]), b[1]))
            elif a[0] == "shift":
                tg, tr = a[1], b[1]
                if tg in pair:
                    if pair[tg] != tr:
                        diffs.append((g, r, sym, ("shift", tg), ("shift", tr, "paired", pair[tg])))
                elif tr in rpair:
                    diffs.append((g, r, sym, ("shift", tg), ("shift-to-already-paired", tr)))
                else:
                    pair[tg], rpair[tr] = tr, tg
                    q.append(tg)
        gg2 = {k: v for k, v in gg.items()}
        rg2 = {k: v for k, v in rg.items() if k != "^"}
        for sym in set(gg2) | set(rg2):
            tg, tr = gg2.get(sym), rg2.get(sym)
            if tg is None or tr is None:
                diffs.append((g, r, "goto " + sym, tg, tr))
                continue
            if tg in pair:
                if pair[tg] != tr:
                    diffs.append((g, r, "goto " + sym, tg, (tr, "paired", pair[tg])))
            elif tr in rpair:
                diffs.append((g, r, "goto " + sym, tg, ("already-paired", tr)))
            else:
                pair[tg], rpair[tr] = tr, tg
                q.append(tg)
    print(f"paired states: {len(pair)} / {len(g_states)}")
    unpaired_g = sorted(set(g_states) - set(pair))
    unpaired_r = sorted(set(r_states) - set(rpair))
    print(f"unpaired goyacc states: {unpaired_g[:10]}  unpaired lrtable states: {unpaired_r[:10]}")
    for u in unpaired_g[:3]:
        refs = [s for s, (a, gt) in g_states.items()
                if any(v == ("shift", u) for v in a.values()) or u in gt.values()]
        print(f"  goyacc state {u} referenced from: {refs[:5]}")
    print(f"differences: {len(diffs)}")
    for d in diffs[:30]:
        print("  ", d)
    # States the walk from the start state cannot pair are unreachable in that automaton (any
    # reachable state gets paired or produces a difference). At 4cee7f2084 both sides have one
    # such state (the same `placement . policy` item set), which is harmless.
    if diffs or len(g_states) != len(r_states):
        sys.exit(1)
    print("OK: automata are isomorphic on all reachable states")


if __name__ == "__main__":
    main()
