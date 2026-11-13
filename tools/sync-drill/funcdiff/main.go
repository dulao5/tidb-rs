// funcdiff maps the upstream Go diff between two TiDB revisions onto functions, for the Go
// files that hold functions tidb-rs has ported (tools/sync-drill/ported.py).
//
//	go run . -repo /tmp/tidb-up -base v8.5.8 -target origin/release-8.5 \
//	    -ported ported.tsv -out out/
//
// A "unit" is a top-level func (symbol as in the tracker: Func or Type.Method), or, for a
// ported package-level var (defaultSysVars, noopSysVars ...), one element of its composite
// literal ("defaultSysVars{TiDBGCEnable}"), else the whole var. For every unit touched by
// some commit on the first-parent path base..target it reports added / deleted lines
// (git diff -U0 of base..target), whether the change is cosmetic (the gofmt-printed unit
// without comments is identical), and the commits that touched it (each commit's own diff
// mapped onto its parent's version of the file).
//
// Output: <out>/units.tsv (changed units, ported or not, in the ported files),
// <out>/commits.tsv (commits touching the ported files), <out>/files.tsv (per file).
package main

import (
	"bufio"
	"bytes"
	"flag"
	"fmt"
	"go/ast"
	"go/parser"
	"go/printer"
	"go/token"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
)

var (
	repo   = flag.String("repo", "", "upstream pingcap/tidb clone")
	base   = flag.String("base", "v8.5.8", "base revision")
	target = flag.String("target", "origin/release-8.5", "target revision")
	ported = flag.String("ported", "", "ported.tsv from ported.py")
	outDir = flag.String("out", "out", "output directory")
)

func git(args ...string) string {
	cmd := exec.Command("git", append([]string{"-C", *repo}, args...)...)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	if err != nil {
		fail("git %s: %v: %s", strings.Join(args, " "), err, stderr.String())
	}
	return string(out)
}

func gitShow(rev, file string) ([]byte, bool) {
	cmd := exec.Command("git", "-C", *repo, "show", rev+":"+file)
	out, err := cmd.Output()
	return out, err == nil
}

func fail(f string, a ...any) {
	fmt.Fprintf(os.Stderr, "funcdiff: "+f+"\n", a...)
	os.Exit(1)
}

type unit struct {
	sym        string
	start, end int    // 1-based lines, inclusive (doc comment included)
	norm       string // printed without comments
}

// units parses src and returns its units; vars names the package-level vars split by element.
func units(src []byte, vars map[string]bool) []unit {
	fset := token.NewFileSet()
	f, err := parser.ParseFile(fset, "x.go", src, parser.ParseComments)
	if err != nil {
		fail("parse: %v", err)
	}
	print := func(n ast.Node) string {
		var b bytes.Buffer
		printer.Fprint(&b, token.NewFileSet(), stripPos(n))
		return b.String()
	}
	line := func(p token.Pos) int { return fset.Position(p).Line }
	var out []unit
	seen := map[string]int{}
	add := func(u unit) {
		if n := seen[u.sym]; n > 0 { // e.g. several init()
			u.sym = fmt.Sprintf("%s#%d", u.sym, n+1)
		}
		seen[strings.SplitN(u.sym, "#", 2)[0]]++
		out = append(out, u)
	}
	for _, d := range f.Decls {
		switch d := d.(type) {
		case *ast.FuncDecl:
			start := d.Pos()
			if d.Doc != nil {
				start = d.Doc.Pos()
			}
			add(unit{sym: funcSym(d), start: line(start), end: line(d.End()), norm: print(d)})
		case *ast.GenDecl:
			if d.Tok != token.VAR {
				continue
			}
			for _, s := range d.Specs {
				vs := s.(*ast.ValueSpec)
				for i, name := range vs.Names {
					if !vars[name.Name] || i >= len(vs.Values) {
						continue
					}
					lit, ok := vs.Values[i].(*ast.CompositeLit)
					if !ok {
						add(unit{sym: name.Name, start: line(vs.Pos()), end: line(vs.End()), norm: print(vs)})
						continue
					}
					for _, e := range lit.Elts {
						add(unit{sym: name.Name + "{" + eltName(e, print) + "}", start: line(e.Pos()), end: line(e.End()), norm: print(e)})
					}
				}
			}
		}
	}
	return out
}

