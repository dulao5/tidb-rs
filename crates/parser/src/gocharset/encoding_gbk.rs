//! go-file: pkg/parser/charset/encoding_gbk.go
//! go-file: golang.org/x/text@v0.39.0/encoding/simplifiedchinese/gbk.go
//! go-file: golang.org/x/text@v0.39.0/encoding/encoding.go
//!
//! `encoding_gbk.go` (block D02.2): TiDB's `customGBK` encoder (it rejects `€`, which
//! x/text's GBK encodes as the Code Page 936 byte 0x80) and decoder (it decodes 0x80 as
//! U+FFFD) over x/text's `simplifiedchinese.GBK`, and `encodingGBK`, the GBK variant of
//! [`super::encoding::Encoding`]. The encoder came with `pkg/util/collate`'s `gbk_bin`
//! (D01.9); the decoder and `encodingGBK` with `types.Datum` (D01.1).
//!
//! x/text's five encode tables are one generated table over the BMP (`gbk_encode_data.rs`),
//! its decode table is `gbk_decode_data.rs`.

// Go declares `size` and assigns it in the branches of x/text gbkEncoder.Transform.
#![allow(clippy::needless_late_init)]

use std::sync::LazyLock;

use tidb_base::gostd::decode_rune;
use tidb_base::{Error, errors, mysql, terror};

use super::encoding::{EncodingTp, EncodingTpGBK, go_strings_map};
use super::gbk_decode_data::gbkDecode;
use super::gbk_encode_data::gbkEncode;
use super::CharsetGBK;

/// ErrInvalidCharacterString returns when the string is invalid in the specific charset.
// glue: pkg/parser/charset/encoding_base.go ErrInvalidCharacterString (package variable)
pub static ErrInvalidCharacterString: LazyLock<Error> =
    LazyLock::new(|| terror::ClassParser.new_std(mysql::ErrInvalidCharacterString));

/// x/text `internal.ErrASCIIReplacement` (a `RepertoireError`): the rune has no code in the
/// destination encoding.
// glue: golang.org/x/text/encoding/internal.ErrASCIIReplacement (package variable)
fn err_ascii_replacement() -> Error {
    errors::new("encoding: rune not supported by encoding.")
}

/// `*encoding.Encoder` wrapping TiDB's `customGBKEncoder`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Encoder {
    gbk_encoder: GbkEncoder,
}

/// x/text `simplifiedchinese.gbkEncoder` with `gb18030 == false`.
#[derive(Debug, Clone, Copy, Default)]
struct GbkEncoder;

/// NewCustomGBKEncoder return a custom GBK encoding.
// go: pkg/parser/charset/encoding_gbk.go NewCustomGBKEncoder L1
pub fn new_custom_gbk_encoder() -> Encoder {
    custom_gbk_new_encoder()
}

/// NewEncoder returns simplifiedchinese.gbk.
// go: pkg/parser/charset/encoding_gbk.go customGBK.NewEncoder L1
fn custom_gbk_new_encoder() -> Encoder {
    Encoder {
        gbk_encoder: GbkEncoder,
    }
}

impl Encoder {
    /// `(*encoding.Encoder).Bytes`: converts a UTF-8 byte string to GBK, or the first error.
    ///
    /// Go runs `transform.Bytes`, which calls `Transform` again with a bigger destination on
    /// `ErrShortDst`. A GBK encoding is never longer than its UTF-8 input (1 byte for ASCII
    /// and `€`, 2 for the 2- and 3-byte runes, none for 4-byte runes), so the first
    /// destination (`len(src)` bytes) is always big enough and `Transform` runs once.
    // go-dep: golang.org/x/text@v0.39.0/encoding.Encoder.Bytes L2 (one Transform call: the destination is never short, see above)
    pub fn bytes(&self, b: &[u8]) -> Result<Vec<u8>, Error> {
        let mut dst = Vec::with_capacity(b.len());
        self.transform(&mut dst, b, true)?;
        Ok(dst)
    }

    /// Transform special treatment for `€`,
    /// see https://github.com/pingcap/tidb/issues/30581 get details.
    // go: pkg/parser/charset/encoding_gbk.go customGBKEncoder.Transform L1
    fn transform(&self, dst: &mut Vec<u8>, src: &[u8], at_eof: bool) -> Result<(), Error> {
        if src.starts_with(&[0xe2, 0x82, 0xac] /* '€' */) {
            return Err(ErrInvalidCharacterString.clone());
        }
        self.gbk_encoder.transform(dst, src, at_eof)
    }

