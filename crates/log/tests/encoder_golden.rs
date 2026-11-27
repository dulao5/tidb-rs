//! Golden Matrix 1: Byte-level text & JSON encoder parity tests against Go v8.5.8
//! (`tools/log-probe/` + `specs/D00.3-logging/spec.md` §1, §8).

use chrono::{DateTime, FixedOffset, TimeZone};
use tidb_base::structured_log::{LogField, LogLevel, LogTime, LogValue};
use tidb_log::encoder::{self, EncoderConfig, Entry};

#[allow(clippy::too_many_arguments)]
fn fixed_dt(
    y: i32,
    m: u32,
    d: u32,
    hh: u32,
    mm: u32,
    ss: u32,
    nanos: u32,
    offset_secs: i32,
) -> DateTime<FixedOffset> {
    let off = FixedOffset::east_opt(offset_secs).unwrap();
    off.with_ymd_and_hms(y, m, d, hh, mm, ss)
        .unwrap()
        .with_timezone(&off)
        + chrono::Duration::nanoseconds(i64::from(nanos))
}

fn assert_single_line(bytes: &[u8]) {
    assert_eq!(bytes.last(), Some(&b'\n'), "must end with single \\n");
    let body = &bytes[..bytes.len() - 1];
    assert!(
        !body.contains(&b'\n'),
        "raw \\n inside line: {:?}",
        String::from_utf8_lossy(body)
    );
    assert!(
        !body.contains(&b'\r'),
        "raw \\r inside line: {:?}",
        String::from_utf8_lossy(body)
    );
}

#[test]
fn matrix1_text_basic_levels_caller_filter_empty_msg_and_bound_order() {
    // 2025-01-02 15:04:05.999999999 +08:00 -> millis truncated to .999 (never rounded).
    let ts = fixed_dt(2025, 1, 2, 15, 4, 5, 999_999_999, 8 * 3600);
    let cfg = EncoderConfig::default();

    for (lvl, tag) in [
        (LogLevel::Debug, "DEBUG"),
        (LogLevel::Info, "INFO"),
        (LogLevel::Warn, "WARN"),
        (LogLevel::Error, "ERROR"),
        (LogLevel::DPanic, "DPANIC"),
        (LogLevel::Panic, "PANIC"),
        (LogLevel::Fatal, "FATAL"),
    ] {
        let mut out = Vec::new();
        let entry = Entry {
            time: ts,
            level: lvl,
            logger_name: "",
            file: "src/main.rs",
            line: 42,
            msg: "hello",
            bound_fields: &[],
            fields: &[],
            stack: "",
        };
        encoder::text::encode_entry(cfg, &entry, &mut out);
        assert_single_line(&out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("[2025/01/02 15:04:05.999 +08:00] [{tag}] [main.rs:42] [hello]\n")
        );
    }

    // Caller filter: /dir/z_test file+1.rs:99 -> [z_testfile1.rs:99] (basename after last `/`
    // only, then ASCII [A-Za-z0-9._-] only — backslashes are stripped, not treated as separators).
    let mut out = Vec::new();
    let entry = Entry {
        time: ts,
        level: LogLevel::Info,
        logger_name: "bad\nname",
        file: r"/dir/z\test file+1.rs",
        line: 99,
        msg: "", // empty msg omitted completely in text mode!
        bound_fields: &[
            LogField::new("bound1", "v1"),
            LogField::new("bound2", 2_i64),
        ],
        fields: &[LogField::new("call1", "c1"), LogField::new("call2", 3_i64)],
        stack: "",
    };
    encoder::text::encode_entry(cfg, &entry, &mut out);
    assert_single_line(&out);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "[2025/01/02 15:04:05.999 +08:00] [INFO] [\"bad\\nname\"] [ztestfile1.rs:99] [bound1=v1] [bound2=2] [call1=c1] [call2=3]\n"
    );

    // disable-timestamp = true: NO leading space before [INFO].
    let mut out = Vec::new();
    let no_ts_cfg = EncoderConfig {
        disable_timestamp: true,
        disable_caller: true,
        ..EncoderConfig::default()
    };
    let entry = Entry {
        time: ts,
        level: LogLevel::Info,
        logger_name: "",
        file: "ignored.rs",
        line: 1,
        msg: "no ts no caller",
        bound_fields: &[],
        fields: &[],
        stack: "",
    };
    encoder::text::encode_entry(no_ts_cfg, &entry, &mut out);
    assert_single_line(&out);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "[INFO] [\"no ts no caller\"]\n"
    );
}

