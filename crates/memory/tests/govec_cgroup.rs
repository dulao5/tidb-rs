//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/cgroup.jsonl.gz`
//! (tracker `govec`, domain "cgroup", block D15.3) against the Rust port.
//!
//! Each file-reading record carries a fixture (`{"files": [[path, hex], ...], "dirs": [...]}`)
//! that is written, as govec writes it, under `<tmp>/n/n/n/n/n/n` (so that `..` in cgroup
//! paths stays inside the temporary directory); error texts are compared with the root
//! replaced by `<ROOT>` and the temporary directory by `<TMP>`.
//!
//! No floating point: the two targets' records are identical.
//!
//! `GOVEC_CGROUP_FILES` (comma-separated paths) replays extra vector files too, such as the
//! extra-seed artifacts of tracker CI.

use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use serde_json::Value;
use serde_json::json;
use tidb_base::Error;
use tidb_base::gostd::bytes::fields;
use tidb_memory::cgroup;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

const NEST: &str = "n/n/n/n/n/n";

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// vec.GoString's JSON form.
fn go_string(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
}

/// A fixture tree under a fresh temporary directory (govec `cgFixture.write`).
struct Fixture {
    tmp: PathBuf,
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn write_fixture(fx: &Value) -> Fixture {
    static N: AtomicUsize = AtomicUsize::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "govec-cgroup-rs-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let root = tmp.join(NEST);
    std::fs::create_dir_all(&root).unwrap();
    let rel = |p: &str| root.join(p.trim_start_matches('/'));
    if let Some(dirs) = fx["dirs"].as_array() {
        for d in dirs {
            let _ = std::fs::create_dir_all(rel(d.as_str().unwrap()));
        }
    }
    if let Some(files) = fx["files"].as_array() {
        for f in files {
            let p = rel(f[0].as_str().unwrap());
            if std::fs::create_dir_all(p.parent().unwrap()).is_err() {
                continue;
            }
            let _ = std::fs::write(&p, unhex(f[1].as_str().unwrap()));
        }
    }
    Fixture { tmp, root }
}

fn bytes_of(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

fn replace_all(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if !from.is_empty() && hay[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

impl Fixture {
    fn root(&self) -> Vec<u8> {
        bytes_of(&self.root)
    }
    fn err(&self, e: &Error) -> Value {
        let m = replace_all(&e.error(), &self.root(), b"<ROOT>");
        let m = replace_all(&m, &bytes_of(&self.tmp), b"<TMP>");
        json!({ "name": "other", "msg": go_string(&m) })
    }
    fn join(&self, p: &str) -> Vec<u8> {
        tidb_base::gostd::path::join_bytes(&[&self.root(), p.as_bytes()])
    }
}

/// (out, err) of one record.
fn exec(op: &str, a: &[Value]) -> (Value, Value) {
    let s = |i: usize| a[i].as_str().unwrap();
    let with = |f: &dyn Fn(&Fixture) -> (Value, Option<Error>)| {
        let fx = write_fixture(&a[0]);
        let (out, err) = f(&fx);
        let err = err.map_or(Value::Null, |e| fx.err(&e));
        (out, err)
    };
    let num = |r: Result<u64, Error>| match r {
        Ok(v) => (json!(v), None),
        Err(e) => (json!(0), Some(e)),
    };
    match op {
        "mem_limit" => with(
            &|fx| match cgroup::internal::get_cgroup_mem_limit_at(&fx.root()) {
                Ok((limit, ver)) => (json!({ "limit": limit, "version": ver as i32 }), None),
                Err(e) => (json!(0), Some(e)),
            },
        ),
        "mem_usage" => with(&|fx| num(cgroup::internal::get_cgroup_mem_usage_at(&fx.root()))),
        "mem_inactive" => with(&|fx| {
            num(cgroup::internal::get_cgroup_mem_inactive_file_usage_at(
                &fx.root(),
            ))
        }),
        "control_path" => with(&|fx| match cgroup::internal::detect_control_path(
            &fx.join("/proc/self/cgroup"),
            s(1),
        ) {
            Ok(p) => (go_string(&p), None),
            Err(e) => (json!(""), Some(e)),
        }),
        "details" => with(&|fx| match cgroup::internal::get_cgroup_details(
            &fx.join("/proc/self/mountinfo"),
            s(1).as_bytes(),
            s(2),
        ) {
            Ok((mount, ver)) => {
                let ms: Vec<Value> = mount.iter().map(|m| go_string(m)).collect();
                (json!({ "mount": ms, "ver": ver }), None)
            }
            Err(e) => (Value::Null, Some(e)),
        }),
        "cgroup_version" => {
            let line = unhex(s(0));
            let f = fields(&line);
            let (ver, found) = match cgroup::internal::detect_cgroup_version(&f, s(1).as_bytes()) {
                Some(v) => (v, true),
                None => (0, false),
            };
            (json!({ "ver": ver, "found": found }), Value::Null)
        }
        "controller_match" => (
            json!(cgroup::internal::controller_match(
                s(0).as_bytes(),
                s(1).as_bytes()
            )),
            Value::Null,
        ),
        "read_file" => with(&|fx| {
            let (data, err) = cgroup::internal::read_file(&fx.join(s(1)));
            (json!(hex(&data)), err)
        }),
        "read_int64" => with(&|fx| {
            num(cgroup::internal::read_int64_value(
                &fx.join(s(1)),
                s(2),
                a[3].as_i64().unwrap() as i32,
            ))
        }),
        "mem_stat" => with(&|fx| {
            num(cgroup::internal::detect_mem_stat_value(
                &fx.join(s(1)),
                s(2),
                s(3),
                a[4].as_i64().unwrap() as i32,
            ))
        }),
        _ => panic!("unknown op {op}"),
    }
}

fn replay(path: &str) -> usize {
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap())
        .read_to_string(&mut s)
        .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "cgroup");
    let (mut n, mut bad) = (0, 0);
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let (out, err) = exec(op, rec["args"].as_array().unwrap());
        n += 1;
        if out != rec["out"] || err != rec["err"] {
            bad += 1;
            if bad <= 8 {
                eprintln!(
                    "MISMATCH {} {op} {}\n got  {out} {err}\n want {} {}",
                    rec["src"],
                    rec["args"]
                        .to_string()
                        .chars()
                        .take(400)
                        .collect::<String>(),
                    rec["out"],
                    rec["err"]
                );
            }
        }
    }
    assert_eq!(n, header["records"].as_i64().unwrap() as usize, "{path}");
    assert_eq!(bad, 0, "{path}: {bad} of {n} records differ");
    n
}

#[test]
fn govec_cgroup() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/cgroup.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    replay(&path);
    if let Ok(extra) = std::env::var("GOVEC_CGROUP_FILES") {
        for f in extra.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}

/// `inContainer`'s checks (govec cannot fixture the fixed /proc paths it compares with).
#[test]
fn in_container_checks() {
    use cgroup::internal::PROC_PATH_CGROUP as CG;
    use cgroup::internal::PROC_PATH_MOUNT_INFO as MI;
    use cgroup::internal::in_container_content as f;
    assert!(f(CG, b"12:memory:/kubepods/besteffort/pod1"));
    assert!(f(CG, b"1:name=systemd:/docker/abc"));
    assert!(f(CG, b"0::/system.slice/containerd.service"));
    assert!(!f(CG, b"0::/user.slice/user-1000.slice"));
    // The cgroup-file words are not checked in mountinfo, and vice versa.
    assert!(!f(MI, b"1:memory:/docker"));
    assert!(f(
        MI,
        b"625 367 0:71 / / rw,relatime master:85 - overlay overlay rw,lowerdir=/x"
    ));
    assert!(!f(
        CG,
        b"625 367 0:71 / / rw,relatime master:85 - overlay overlay rw"
    ));
    // Fields are split on single spaces and v[8] is taken as the fs type, which it is only
    // with exactly one optional field: without one, v[8] is the mount source (here also
    // "overlay"); with two, v[8] is "-".
    assert!(f(MI, b"625 367 0:71 / / rw,relatime - overlay overlay rw"));
    assert!(!f(MI, b"625 367 0:71 / / rw,relatime - overlay none rw"));
    assert!(!f(
        MI,
        b"625 367 0:71 / / rw,relatime shared:1 master:85 - overlay overlay rw"
    ));
    assert!(!f(
        MI,
        b"625  367 0:71 / / rw,relatime master:85 - overlay overlay rw"
    ));
    assert!(!f(
        MI,
        b"625 367 0:71 / /sys rw,relatime master:85 - overlay overlay rw"
    ));
    assert!(f(
        MI,
        b"x\n625 367 0:71 / / rw,relatime master:85 - overlay overlay rw\n"
    ));
    assert!(!f("/other", b"docker"));
}
