// Minimal reproducible probe for D00.3 runtime logging specification
// (specs/D00.3-logging/spec.md). Exercises the exact pinned Go TiDB v8.5.8
// versions of pingcap/log, pingcap/errors, go.uber.org/zap, go.uber.org/multierr,
// and coreos/go-semver to verify byte-level text/JSON encoding, quoting/escaping,
// error/stack formatting, and SemVer version-string comparisons.
//
// Usage:
//   cd tools/log-probe && go run .
package main

import (
	"bytes"
	"errors"
	"fmt"
	"math"
	"strings"
	"time"

	"github.com/coreos/go-semver/semver"
	perrors "github.com/pingcap/errors"
	plog "github.com/pingcap/log"
	"go.uber.org/multierr"
	"go.uber.org/zap"
	"go.uber.org/zap/zapcore"
)

type sampleObj struct {
	fail bool
}

func (s sampleObj) MarshalLogObject(enc zapcore.ObjectEncoder) error {
	enc.AddString("username", "alice")
	enc.AddInt("len", 5)
	if s.fail {
		return errors.New("boom obj")
	}
	return nil
}

type sampleArr struct {
	fail bool
}

func (s sampleArr) MarshalLogArray(enc zapcore.ArrayEncoder) error {
	enc.AppendInt(7)
	if s.fail {
		return errors.New("boom arr")
	}
	return nil
}

type panicStringer struct{}

func (panicStringer) String() string { panic("boom stringer") }

type fixedClock struct{ t time.Time }

func (c fixedClock) Now() time.Time                         { return c.t }
func (c fixedClock) NewTicker(d time.Duration) *time.Ticker { return time.NewTicker(d) }

func newBufLogger(cfg *plog.Config, t time.Time, opts ...zap.Option) (*zap.Logger, *bytes.Buffer) {
	var buf bytes.Buffer
	enc, err := plog.NewTextEncoder(cfg)
	if err != nil {
		panic(err)
	}
	var core zapcore.Core
	if cfg.Format == "json" {
		core = zapcore.NewCore(enc, zapcore.AddSync(&buf), zapcore.DebugLevel)
	} else {
		core = plog.NewTextCore(enc, zapcore.AddSync(&buf), zapcore.DebugLevel)
	}
	baseOpts := []zap.Option{
		zap.WithClock(fixedClock{t: t}),
	}
	if !cfg.DisableCaller {
		baseOpts = append(baseOpts, zap.AddCaller())
	}
	baseOpts = append(baseOpts, opts...)
	return zap.New(core, baseOpts...), &buf
}

func printSection(title string, buf *bytes.Buffer) {
	fmt.Printf("=== %s ===\n", title)
	for i, line := range strings.Split(strings.TrimSuffix(buf.String(), "\n"), "\n") {
		if line == "" {
			continue
		}
		// Assert single-line invariant: no raw CR/LF inside a record.
		if strings.ContainsAny(line, "\r\n") {
			panic(fmt.Sprintf("raw CR/LF inside line %d of %s", i, title))
		}
		fmt.Printf("[%02d] %s\n", i, line)
	}
}