#[test]
fn matrix1_quoting_escaping_del_and_invalid_utf8_bytes() {
    let ts = fixed_dt(2025, 1, 2, 0, 0, 0, 0, 0);
    let cfg = EncoderConfig {
        disable_timestamp: true,
        disable_caller: true,
        ..EncoderConfig::default()
    };

    let del_only: &[u8] = b"a\x7fb";
    let bad_no_space: &[u8] = &[0xff, 0xfe, b'a'];
    let bad_with_space: &[u8] = &[0xff, b' ', b'a'];
    let ctrl: &[u8] = b"x\x00\x1by\tz\r\n\\\"[]=";

    let fields = [
        LogField::new("empty", ""),
        LogField::new("", "emptykey"),
        LogField::new("utf8_clean", "欢迎"),
        LogField::new("utf8_space", "欢迎来到 TiDB 💖"),
        LogField {
            key: "del_only",
            value: LogValue::Bytes(del_only),
        },
        LogField {
            key: "bad_str",
            value: LogValue::Bytes(bad_no_space),
        },
        LogField {
            key: "bad_sp",
            value: LogValue::Bytes(bad_with_space),
        },
        LogField {
            key: "ctrl",
            value: LogValue::Bytes(ctrl),
        },
    ];
    let entry = Entry {
        time: ts,
        level: LogLevel::Info,
        logger_name: "",
        file: "",
        line: 0,
        msg: "q",
        bound_fields: &[],
        fields: &fields,
        stack: "",
    };

    let mut text_out = Vec::new();
    encoder::text::encode_entry(cfg, &entry, &mut text_out);
    assert_single_line(&text_out);
    // Notice: del_only keeps raw 0x7f unquoted, bad_str is unquoted `\ufffd\ufffda`,
    // bad_sp is quoted `"\ufffd a"`.
    let expected_text = b"[INFO] [q] [empty=] [=emptykey] [utf8_clean=\xe6\xac\xa2\xe8\xbf\x8e] [utf8_space=\"\xe6\xac\xa2\xe8\xbf\x8e\xe6\x9d\xa5\xe5\x88\xb0 TiDB \xf0\x9f\x92\x96\"] [del_only=a\x7fb] [bad_str=\\ufffd\\ufffda] [bad_sp=\"\\ufffd a\"] [ctrl=\"x\\u0000\\u001by\\tz\\r\\n\\\\\\\"[]=\"]\n";
    assert_eq!(text_out.as_slice(), expected_text.as_slice());

    let mut json_out = Vec::new();
    encoder::json::encode_entry(cfg, &entry, &mut json_out);
    assert_single_line(&json_out);
    let json_str = String::from_utf8(json_out).unwrap();
    assert_eq!(
        json_str,
        "{\"level\":\"INFO\",\"message\":\"q\",\"empty\":\"\",\"\":\"emptykey\",\
         \"utf8_clean\":\"欢迎\",\"utf8_space\":\"欢迎来到 TiDB 💖\",\
         \"del_only\":\"a\x7fb\",\"bad_str\":\"\\ufffd\\ufffda\",\"bad_sp\":\"\\ufffd a\",\
         \"ctrl\":\"x\\u0000\\u001by\\tz\\r\\n\\\\\\\"[]=\"}\n"
    );
}

