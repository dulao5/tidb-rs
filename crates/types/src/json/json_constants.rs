//! go-file: pkg/types/json_constants.go
//!
//! Type codes, literals, layout constants, the type precedences of `CompareBinaryJSON` and the
//! JSON errors (exception A4, `specs/exceptions/A4-json/spec.md`). Only declarations: the file
//! has no functions.

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::{ClassJSON, ClassTypes};
use tidb_base::errno;

/// JSONTypeCode indicates JSON type.
pub type JSONTypeCode = u8;

/// JSONTypeCodeObject indicates the JSON is an object.
pub const JSONTypeCodeObject: JSONTypeCode = 0x01;
/// JSONTypeCodeArray indicates the JSON is an array.
pub const JSONTypeCodeArray: JSONTypeCode = 0x03;
/// JSONTypeCodeLiteral indicates the JSON is a literal.
pub const JSONTypeCodeLiteral: JSONTypeCode = 0x04;
/// JSONTypeCodeInt64 indicates the JSON is a signed integer.
pub const JSONTypeCodeInt64: JSONTypeCode = 0x09;
/// JSONTypeCodeUint64 indicates the JSON is a unsigned integer.
pub const JSONTypeCodeUint64: JSONTypeCode = 0x0a;
/// JSONTypeCodeFloat64 indicates the JSON is a double float number.
pub const JSONTypeCodeFloat64: JSONTypeCode = 0x0b;
/// JSONTypeCodeString indicates the JSON is a string.
pub const JSONTypeCodeString: JSONTypeCode = 0x0c;
/// JSONTypeCodeOpaque indicates the JSON is a opaque
pub const JSONTypeCodeOpaque: JSONTypeCode = 0x0d;
/// JSONTypeCodeDate indicates the JSON is a opaque
pub const JSONTypeCodeDate: JSONTypeCode = 0x0e;
/// JSONTypeCodeDatetime indicates the JSON is a opaque
pub const JSONTypeCodeDatetime: JSONTypeCode = 0x0f;
/// JSONTypeCodeTimestamp indicates the JSON is a opaque
pub const JSONTypeCodeTimestamp: JSONTypeCode = 0x10;
/// JSONTypeCodeDuration indicates the JSON is a opaque
pub const JSONTypeCodeDuration: JSONTypeCode = 0x11;

/// JSONLiteralNil represents JSON null.
pub const JSONLiteralNil: u8 = 0x00;
/// JSONLiteralTrue represents JSON true.
pub const JSONLiteralTrue: u8 = 0x01;
/// JSONLiteralFalse represents JSON false.
pub const JSONLiteralFalse: u8 = 0x02;

pub(crate) const unknownTypeCodeErrorMsg: &str = "unknown type code: %d";

/// jsonSafeSet holds the value true if the ASCII character with the given array position can
/// be represented inside a JSON string without any further escaping: everything from `' '` to
/// `'\u007f'` except `"` and `\`.
pub(crate) static jsonSafeSet: [bool; 0x80] = {
    let mut t = [false; 0x80];
    let mut b = 0x20;
    while b < 0x80 {
        t[b] = b != b'"' as usize && b != b'\\' as usize;
        b += 1;
    }
    t
};

pub(crate) const jsonHexChars: &[u8; 16] = b"0123456789abcdef";

pub(crate) const headerSize: usize = 8; // element size + data size.
pub(crate) const dataSizeOff: usize = 4;
pub(crate) const keyEntrySize: usize = 6; // keyOff +  keyLen
pub(crate) const keyLenOff: usize = 4;
pub(crate) const valTypeSize: usize = 1;
pub(crate) const valEntrySize: usize = 5;

