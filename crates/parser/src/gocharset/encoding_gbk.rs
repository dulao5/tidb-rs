//! go-file: pkg/parser/charset/encoding_gbk.go
//! go-file: golang.org/x/text@v0.39.0/encoding/simplifiedchinese/gbk.go
//! go-file: golang.org/x/text@v0.39.0/encoding/encoding.go
//!
//! The GBK encoder that `pkg/util/collate`'s `gbk_bin` uses: TiDB's `customGBK` encoder
//! (it rejects `€`, which x/text's GBK encodes as the Code Page 936 byte 0x80) over x/text's
//! `simplifiedchinese.GBK` encoder. Only the encoder side of `encoding_gbk.go` is here; the
//! `Encoding` implementation (`encodingGBK`) belongs with the rest of the `Encoding` files
//! of D02.2 and is not ported yet.
//!
//! x/text's five encode tables are one generated table over the BMP (`gbk_encode_data.rs`).

// Go declares `size` and assigns it in the branches of x/text gbkEncoder.Transform.
#![allow(clippy::needless_late_init)]

use std::sync::LazyLock;

use tidb_base::gostd::decode_rune;
use tidb_base::{Error, errors, mysql, terror};

use super::gbk_encode_data::gbkEncode;

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
        self.transform(&mut dst, b)?;
        Ok(dst)
    }

    /// Transform special treatment for `€`,
    /// see https://github.com/pingcap/tidb/issues/30581 get details.
    // go: pkg/parser/charset/encoding_gbk.go customGBKEncoder.Transform L1
    fn transform(&self, dst: &mut Vec<u8>, src: &[u8]) -> Result<(), Error> {
        if src.starts_with(&[0xe2, 0x82, 0xac] /* '€' */) {
            return Err(ErrInvalidCharacterString.clone());
        }
        self.gbk_encoder.transform(dst, src)
    }

    /// Reset is same as simplifiedchinese.gbk.
    // go: pkg/parser/charset/encoding_gbk.go customGBKEncoder.Reset L1
    pub fn reset(&self) {}
}

impl GbkEncoder {
    /// `gbkEncoder.Transform(dst, src, atEOF = true)`: what `transform.Bytes` passes. The
    /// encoded bytes are appended to `dst` (Go writes `dst[:nDst]` and returns the error,
    /// which `Encoder.Bytes` turns into `nil, err`).
    // go-dep: golang.org/x/text@v0.39.0/encoding/simplifiedchinese.gbkEncoder.Transform L2 (atEOF is true; the encode0..encode4 lookups are one BMP table)
    fn transform(&self, dst: &mut Vec<u8>, src: &[u8]) -> Result<(), Error> {
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
                // (size == 1: invalid UTF-8; at EOF it is encoded as utf8.RuneError.)

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