    /// Reset is same as simplifiedchinese.gbk.
    // go: pkg/parser/charset/encoding_gbk.go customGBKEncoder.Reset L1
    pub fn reset(&self) {}
}

impl GbkEncoder {
    /// `gbkEncoder.Transform(dst, src, atEOF)`. The encoded bytes are appended to `dst` (Go
    /// writes `dst[:nDst]` and returns the error, which `Encoder.Bytes` turns into
    /// `nil, err`). `transform.Bytes` passes `atEOF = true`; `encodingBase.Foreach` passes
    /// `false` with a 4-byte destination, which one character never overflows (`ErrShortDst`
    /// cannot happen: the encoder writes at most 2 bytes per rune).
    // go-dep: golang.org/x/text@v0.39.0/encoding/simplifiedchinese.gbkEncoder.Transform L2 (the encode0..encode4 lookups are one BMP table; ErrShortDst never happens for the callers)
    fn transform(&self, dst: &mut Vec<u8>, src: &[u8], at_eof: bool) -> Result<(), Error> {
        let mut n_src = 0;
        while n_src < src.len() {
            let mut r = src[n_src] as u32;
            let size;

            // Decode a 1-byte rune.
            if r < 0x80 {
                size = 1;
            } else {
                // Decode a multi-byte rune.
                let (c, s) = decode_rune(&src[n_src..]);
                r = c as u32;
                size = s;
                if size == 1 {
                    // All valid runes of size 1 (those below utf8.RuneSelf) were
                    // handled above. We have invalid UTF-8 or we haven't seen the
                    // full character yet.
                    if !at_eof && !full_rune(&src[n_src..]) {
                        return Err(err_short_src());
                    }
                }

                // Microsoft's Code Page 936 extends GBK 1.0 to encode the euro sign U+20AC
                // as 0x80. The HTML5 specification at http://encoding.spec.whatwg.org/#gbk
                // says to treat "gbk" as Code Page 936.
                // GBK’s encoder is gb18030’s encoder with its _is GBK_ set to true. https://encoding.spec.whatwg.org/#gbk-encoder
                // If _is GBK_ is true and code point is U+20AC, return byte 0x80. https://encoding.spec.whatwg.org/#gb18030-encoder
                if r == '€' as u32 {
                    dst.push(0x80);
                    n_src += size;
                    continue;
                }
                let r2 = if r < 0x10000 {
                    gbkEncode[r as usize]
                } else {
                    0
                };
                if r2 == 0 {
                    return Err(err_ascii_replacement());
                }
                // write2
                dst.push((r2 >> 8) as u8);
                dst.push(r2 as u8);
                n_src += size;
                continue;
            }

            // write1
            dst.push(r as u8);
            n_src += size;
        }
        Ok(())
    }
}

/// x/text `transform.ErrShortSrc`.
// glue: golang.org/x/text/transform.ErrShortSrc (package variable)
fn err_short_src() -> Error {
    errors::new("transform: short source buffer")
}

/// `utf8.FullRune`: whether `p` begins with a full UTF-8 encoding of a rune (an invalid
/// encoding counts as a full rune: it decodes as a width-1 error).
// go-std: unicode/utf8.FullRune L1
#[allow(clippy::if_same_then_else)] // Go's branch structure
fn full_rune(p: &[u8]) -> bool {
    let n = p.len();
    if n == 0 {
        return false;
    }
    let x = p[0];
    // utf8.first: the length and accept range of a lead byte.
    let (len, lo, hi): (usize, u8, u8) = match x {
        0x00..=0x7f => return true,
        0xc2..=0xdf => (2, 0x80, 0xbf),
        0xe0 => (3, 0xa0, 0xbf),
        0xe1..=0xec | 0xee..=0xef => (3, 0x80, 0xbf),
        0xed => (3, 0x80, 0x9f),
        0xf0 => (4, 0x90, 0xbf),
        0xf1..=0xf3 => (4, 0x80, 0xbf),
        0xf4 => (4, 0x80, 0x8f),
        _ => return true, // invalid lead byte: a width-1 error
    };
    if n >= len {
        return true;
    }
    // Must be short or invalid.
    if n > 1 && (p[1] < lo || hi < p[1]) {
        return true;
    } else if n > 2 && (p[2] < 0x80 || 0xbf < p[2]) {
        return true;
    }
    false
}

