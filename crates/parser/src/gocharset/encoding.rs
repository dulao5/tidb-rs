//! go-file: pkg/parser/charset/encoding.go
//! go-file: pkg/parser/charset/encoding_base.go
//! go-file: pkg/parser/charset/encoding_utf8.go
//! go-file: pkg/parser/charset/encoding_latin1.go
//! go-file: pkg/parser/charset/encoding_ascii.go
//! go-file: pkg/parser/charset/encoding_bin.go
//!
//! The `Encoding` interface and its six implementations (block D02.2), ported ahead of the
//! block for `types.Datum` (D01.1): string conversions check and transcode through it
//! (constitution §4.5: the files only reference tidb-base and this module). The GBK
//! implementation is in [`super::encoding_gbk`].
//!
//! Mapping (M-IFACE): Go's interface over six pointer types, which embed each other
//! (`encodingUTF8MB3Strict` and `encodingLatin1` embed `encodingUTF8`, every type embeds
//! `encodingBase`, whose methods call back into the outer type through `self`), is the
//! enum [`Encoding`]; every method matches on it and calls the method the Go method set
//! of that type resolves to (the type's own method, else the promoted one).
//!
//! Go strings are bytes here (`&[u8]`); `Transform` returns `Cow::Borrowed(src)` where Go
//! returns `src` itself (the alias the Go comment warns about) and an owned buffer where Go
//! returns `dest.Bytes()`. Go's optional `dest *bytes.Buffer` (a reusable buffer) is not
//! taken: every caller in the ported code passes `nil` (mapping rule M-POOL).

// Go names (`EncodingTpUTF8`, `OpReplace`, ...) are kept (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::borrow::Cow;

use tidb_base::Error;
use tidb_base::gofmt::{Arg, sprintf};
use tidb_base::gostd::{decode_rune, to_lower_rune, to_upper_rune};

use super::encoding_gbk::{self, ErrInvalidCharacterString};
use super::{CharsetASCII, CharsetBin, CharsetGBK, CharsetLatin1, CharsetUTF8, CharsetUTF8MB4};

/// Encoding provide encode/decode functions for a string with a specific charset.
///
/// The variants are Go's `EncodingUTF8Impl`, `EncodingUTF8MB3StrictImpl`,
/// `EncodingASCIIImpl`, `EncodingLatin1Impl`, `EncodingBinImpl` and `EncodingGBKImpl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// `*encodingUTF8`: TiDB's default encoding.
    UTF8,
    /// `*encodingUTF8MB3Strict`: the strict mode of EncodingUTF8MB3; MB4 characters are
    /// considered invalid.
    UTF8MB3Strict,
    /// `*encodingASCII`.
    ASCII,
    /// `*encodingLatin1`: compatibles with latin1 in old version TiDB (TiDB uses utf8
    /// implementation for latin1 charset because of the backward compatibility).
    Latin1,
    /// `*encodingBin`: the binary encoding.
    Bin,
    /// `*encodingGBK`.
    GBK,
}

/// EncodingUTF8Impl is the instance of encodingUTF8.
pub const EncodingUTF8Impl: Encoding = Encoding::UTF8;
/// EncodingUTF8MB3StrictImpl is the instance of encodingUTF8MB3Strict.
pub const EncodingUTF8MB3StrictImpl: Encoding = Encoding::UTF8MB3Strict;
/// EncodingASCIIImpl is the instance of encodingASCII
pub const EncodingASCIIImpl: Encoding = Encoding::ASCII;
/// EncodingLatin1Impl is the instance of encodingLatin1.
pub const EncodingLatin1Impl: Encoding = Encoding::Latin1;
/// EncodingBinImpl is the instance of encodingBin.
pub const EncodingBinImpl: Encoding = Encoding::Bin;
/// EncodingGBKImpl is the instance of encodingGBK
pub const EncodingGBKImpl: Encoding = Encoding::GBK;

