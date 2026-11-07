//! Port of `github.com/BurntSushi/toml` v1.5.0 (the decoder of `pkg/config`), a go-dep of
//! block D00.2: lexer, parser and the reflection-driven `Decode` into the config structs,
//! including `MetaData.Undecoded` / `IsDefined` that `Config.Load` uses for unknown and
//! moved options. Encoding is not ported (TiDB only decodes TOML).

mod datetime;
mod decode;
mod error;
mod lex;
mod parse;

pub use decode::{MetaData, decode, decode_file};
pub use error::{DecodeError, LexErr, ParseError, Position};
pub use parse::{Key, key_string};
