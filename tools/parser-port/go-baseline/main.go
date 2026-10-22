// Go baseline for crates/parser/examples/bench.rs: TiDB's own parser (lex + parse + full AST)
// over the same corpus. Run through bench.sh, which builds it against a TiDB checkout.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/pingcap/tidb/pkg/parser"
	_ "github.com/pingcap/tidb/pkg/parser/test_driver"
)

type Case struct {
	Func   string `json:"func"`
	Line   int    `json:"line"`
	Src    string `json:"src"`
	Ok     bool   `json:"ok"`
	Window bool   `json:"window"`
	Mode   string `json:"mode"`
}

func load(path string) []Case {
	f, _ := os.Open(path)
	defer f.Close()
	var cs []Case
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<24)
	for sc.Scan() {
		var c Case
		if json.Unmarshal(sc.Bytes(), &c) == nil && c.Mode != "" { // skips the provenance line
			cs = append(cs, c)
		}
	}
	return cs
}

func main() {
	cases := load(os.Args[2])
	p := parser.New()
	switch os.Args[1] {
	case "errors": // print TiDB's error for given line numbers (from stdin)
		want := map[int]bool{}
		sc := bufio.NewScanner(os.Stdin)
		for sc.Scan() {
			var n int
			fmt.Sscan(sc.Text(), &n)
			want[n] = true
		}
		for _, c := range cases {
			if !want[c.Line] {
				continue
			}
			p.EnableWindowFunc(c.Window)
			_, _, err := p.Parse(c.Src, "", "")
			msg := "<nil>"
			if err != nil {
				msg = strings.ReplaceAll(err.Error(), "\n", " ")
			}
			fmt.Printf("%d\t%s\n", c.Line, msg)
		}
	case "bench":
		var sel []Case
		bytes := 0
		for _, c := range cases {
			if c.Mode == "default" && c.Ok {
				p.EnableWindowFunc(c.Window)
				if _, _, err := p.Parse(c.Src, "", ""); err == nil {
					sel = append(sel, c)
					bytes += len(c.Src)
				}
			}
		}
		t0 := time.Now()
		n := 0
		for time.Since(t0) < 3*time.Second {
			for _, c := range sel {
				p.EnableWindowFunc(c.Window)
				p.Parse(c.Src, "", "")
			}
			n++
		}
		dt := time.Since(t0).Seconds()
		fmt.Printf("go tidb parser (lex+parse+AST): %d stmts, %.0f stmts/s, %.1f MB/s\n", len(sel), float64(n*len(sel))/dt, float64(n*bytes)/dt/1e6)
	}
}
