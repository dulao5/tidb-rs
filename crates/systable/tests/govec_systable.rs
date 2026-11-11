//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/systable.jsonl.gz`
//! (tidb-rs-tracker govec, domain `systable`, block D06.7).
//!
//! The oracle is Go's real REPLACE on a bootstrapped mockstore: each `replace` record holds
//! the rows the statement can read ("before") and the transaction's memory buffer over the
//! table before the commit ("muts": key, value, key flags). The replay runs
//! [`replace::replace_row`] over the same snapshot, handing out the handle Go's allocator gave,
//! and compares the buffer byte for byte, flags included. `alloc` records replay the row-ID
//! allocator over an in-memory meta counter; `next_step` records `NextStep`.
//! Extra files: `GOVEC_SYSTABLE_FILES=a.jsonl.gz,b.jsonl.gz` (other seeds).

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_model::TableInfo;
use tidb_systable::autoid::{IdAccessor, RowIdAllocator, next_step_fn};
use tidb_systable::memdb::MemBuffer;
use tidb_systable::replace::{self, RowEncodingConfig};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/systable.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn gs(e: &Error) -> Value {
    Value::String(String::from_utf8_lossy(&e.error()).into_owned())
}

/// The meta counter of one table (`meta.AutoIDAccessor` over an in-memory value).
struct Counter<'a>(&'a mut i64);

impl IdAccessor for Counter<'_> {
    fn get(&mut self) -> Result<i64, Error> {
        Ok(*self.0)
    }
    fn inc(&mut self, step: i64) -> Result<i64, Error> {
        *self.0 = self.0.wrapping_add(step);
        Ok(*self.0)
    }
}

/// Replays one file: the number of records checked.
fn replay(path: &str) -> usize {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "systable");
    let mut tables: HashMap<String, TableInfo> = HashMap::new();
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let a = rec["args"].as_array().unwrap();
        let want = &rec["out"];
        n += 1;
        let got = match op {
            "tableinfo" => {
                let mut t = TableInfo::default();
                t.unmarshal_json(want["info"].to_string().as_bytes())
                    .unwrap();
                assert_eq!(t.id, want["table_id"].as_i64().unwrap());
                if let Err(e) = replace::check_table_shape(&t) {
                    fails.push(format!("tableinfo {}: {e:?}", a[0]));
                }
                tables.insert(a[0].as_str().unwrap().to_owned(), t);
                continue;
            }
            "replace" => {
                let tbl = &tables[a[0].as_str().unwrap()];
                let row: Vec<Option<&str>> = a[2]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str())
                    .collect();
                let snap: BTreeMap<Vec<u8>, Vec<u8>> = want["before"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (unhex(k), unhex(v.as_str().unwrap())))
                    .collect();
                let go_handle = want["handle"].as_i64();
                let mut mb = MemBuffer::new();
                let mut alloc = || match go_handle {
                    Some(h) => Ok(h),
                    None => Err(tidb_base::errors::new("allocated a handle Go did not")),
                };
                let r = replace::replace_row(
                    tbl,
                    &row,
                    &snap,
                    &mut mb,
                    &mut alloc,
                    &RowEncodingConfig::default(),
                );
                let muts: Vec<Value> = mb
                    .entries()
                    .iter()
                    .map(|(k, e)| {
                        let f = e.flags.render();
                        let mut m = serde_json::Map::new();
                        m.insert("k".into(), json!(hex(k)));
                        m.insert("v".into(), json!(hex(&e.value)));
                        if !f.is_empty() {
                            m.insert("f".into(), json!(f));
                        }
                        Value::Object(m)
                    })
                    .collect();
                let mut out = serde_json::Map::new();
                out.insert("before".into(), want["before"].clone());
                match &r {
                    Ok(Some(h)) => {
                        out.insert("handle".into(), json!(h));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        fails.push(format!("replace {}: error {:?}", rec["args"], gs(e)));
                        continue;
                    }
                }
                out.insert(
                    "muts".into(),
                    if muts.is_empty() {
                        Value::Null
                    } else {
                        Value::Array(muts)
                    },
                );
                Value::Object(out)
            }
            "alloc" => {
                let (tbl_id, start, step) = (
                    a[0].as_i64().unwrap(),
                    a[1].as_i64().unwrap(),
                    a[2].as_i64().unwrap(),
                );
                let now = Instant::now();
                let mut counter = start;
                let mut al = RowIdAllocator::new(tbl_id, step, 0, now);
                let mut outs = vec![];
                for c in a[3].as_array().unwrap() {
                    let c: Vec<i64> = c
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|x| x.as_i64().unwrap())
                        .collect();
                    let r = al.alloc(c[0] as u64, c[1], c[2], now, &mut |f| {
                        // kv.RunInNewTxn: the counter changes only if f succeeds.
                        let mut tmp = counter;
                        f(&mut Counter(&mut tmp))?;
                        counter = tmp;
                        Ok(())
                    });
                    let (min, max, err) = match r {
                        Ok((lo, hi)) => (lo, hi, Value::Null),
                        Err(e) => (0, 0, gs(&e)),
                    };
                    outs.push(
                        json!({"base": al.base, "end": al.end, "err": err, "max": max,
                        "meta": counter, "min": min}),
                    );
                }
                Value::Array(outs)
            }
            "next_step" => json!(next_step_fn(
                a[0].as_i64().unwrap(),
                Duration::from_nanos(a[1].as_i64().unwrap() as u64)
            )),
            _ => panic!("unknown op {op}"),
        };
        if &got != want {
            fails.push(format!("{op} {}: got {got}, want {want}", rec["args"]));
        }
    }
    assert!(
        fails.is_empty(),
        "{} mismatches of {n}:\n{}",
        fails.len(),
        fails[..fails.len().min(20)].join("\n")
    );
    n
}

#[test]
fn govec_systable() {
    let n = replay(&vectors());
    eprintln!("systable: {n} records");
    if let Ok(files) = std::env::var("GOVEC_SYSTABLE_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            eprintln!("{f}: {} records", replay(f));
        }
    }
}