/// EncodingTp is the type of the encoding.
pub type EncodingTp = i8;

/// The types of the encodings.
pub const EncodingTpNone: EncodingTp = 0;
/// The types of the encodings.
pub const EncodingTpUTF8: EncodingTp = 1;
/// The types of the encodings.
pub const EncodingTpUTF8MB3Strict: EncodingTp = 2;
/// The types of the encodings.
pub const EncodingTpASCII: EncodingTp = 3;
/// The types of the encodings.
pub const EncodingTpLatin1: EncodingTp = 4;
/// The types of the encodings.
pub const EncodingTpBin: EncodingTp = 5;
/// The types of the encodings.
pub const EncodingTpGBK: EncodingTp = 6;

/// Op is used by Encoding.Transform.
pub type Op = i16;

const opFromUTF8: Op = 1 << 0;
const opToUTF8: Op = 1 << 1;
const opTruncateTrim: Op = 1 << 2;
const opTruncateReplace: Op = 1 << 3;
const opCollectFrom: Op = 1 << 4;
const opCollectTo: Op = 1 << 5;
const opSkipError: Op = 1 << 6;

/// OpReplaceNoErr is used to replace invalid bytes with '?'.
pub const OpReplaceNoErr: Op = opFromUTF8 | opTruncateReplace | opCollectFrom | opSkipError;
/// Transform operation.
pub const OpReplace: Op = opFromUTF8 | opTruncateReplace | opCollectFrom;
/// Transform operation.
pub const OpEncode: Op = opFromUTF8 | opTruncateTrim | opCollectTo;
/// Transform operation.
pub const OpEncodeNoErr: Op = OpEncode | opSkipError;
/// Transform operation.
pub const OpEncodeReplace: Op = opFromUTF8 | opTruncateReplace | opCollectTo;
/// Transform operation.
pub const OpDecode: Op = opToUTF8 | opTruncateTrim | opCollectTo;
/// Transform operation.
pub const OpDecodeNoErr: Op = OpDecode | opSkipError;
/// Transform operation.
pub const OpDecodeReplace: Op = opToUTF8 | opTruncateReplace | opCollectTo;

/// IsSupportedEncoding checks if the charset is fully supported.
// go: pkg/parser/charset/encoding.go IsSupportedEncoding L1
pub fn is_supported_encoding(charset: &str) -> bool {
    encoding_map(charset).is_some()
}

/// FindEncodingTakeUTF8AsNoop finds the encoding according to the charset
/// except that utf-8 is treated as no-operation encoding. This is used to
/// reduce the overhead of utf-8 validation in some cases.
// go: pkg/parser/charset/encoding.go FindEncodingTakeUTF8AsNoop L1
pub fn find_encoding_take_utf8_as_noop(charset: &str) -> Encoding {
    let enc = find_encoding(charset);
    if enc.tp() == EncodingTpUTF8 {
        return EncodingBinImpl;
    }
    enc
}

/// FindEncoding finds the encoding according to charset.
// go: pkg/parser/charset/encoding.go FindEncoding L1
pub fn find_encoding(charset: &str) -> Encoding {
    if charset.is_empty() {
        return EncodingBinImpl;
    }
    if let Some(e) = encoding_map(charset) {
        return e;
    }
    EncodingBinImpl
}

/// Go's `encodingMap` (a map literal).
// glue: pkg/parser/charset/encoding.go encodingMap (package variable)
fn encoding_map(charset: &str) -> Option<Encoding> {
    match charset {
        CharsetUTF8MB4 => Some(EncodingUTF8Impl),
        CharsetUTF8 => Some(EncodingUTF8Impl),
        CharsetGBK => Some(EncodingGBKImpl),
        CharsetLatin1 => Some(EncodingLatin1Impl),
        CharsetBin => Some(EncodingBinImpl),
        CharsetASCII => Some(EncodingASCIIImpl),
        _ => None,
    }
}