#[test]
fn matrix1_all_field_types_text_and_json_and_r34_skip() {
    let ts = fixed_dt(2025, 1, 2, 15, 4, 5, 999_999_999, 8 * 3600);
    let t_plus9 = LogTime {
        secs: 1_735_797_845, // 2025-01-02 06:04:05 UTC = 15:04:05 +09:00
        nanos: 999_999_999,
        offset_secs: 9 * 3600,
    };
    let t_utc = LogTime {
        secs: 1_735_797_846,
        nanos: 123_456_789,
        offset_secs: 0,
    };
    let times = [t_plus9, t_utc];
    let strs = ["a", "b"];
    let strs_complex = ["a b", "c\"d", "[x]", ""];
    let empty_strs: [&str; 0] = [];
    let u64s = [1_u64, 4, 5];
    let bools = [true, false];
    let f32s = [1.1_f32];
    let neg_zero = (-0.0_f64).copysign(-1.0);

    let fields = [
        LogField {
            key: "null_f",
            value: LogValue::Null,
        },
        LogField {
            key: "skipped",
            value: LogValue::Skip, // R34 (#454): omitted completely
        },
        LogField::new("b_true", true),
        LogField::new("b_false", false),
        LogField::new("int", -42_i64),
        LogField::new("uint64", u64::MAX),
        LogField::new("f32", 1.1_f32),
        LogField::new("f32s", &f32s[..]),
        LogField::new("f_big", 1e21_f64),
        LogField::new("f_neg0", neg_zero),
        LogField::new("f_nan", f64::NAN),
        LogField::new("f_pinf", f64::INFINITY),
        LogField::new("f_ninf", f64::NEG_INFINITY),
        LogField {
            key: "bin_no_pad",
            value: LogValue::Binary(b"abc"),
        },
        LogField {
            key: "bin_pad",
            value: LogValue::Binary(b"ab123"),
        },
        LogField {
            key: "dur_0",
            value: LogValue::Duration(0),
        },
        LogField {
            key: "dur_1",
            value: LogValue::Duration(1_300_000_000),
        },
        LogField {
            key: "dur_neg",
            value: LogValue::Duration(-5_000_000),
        },
        LogField::new("t_plus9", t_plus9),
        LogField::new("ts_arr", &times[..]),
        LogField::new("strs_simple", &strs[..]),
        LogField::new("strs_complex", &strs_complex[..]),
        LogField::new("strs_empty", &empty_strs[..]),
        LogField::new("u64s", &u64s[..]),
        LogField::new("bools", &bools[..]),
        LogField::new("html", "<a>&b"),
    ];

    let cfg = EncoderConfig {
        disable_timestamp: false,
        disable_caller: true,
        ..EncoderConfig::default()
    };
    let entry = Entry {
        time: ts,
        level: LogLevel::Info,
        logger_name: "",
        file: "",
        line: 0,
        msg: "",
        bound_fields: &[],
        fields: &fields,
        stack: "",
    };

    let mut text_out = Vec::new();
    encoder::text::encode_entry(cfg, &entry, &mut text_out);
    assert_single_line(&text_out);
    let text_s = String::from_utf8(text_out).unwrap();
    assert_eq!(
        text_s,
        concat!(
            "[2025/01/02 15:04:05.999 +08:00] [INFO]",
            " [null_f=null]",
            " [b_true=true] [b_false=false]",
            " [int=-42] [uint64=18446744073709551615]",
            " [f32=1.100000023841858] [f32s=\"[1.1]\"]",
            " [f_big=1000000000000000000000] [f_neg0=-0]",
            " [f_nan=NaN] [f_pinf=+Inf] [f_ninf=-Inf]",
            " [bin_no_pad=YWJj] [bin_pad=\"YWIxMjM=\"]",
            " [dur_0=0s] [dur_1=1.3s] [dur_neg=-5ms]",
            " [t_plus9=2025/01/02 15:04:05.999 +09:00]",
            " [ts_arr=\"[2025/01/02 15:04:05.999 +09:002025/01/02 06:04:06.123 +00:00]\"]",
            " [strs_simple=\"[a,b]\"]",
            " [strs_complex=\"[\\\"a b\\\",\\\"c\\\\\\\"d\\\",\\\"[x]\\\",]\"]",
            " [strs_empty=\"[]\"]",
            " [u64s=\"[1,4,5]\"]",
            " [bools=\"[true,false]\"]",
            " [html=<a>&b]\n",
        )
    );

    let mut json_out = Vec::new();
    encoder::json::encode_entry(cfg, &entry, &mut json_out);
    assert_single_line(&json_out);
    let json_s = String::from_utf8(json_out).unwrap();
    assert_eq!(
        json_s,
        concat!(
            "{\"level\":\"INFO\",\"time\":\"2025/01/02 15:04:05.999 +08:00\",\"message\":\"\",",
            "\"null_f\":null,",
            "\"b_true\":true,\"b_false\":false,",
            "\"int\":-42,\"uint64\":18446744073709551615,",
            "\"f32\":1.1,\"f32s\":[1.1],",
            "\"f_big\":1000000000000000000000,\"f_neg0\":-0,",
            "\"f_nan\":\"NaN\",\"f_pinf\":\"+Inf\",\"f_ninf\":\"-Inf\",",
            "\"bin_no_pad\":\"YWJj\",\"bin_pad\":\"YWIxMjM=\",",
            "\"dur_0\":\"0s\",\"dur_1\":\"1.3s\",\"dur_neg\":\"-5ms\",",
            "\"t_plus9\":\"2025/01/02 15:04:05.999 +09:00\",",
            "\"ts_arr\":[\"2025/01/02 15:04:05.999 +09:00\",\"2025/01/02 06:04:06.123 +00:00\"],",
            "\"strs_simple\":[\"a\",\"b\"],",
            "\"strs_complex\":[\"a b\",\"c\\\"d\",\"[x]\",\"\"],",
            "\"strs_empty\":[],",
            "\"u64s\":[1,4,5],",
            "\"bools\":[true,false],",
            "\"html\":\"<a>&b\"}\n",
        )
    );
}

