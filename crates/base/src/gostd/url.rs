//! `net/url` (Go 1.25.12): the part that `ast.RedactURL` uses — `Parse`, `URL.String`,
//! `URL.Query`, `Values.Encode` and the escaping helpers they call.
//!
//! go-file: $GOROOT/src/net/url/url.go
//!
//! Go strings are byte strings: an escape such as `%ff` unescapes to a non-UTF-8 byte, so
//! the parsed fields are `Vec<u8>`. `URL::string` escapes every non-ASCII byte it emits from
//! a decoded field and copies the other parts from the (UTF-8) input, so its result is UTF-8.
//!
//! Deviation (L2): the IP-literal check of `parseHost` (`netip.ParseAddr` + `Is4`) uses Rust's
//! `Ipv6Addr` / `Ipv4Addr` parsers with Go's zone rule (non-empty zone after `%`). The error
//! texts of that branch differ from Go's; `RedactURL` discards parse errors.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;

/// `url.encoding`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Path,
    Host,
    Zone,
    UserPassword,
    QueryComponent,
    Fragment,
}

// go-std: net/url.ishex L1
fn ishex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

// go-std: net/url.unhex L1
fn unhex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("invalid hex character"),
    }
}

// go-std: net/url.shouldEscape L1
fn should_escape(c: u8, mode: Encoding) -> bool {
    if c.is_ascii_alphanumeric() {
        return false;
    }
    if (mode == Encoding::Host || mode == Encoding::Zone)
        && matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'"'
        )
    {
        return false;
    }
    match c {
        b'-' | b'_' | b'.' | b'~' => return false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
            Encoding::Path => return c == b'?',
            Encoding::UserPassword => {
                return c == b'@' || c == b'/' || c == b'?' || c == b':';
            }
            Encoding::QueryComponent => return true,
            Encoding::Fragment => return false,
            Encoding::Host | Encoding::Zone => {}
        },
        _ => {}
    }
    if mode == Encoding::Fragment && matches!(c, b'!' | b'(' | b')' | b'*') {
        return false;
    }
    true
}

/// `url.EscapeError` / `url.InvalidHostError` and the other parse errors, as Go's text.
pub type Error = String;

// go-std: strconv.Quote L2 (ASCII printable bytes as-is, others as \xNN; enough for the
// 1–3 byte snippets of EscapeError / InvalidHostError)
fn quote(s: &[u8]) -> String {
    let mut out = String::from("\"");
    for &b in s {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push('"');
    out
}

// go-std: net/url.EscapeError.Error L1
fn escape_error(s: &[u8]) -> Error {
    format!("invalid URL escape {}", quote(s))
}

// go-std: net/url.InvalidHostError.Error L1
fn invalid_host_error(s: &[u8]) -> Error {
    format!("invalid character {} in host name", quote(s))
}

/// `url.QueryUnescape`.
// go-std: net/url.QueryUnescape L1
pub fn query_unescape(s: &[u8]) -> Result<Vec<u8>, Error> {
    unescape(s, Encoding::QueryComponent)
}

// go-std: net/url.unescape L1
fn unescape(s: &[u8], mode: Encoding) -> Result<Vec<u8>, Error> {
    let mut n = 0;
    let mut has_plus = false;
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'%' => {
                n += 1;
                if i + 2 >= s.len() || !ishex(s[i + 1]) || !ishex(s[i + 2]) {
                    let rest = &s[i..];
                    return Err(escape_error(&rest[..rest.len().min(3)]));
                }
                if mode == Encoding::Host && unhex(s[i + 1]) < 8 && &s[i..i + 3] != b"%25" {
                    return Err(escape_error(&s[i..i + 3]));
                }
                if mode == Encoding::Zone {
                    let v = (unhex(s[i + 1]) << 4) | unhex(s[i + 2]);
                    if &s[i..i + 3] != b"%25" && v != b' ' && should_escape(v, Encoding::Host) {
                        return Err(escape_error(&s[i..i + 3]));
                    }
                }
                i += 3;
            }
            b'+' => {
                has_plus = mode == Encoding::QueryComponent;
                i += 1;
            }
            c => {
                if (mode == Encoding::Host || mode == Encoding::Zone)
                    && c < 0x80
                    && should_escape(c, mode)
                {
                    return Err(invalid_host_error(&s[i..i + 1]));
                }
                i += 1;
            }
        }
    }
    if n == 0 && !has_plus {
        return Ok(s.to_vec());
    }
    let mut t = Vec::with_capacity(s.len() - 2 * n);
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'%' => {
                t.push((unhex(s[i + 1]) << 4) | unhex(s[i + 2]));
                i += 2;
            }
            b'+' => t.push(if mode == Encoding::QueryComponent {
                b' '
            } else {
                b'+'
            }),
            c => t.push(c),
        }
        i += 1;
    }
    Ok(t)
}

