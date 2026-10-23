//! Shim of TiKV `log_wrappers`: hex-printing of keys/values. TiKV's redaction
//! (`REDACT_INFO_LOG`, online_config) is omitted; a real port would make it a hook.
use std::fmt;

pub fn hex_encode_upper<T: AsRef<[u8]>>(data: T) -> String { hex::encode_upper(data) }

pub struct Value<'a>(pub &'a [u8]);
impl<'a> Value<'a> {
    pub fn key(key: &'a [u8]) -> Self { Value(key) }
    pub fn value(v: &'a [u8]) -> Self { Value(v) }
}
impl slog::Value for Value<'_> {
    fn serialize(&self, _r: &slog::Record<'_>, key: slog::Key, s: &mut dyn slog::Serializer) -> slog::Result {
        s.emit_arguments(key, &format_args!("{}", hex_encode_upper(self.0)))
    }
}
impl fmt::Display for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", hex_encode_upper(self.0)) }
}
impl fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Display::fmt(self, f) }
}