/// CountValidBytes counts the first valid bytes in src that
/// can be encoded to the current encoding.
// go: pkg/parser/charset/encoding.go CountValidBytes L1
pub fn count_valid_bytes(e: Encoding, src: &[u8]) -> isize {
    let mut n_src = 0;
    e.foreach(src, opFromUTF8, &mut |from, _, ok| {
        if ok {
            n_src += from.len() as isize;
        }
        ok
    });
    n_src
}

/// CountValidBytesDecode counts the first valid bytes in src that
/// can be decoded to utf-8.
// go: pkg/parser/charset/encoding.go CountValidBytesDecode L1
pub fn count_valid_bytes_decode(e: Encoding, src: &[u8]) -> isize {
    let mut n_src = 0;
    e.foreach(src, opToUTF8, &mut |from, _, ok| {
        if ok {
            n_src += from.len() as isize;
        }
        ok
    });
    n_src
}

/// The callback of `Foreach`: `func(from, to []byte, ok bool) bool`.
pub type ForeachFn<'f> = dyn FnMut(&[u8], &[u8], bool) -> bool + 'f;

impl Encoding {
    /// Name is the name of the encoding.
    // glue: dispatch of the Encoding.Name interface method
    pub fn name(self) -> &'static str {
        match self {
            Encoding::UTF8 | Encoding::UTF8MB3Strict => utf8_name(),
            Encoding::ASCII => ascii_name(),
            Encoding::Latin1 => latin1_name(),
            Encoding::Bin => bin_name(),
            Encoding::GBK => encoding_gbk::gbk_name(),
        }
    }

    /// Tp is the type of the encoding.
    // glue: dispatch of the Encoding.Tp interface method
    pub fn tp(self) -> EncodingTp {
        match self {
            // encodingUTF8MB3Strict has no Tp method: encodingUTF8's is promoted.
            Encoding::UTF8 | Encoding::UTF8MB3Strict => utf8_tp(),
            Encoding::ASCII => ascii_tp(),
            Encoding::Latin1 => latin1_tp(),
            Encoding::Bin => bin_tp(),
            Encoding::GBK => encoding_gbk::gbk_tp(),
        }
    }

    /// Peek returns the next char.
    // glue: dispatch of the Encoding.Peek interface method
    pub fn peek(self, src: &[u8]) -> &[u8] {
        match self {
            Encoding::UTF8 | Encoding::UTF8MB3Strict => utf8_peek(src),
            Encoding::ASCII => ascii_peek(src),
            Encoding::Latin1 => latin1_peek(src),
            Encoding::Bin => bin_peek(src),
            Encoding::GBK => encoding_gbk::gbk_peek(src),
        }
    }

    /// MbLen returns multiple byte length, if the next character is single byte, return 0.
    // glue: dispatch of the Encoding.MbLen interface method
    pub fn mb_len(self, bs: &[u8]) -> isize {
        match self {
            Encoding::UTF8 | Encoding::UTF8MB3Strict | Encoding::Latin1 => utf8_mb_len(bs),
            Encoding::ASCII | Encoding::Bin => base_mb_len(bs),
            Encoding::GBK => encoding_gbk::gbk_mb_len(bs),
        }
    }

    /// IsValid checks whether the utf-8 bytes can be convert to valid string in current encoding.
    // glue: dispatch of the Encoding.IsValid interface method
    pub fn is_valid(self, src: &[u8]) -> bool {
        match self {
            Encoding::UTF8 => utf8_is_valid(self, src),
            Encoding::UTF8MB3Strict => utf8mb3_strict_is_valid(self, src),
            Encoding::ASCII => ascii_is_valid(src),
            Encoding::Latin1 => latin1_is_valid(src),
            Encoding::Bin => bin_is_valid(src),
            Encoding::GBK => base_is_valid(self, src),
        }
    }

    /// Foreach iterates the characters in current encoding.
    // glue: dispatch of the Encoding.Foreach interface method
    pub fn foreach(self, src: &[u8], op: Op, f: &mut ForeachFn<'_>) {
        match self {
            Encoding::UTF8 | Encoding::Latin1 => utf8_foreach(src, op, f),
            Encoding::UTF8MB3Strict => utf8mb3_strict_foreach(src, op, f),
            Encoding::ASCII => ascii_foreach(src, op, f),
            Encoding::Bin => bin_foreach(src, op, f),
            Encoding::GBK => base_foreach(self, src, op, f),
        }
    }

    /// Transform map the bytes in src to dest according to Op.
    ///
    /// **the returned array may be the alias of `src`, edit the returned array on your own risk**
    // glue: dispatch of the Encoding.Transform interface method
    pub fn transform(self, src: &[u8], op: Op) -> (Cow<'_, [u8]>, Result<(), Error>) {
        match self {
            Encoding::UTF8 => utf8_transform(self, src, op),
            Encoding::UTF8MB3Strict => utf8mb3_strict_transform(self, src, op),
            Encoding::ASCII => ascii_transform(self, src, op),
            Encoding::Latin1 => latin1_transform(src, op),
            Encoding::Bin => bin_transform(src, op),
            Encoding::GBK => base_transform(self, src, op),
        }
    }

    /// ToUpper change a string to uppercase.
    // glue: dispatch of the Encoding.ToUpper interface method
    pub fn to_upper(self, src: &[u8]) -> Vec<u8> {
        match self {
            Encoding::GBK => encoding_gbk::gbk_to_upper(src),
            _ => base_to_upper(src),
        }
    }

    /// ToLower change a string to lowercase.
    // glue: dispatch of the Encoding.ToLower interface method
    pub fn to_lower(self, src: &[u8]) -> Vec<u8> {
        match self {
            Encoding::GBK => encoding_gbk::gbk_to_lower(src),
            _ => base_to_lower(src),
        }
    }
}