/// `url.QueryEscape`.
// go-std: net/url.QueryEscape L1
pub fn query_escape(s: &[u8]) -> Vec<u8> {
    escape(s, Encoding::QueryComponent)
}

// go-std: net/url.escape L1
fn escape(s: &[u8], mode: Encoding) -> Vec<u8> {
    const UPPERHEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut space_count = 0;
    let mut hex_count = 0;
    for &c in s {
        if should_escape(c, mode) {
            if c == b' ' && mode == Encoding::QueryComponent {
                space_count += 1;
            } else {
                hex_count += 1;
            }
        }
    }
    if space_count == 0 && hex_count == 0 {
        return s.to_vec();
    }
    if hex_count == 0 {
        return s
            .iter()
            .map(|&c| if c == b' ' { b'+' } else { c })
            .collect();
    }
    let mut t = Vec::with_capacity(s.len() + 2 * hex_count);
    for &c in s {
        if c == b' ' && mode == Encoding::QueryComponent {
            t.push(b'+');
        } else if should_escape(c, mode) {
            t.push(b'%');
            t.push(UPPERHEX[(c >> 4) as usize]);
            t.push(UPPERHEX[(c & 15) as usize]);
        } else {
            t.push(c);
        }
    }
    t
}

/// `url.Userinfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Userinfo {
    pub username: Vec<u8>,
    pub password: Vec<u8>,
    pub password_set: bool,
}

impl Userinfo {
    // go-std: net/url.Userinfo.String L1
    fn string(&self) -> Vec<u8> {
        let mut s = escape(&self.username, Encoding::UserPassword);
        if self.password_set {
            s.push(b':');
            s.extend(escape(&self.password, Encoding::UserPassword));
        }
        s
    }
}

/// `url.URL`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Url {
    pub scheme: Vec<u8>,
    pub opaque: Vec<u8>,
    pub user: Option<Userinfo>,
    pub host: Vec<u8>,
    pub path: Vec<u8>,
    pub raw_path: Vec<u8>,
    pub omit_host: bool,
    pub force_query: bool,
    pub raw_query: Vec<u8>,
    pub fragment: Vec<u8>,
    pub raw_fragment: Vec<u8>,
}

// go-std: net/url.getScheme L1
fn get_scheme(raw: &[u8]) -> Result<(&[u8], &[u8]), Error> {
    for (i, &c) in raw.iter().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' => {
                if i == 0 {
                    return Ok((b"", raw));
                }
            }
            b':' => {
                if i == 0 {
                    return Err("missing protocol scheme".to_owned());
                }
                return Ok((&raw[..i], &raw[i + 1..]));
            }
            _ => return Ok((b"", raw)),
        }
    }
    Ok((b"", raw))
}

// glue: strings.Cut on bytes
fn cut(s: &[u8], sep: u8) -> (&[u8], &[u8], bool) {
    match s.iter().position(|&b| b == sep) {
        Some(i) => (&s[..i], &s[i + 1..], true),
        None => (s, b"", false),
    }
}

/// `url.Parse`; the error is Go's `*url.Error` text.
// go-std: net/url.Parse L1
pub fn parse(raw: &str) -> Result<Url, Error> {
    let raw = raw.as_bytes();
    let (u, frag, _) = cut(raw, b'#');
    let mut url = parse_inner(u, false).map_err(|e| format!("parse {}: {e}", quote_url(u)))?;
    if frag.is_empty() {
        return Ok(url);
    }
    url.set_fragment(frag)
        .map_err(|e| format!("parse {}: {e}", quote_url(raw)))?;
    Ok(url)
}

