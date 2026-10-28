//! Prometheus text exposition (format 0.0.4) written the way Go's `client_golang`
//! (`expfmt.MetricFamilyToText`) writes it.
//!
//! The `prometheus` crate's own `TextEncoder` formats floats with Rust's `Display`
//! (`0.00004`, `1048576`), while Go uses `strconv.FormatFloat(v, 'g', -1, 64)` (`4e-05`,
//! `1.048576e+06`). Bucket bounds end up in the `le` *label*, and Prometheus 2.x keeps label
//! values as strings: a Go TiDB and tidb-rs exposing the same histogram with differently
//! spelled `le` values would produce different series, and the dashboard's
//! `sum(rate(..._bucket[1m])) by (le)` across instances would no longer line up. So every
//! number is written with Go's formatting ([`go_float`]).

use std::fmt::Write as _;

use prometheus::proto::LabelPair;
use prometheus::proto::Metric;
use prometheus::proto::MetricFamily;
use prometheus::proto::MetricType;

/// Content type of the exposition (Go: `expfmt.FmtText`).
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Write `families` in the text format. Families are written in the given order (the
/// registry's `gather` sorts them by name); the metrics of a family are sorted by their label
/// values, as Go's registry does.
pub fn encode_text(families: &[MetricFamily]) -> String {
    let mut out = String::with_capacity(64 * 1024);
    for mf in families {
        let mut metrics: Vec<&Metric> = mf.get_metric().iter().collect();
        if metrics.is_empty() {
            continue;
        }
        metrics.sort_by(|a, b| label_values(a).cmp(&label_values(b)));
        let name = mf.get_name();
        out.push_str("# HELP ");
        out.push_str(name);
        out.push(' ');
        escape_into(&mut out, mf.get_help(), false);
        out.push('\n');
        let ty = match mf.get_field_type() {
            MetricType::COUNTER => "counter",
            MetricType::GAUGE => "gauge",
            MetricType::HISTOGRAM => "histogram",
            MetricType::SUMMARY => "summary",
            MetricType::UNTYPED => "untyped",
        };
        let _ = writeln!(out, "# TYPE {name} {ty}");
        for m in metrics {
            match mf.get_field_type() {
                MetricType::COUNTER => {
                    sample(&mut out, name, "", m, None, m.get_counter().get_value())
                }
                MetricType::GAUGE => sample(&mut out, name, "", m, None, m.get_gauge().get_value()),
                // Nothing here registers untyped metrics; the accessor is protobuf-specific
                // (deprecated) but the only way to read one.
                #[allow(deprecated)]
                MetricType::UNTYPED => {
                    sample(&mut out, name, "", m, None, m.get_untyped().get_value())
                }
                MetricType::HISTOGRAM => {
                    let h = m.get_histogram();
                    let mut inf_seen = false;
                    for b in h.get_bucket() {
                        let ub = b.get_upper_bound();
                        inf_seen |= ub == f64::INFINITY;
                        sample(
                            &mut out,
                            name,
                            "_bucket",
                            m,
                            Some(("le", ub)),
                            b.get_cumulative_count() as f64,
                        );
                    }
                    if !inf_seen {
                        sample(
                            &mut out,
                            name,
                            "_bucket",
                            m,
                            Some(("le", f64::INFINITY)),
                            h.get_sample_count() as f64,
                        );
                    }
                    sample(&mut out, name, "_sum", m, None, h.get_sample_sum());
                    sample(
                        &mut out,
                        name,
                        "_count",
                        m,
                        None,
                        h.get_sample_count() as f64,
                    );
                }
                MetricType::SUMMARY => {
                    let s = m.get_summary();
                    for q in s.get_quantile() {
                        sample(
                            &mut out,
                            name,
                            "",
                            m,
                            Some(("quantile", q.get_quantile())),
                            q.get_value(),
                        );
                    }
                    sample(&mut out, name, "_sum", m, None, s.get_sample_sum());
                    sample(
                        &mut out,
                        name,
                        "_count",
                        m,
                        None,
                        s.get_sample_count() as f64,
                    );
                }
            }
        }
    }
    out
}

fn label_values(m: &Metric) -> Vec<&str> {
    m.get_label().iter().map(LabelPair::get_value).collect()
}

/// One sample line: `name{labels[,extra]} value`.
fn sample(
    out: &mut String,
    name: &str,
    suffix: &str,
    m: &Metric,
    extra: Option<(&str, f64)>,
    value: f64,
) {
    out.push_str(name);
    out.push_str(suffix);
    let labels = m.get_label();
    if !labels.is_empty() || extra.is_some() {
        out.push('{');
        let mut first = true;
        for l in labels {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(l.get_name());
            out.push_str("=\"");
            escape_into(out, l.get_value(), true);
            out.push('"');
        }
        if let Some((k, v)) = extra {
            if !first {
                out.push(',');
            }
            out.push_str(k);
            out.push_str("=\"");
            out.push_str(&go_float(v));
            out.push('"');
        }
        out.push('}');
    }
    out.push(' ');
    out.push_str(&go_float(value));
    out.push('\n');
}

/// Go's `escapeString`: `\` and newline always, `"` in label values only.
fn escape_into(out: &mut String, s: &str, quote: bool) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '"' if quote => out.push_str("\\\""),
            c => out.push(c),
        }
    }
}