// --- encoding_base.go -----------------------------------------------------------------

// go: pkg/parser/charset/encoding_base.go encodingBase.MbLen L1
fn base_mb_len(_bs: &[u8]) -> isize {
    0
}

// go: pkg/parser/charset/encoding_base.go encodingBase.ToUpper L1
fn base_to_upper(src: &[u8]) -> Vec<u8> {
    go_strings_map(src, to_upper_rune)
}

// go: pkg/parser/charset/encoding_base.go encodingBase.ToLower L1
fn base_to_lower(src: &[u8]) -> Vec<u8> {
    go_strings_map(src, to_lower_rune)
}

/// `strings.Map(mapping, s)` (what `strings.ToUpper`, `ToLower` and their `Special` forms
/// reduce to for a non-ASCII string; on ASCII input the result is the same): every rune
/// mapped, an invalid byte written as U+FFFD.
// go-std: strings.Map L1
pub(crate) fn go_strings_map(s: &[u8], mapping: impl Fn(char) -> char) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let (c, w) = decode_rune(&s[i..]);
        let r = mapping(c);
        let mut buf = [0u8; 4];
        out.extend_from_slice(r.encode_utf8(&mut buf).as_bytes());
        i += w;
    }
    out
}

// go: pkg/parser/charset/encoding_base.go encodingBase.IsValid L1
fn base_is_valid(this: Encoding, src: &[u8]) -> bool {
    let mut is_valid = true;
    this.foreach(src, opFromUTF8, &mut |_, _, ok| {
        is_valid = ok;
        ok
    });
    is_valid
}

