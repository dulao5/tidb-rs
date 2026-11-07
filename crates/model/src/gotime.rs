//! go-file: $GOROOT/src/time/format_rfc3339.go
//! go-file: $GOROOT/src/time/time.go
//!
//! `time.Time`'s JSON methods for the model structs that hold a `time.Time`
//! (`StatsWindowSettings`).

use tidb_base::Error;
use tidb_base::gostd::json::{DecodeError, Raw};
use tidb_base::gostd::time::Time;

/// `Time.MarshalJSON`: RFC 3339 with nanoseconds, quoted; an error for a year outside
/// [0, 9999] or a zone offset of 24 hours or more.
// go-std: time.Time.MarshalJSON L1
pub(crate) fn encode_time(out: &mut String, t: &Time) -> Result<(), Error> {
    let s = t.append_strict_rfc3339().map_err(|e| {
        Error::plain(format!(
            "json: error calling MarshalJSON for type time.Time: Time.MarshalJSON: {e}"
        ))
    })?;
    out.push('"');
    out.push_str(&s);
    out.push('"');
    Ok(())
}

/// `Time.UnmarshalJSON`: `null` is a no-op; otherwise a JSON string holding a strict RFC 3339
/// time. Go reads the bytes between the quotes without unescaping them; here the decoded
/// string is used (equal unless the time is written with JSON escapes).
// go-std: time.Time.UnmarshalJSON L2 (the string is the decoded one, not the raw bytes)
pub(crate) fn decode_time(t: &mut Time, v: &Raw) -> Result<(), DecodeError> {
    match v {
        Raw::Null => Ok(()),
        Raw::String(s) => {
            *t = Time::parse_strict_rfc3339(s).map_err(DecodeError::Other)?;
            Ok(())
        }
        _ => Err(DecodeError::Other(
            "Time.UnmarshalJSON: input is not a JSON string".to_owned(),
        )),
    }
}
