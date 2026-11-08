//! Random-input checks of the JSON port without a Go oracle (A4 §6.3; the Go oracle is the
//! govec `json` domain):
//!
//! 1. **Parser bytes.** Random and mutated texts: `parse_binary_json_from_string`,
//!    `json_valid` and `unmarshal_json` never panic; a text `json_valid` rejects fails with
//!    ErrInvalidJSONText, and `unmarshal_json` succeeds exactly when parsing does.
//! 2. **Deep path vs recursive path.** Wrapping a text in enough `[` to exceed
//!    maxJSONDepth sends it down the iterative deep path, which must report the error the
//!    recursive path reports for the bare text (the first appendBinaryJSON error), or
//!    ErrJSONDocumentTooDeep when the bare text converts.
//! 3. **Round trip.** A converted document prints to a text that parses back to a document
//!    that compares equal; printing is a fixed point from the second round on.
//! 4. **Binary accessors.** Random bytes through `PeekBytesAsJSON`, and truncated values
//!    (plus changed bytes in scalar values) through the accessors, may hit Go's own index
//!    panics but never a Rust integer-overflow panic (M-INT). Containers with changed
//!    offsets are left out: they can make the accessors recurse without end, in Go too.
//!
//! Short by default; longer runs:
//!   TIDB_TYPES_FUZZ_JSON_ITERS=2000000 cargo test -p tidb-types --test fuzz_json
//! `TIDB_TYPES_FUZZ_JSON_SEED` picks the seed (printed on failure).

use std::panic::{AssertUnwindSafe, catch_unwind};
use tidb_types::json::{
    self as j, BinaryJSON, ErrInvalidJSONText, ErrJSONDocumentTooDeep, JSONPathExpression,
    JSONTypeCodeArray, JSONTypeCodeObject,
};

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

const NUMBERS: &[&str] = &[
    "0",
    "-0",
    "1",
    "-1",
    "1.5",
    "1e400",
    "-1e400",
    "1e-400",
    "9223372036854775807",
    "9223372036854775808",
    "18446744073709551615",
    "18446744073709551616",
    "1E2",
    "0.0",
    "123456789012345678901234567890",
    "2.5e+10",
    "-0.0e-0",
];
const STRINGS: &[&str] = &[
    r#""""#,
    r#""a""#,
    r#""\u0000""#,
    r#""\ud800""#,
    r#""😀""#,
    "\"\u{e9}\"",
    "\"\u{1f600}\"",
    r#""\"\\\/\b\f\n\r\t""#,
    "\"\u{2028}\"",
];