/// `json_type_precedences[t]` for the type names `BinaryJSON.Type` returns (the Go map). The
/// names are the only keys Go looks up, so a `match` returns the same values.
// glue: the json_type_precedences map literal as a match (A4 spec 5.6)
pub(crate) fn json_type_precedences(t: &str) -> isize {
    match t {
        "BLOB" => -1,
        "BIT" => -2,
        "OPAQUE" => -3,
        "DATETIME" => -4,
        "TIME" => -5,
        "DATE" => -6,
        "BOOLEAN" => -7,
        "ARRAY" => -8,
        "OBJECT" => -9,
        "STRING" => -10,
        "INTEGER" => -11,
        "UNSIGNED INTEGER" => -11,
        "DOUBLE" => -11,
        "NULL" => -12,
        _ => 0,
    }
}

/// JSONModifyType is for modify a JSON. There are three valid values:
/// JSONModifyInsert, JSONModifyReplace and JSONModifySet.
pub type JSONModifyType = u8;

/// JSONModifyInsert is for insert a new element into a JSON.
/// If an old elemList exists, it would NOT replace it.
pub const JSONModifyInsert: JSONModifyType = 0x01;
/// JSONModifyReplace is for replace an old elemList from a JSON.
/// If no elemList exists, it would NOT insert it.
pub const JSONModifyReplace: JSONModifyType = 0x02;
/// JSONModifySet = JSONModifyInsert | JSONModifyReplace
pub const JSONModifySet: JSONModifyType = 0x03;

/// ErrInvalidJSONText means invalid JSON text.
pub static ErrInvalidJSONText: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrInvalidJSONText));
/// ErrInvalidJSONType means invalid JSON type.
pub static ErrInvalidJSONType: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrInvalidJSONType));
/// ErrInvalidJSONTextInParam means invalid JSON text in arguments.
pub static ErrInvalidJSONTextInParam: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrInvalidJSONTextInParam));
/// ErrInvalidJSONPath means invalid JSON path.
pub static ErrInvalidJSONPath: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrInvalidJSONPath));
/// ErrInvalidJSONCharset means invalid JSON charset.
pub static ErrInvalidJSONCharset: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrInvalidJSONCharset));
/// ErrInvalidJSONData means invalid JSON data.
pub static ErrInvalidJSONData: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrInvalidJSONData));
/// ErrInvalidJSONPathMultipleSelection means invalid JSON path that contain wildcard characters or range selection.
pub static ErrInvalidJSONPathMultipleSelection: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrInvalidJSONPathMultipleSelection));
/// ErrInvalidJSONContainsPathType means invalid JSON contains path type.
pub static ErrInvalidJSONContainsPathType: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrInvalidJSONContainsPathType));
/// ErrJSONDocumentNULLKey means that json's key is null
pub static ErrJSONDocumentNULLKey: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrJSONDocumentNULLKey));
/// ErrJSONDocumentTooDeep means that json's depth is too deep.
pub static ErrJSONDocumentTooDeep: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrJSONDocumentTooDeep));
/// ErrJSONObjectKeyTooLong means JSON object with key length >= 65536 which is not yet supported.
pub static ErrJSONObjectKeyTooLong: LazyLock<Error> = LazyLock::new(|| {
    ClassTypes.new_std_err(
        errno::ErrJSONObjectKeyTooLong,
        errno::MySQLErrName
            .get(&errno::ErrJSONObjectKeyTooLong)
            .expect("ErrJSONObjectKeyTooLong has a message"),
    )
});
/// ErrInvalidJSONPathArrayCell means invalid JSON path for an array cell.
pub static ErrInvalidJSONPathArrayCell: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrInvalidJSONPathArrayCell));
/// ErrUnsupportedSecondArgumentType means unsupported second argument type in json_objectagg
pub static ErrUnsupportedSecondArgumentType: LazyLock<Error> =
    LazyLock::new(|| ClassJSON.new_std(errno::ErrUnsupportedSecondArgumentType));

/// JSONContainsPathAll is for JSON_CONTAINS_PATH, requires all paths exist.
pub const JSONContainsPathAll: &str = "all";
/// JSONContainsPathOne is for JSON_CONTAINS_PATH, requires one path exists.
pub const JSONContainsPathOne: &str = "one";