// go: pkg/parser/charset/encoding_base.go encodingBase.Transform L1
fn base_transform(this: Encoding, src: &[u8], op: Op) -> (Cow<'_, [u8]>, Result<(), Error>) {
    let mut dest = Vec::with_capacity(src.len());
    let mut err = Ok(());
    this.foreach(src, op, &mut |from, to, ok| {
        if !ok {
            if err.is_ok() && (op & opSkipError == 0) {
                err = Err(generate_encoding_err(this.name(), from));
            }
            if op & opTruncateTrim != 0 {
                return false;
            }
            if op & opTruncateReplace != 0 {
                dest.push(b'?');
                return true;
            }
        }
        if op & opCollectFrom != 0 {
            dest.extend_from_slice(from);
        } else if op & opCollectTo != 0 {
            dest.extend_from_slice(to);
        }
        true
    });
    (Cow::Owned(dest), err)
}

/// `encodingBase.Foreach`: only `encodingGBK` does not override it, so the transformer is
/// `customGBK`'s encoder or decoder ([`encoding_gbk`]), called on one character at a time
/// with `atEOF == false` and a 4-byte destination.
// go: pkg/parser/charset/encoding_base.go encodingBase.Foreach L1
fn base_foreach(this: Encoding, src: &[u8], op: Op, f: &mut ForeachFn<'_>) {
    let from_utf8 = op & opFromUTF8 != 0;
    let mut buf = [0u8; 4];
    let mut i = 0;
    while i < src.len() {
        let w = if from_utf8 {
            EncodingUTF8Impl.peek(&src[i..]).len()
        } else {
            this.peek(&src[i..]).len()
        };
        let (n_dst, err) = if from_utf8 {
            encoding_gbk::custom_gbk_encoder_transform(&mut buf, &src[i..i + w])
        } else {
            encoding_gbk::custom_gbk_decoder_transform(&mut buf, &src[i..i + w])
        };
        let meet_err = err || (op & opToUTF8 != 0 && begin_with_replacement_char(&buf[..n_dst]));
        if !f(&src[i..i + w], &buf[..n_dst], !meet_err) {
            return;
        }
        i += w;
    }
}

/// replacementBytes are bytes for the replacement rune 0xfffd.
const replacementBytes: [u8; 3] = [0xEF, 0xBF, 0xBD];

/// beginWithReplacementChar check if dst has the prefix '0xEFBFBD'.
// go: pkg/parser/charset/encoding_base.go beginWithReplacementChar L1
fn begin_with_replacement_char(dst: &[u8]) -> bool {
    dst.starts_with(&replacementBytes)
}

/// generateEncodingErr generates an invalid string in charset error.
// go: pkg/parser/charset/encoding_base.go generateEncodingErr L1
fn generate_encoding_err(name: &str, invalid_bytes: &[u8]) -> Error {
    let arg = sprintf("%X", &[Arg::Bytes(invalid_bytes.to_vec())]);
    ErrInvalidCharacterString.fast_gen_by_args(vec![
        Arg::from(name.to_owned()),
        Arg::go_string(arg),
    ])
}

/// HackSlice converts string to slice without copy.
/// Use at your own risk.
// go: pkg/parser/charset/encoding_base.go HackSlice L1
pub fn hack_slice(s: &[u8]) -> &[u8] {
    s
}

/// HackString converts slice to string without copy.
/// Use it at your own risk.
// go: pkg/parser/charset/encoding_base.go HackString L1
pub fn hack_string(b: &[u8]) -> &[u8] {
    b
}

// --- encoding_utf8.go -----------------------------------------------------------------

/// Name implements Encoding interface.
// go: pkg/parser/charset/encoding_utf8.go encodingUTF8.Name L1
fn utf8_name() -> &'static str {
    CharsetUTF8MB4
}

/// Tp implements Encoding interface.
// go: pkg/parser/charset/encoding_utf8.go encodingUTF8.Tp L1
fn utf8_tp() -> EncodingTp {
    EncodingTpUTF8
}

/// Peek implements Encoding interface.
// go: pkg/parser/charset/encoding_utf8.go encodingUTF8.Peek L1
fn utf8_peek(src: &[u8]) -> &[u8] {
    let mut next_len = 4;
    if src.is_empty() || src[0] < 0x80 {
        next_len = 1;
    } else if src[0] < 0xe0 {
        next_len = 2;
    } else if src[0] < 0xf0 {
        next_len = 3;
    }
    if src.len() < next_len {
        return src;
    }
    &src[..next_len]
}