// go-std: strconv.Quote L2 (the URL of url.Error; inputs here are UTF-8 sysvar values)
fn quote_url(s: &[u8]) -> String {
    format!("{:?}", String::from_utf8_lossy(s))
}

// go-std: net/url.stringContainsCTLByte L1
fn contains_ctl_byte(s: &[u8]) -> bool {
    s.iter().any(|&b| b < b' ' || b == 0x7f)
}

// go-std: net/url.parse L1
fn parse_inner(raw: &[u8], via_request: bool) -> Result<Url, Error> {
    if contains_ctl_byte(raw) {
        return Err("net/url: invalid control character in URL".to_owned());
    }
    if raw.is_empty() && via_request {
        return Err("empty url".to_owned());
    }
    let mut url = Url::default();
    if raw == b"*" {
        url.path = b"*".to_vec();
        return Ok(url);
    }
    let (scheme, rest) = get_scheme(raw)?;
    url.scheme = scheme.to_ascii_lowercase();
    let mut rest: &[u8] = rest;
    if rest.ends_with(b"?") && rest.iter().filter(|&&b| b == b'?').count() == 1 {
        url.force_query = true;
        rest = &rest[..rest.len() - 1];
    } else {
        let (r, q, _) = cut(rest, b'?');
        rest = r;
        url.raw_query = q.to_vec();
    }
    if !rest.starts_with(b"/") {
        if !url.scheme.is_empty() {
            url.opaque = rest.to_vec();
            return Ok(url);
        }
        if via_request {
            return Err("invalid URI for request".to_owned());
        }
        let (segment, _, _) = cut(rest, b'/');
        if segment.contains(&b':') {
            return Err("first path segment in URL cannot contain colon".to_owned());
        }
    }
    if (!url.scheme.is_empty() || !via_request && !rest.starts_with(b"///"))
        && rest.starts_with(b"//")
    {
        let mut authority = &rest[2..];
        rest = b"";
        if let Some(i) = authority.iter().position(|&b| b == b'/') {
            rest = &authority[i..];
            authority = &authority[..i];
        }
        let (user, host) = parse_authority(authority)?;
        url.user = user;
        url.host = host;
    } else if !url.scheme.is_empty() && rest.starts_with(b"/") {
        url.omit_host = true;
    }
    url.set_path(rest)?;
    Ok(url)
}

// go-std: net/url.parseAuthority L1
fn parse_authority(authority: &[u8]) -> Result<(Option<Userinfo>, Vec<u8>), Error> {
    let at = authority.iter().rposition(|&b| b == b'@');
    let host = match at {
        None => parse_host(authority)?,
        Some(i) => parse_host(&authority[i + 1..])?,
    };
    let Some(i) = at else {
        return Ok((None, host));
    };
    let userinfo = &authority[..i];
    if !valid_userinfo(userinfo) {
        return Err("net/url: invalid userinfo".to_owned());
    }
    let user = if !userinfo.contains(&b':') {
        Userinfo {
            username: unescape(userinfo, Encoding::UserPassword)?,
            password: Vec::new(),
            password_set: false,
        }
    } else {
        let (username, password, _) = cut(userinfo, b':');
        Userinfo {
            username: unescape(username, Encoding::UserPassword)?,
            password: unescape(password, Encoding::UserPassword)?,
            password_set: true,
        }
    };
    Ok((Some(user), host))
}

// glue: strings.LastIndex / Index on bytes
fn last_index(s: &[u8], sep: &[u8]) -> Option<usize> {
    if sep.len() > s.len() {
        return None;
    }
    (0..=s.len() - sep.len())
        .rev()
        .find(|&i| &s[i..i + sep.len()] == sep)
}

// glue: strings.Index on bytes
fn index(s: &[u8], sep: &[u8]) -> Option<usize> {
    if sep.len() > s.len() {
        return None;
    }
    (0..=s.len() - sep.len()).find(|&i| &s[i..i + sep.len()] == sep)
}