fn text(rng: &mut Rng, depth: u32, out: &mut String) {
    let k = if depth > 4 {
        rng.below(3)
    } else {
        rng.below(5)
    };
    match k {
        0 => out.push_str(rng.pick(&["null", "true", "false"])),
        1 => out.push_str(rng.pick(NUMBERS)),
        2 => out.push_str(rng.pick(STRINGS)),
        3 => {
            out.push('[');
            for i in 0..rng.below(4) {
                if i > 0 {
                    out.push(',');
                }
                text(rng, depth + 1, out);
            }
            out.push(']');
        }
        _ => {
            out.push('{');
            for i in 0..rng.below(4) {
                if i > 0 {
                    out.push(',');
                }
                if rng.chance(3) {
                    out.push('"');
                    out.push_str(&"k".repeat(65535 + rng.below(2) as usize));
                    out.push('"');
                } else {
                    out.push_str(rng.pick(&[r#""a""#, r#""b""#, r#""""#, r#""a""#, "\"\u{e9}\""]));
                }
                out.push(':');
                text(rng, depth + 1, out);
            }
            out.push('}');
        }
    }
}

fn mutate(rng: &mut Rng, s: &mut Vec<u8>) {
    for _ in 0..1 + rng.below(3) {
        let i = if s.is_empty() {
            0
        } else {
            rng.below(s.len() as u64 + 1) as usize
        };
        match rng.below(4) {
            0 if i < s.len() => {
                s.remove(i);
            }
            1 => {
                const BYTES: &[u8] = b"[]{},:\"\\ -0e.tfnx\xff\x00";
                s.insert(i, BYTES[rng.below(BYTES.len() as u64) as usize]);
            }
            2 if i < s.len() => s[i] = rng.next() as u8,
            _ => s.truncate(i),
        }
    }
}

fn is_overflow_panic(p: &(dyn std::any::Any + Send)) -> Option<String> {
    let msg = p
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    msg.contains("with overflow").then_some(msg)
}

#[test]
fn fuzz_json() {
    let seed = env_u64("TIDB_TYPES_FUZZ_JSON_SEED", 0x6a73_6f6e);
    let iters = env_u64("TIDB_TYPES_FUZZ_JSON_ITERS", 20_000);
    let mut rng = Rng(seed);
    let paths: Vec<JSONPathExpression> = ["$", "$[0]", "$.a", "$**[0]", "$[*]", "$.*", "$[last]"]
        .iter()
        .map(|p| j::parse_json_path_expr(p.as_bytes()).unwrap())
        .collect();
    let mut go_panics = 0u64;
    for it in 0..iters {
        let ctx = format!("seed={seed:#x} iter={it}");
        let mut s = String::new();
        text(&mut rng, 0, &mut s);
        let mut t = s.clone().into_bytes();
        if rng.chance(40) {
            mutate(&mut rng, &mut t);
        }

        // 1. Parser bytes.
        let valid = j::json_valid(&t);
        let parsed = j::parse_binary_json_from_string(&t);
        if !valid {
            let err = parsed.as_ref().unwrap_err();
            assert!(ErrInvalidJSONText.equal(err), "{ctx}: {err}");
        }
        let unmarshaled = BinaryJSON::default().unmarshal_json(&t);
        assert_eq!(parsed.is_ok(), unmarshaled.is_ok(), "{ctx}");

        // 2. Deep path vs recursive path (bare texts here are nested at most 6 deep).
        if valid {
            let wrapped = [b"[".repeat(120), t.clone(), b"]".repeat(120)].concat();
            let deep = j::parse_binary_json_from_string(&wrapped).unwrap_err();
            match &parsed {
                Ok(_) => assert!(ErrJSONDocumentTooDeep.equal(&deep), "{ctx}: {deep}"),
                Err(e) => assert_eq!(e.to_string(), deep.to_string(), "{ctx}"),
            }
        }

        // 3. Round trip.
        if let Ok(bj) = &parsed {
            let back = j::parse_binary_json_from_string(&bj.string()).expect("round trip");
            // Not an identity: Go prints the floats 1.0 and -0.0 as `1` and `-0`, which parse
            // as ints. The second round is a fixed point.
            assert_eq!(
                j::compare_binary_json(bj.as_ref(), back.as_ref()),
                0,
                "{ctx}"
            );
            let again = j::parse_binary_json_from_string(&back.string()).expect("round trip");
            assert_eq!(back.string(), again.string(), "{ctx}");
        }

        // 4. Binary accessors. Random bytes only go to `PeekBytesAsJSON`: a changed offset
        // inside a container can point back at the container itself, and the accessors then
        // recurse without end, as Go does (a stack overflow aborts the process, so it cannot
        // be caught; A4 §8.5 leaves corrupted containers out). Container values are only
        // truncated (offsets keep pointing forward); scalar values may also get a byte changed.
        let (b, walk): (Vec<u8>, bool) = match &parsed {
            Ok(bj) if rng.chance(70) => {
                let mut v = vec![bj.type_code];
                v.extend_from_slice(&bj.value);
                let container =
                    bj.type_code == JSONTypeCodeArray || bj.type_code == JSONTypeCodeObject;
                if v.len() > 1 && rng.chance(50) {
                    let i = 1 + rng.below(v.len() as u64 - 1) as usize;
                    if container || rng.chance(50) {
                        v.truncate(i);
                    } else {
                        v[i] = rng.next() as u8;
                    }
                }
                (v, true)
            }
            _ => (
                (0..rng.below(24)).map(|_| rng.next() as u8).collect(),
                false,
            ),
        };
        // Silence the expected Go-style index panics; restore the hook for the asserts.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let r = catch_unwind(AssertUnwindSafe(|| {
            if j::peek_bytes_as_json(&b).is_err() || !walk {
                return;
            }
            let bj = BinaryJSON::borrowed(b[0], &b[1..]);
            let _ = bj.r#type();
            let _ = bj.string();
            let _ = bj.get_elem_depth();
            let _ = bj.hash_value(Vec::new());
            let _ = bj.unquote();
            let _ = j::compare_binary_json(bj.as_ref(), bj.as_ref());
            for p in &paths {
                let _ = bj.extract(std::slice::from_ref(p));
            }
        }));
        std::panic::set_hook(hook);
        if let Err(p) = r {
            if let Some(msg) = is_overflow_panic(p.as_ref()) {
                panic!("{ctx}: integer overflow on binary {b:02x?}: {msg}");
            }
            go_panics += 1;
        }
    }
    eprintln!(
        "fuzz_json: seed {seed:#x}, {iters} iterations, {go_panics} Go-style accessor panics"
    );
}