func main() {
	locPlus8 := time.FixedZone("UTC+8", 8*3600)
	locPlus9 := time.FixedZone("UTC+9", 9*3600)
	ts := time.Date(2025, 1, 2, 15, 4, 5, 999_999_999, locPlus8)
	tsUTC := time.Date(2025, 1, 2, 15, 4, 6, 123_456_789, time.UTC)
	tsPlus9 := time.Date(2025, 1, 2, 15, 4, 5, 999_999_999, locPlus9)

	// 1. Text & JSON structure: levels, logger name, empty msg, .With order, disable-timestamp, caller filter.
	for _, format := range []string{"text", "json"} {
		cfg := &plog.Config{Format: format, DisableErrorVerbose: false, DisableCaller: true}
		lg, buf := newBufLogger(cfg, ts)
		lg.Debug("lvl")
		lg.Info("lvl")
		lg.Warn("lvl")
		lg.Error("lvl")
		lg.Named("sub.comp").Info("named msg")
		lg.Named("bad\nname").Info("")
		lg.With(zap.String("bound1", "v1")).With(zap.Int("bound2", 2)).
			Info("with order", zap.String("call1", "c1"), zap.Int("call2", 3))
		printSection("1_"+strings.ToUpper(format)+"_BASIC", buf)
	}

	// DisableTimestamp + DisableCaller (no leading space in text).
	cfgNoTS := &plog.Config{Format: "text", DisableTimestamp: true, DisableCaller: true}
	lgNoTS, bufNoTS := newBufLogger(cfgNoTS, ts)
	lgNoTS.Info("no ts no caller")
	printSection("1_NO_TS", bufNoTS)

	// ShortCallerEncoder filter on unusual path characters.
	var callerBuf bytes.Buffer
	encCaller, err := plog.NewTextEncoder(&plog.Config{Format: "text"})
	if err != nil {
		panic(err)
	}
	b, _ := encCaller.EncodeEntry(zapcore.Entry{
		Level:   zapcore.InfoLevel,
		Time:    ts,
		Message: "caller filter",
		Caller:  zapcore.EntryCaller{Defined: true, File: "/dir/z\\test file+1.go", Line: 99},
	}, nil)
	callerBuf.Write(b.Bytes())
	printSection("1_CALLER_FILTER", &callerBuf)

	// 2. Quoting, escaping, non-UTF-8 (\ufffd), DEL (0x7f), empty key/val.
	for _, format := range []string{"text", "json"} {
		cfg := &plog.Config{Format: format, DisableCaller: true}
		lg, buf := newBufLogger(cfg, ts)
		lg.Info("quote edge",
			zap.String("empty", ""),
			zap.String("", "emptykey"),
			zap.String("del_only", "a\x7fb"),
			zap.String("utf8_ok", "欢迎"),
			zap.String("utf8_sp", "欢迎来到 TiDB"),
			zap.ByteString("bad_str", []byte{0xff, 0xfe, 'a'}),
			zap.ByteString("bad_sp", []byte{0xff, ' ', 'a'}),
			zap.String("ctrl", "a\nb\rc\td\x00\x1be\"\\[]="),
			zap.String("html", "<a>&b"),
		)
		printSection("2_"+strings.ToUpper(format)+"_QUOTING", buf)
	}

	// 3. All field types: float32/64, 1e21, -0, complex64/128, time offset, zap.Times, binary,
	//    nil pointers, reflect (nil/map/err), object (ok/err), inline, stringer, skip, namespace.
	for _, format := range []string{"text", "json"} {
		cfg := &plog.Config{Format: format, DisableCaller: true}
		lg, buf := newBufLogger(cfg, ts)
		var nilStr *string
		var nilErr *perrors.Error
		negZero := math.Copysign(0, -1)
		lg.Info("all fields",
			zap.Stringp("nil_str", nilStr),
			zap.Binary("bin_empty", []byte{}),
			zap.Binary("bin_no_pad", []byte("abc")),
			zap.Binary("bin_pad", []byte("ab123")),
			zap.Float32("f32", 1.1),
			zap.Float32s("f32s", []float32{1.1}),
			zap.Float64("f_1e21", 1e21),
			zap.Float64("f_neg0", negZero),
			zap.Float64("f_nan", math.NaN()),
			zap.Float64("f_pinf", math.Inf(1)),
			zap.Float64("f_ninf", math.Inf(-1)),
			zap.Complex64("c64", complex(float32(1.1), float32(-2.2))),
			zap.Complex128("c128_neg", complex(1, -2)),
			zap.Duration("dur_0", 0),
			zap.Duration("dur_1", 1300*time.Millisecond),
			zap.Duration("dur_neg", -5*time.Millisecond),
			zap.Time("t_plus9", tsPlus9),
			zap.Times("ts_arr", []time.Time{tsPlus9, tsUTC}),
			zap.Strings("strs_simple", []string{"a", "b"}),
			zap.Strings("strs_complex", []string{"a b", `c"d`, "[x]", ""}),
			zap.Strings("strs_empty", []string{}),
			zap.Object("obj_ok", sampleObj{fail: false}),
			zap.Object("obj_err", sampleObj{fail: true}),
			zap.Array("arr_err", sampleArr{fail: true}),
			zap.Inline(sampleObj{fail: false}),
			zap.Reflect("rn", nil),
			zap.Reflect("rs", map[string]string{"k": "<a>&b"}),
			zap.Reflect("rerr", func() {}),
			zap.Stringer("str_panic", panicStringer{}),
			zap.Error(nil),
			zap.Error(nilErr),
			zap.Skip(),
		)
		lg.Info("ns test", zap.Namespace("ns"), zap.String("in_ns", "v"), zap.Int("in_ns_i", 7))
		printSection("3_"+strings.ToUpper(format)+"_FIELDS", buf)
	}

	// 4. Errors, errorVerbose, DisableErrorVerbose (text vs json), FastGen + Trace, multierr, redaction.
	cls := perrors.Normalize("dup entry %s for %s", perrors.RFCCodeText("kv:1062"), perrors.MySQLErrorCode(1062), perrors.RedactArgs([]int{0, 1}))
	fastErr := cls.FastGenByArgs("secret", "k")
	tracedFast := perrors.Trace(fastErr)
	annotatedFast := perrors.Annotate(fastErr, "outer ctx")
	multiErr := multierr.Combine(perrors.New("a"), perrors.New("b c"))

	for _, format := range []string{"text", "json"} {
		for _, disableVerbose := range []bool{true, false} {
			cfg := &plog.Config{Format: format, DisableErrorVerbose: disableVerbose, DisableCaller: true}
			lg, buf := newBufLogger(cfg, ts)
			lg.Info("fastgen", zap.Error(fastErr))
			lg.Info("traced_fast", zap.Error(tracedFast))
			lg.Info("annotated_fast", zap.Error(annotatedFast))
			lg.Info("multierr", zap.Error(multiErr))
			tag := fmt.Sprintf("4_%s_DISABLE_VERBOSE_%v", strings.ToUpper(format), disableVerbose)
			printSection(tag, buf)
		}
	}

	// Verify HasStack & GenWithStack redaction behavior (Go GenWithStack does NOT redact).
	fmt.Println("=== 5_ERROR_REDACT_AND_HASSTACK ===")
	perrors.RedactLogEnabled.Store(perrors.RedactLogEnable)
	fmt.Printf("GenWithStack(ON)       = %s\n", cls.GenWithStack("val=%s", "secret").Error())
	fmt.Printf("GenWithStackByArgs(ON) = %s\n", cls.GenWithStackByArgs("secret", "k").Error())
	fmt.Printf("FastGen(ON)            = %s\n", cls.FastGen("val=%s", "secret").Error())
	fmt.Printf("FastGenByArgs(ON)      = %s\n", cls.FastGenByArgs("secret", "k").Error())
	perrors.RedactLogEnabled.Store(perrors.RedactLogMarker)
	fmt.Printf("GenWithStackByArgs(MK) = %s\n", cls.GenWithStackByArgs("secret", "‹k›").Error())
	perrors.RedactLogEnabled.Store(perrors.RedactLogDisable)
	fmt.Printf("HasStack(FastGenByArgs)        = %v\n", perrors.HasStack(fastErr))
	fmt.Printf("HasStack(Trace(FastGenByArgs)) = %v\n", perrors.HasStack(tracedFast))

	// 6. AddStacktrace(FatalLevel) vs explicit zap.Stack("stack") / zap.Stack("stack trace").
	cfgStack := &plog.Config{Format: "text", DisableCaller: true}
	lgStack, bufStack := newBufLogger(cfgStack, ts, zap.AddStacktrace(zapcore.FatalLevel))
	lgStack.Error("err without auto stack")
	lgStack.Error("panic in the recoverable goroutine", zap.Any("r", "boom"), zap.Stack("stack trace"))
	printSection("6_STACKTRACE", bufStack)

	// 7. SemVer comparison (D1): pre-release vs build metadata vs bare v8.5.8.
	fmt.Println("=== 7_SEMVER_D1 ===")
	base := semver.New("8.5.8")
	for _, s := range []string{"8.5.8", "8.5.8-tidb-rs-abc1234", "8.5.8+tidb-rs.abc1234"} {
		v := semver.New(s)
		fmt.Printf("%-24s pre=%-16q meta=%-16q lessThan(8.5.8)=%v equal(8.5.8)=%v\n",
			s, v.PreRelease, v.Metadata, v.LessThan(*base), v.Equal(*base))
	}
}