// go: pkg/parser/charset/encoding_utf8.go encodingUTF8.MbLen L1
fn utf8_mb_len(bs: &[u8]) -> isize {
    let (_, size) = decode_rune(bs);
    if size <= 1 {
        return 0;
    }
    size as isize
}

/// IsValid implements Encoding interface.
// go: pkg/parser/charset/encoding_utf8.go encodingUTF8.IsValid L1
fn utf8_is_valid(this: Encoding, src: &[u8]) -> bool {
    if std::str::from_utf8(src).is_ok() {
        return true;
    }
    base_is_valid(this, src)
}

/// Transform implements Encoding interface.
// go: pkg/parser/charset/encoding_utf8.go encodingUTF8.Transform L1
fn utf8_transform(this: Encoding, src: &[u8], op: Op) -> (Cow<'_, [u8]>, Result<(), Error>) {
    if utf8_is_valid(this, src) {
        return (Cow::Borrowed(src), Ok(()));
    }
    base_transform(this, src, op)
}

/// Foreach implements Encoding interface.
// go: pkg/parser/charset/encoding_utf8.go encodingUTF8.Foreach L1
fn utf8_foreach(src: &[u8], _op: Op, f: &mut ForeachFn<'_>) {
    let mut i = 0;
    while i < src.len() {
        let (rv, w) = decode_rune(&src[i..]);
        let meet_err = rv == char::REPLACEMENT_CHARACTER && w == 1;
        if !f(&src[i..i + w], &src[i..i + w], !meet_err) {
            return;
        }
        i += w;
    }
}

/// IsValid implements Encoding interface.
// go: pkg/parser/charset/encoding_utf8.go encodingUTF8MB3Strict.IsValid L1
fn utf8mb3_strict_is_valid(this: Encoding, src: &[u8]) -> bool {
    base_is_valid(this, src)
}

/// Foreach implements Encoding interface.
// go: pkg/parser/charset/encoding_utf8.go encodingUTF8MB3Strict.Foreach L1
fn utf8mb3_strict_foreach(src: &[u8], _op: Op, f: &mut ForeachFn<'_>) {
    let mut i = 0;
    while i < src.len() {
        let (rv, w) = decode_rune(&src[i..]);
        let meet_err = (rv == char::REPLACEMENT_CHARACTER && w == 1) || w > 3;
        if !f(&src[i..i + w], &src[i..i + w], !meet_err) {
            return;
        }
        i += w;
    }
}

/// Transform implements Encoding interface.
// go: pkg/parser/charset/encoding_utf8.go encodingUTF8MB3Strict.Transform L1
fn utf8mb3_strict_transform(
    this: Encoding,
    src: &[u8],
    op: Op,
) -> (Cow<'_, [u8]>, Result<(), Error>) {
    if utf8mb3_strict_is_valid(this, src) {
        return (Cow::Borrowed(src), Ok(()));
    }
    base_transform(this, src, op)
}

// --- encoding_latin1.go ---------------------------------------------------------------

/// Name implements Encoding interface.
// go: pkg/parser/charset/encoding_latin1.go encodingLatin1.Name L1
fn latin1_name() -> &'static str {
    CharsetLatin1
}

/// Peek implements Encoding interface.
// go: pkg/parser/charset/encoding_latin1.go encodingLatin1.Peek L1
fn latin1_peek(src: &[u8]) -> &[u8] {
    if src.is_empty() {
        return src;
    }
    &src[..1]
}

/// IsValid implements Encoding interface.
// go: pkg/parser/charset/encoding_latin1.go encodingLatin1.IsValid L1
fn latin1_is_valid(_src: &[u8]) -> bool {
    true
}