#[test]
fn objects_array_matches_go_text_and_json_probe() {
    let ts = DateTime::parse_from_rfc3339("2025-01-02T15:04:05.999+08:00").unwrap();
    let cfg = EncoderConfig {
        disable_timestamp: true,
        disable_caller: false,
        ..EncoderConfig::default()
    };

    let ev0 = [
        LogField::new("event", "enter txn"),
        LogField::new("gap", std::time::Duration::ZERO),
    ];
    let ev1 = [
        LogField::new("event", "SELECT"),
        LogField::new("gap", std::time::Duration::from_millis(12)),
    ];
    let ev2 = [
        LogField::new("event", "txn end"),
        LogField::new("gap", std::time::Duration::from_millis(5)),
    ];
    let objs: [&[LogField<'_>]; 3] = [&ev0, &ev1, &ev2];
    let fields = [
        LogField::new("duration", std::time::Duration::from_millis(17)),
        LogField::new("conn", 7_u64),
        LogField::new("txnStartTS", 123_u64),
        LogField::new("events", &objs[..]),
    ];
    let entry = Entry {
        time: ts,
        level: LogLevel::Info,
        logger_name: "",
        file: "probe_objects.go",
        line: 29,
        msg: "slow transaction",
        bound_fields: &[],
        fields: &fields,
        stack: "",
    };

    let mut text_out = Vec::new();
    encoder::text::encode_entry(cfg, &entry, &mut text_out);
    assert_single_line(&text_out);
    assert_eq!(
        String::from_utf8(text_out).unwrap(),
        "[INFO] [probe_objects.go:29] [\"slow transaction\"] [duration=17ms] [conn=7] [txnStartTS=123] [events=\"[\\\"{event=\\\\\\\"enter txn\\\\\\\",gap=0s}\\\",\\\"{event=SELECT,gap=12ms}\\\",\\\"{event=\\\\\\\"txn end\\\\\\\",gap=5ms}\\\"]\"]\n"
    );

    let mut json_out = Vec::new();
    encoder::json::encode_entry(cfg, &entry, &mut json_out);
    assert_single_line(&json_out);
    assert_eq!(
        String::from_utf8(json_out).unwrap(),
        "{\"level\":\"INFO\",\"caller\":\"probe_objects.go:29\",\"message\":\"slow transaction\",\"duration\":\"17ms\",\"conn\":7,\"txnStartTS\":123,\"events\":[{\"event\":\"enter txn\",\"gap\":\"0s\"},{\"event\":\"SELECT\",\"gap\":\"12ms\"},{\"event\":\"txn end\",\"gap\":\"5ms\"}]}\n"
    );

    let empty_objs: [&[LogField<'_>]; 0] = [];
    let empty_fields = [LogField::new("events", &empty_objs[..])];
    let empty_entry = Entry {
        msg: "empty events",
        fields: &empty_fields,
        line: 30,
        ..entry
    };
    let mut empty_text = Vec::new();
    encoder::text::encode_entry(cfg, &empty_entry, &mut empty_text);
    assert_eq!(
        String::from_utf8(empty_text).unwrap(),
        "[INFO] [probe_objects.go:30] [\"empty events\"] [events=\"[]\"]\n"
    );
    let mut empty_json = Vec::new();
    encoder::json::encode_entry(cfg, &empty_entry, &mut empty_json);
    assert_eq!(
        String::from_utf8(empty_json).unwrap(),
        "{\"level\":\"INFO\",\"caller\":\"probe_objects.go:30\",\"message\":\"empty events\",\"events\":[]}\n"
    );
}
