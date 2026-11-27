//! go-file: pkg/server/internal/column/result_encoder.go
//!
//! The encoder that converts column names and string values to `@@character_set_results`.

#![allow(non_upper_case_globals)]

use std::borrow::Cow;

use tidb_base::mysql::*;
use tidb_parser::gocharset::encoding::{
    Encoding, OpEncodeReplace, find_encoding_take_utf8_as_noop,
};
use tidb_parser::gocharset::{CharsetBin, get_charset_info_by_id};

use crate::column::Info;

/// ResultEncoder encodes a column value to a byte slice.
///
/// Go keeps a scratch `bytes.Buffer` for the transformations; here each encoding returns
/// its own bytes, so `Clean` has nothing to release.
#[derive(Clone, Debug)]
pub struct ResultEncoder {
    encoding: Encoding,
    /// dataEncoding can be updated to match the column data charset (Go: nil until the
    /// first `UpdateDataEncoding`).
    data_encoding: Option<Encoding>,
    /// chsName and encoding are unchanged after the initialization from session variable
    /// @@character_set_results.
    chs_name: String,
    is_binary: bool,
    is_null: bool,
    data_is_binary: bool,
}

impl ResultEncoder {
    /// NewResultEncoder creates a new ResultEncoder.
    // go: NewResultEncoder L1
    pub fn new(chs: &str) -> ResultEncoder {
        ResultEncoder {
            chs_name: chs.to_owned(),
            encoding: find_encoding_take_utf8_as_noop(chs),
            data_encoding: None,
            is_binary: chs == CharsetBin,
            is_null: chs.is_empty(),
            data_is_binary: false,
        }
    }

    /// Clean prevent the ResultEncoder from holding too much memory.
    // go: ResultEncoder.Clean L2 (no scratch buffer to drop)
    pub fn clean(&mut self) {}

    /// UpdateDataEncoding updates the data encoding.
    // go: ResultEncoder.UpdateDataEncoding L1
    pub fn update_data_encoding(&mut self, chs_id: u16) {
        let (chs, _, err) = get_charset_info_by_id(chs_id as isize);
        if let Err(err) = err {
            tidb_base::log_warn!("unknown charset ID", "error" => &err);
        }
        self.data_encoding = Some(find_encoding_take_utf8_as_noop(&chs));
        self.data_is_binary = chs_id == u16::from(BinaryDefaultCollationID);
    }

    /// ColumnTypeInfoCharsetID returns the charset ID for the column type info.
    // go: ResultEncoder.ColumnTypeInfoCharsetID L1
    pub fn column_type_info_charset_id(&self, info: &Info) -> u16 {
        // Only replace the charset when @@character_set_results is valid and
        // the target column is a non-binary string.
        let charset = info.dump_charset();
        if self.is_null || self.chs_name.is_empty() || !is_string_column_type(info.tp) {
            return charset;
        }
        if charset == u16::from(BinaryDefaultCollationID) {
            return u16::from(BinaryDefaultCollationID);
        }
        u16::from(charset_name_to_id(&self.chs_name))
    }

    /// EncodeMeta encodes bytes for meta info like column names.
    // go: ResultEncoder.EncodeMeta L1
    pub fn encode_meta<'a>(&self, src: &'a [u8]) -> Cow<'a, [u8]> {
        self.encode_with(src, self.encoding)
    }

    /// EncodeData encodes bytes for row data.
    // go: ResultEncoder.EncodeData L1
    pub fn encode_data<'a>(&self, src: &'a [u8]) -> Cow<'a, [u8]> {
        // For the following cases, TiDB encodes the results with column charset
        // instead of @@character_set_results:
        //   - @@character_set_result = null.
        //   - @@character_set_result = binary.
        //   - The column is binary type like blob, binary char/varchar.
        if self.is_null || self.is_binary || self.data_is_binary {
            // Use the column charset to encode.
            let enc = self
                .data_encoding
                .expect("UpdateDataEncoding runs before EncodeData");
            return self.encode_with(src, enc);
        }
        self.encode_with(src, self.encoding)
    }

    /// EncodeWith encodes bytes with the given encoding.
    // go: ResultEncoder.EncodeWith L1
    ///
    /// The result borrows `src` when the encoding leaves it unchanged (Go returns `src`
    /// itself then, with no copy).
    pub fn encode_with<'a>(&self, src: &'a [u8], enc: Encoding) -> Cow<'a, [u8]> {
        let (data, err) = enc.transform(src, OpEncodeReplace);
        if let Err(err) = err {
            tidb_base::log_debug!("encode error", "error" => &err);
        }
        data
    }
}

// go: isStringColumnType L1
fn is_string_column_type(tp: u8) -> bool {
    matches!(
        tp,
        TypeString
            | TypeVarString
            | TypeVarchar
            | TypeBit
            | TypeTinyBlob
            | TypeMediumBlob
            | TypeLongBlob
            | TypeBlob
            | TypeEnum
            | TypeSet
            | TypeJSON
            // When passing Vector column to the SQL Client, pretend to be a non-binary String.
            | TypeTiDBVectorFloat32
    )
}