// go-std: net/url.parseHost L2 (netip.ParseAddr via Rust's IP parsers, see module docs)
fn parse_host(host: &[u8]) -> Result<Vec<u8>, Error> {
    match last_index(host, b"[") {
        Some(i) if i > 0 => return Err("invalid IP-literal".to_owned()),
        Some(_) => {
            let Some(close) = last_index(host, b"]") else {
                return Err("missing ']' in host".to_owned());
            };
            let colon_port = &host[close + 1..];
            if !valid_optional_port(colon_port) {
                return Err(format!("invalid port {} after host", quote_url(colon_port)));
            }
            let unescaped_colon_port = unescape(colon_port, Encoding::Host)?;
            let hostname = &host[1..close];
            let unescaped_hostname = match index(hostname, b"%25") {
                Some(z) => {
                    let mut h = unescape(&hostname[..z], Encoding::Host)?;
                    h.extend(unescape(&hostname[z..], Encoding::Zone)?);
                    h
                }
                None => unescape(hostname, Encoding::Host)?,
            };
            match parse_addr_is4(&unescaped_hostname) {
                None => return Err("invalid host: ParseAddr".to_owned()),
                Some(true) => return Err("invalid IP-literal".to_owned()),
                Some(false) => {}
            }
            let mut out = b"[".to_vec();
            out.extend(unescaped_hostname);
            out.push(b']');
            out.extend(unescaped_colon_port);
            return Ok(out);
        }
        None => {
            if let Some(i) = last_index(host, b":") {
                let colon_port = &host[i..];
                if !valid_optional_port(colon_port) {
                    return Err(format!("invalid port {} after host", quote_url(colon_port)));
                }
            }
        }
    }
    unescape(host, Encoding::Host)
}

/// `netip.ParseAddr(s)` succeeds → `Some(addr.Is4())`.
// go-std: net/netip.ParseAddr L2 (Rust's parsers; zone must be non-empty, IPv4 has no zone)
fn parse_addr_is4(s: &[u8]) -> Option<bool> {
    let s = std::str::from_utf8(s).ok()?;
    if s.parse::<Ipv4Addr>().is_ok() {
        return Some(true);
    }
    let (ip, zone) = match s.split_once('%') {
        Some((ip, zone)) => (ip, Some(zone)),
        None => (s, None),
    };
    if zone == Some("") {
        return None;
    }
    ip.parse::<Ipv6Addr>().ok().map(|_| false)
}

impl Url {
    // go-std: net/url.URL.setPath L1
    fn set_path(&mut self, p: &[u8]) -> Result<(), Error> {
        let path = unescape(p, Encoding::Path)?;
        self.raw_path = if escape(&path, Encoding::Path) == p {
            Vec::new()
        } else {
            p.to_vec()
        };
        self.path = path;
        Ok(())
    }

    // go-std: net/url.URL.EscapedPath L1
    fn escaped_path(&self) -> Vec<u8> {
        if !self.raw_path.is_empty()
            && valid_encoded(&self.raw_path, Encoding::Path)
            && unescape(&self.raw_path, Encoding::Path).is_ok_and(|p| p == self.path)
        {
            return self.raw_path.clone();
        }
        if self.path == b"*" {
            return b"*".to_vec();
        }
        escape(&self.path, Encoding::Path)
    }

    // go-std: net/url.URL.setFragment L1
    fn set_fragment(&mut self, f: &[u8]) -> Result<(), Error> {
        let frag = unescape(f, Encoding::Fragment)?;
        self.raw_fragment = if escape(&frag, Encoding::Fragment) == f {
            Vec::new()
        } else {
            f.to_vec()
        };
        self.fragment = frag;
        Ok(())
    }

    // go-std: net/url.URL.EscapedFragment L1
    fn escaped_fragment(&self) -> Vec<u8> {
        if !self.raw_fragment.is_empty()
            && valid_encoded(&self.raw_fragment, Encoding::Fragment)
            && unescape(&self.raw_fragment, Encoding::Fragment).is_ok_and(|f| f == self.fragment)
        {
            return self.raw_fragment.clone();
        }
        escape(&self.fragment, Encoding::Fragment)
    }