/// A float as Go's `expfmt.writeFloat` writes it: `1`, `0`, `-1`, `NaN`, `+Inf`, `-Inf`, else
/// `strconv.FormatFloat(f, 'g', -1, 64)`: the shortest digits that round-trip, in `%e` form
/// (`d.ddde±XX`, at least two exponent digits) if the decimal exponent is < -4 or >= 6 (Go uses
/// precision 6 for this decision when formatting with the shortest precision), else in `%f`
/// form.
pub fn go_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_owned();
    }
    if f.is_infinite() {
        return if f > 0.0 { "+Inf" } else { "-Inf" }.to_owned();
    }
    if f == 0.0 {
        // Go prints negative zero as "-0" in 'g' format, but writeFloat special-cases 0
        // (which also matches -0.0).
        return "0".to_owned();
    }
    // Rust's `{:e}` gives the shortest round-trip digits: `d[.ddd]e<exp>`.
    let sci = format!("{:e}", f.abs());
    let (mantissa, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let mut out = String::with_capacity(24);
    if f < 0.0 {
        out.push('-');
    }
    if !(-4..6).contains(&exp) {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        let _ = write!(out, "{:02}", exp.unsigned_abs());
    } else if exp < 0 {
        out.push_str("0.");
        for _ in 0..(-exp - 1) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let int_len = exp as usize + 1;
        if digits.len() <= int_len {
            out.push_str(&digits);
            for _ in digits.len()..int_len {
                out.push('0');
            }
        } else {
            out.push_str(&digits[..int_len]);
            out.push('.');
            out.push_str(&digits[int_len..]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(f64 bits, strconv.FormatFloat(v, 'g', -1, 64))`, produced by Go 1.25 for special
    /// values and every bucket layout used by the metrics here.
    const GO_VECTORS: &[(u64, &str)] = &include!("go_float_vectors.txt");

    #[test]
    fn go_float_matches_go_strconv() {
        for &(bits, want) in GO_VECTORS {
            let v = f64::from_bits(bits);
            // writeFloat special-cases these before calling strconv.
            let want = match want {
                "-0" => "0",
                w => w,
            };
            assert_eq!(go_float(v), want, "value {v:?}");
        }
        assert_eq!(go_float(f64::NAN), "NaN");
        assert_eq!(go_float(f64::INFINITY), "+Inf");
        assert_eq!(go_float(f64::NEG_INFINITY), "-Inf");
    }

    #[test]
    fn text_format_matches_go_layout() {
        let r = prometheus::Registry::new();
        let h = prometheus::HistogramVec::new(
            prometheus::HistogramOpts::new("h_seconds", "A \"histogram\".\nSecond line \\.")
                .buckets(vec![0.00004, 1.0, 1048576.0]),
            &["b", "a"],
        )
        .unwrap();
        r.register(Box::new(h.clone())).unwrap();
        h.with_label_values(&["x\"y", "1"]).observe(0.5);
        let c = prometheus::IntCounter::new("c_total", "Counter.").unwrap();
        r.register(Box::new(c.clone())).unwrap();
        c.inc_by(1234567);
        let text = encode_text(&r.gather());
        let want = concat!(
            "# HELP c_total Counter.\n",
            "# TYPE c_total counter\n",
            "c_total 1.234567e+06\n",
            "# HELP h_seconds A \"histogram\".\\nSecond line \\\\.\n",
            "# TYPE h_seconds histogram\n",
            "h_seconds_bucket{a=\"1\",b=\"x\\\"y\",le=\"4e-05\"} 0\n",
            "h_seconds_bucket{a=\"1\",b=\"x\\\"y\",le=\"1\"} 1\n",
            "h_seconds_bucket{a=\"1\",b=\"x\\\"y\",le=\"1.048576e+06\"} 1\n",
            "h_seconds_bucket{a=\"1\",b=\"x\\\"y\",le=\"+Inf\"} 1\n",
            "h_seconds_sum{a=\"1\",b=\"x\\\"y\"} 0.5\n",
            "h_seconds_count{a=\"1\",b=\"x\\\"y\"} 1\n",
        );
        assert_eq!(text, want);
    }
}