/// Tp implements Encoding interface.
// go: pkg/parser/charset/encoding_latin1.go encodingLatin1.Tp L1
fn latin1_tp() -> EncodingTp {
    EncodingTpLatin1
}

// go: pkg/parser/charset/encoding_latin1.go encodingLatin1.Transform L1
fn latin1_transform(src: &[u8], _op: Op) -> (Cow<'_, [u8]>, Result<(), Error>) {
    (Cow::Borrowed(src), Ok(()))
}

// --- encoding_ascii.go ----------------------------------------------------------------

/// Name implements Encoding interface.
// go: pkg/parser/charset/encoding_ascii.go encodingASCII.Name L1
fn ascii_name() -> &'static str {
    CharsetASCII
}

/// Tp implements Encoding interface.
// go: pkg/parser/charset/encoding_ascii.go encodingASCII.Tp L1
fn ascii_tp() -> EncodingTp {
    EncodingTpASCII
}

/// Peek implements Encoding interface.
// go: pkg/parser/charset/encoding_ascii.go encodingASCII.Peek L1
fn ascii_peek(src: &[u8]) -> &[u8] {
    if src.is_empty() {
        return src;
    }
    &src[..1]
}

/// IsValid implements Encoding interface.
// go: pkg/parser/charset/encoding_ascii.go encodingASCII.IsValid L1
fn ascii_is_valid(src: &[u8]) -> bool {
    for &b in src {
        if b > 0x7f {
            return false;
        }
    }
    true
}

/// Transform implements Encoding interface.
// go: pkg/parser/charset/encoding_ascii.go encodingASCII.Transform L1
fn ascii_transform(this: Encoding, src: &[u8], op: Op) -> (Cow<'_, [u8]>, Result<(), Error>) {
    if ascii_is_valid(src) {
        return (Cow::Borrowed(src), Ok(()));
    }
    base_transform(this, src, op)
}

/// Foreach implements Encoding interface.
// go: pkg/parser/charset/encoding_ascii.go encodingASCII.Foreach L1
fn ascii_foreach(src: &[u8], _op: Op, f: &mut ForeachFn<'_>) {
    let mut i = 0;
    while i < src.len() {
        let mut w = 1;
        let mut ok = true;
        if src[i] > 0x7f {
            w = EncodingUTF8Impl.peek(&src[i..]).len();
            ok = false;
        }
        if !f(&src[i..i + w], &src[i..i + w], ok) {
            return;
        }
        i += w;
    }
}

// --- encoding_bin.go ------------------------------------------------------------------

/// Name implements Encoding interface.
// go: pkg/parser/charset/encoding_bin.go encodingBin.Name L1
fn bin_name() -> &'static str {
    CharsetBin
}

/// Tp implements Encoding interface.
// go: pkg/parser/charset/encoding_bin.go encodingBin.Tp L1
fn bin_tp() -> EncodingTp {
    EncodingTpBin
}

/// Peek implements Encoding interface.
// go: pkg/parser/charset/encoding_bin.go encodingBin.Peek L1
fn bin_peek(src: &[u8]) -> &[u8] {
    if src.is_empty() {
        return src;
    }
    &src[..1]
}

/// IsValid implements Encoding interface.
// go: pkg/parser/charset/encoding_bin.go encodingBin.IsValid L1
fn bin_is_valid(_src: &[u8]) -> bool {
    true
}

/// Foreach implements Encoding interface.
// go: pkg/parser/charset/encoding_bin.go encodingBin.Foreach L1
fn bin_foreach(src: &[u8], _op: Op, f: &mut ForeachFn<'_>) {
    for i in 0..src.len() {
        if !f(&src[i..i + 1], &src[i..i + 1], true) {
            return;
        }
    }
}

// go: pkg/parser/charset/encoding_bin.go encodingBin.Transform L1
fn bin_transform(src: &[u8], _op: Op) -> (Cow<'_, [u8]>, Result<(), Error>) {
    (Cow::Borrowed(src), Ok(()))
}