/// `customGBK{}.NewEncoder()` run by `encodingBase.Foreach` on one character:
/// `Transform(buf[:4], src, false)`. Returns `nDst` (the bytes are in `dst`) and whether
/// `Transform` returned an error.
// glue: encodingBase.Foreach's call of the customGBK encoder's Transform
pub(crate) fn custom_gbk_encoder_transform(dst: &mut [u8; 4], src: &[u8]) -> (usize, bool) {
    let mut out = Vec::with_capacity(4);
    let res = custom_gbk_new_encoder().transform(&mut out, src, false);
    let n = out.len().min(4);
    dst[..n].copy_from_slice(&out[..n]);
    (n, res.is_err())
}

/// NewDecoder returns simplifiedchinese.GBK.NewDecoder().
// go: pkg/parser/charset/encoding_gbk.go customGBK.NewDecoder L1
fn custom_gbk_new_decoder() -> CustomGbkDecoder {
    CustomGbkDecoder
}

/// `customGBKDecoder` over x/text's `simplifiedchinese.GBK` decoder (stateless).
#[derive(Debug, Clone, Copy, Default)]
struct CustomGbkDecoder;

impl CustomGbkDecoder {
    /// Transform special treatment for 0x80,
    /// see https://github.com/pingcap/tidb/issues/30581 get details.
    // go: pkg/parser/charset/encoding_gbk.go customGBKDecoder.Transform L1
    fn transform(&self, dst: &mut [u8], src: &[u8], at_eof: bool) -> (usize, usize, bool) {
        if src.is_empty() {
            return (0, 0, false);
        }
        if src[0] == 0x80 {
            dst[..3].copy_from_slice(&[0xEF, 0xBF, 0xBD]);
            return (3, 1, false);
        }
        gbk_decoder_transform(dst, src, at_eof)
    }

    /// Reset is same as simplifiedchinese.GBK.Reset().
    // go: pkg/parser/charset/encoding_gbk.go customGBKDecoder.Reset L1
    #[allow(dead_code)]
    fn reset(&self) {}
}

/// x/text `gbkDecoder.Transform` (`gb18030 == false`): `(nDst, nSrc, err != nil)`; the
/// errors are `ErrShortSrc` and `ErrShortDst`, which the callers only test for non-nil.
// go-dep: golang.org/x/text@v0.39.0/encoding/simplifiedchinese.gbkDecoder.Transform L2 (gb18030 is false: the four-byte branch is gone; errors as a flag)
fn gbk_decoder_transform(dst: &mut [u8], src: &[u8], at_eof: bool) -> (usize, usize, bool) {
    let (mut n_dst, mut n_src) = (0usize, 0usize);
    while n_src < src.len() {
        let c0 = src[n_src];
        let (r, size): (u32, usize) = if c0 < 0x80 {
            (u32::from(c0), 1)
        } else if c0 == 0x80 {
            // Microsoft's Code Page 936 extends GBK 1.0 to encode the euro sign U+20AC
            // as 0x80. The HTML5 specification at http://encoding.spec.whatwg.org/#gbk
            // says to treat "gbk" as Code Page 936.
            ('€' as u32, 1)
        } else if c0 < 0xff {
            if n_src + 1 >= src.len() {
                if !at_eof {
                    return (n_dst, n_src, true);
                }
                (0xfffd, 1)
            } else {
                let c1 = src[n_src + 1];
                let c1 = match c1 {
                    0x40..=0x7e => Some(c1 - 0x40),
                    0x80..=0xfe => Some(c1 - 0x41),
                    _ => None,
                };
                match c1 {
                    None => (0xfffd, 1),
                    Some(c1) => {
                        let i = usize::from(c0 - 0x81) * 190 + usize::from(c1);
                        let r = gbkDecode[i];
                        (if r == 0 { 0xfffd } else { u32::from(r) }, 2)
                    }
                }
            }
        } else {
            (0xfffd, 1)
        };
        // write:
        let ch = char::from_u32(r).unwrap_or(char::REPLACEMENT_CHARACTER);
        if n_dst + ch.len_utf8() > dst.len() {
            return (n_dst, n_src, true);
        }
        n_dst += ch.encode_utf8(&mut dst[n_dst..]).len();
        n_src += size;
    }
    (n_dst, n_src, false)
}