// eltName names a composite literal element: its Name: field, else its key, else its text.
func eltName(e ast.Expr, print func(ast.Node) string) string {
	if kv, ok := e.(*ast.KeyValueExpr); ok {
		return print(kv.Key)
	}
	if u, ok := e.(*ast.UnaryExpr); ok {
		e = u.X
	}
	if cl, ok := e.(*ast.CompositeLit); ok {
		for _, f := range cl.Elts {
			if kv, ok := f.(*ast.KeyValueExpr); ok {
				if id, ok := kv.Key.(*ast.Ident); ok && id.Name == "Name" {
					return print(kv.Value)
				}
			}
		}
	}
	s := print(e)
	if len(s) > 40 {
		s = s[:40]
	}
	return s
}

func funcSym(d *ast.FuncDecl) string {
	if d.Recv == nil || len(d.Recv.List) == 0 {
		return d.Name.Name
	}
	t := d.Recv.List[0].Type
	for {
		switch x := t.(type) {
		case *ast.StarExpr:
			t = x.X
			continue
		case *ast.IndexExpr:
			t = x.X
			continue
		case *ast.IndexListExpr:
			t = x.X
			continue
		case *ast.ParenExpr:
			t = x.X
			continue
		}
		break
	}
	if id, ok := t.(*ast.Ident); ok {
		return id.Name + "." + d.Name.Name
	}
	return d.Name.Name
}

// stripPos drops comments and positions so that printing is layout-independent.
func stripPos(n ast.Node) ast.Node {
	ast.Inspect(n, func(n ast.Node) bool {
		switch x := n.(type) {
		case *ast.FuncDecl:
			x.Doc = nil
		case *ast.Field:
			x.Doc, x.Comment = nil, nil
		case *ast.ValueSpec:
			x.Doc, x.Comment = nil, nil
		case *ast.TypeSpec:
			x.Doc, x.Comment = nil, nil
		case *ast.GenDecl:
			x.Doc = nil
		}
		return true
	})
	return n
}

func methodName(sym string) string {
	if i := strings.LastIndexByte(sym, '.'); i >= 0 {
		return sym[i+1:]
	}
	return sym
}

// lcs is the length of the longest common subsequence of two line slices.
func lcs(a, b []string) int {
	prev, cur := make([]int, len(b)+1), make([]int, len(b)+1)
	for i := range a {
		for j := range b {
			switch {
			case a[i] == b[j]:
				cur[j+1] = prev[j] + 1
			case cur[j] >= prev[j+1]:
				cur[j+1] = cur[j]
			default:
				cur[j+1] = prev[j+1]
			}
		}
		prev, cur = cur, prev
	}
	return prev[len(b)]
}

// similarity of two printed units: 2*LCS(lines) / (len(a)+len(b)).
func similarity(a, b string) float64 {
	x, y := strings.Split(a, "\n"), strings.Split(b, "\n")
	return 2 * float64(lcs(x, y)) / float64(len(x)+len(y))
}

func unitAt(us []unit, l int) string {
	for _, u := range us {
		if l >= u.start && l <= u.end {
			return u.sym
		}
	}
	return ""
}

var hunkRE = regexp.MustCompile(`^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@`)

type hunkLines struct{ del, add []int } // old-side deleted lines, new-side added lines

func parseDiff(d string) hunkLines {
	var h hunkLines
	var o, n int
	sc := bufio.NewScanner(strings.NewReader(d))
	sc.Buffer(make([]byte, 1<<20), 1<<26)
	for sc.Scan() {
		l := sc.Text()
		if m := hunkRE.FindStringSubmatch(l); m != nil {
			o, _ = strconv.Atoi(m[1])
			n, _ = strconv.Atoi(m[3])
			continue
		}
		if strings.HasPrefix(l, "---") || strings.HasPrefix(l, "+++") || o == 0 && n == 0 {
			continue
		}
		switch {
		case strings.HasPrefix(l, "-"):
			h.del = append(h.del, o)
			o++
		case strings.HasPrefix(l, "+"):
			h.add = append(h.add, n)
			n++
		}
	}
	return h
}

type portedFn struct{ block string }

type stat struct {
	add, del   int
	commits    []string
	oldN, newN int
	status     string
}

