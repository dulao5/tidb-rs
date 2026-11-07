//! `$ZONEINFO` naming an uncompressed zip file (Go `loadTzinfoFromZip`), and the lookup
//! cache of `Location` (Go `cacheStart`/`cacheEnd`/`cacheZone`).
//!
//! The zips are written here (stored entries, as Go's `lib/time/mkzip.go` writes them).
//! `testdata/Asia_Tokyo` is the TZif file of Go 1.25's `zoneinfo.zip`.

use std::process::Command;

use tidb_base::gostd::time::{
    self as gotime, TzError, load_location, load_location_from_tzdata, load_tzinfo_from_dir_or_zip,
};

const TOKYO: &[u8] = include_bytes!("testdata/Asia_Tokyo");

/// A zip archive of `(name, method, data)` entries (method 0 = stored); data is written as is.
fn zip(entries: &[(&str, u16, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, meth, data) in entries {
        let off = out.len() as u32;
        let le16 = |v: u16| v.to_le_bytes();
        let le32 = |v: u32| v.to_le_bytes();
        // local file header
        out.extend_from_slice(&le32(0x04034b50));
        out.extend_from_slice(&le16(20)); // version
        out.extend_from_slice(&le16(0)); // flags
        out.extend_from_slice(&le16(*meth));
        out.extend_from_slice(&le32(0)); // time, date
        out.extend_from_slice(&le32(0)); // crc (not checked by Go)
        out.extend_from_slice(&le32(data.len() as u32));
        out.extend_from_slice(&le32(data.len() as u32));
        out.extend_from_slice(&le16(name.len() as u16));
        out.extend_from_slice(&le16(0)); // xlen
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        // central directory entry
        central.extend_from_slice(&le32(0x02014b50));
        central.extend_from_slice(&le16(20));
        central.extend_from_slice(&le16(20));
        central.extend_from_slice(&le16(0));
        central.extend_from_slice(&le16(*meth));
        central.extend_from_slice(&le32(0));
        central.extend_from_slice(&le32(0));
        central.extend_from_slice(&le32(data.len() as u32));
        central.extend_from_slice(&le32(data.len() as u32));
        central.extend_from_slice(&le16(name.len() as u16));
        central.extend_from_slice(&le16(0)); // xlen
        central.extend_from_slice(&le16(0)); // fclen
        central.extend_from_slice(&le16(0)); // disknum
        central.extend_from_slice(&le16(0)); // iattr
        central.extend_from_slice(&le32(0)); // eattr
        central.extend_from_slice(&le32(off));
        central.extend_from_slice(name.as_bytes());
    }
    let cd_off = out.len() as u32;
    out.extend_from_slice(&central);
    // end of central directory
    out.extend_from_slice(&0x06054b50u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

fn write_zip(tag: &str, bytes: &[u8]) -> String {
    let dir = std::env::temp_dir().join(format!("tidb-rs-zoneinfo-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("zoneinfo.zip");
    std::fs::write(&path, bytes).unwrap();
    path.to_str().unwrap().to_owned()
}

#[test]
fn zip_reader() {
    let z = write_zip(
        "reader",
        &zip(&[
            ("Asia/Tokyo", 0, TOKYO),
            ("Deflated/Zone", 8, b"xx"),
            ("Only/InZip", 0, TOKYO),
        ]),
    );
    assert_eq!(
        load_tzinfo_from_dir_or_zip(&z, "Asia/Tokyo").unwrap(),
        TOKYO
    );
    assert_eq!(
        load_tzinfo_from_dir_or_zip(&z, "Only/InZip").unwrap(),
        TOKYO
    );
    assert_eq!(
        load_tzinfo_from_dir_or_zip(&z, "No/Such"),
        Err(TzError::NotFound)
    );
    assert_eq!(
        load_tzinfo_from_dir_or_zip(&z, "Deflated/Zone"),
        Err(TzError::Other(format!(
            "unsupported compression for Deflated/Zone in {z}"
        )))
    );

    // Truncated: no end-of-central-directory record.
    let bad = write_zip("truncated", &zip(&[("Asia/Tokyo", 0, TOKYO)])[..100]);
    assert_eq!(
        load_tzinfo_from_dir_or_zip(&bad, "Asia/Tokyo"),
        Err(TzError::Other(format!("corrupt zip file {bad}")))
    );

    // A missing zip is ENOENT, like a missing directory entry.
    assert_eq!(
        load_tzinfo_from_dir_or_zip("/nonexistent/tidb-rs/zoneinfo.zip", "Asia/Tokyo"),
        Err(TzError::NotFound)
    );
}

/// `load_location` with `$ZONEINFO` set to a zip. `$ZONEINFO` is read once per process,
/// so the check runs in a child process with the variable set.
#[test]
fn zoneinfo_env_zip() {
    if let Ok(name) = std::env::var("TIDB_RS_ZONEINFO_CHILD") {
        let l = load_location(&name).expect("zone from the zip");
        assert_eq!(l.string(), name);
        assert_eq!(
            gotime::unix(0, 0).in_loc(&l).string(),
            "1970-01-01 09:00:00 +0900 JST"
        );
        return;
    }
    let z = write_zip("env", &zip(&[("Only/InZip", 0, TOKYO)]));
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "zoneinfo_env_zip", "--nocapture"])
        .env("ZONEINFO", &z)
        .env("TIDB_RS_ZONEINFO_CHILD", "Only/InZip")
        .status()
        .unwrap();
    assert!(status.success());
}

/// The cache answers the zone of "now" without the search; the answer is the same as
/// without it (a Location loaded from the same bytes, looked up far from now, then near).
#[test]
fn lookup_cache_same_answer() {
    let l = load_location_from_tzdata("Asia/Tokyo", TOKYO).unwrap();
    let now = gotime::now().unix();
    for sec in [
        now,
        now - 86400 * 400,
        now + 86400 * 400,
        -2_000_000_000,
        4_000_000_000,
    ] {
        let z = l.lookup(sec);
        assert_eq!((z.name, z.offset), ("JST", 32400), "sec {sec}");
    }
}