/// `customGBK{}.NewDecoder()` run by `encodingBase.Foreach` on one character:
/// `Transform(buf[:4], src, false)`. Returns `nDst` and whether `Transform` returned an
/// error.
// glue: encodingBase.Foreach's call of the customGBK decoder's Transform
pub(crate) fn custom_gbk_decoder_transform(dst: &mut [u8; 4], src: &[u8]) -> (usize, bool) {
    let (n_dst, _, err) = custom_gbk_new_decoder().transform(dst, src, false);
    (n_dst, err)
}

/// Name implements Encoding interface.
// go: pkg/parser/charset/encoding_gbk.go encodingGBK.Name L1
pub(crate) fn gbk_name() -> &'static str {
    CharsetGBK
}

/// Tp implements Encoding interface.
// go: pkg/parser/charset/encoding_gbk.go encodingGBK.Tp L1
pub(crate) fn gbk_tp() -> EncodingTp {
    EncodingTpGBK
}

/// Peek implements Encoding interface.
// go: pkg/parser/charset/encoding_gbk.go encodingGBK.Peek L1
pub(crate) fn gbk_peek(src: &[u8]) -> &[u8] {
    let mut char_len = 2;
    if src.is_empty() || src[0] < 0x80 {
        // A byte in the range 00–7F is a single byte that means the same thing as it does in ASCII.
        char_len = 1;
    }
    if char_len < src.len() {
        return &src[..char_len];
    }
    src
}

// go: pkg/parser/charset/encoding_gbk.go encodingGBK.MbLen L1
pub(crate) fn gbk_mb_len(bs: &[u8]) -> isize {
    if bs.len() < 2 {
        return 0;
    }
    if 0x81 <= bs[0] && bs[0] <= 0xfe && ((0x40 <= bs[1] && bs[1] <= 0x7e) || (0x80 <= bs[1] && bs[1] <= 0xfe)) {
        return 2;
    }
    0
}

/// ToUpper implements Encoding interface.
// go: pkg/parser/charset/encoding_gbk.go encodingGBK.ToUpper L1
pub(crate) fn gbk_to_upper(d: &[u8]) -> Vec<u8> {
    go_strings_map(d, |r| if in_gbk_case(r) { r } else { tidb_base::gostd::to_upper_rune(r) })
}

/// ToLower implements Encoding interface.
// go: pkg/parser/charset/encoding_gbk.go encodingGBK.ToLower L1
pub(crate) fn gbk_to_lower(d: &[u8]) -> Vec<u8> {
    go_strings_map(d, |r| if in_gbk_case(r) { r } else { tidb_base::gostd::to_lower_rune(r) })
}

/// GBKCase follows https://dev.mysql.com/worklog/task/?id=4583.
///
/// Every range has the deltas `{0, 0, 0}`: `SpecialCase.ToUpper` / `ToLower` keep these runes
/// as they are (the mapping exists, so `unicode.ToUpper` is not consulted).
pub static GBKCase: [(u32, u32); 23] = [
    (0x00E0, 0x00E1),
    (0x00E8, 0x00EA),
    (0x00EC, 0x00ED),
    (0x00F2, 0x00F3),
    (0x00F9, 0x00FA),
    (0x00FC, 0x00FC),
    (0x0101, 0x0101),
    (0x0113, 0x0113),
    (0x011B, 0x011B),
    (0x012B, 0x012B),
    (0x0144, 0x0144),
    (0x0148, 0x0148),
    (0x014D, 0x014D),
    (0x016B, 0x016B),
    (0x01CE, 0x01CE),
    (0x01D0, 0x01D0),
    (0x01D2, 0x01D2),
    (0x01D4, 0x01D4),
    (0x01D6, 0x01D6),
    (0x01D8, 0x01D8),
    (0x01DA, 0x01DA),
    (0x01DC, 0x01DC),
    (0x216A, 0x216B),
];

/// Whether `unicode.SpecialCase.ToUpper` / `ToLower` finds `r` in [`GBKCase`].
// go-std: unicode.SpecialCase.ToUpper L1 (all deltas of GBKCase are zero)
fn in_gbk_case(r: char) -> bool {
    let r = r as u32;
    GBKCase.iter().any(|&(lo, hi)| lo <= r && r <= hi)
}