func main() {
	flag.Parse()
	if *repo == "" || *ported == "" {
		fail("need -repo and -ported")
	}
	baseSHA := strings.TrimSpace(git("rev-parse", *base+"^{commit}"))
	targetSHA := strings.TrimSpace(git("rev-parse", *target+"^{commit}"))

	// ported (file, symbol) -> block; vars per file
	ported_ := map[string]map[string]portedFn{}
	vars := map[string]map[string]bool{}
	pf, err := os.ReadFile(*ported)
	if err != nil {
		fail("%v", err)
	}
	for i, l := range strings.Split(strings.TrimSpace(string(pf)), "\n") {
		c := strings.Split(l, "\t")
		if i == 0 || len(c) < 3 {
			continue
		}
		if ported_[c[0]] == nil {
			ported_[c[0]] = map[string]portedFn{}
			vars[c[0]] = map[string]bool{}
		}
		ported_[c[0]][c[1]] = portedFn{block: c[2]}
		if c[2] == "-" {
			vars[c[0]][c[1]] = true
		}
	}
	files := make([]string, 0, len(ported_))
	for f := range ported_ {
		files = append(files, f)
	}
	sort.Strings(files)

	isPorted := func(file, sym string) (string, bool) {
		s := strings.SplitN(sym, "#", 2)[0]
		if p, ok := ported_[file][s]; ok {
			return p.block, true
		}
		if i := strings.IndexByte(s, '{'); i > 0 { // element of a ported var
			if _, ok := ported_[file][s[:i]]; ok {
				return "-", true
			}
		}
		return "", false
	}

	// Renames along the path (a ported file moved upstream).
	renamed := map[string]string{}
	for _, l := range strings.Split(git("diff", "--name-status", "-M", baseSHA, targetSHA, "--", "pkg"), "\n") {
		c := strings.Split(l, "\t")
		if len(c) == 3 && strings.HasPrefix(c[0], "R") {
			renamed[c[1]] = c[2]
		}
	}

	type key struct{ file, sym string }
	stats := map[key]*stat{}
	get := func(k key) *stat {
		if stats[k] == nil {
			stats[k] = &stat{}
		}
		return stats[k]
	}
	fileOut := map[string][2]int{} // file -> lines changed outside any unit (del, add)
	renamedTo := map[key]string{}  // removed ported unit -> the added unit it became

	// 1. cumulative diff base..target
	for _, f := range files {
		nf := f
		if r, ok := renamed[f]; ok {
			nf = r
		}
		oldSrc, ok1 := gitShow(baseSHA, f)
		newSrc, ok2 := gitShow(targetSHA, nf)
		if !ok1 {
			fail("%s missing at base", f)
		}
		ou := units(oldSrc, vars[f])
		var nu []unit
		if ok2 {
			nu = units(newSrc, vars[f])
		}
		d := git("diff", "-U0", "-M", baseSHA, targetSHA, "--", f, nf)
		h := parseDiff(d)
		if !ok2 { // file deleted
			h.del = nil
			for i := 1; i <= bytes.Count(oldSrc, []byte("\n")); i++ {
				h.del = append(h.del, i)
			}
		}
		out := fileOut[f]
		for _, l := range h.del {
			if s := unitAt(ou, l); s != "" {
				get(key{f, s}).del++
			} else {
				out[0]++
			}
		}
		for _, l := range h.add {
			if s := unitAt(nu, l); s != "" {
				get(key{f, s}).add++
			} else {
				out[1]++
			}
		}
		fileOut[f] = out
		om, nm := map[string]unit{}, map[string]unit{}
		for _, u := range ou {
			om[u.sym] = u
		}
		for _, u := range nu {
			nm[u.sym] = u
		}
		for k, s := range stats {
			if k.file != f {
				continue
			}
			o, okO := om[k.sym]
			n, okN := nm[k.sym]
			if okO {
				s.oldN = o.end - o.start + 1
			}
			if okN {
				s.newN = n.end - n.start + 1
			}
			switch {
			case !okO:
				s.status = "added"
			case !okN:
				s.status = "removed"
			case o.norm == n.norm:
				s.status = "cosmetic"
			default:
				s.status = "changed"
			}
		}
		// A removed ported unit whose body reappears under a new name (receiver type or
		// function renamed): pair it with the most similar added unit of the same file.
		for k, s := range stats {
			if k.file != f || s.status != "removed" {
				continue
			}
			if _, ok := isPorted(f, k.sym); !ok {
				continue
			}
			best, bestSim := "", 0.4
			for k2, s2 := range stats {
				if k2.file != f || s2.status != "added" || methodName(k2.sym) != methodName(k.sym) && strings.Contains(k.sym, ".") {
					continue
				}
				if sim := similarity(om[k.sym].norm, nm[k2.sym].norm); sim > bestSim {
					best, bestSim = k2.sym, sim
				}
			}
			if best == "" {
				continue
			}
			s2 := stats[key{f, best}]
			o, n := strings.Split(om[k.sym].norm, "\n"), strings.Split(nm[best].norm, "\n")
			s.status = "renamed"
			if lcs(o[1:], n[1:]) != len(o)-1 || len(o) != len(n) { // more than the signature line differs
				s.status = "renamed+changed"
			}
			s.newN, s.add = s2.newN, s.add+s2.add
			renamedTo[k] = best
			delete(stats, key{f, best})
		}
	}

	// 2. per commit (first parent path), attributed on the parent's version
	type commitInfo struct {
		sha, date, subject string
		files              []string
		portedSyms         []string
	}
	var commits []commitInfo
	logOut := git(append([]string{"log", "--first-parent", "--reverse", "--format=%H\t%cs\t%s", baseSHA + ".." + targetSHA, "--"}, files...)...)
	for _, l := range strings.Split(strings.TrimSpace(logOut), "\n") {
		if l == "" {
			continue
		}
		c := strings.SplitN(l, "\t", 3)
		ci := commitInfo{sha: c[0], date: c[1], subject: c[2]}
		touched := map[string]bool{}
		for _, f := range files {
			d := git("diff", "-U0", c[0]+"^", c[0], "--", f)
			if d == "" {
				continue
			}
			ci.files = append(ci.files, f)
			h := parseDiff(d)
			if src, ok := gitShow(c[0]+"^", f); ok {
				for _, l := range h.del {
					if s := unitAt(units(src, vars[f]), l); s != "" {
						touched[f+"\t"+s] = true
					}
				}
			}
			if src, ok := gitShow(c[0], f); ok {
				us := units(src, vars[f])
				for _, l := range h.add {
					if s := unitAt(us, l); s != "" {
						touched[f+"\t"+s] = true
					}
				}
			}
		}
		for t := range touched {
			p := strings.SplitN(t, "\t", 2)
			k := key{p[0], p[1]}
			if st, ok := stats[k]; ok {
				st.commits = append(st.commits, c[0][:10])
			}
			if _, ok := isPorted(p[0], p[1]); ok {
				ci.portedSyms = append(ci.portedSyms, p[1])
			}
		}
		sort.Strings(ci.portedSyms)
		commits = append(commits, ci)
	}

	os.MkdirAll(*outDir, 0o755)
	w := func(name string, rows [][]string) {
		var b strings.Builder
		for _, r := range rows {
			b.WriteString(strings.Join(r, "\t") + "\n")
		}
		if err := os.WriteFile(filepath.Join(*outDir, name), []byte(b.String()), 0o644); err != nil {
			fail("%v", err)
		}
	}
	keys := make([]key, 0, len(stats))
	for k := range stats {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(i, j int) bool {
		if keys[i].file != keys[j].file {
			return keys[i].file < keys[j].file
		}
		return keys[i].sym < keys[j].sym
	})
	rows := [][]string{{"ported", "block", "go_file", "symbol", "status", "old_lines", "new_lines", "add", "del", "commits", "renamed_to"}}
	nPorted, nPortedBeh, linesPorted := 0, 0, 0
	for _, k := range keys {
		s := stats[k]
		block, ok := isPorted(k.file, k.sym)
		p := "no"
		if ok {
			p = "yes"
			nPorted++
			if s.status != "cosmetic" {
				nPortedBeh++
			}
			linesPorted += s.add + s.del
		}
		rows = append(rows, []string{p, block, k.file, k.sym, s.status, strconv.Itoa(s.oldN), strconv.Itoa(s.newN),
			strconv.Itoa(s.add), strconv.Itoa(s.del), strings.Join(s.commits, ","), renamedTo[k]})
	}
	w("units.tsv", rows)
	rows = [][]string{{"commit", "date", "subject", "files", "ported_units"}}
	for _, c := range commits {
		rows = append(rows, []string{c.sha[:10], c.date, c.subject, strings.Join(c.files, ","), strings.Join(c.portedSyms, ",")})
	}
	w("commits.tsv", rows)
	rows = [][]string{{"go_file", "ported_units", "renamed_to", "outside_del", "outside_add"}}
	for _, f := range files {
		rows = append(rows, []string{f, strconv.Itoa(len(ported_[f])), renamed[f], strconv.Itoa(fileOut[f][0]), strconv.Itoa(fileOut[f][1])})
	}
	w("files.tsv", rows)
	fmt.Printf("base %s target %s: %d commits on the first-parent path touch %d ported files; %d ported units changed (%d not cosmetic), %d lines\n",
		baseSHA[:10], targetSHA[:10], len(commits), len(files), nPorted, nPortedBeh, linesPorted)
}
