//! `path/filepath` (Unix) and `encoding/base64` helpers used by `pkg/config`.

/// `filepath.Clean` (Unix separators).
// go-std: path/filepath.Clean L1 (Unix)
pub fn clean(path: &str) -> String {
    String::from_utf8(clean_bytes(path.as_bytes())).expect("clean keeps UTF-8")
}

/// `filepath.Clean` (Unix separators) on a Go string's bytes.
// go-std: path/filepath.Clean L1 (Unix)
pub fn clean_bytes(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return b".".to_vec();
    }
    let rooted = path[0] == b'/';
    let mut out: Vec<&[u8]> = Vec::new();
    for part in path.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                if matches!(out.last(), Some(p) if *p != b"..") {
                    out.pop();
                } else if !rooted {
                    out.push(b"..");
                }
            }
            p => out.push(p),
        }
    }
    let joined = out.join(&b'/');
    match (rooted, joined.is_empty()) {
        (true, _) => [b"/".as_slice(), &joined].concat(),
        (false, true) => b".".to_vec(),
        (false, false) => joined,
    }
}

/// `filepath.Join` (Unix): joins the non-empty elements with `/` and cleans the result.
// go-std: path/filepath.Join L1 (Unix)
pub fn join(elem: &[&str]) -> String {
    let b: Vec<&[u8]> = elem.iter().map(|e| e.as_bytes()).collect();
    String::from_utf8(join_bytes(&b)).expect("join keeps UTF-8")
}

/// `filepath.Join` (Unix) on Go strings' bytes.
// go-std: path/filepath.Join L1 (Unix)
pub fn join_bytes(elem: &[&[u8]]) -> Vec<u8> {
    let parts: Vec<&[u8]> = elem.iter().copied().filter(|e| !e.is_empty()).collect();
    if parts.is_empty() {
        return Vec::new();
    }
    clean_bytes(&parts.join(&b'/'))
}

/// `filepath.Rel(basepath, targpath)` (Unix): a path that is lexically equivalent to
/// `targpath` when joined to `basepath`, or Go's error text.
// go-std: path/filepath.Rel L1 (Unix: no volume names, sameWord is ==)
pub fn rel_bytes(basepath: &[u8], targpath: &[u8]) -> Result<Vec<u8>, Vec<u8>> {
    let err = || {
        [
            b"Rel: can't make ".as_slice(),
            targpath,
            b" relative to ",
            basepath,
        ]
        .concat()
    };
    let mut base = clean_bytes(basepath);
    let targ = clean_bytes(targpath);
    if targ == base {
        return Ok(b".".to_vec());
    }
    if base == b"." {
        base.clear();
    }
    let base_slashed = base.first() == Some(&b'/');
    let targ_slashed = targ.first() == Some(&b'/');
    if base_slashed != targ_slashed {
        return Err(err());
    }
    // Position base[b0:bi] and targ[t0:ti] at the first differing elements.
    let (bl, tl) = (base.len(), targ.len());
    let (mut b0, mut bi, mut t0, mut ti) = (0, 0, 0, 0);
    loop {
        while bi < bl && base[bi] != b'/' {
            bi += 1;
        }
        while ti < tl && targ[ti] != b'/' {
            ti += 1;
        }
        if targ[t0..ti] != base[b0..bi] {
            break;
        }
        if bi < bl {
            bi += 1;
        }
        if ti < tl {
            ti += 1;
        }
        b0 = bi;
        t0 = ti;
    }
    if &base[b0..bi] == b".." {
        return Err(err());
    }
    if b0 != bl {
        // Base elements left. Must go up before going down.
        let seps = base[b0..bl].iter().filter(|&&b| b == b'/').count();
        let mut buf = b"..".to_vec();
        for _ in 0..seps {
            buf.extend_from_slice(b"/..");
        }
        if t0 != tl {
            buf.push(b'/');
            buf.extend_from_slice(&targ[t0..]);
        }
        return Ok(buf);
    }
    Ok(targ[t0..].to_vec())
}

/// `base64.URLEncoding.EncodeToString` (padded, URL alphabet).
// go-std: encoding/base64.Encoding.EncodeToString L1 (URLEncoding)
pub fn base64_url_encode(src: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(src.len().div_ceil(3) * 4);
    for chunk in src.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}