    /// `URL.String`.
    // go-std: net/url.URL.String L1
    pub fn string(&self) -> String {
        let mut buf: Vec<u8> = Vec::new();
        if !self.scheme.is_empty() {
            buf.extend(&self.scheme);
            buf.push(b':');
        }
        if !self.opaque.is_empty() {
            buf.extend(&self.opaque);
        } else {
            if !self.scheme.is_empty() || !self.host.is_empty() || self.user.is_some() {
                if self.omit_host && self.host.is_empty() && self.user.is_none() {
                    // omit empty host
                } else {
                    if !self.host.is_empty() || !self.path.is_empty() || self.user.is_some() {
                        buf.extend(b"//");
                    }
                    if let Some(ui) = &self.user {
                        buf.extend(ui.string());
                        buf.push(b'@');
                    }
                    if !self.host.is_empty() {
                        buf.extend(escape(&self.host, Encoding::Host));
                    }
                }
            }
            let path = self.escaped_path();
            if !path.is_empty() && path[0] != b'/' && !self.host.is_empty() {
                buf.push(b'/');
            }
            if buf.is_empty() {
                let (segment, _, _) = cut(&path, b'/');
                if segment.contains(&b':') {
                    buf.extend(b"./");
                }
            }
            buf.extend(path);
        }
        if self.force_query || !self.raw_query.is_empty() {
            buf.push(b'?');
            buf.extend(&self.raw_query);
        }
        if !self.fragment.is_empty() {
            buf.push(b'#');
            buf.extend(self.escaped_fragment());
        }
        String::from_utf8(buf).expect("URL.String output is ASCII or copied from UTF-8 input")
    }

    /// `URL.Query` (errors ignored, as Go).
    // go-std: net/url.URL.Query L1
    pub fn query(&self) -> Values {
        let mut m = Values::new();
        let _ = parse_query(&mut m, &self.raw_query);
        m
    }
}

// go-std: net/url.validEncoded L1
fn valid_encoded(s: &[u8], mode: Encoding) -> bool {
    s.iter().all(|&c| {
        matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
                | b'['
                | b']'
                | b'%'
        ) || !should_escape(c, mode)
    })
}

// go-std: net/url.validOptionalPort L1
fn valid_optional_port(port: &[u8]) -> bool {
    if port.is_empty() {
        return true;
    }
    if port[0] != b':' {
        return false;
    }
    port[1..].iter().all(u8::is_ascii_digit)
}

// go-std: net/url.validUserinfo L1 (ranges over runes: non-ASCII runes are invalid, as are
// their bytes)
fn valid_userinfo(s: &[u8]) -> bool {
    s.iter().all(|&r| {
        r.is_ascii_alphanumeric()
            || matches!(
                r,
                b'-' | b'.'
                    | b'_'
                    | b':'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b'%'
                    | b'@'
            )
    })
}

/// `url.Values` (`map[string][]string`; a `BTreeMap` iterates in `Encode`'s sorted order).
pub type Values = BTreeMap<Vec<u8>, Vec<Vec<u8>>>;

/// Go's `defaultMaxParams` (the `urlmaxqueryparams` GODEBUG is not consulted).
const DEFAULT_MAX_PARAMS: usize = 10000;

// go-std: net/url.parseQuery L1 (urlParamsWithinMax with the default GODEBUG)
fn parse_query(m: &mut Values, query: &[u8]) -> Result<(), Error> {
    if query.iter().filter(|&&b| b == b'&').count() + 1 > DEFAULT_MAX_PARAMS {
        return Err("number of URL query parameters exceeded limit".to_owned());
    }
    let mut err: Option<Error> = None;
    let mut query = query;
    while !query.is_empty() {
        let (key, rest, _) = cut(query, b'&');
        query = rest;
        if key.contains(&b';') {
            err = Some("invalid semicolon separator in query".to_owned());
            continue;
        }
        if key.is_empty() {
            continue;
        }
        let (key, value, _) = cut(key, b'=');
        let key = match query_unescape(key) {
            Ok(k) => k,
            Err(e) => {
                err.get_or_insert(e);
                continue;
            }
        };
        let value = match query_unescape(value) {
            Ok(v) => v,
            Err(e) => {
                err.get_or_insert(e);
                continue;
            }
        };
        m.entry(key).or_default().push(value);
    }
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `Values.Encode`.
// go-std: net/url.Values.Encode L1
pub fn encode(v: &Values) -> Vec<u8> {
    let mut buf = Vec::new();
    for (k, vs) in v {
        let key_escaped = query_escape(k);
        for val in vs {
            if !buf.is_empty() {
                buf.push(b'&');
            }
            buf.extend(&key_escaped);
            buf.push(b'=');
            buf.extend(query_escape(val));
        }
    }
    buf
}
