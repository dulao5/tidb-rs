//! go-file: pkg/server/conn_stmt_params.go
//!
//! The parameters of `COM_STMT_EXECUTE` in the binary protocol.

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::ClassServer;
use tidb_base::errno;
use tidb_base::gofmt::Arg;
use tidb_base::mysql;
use tidb_expression::param::BinaryParam;
use tidb_parser::gocharset::CharsetUTF8;

use crate::util::{InputDecoder, parse_length_encoded_int};

static errUnknownFieldType: LazyLock<Error> =
    LazyLock::new(|| ClassServer.new_std(errno::ErrUnknownFieldType));

/// parseBinaryParams decodes the binary params according to the protocol
///
/// `bound_params[i]` is the data of `COM_STMT_SEND_LONG_DATA` for parameter `i` (Go's
/// non-nil `boundParams[i]`). Like Go it indexes the NULL bitmap and the length-encoded
/// lengths without checking them: a packet that is too short there panics, and the
/// connection loop recovers as Go's `clientConn.Run` does.
// go: parseBinaryParams L1
pub fn parse_binary_params(
    params: &mut [BinaryParam],
    bound_params: &[Option<Vec<u8>>],
    null_bitmap: &[u8],
    param_types: &[u8],
    param_values: &[u8],
    enc: Option<&InputDecoder>,
) -> Result<(), Error> {
    use mysql::*;
    let mut pos = 0usize;
    let default_enc;
    let enc = match enc {
        Some(e) => e,
        None => {
            default_enc = InputDecoder::new(CharsetUTF8);
            &default_enc
        }
    };

    for i in 0..params.len() {
        // if params had received via ComStmtSendLongData, use them directly.
        // ref https://dev.mysql.com/doc/internals/en/com-stmt-send-long-data.html
        // see clientConn#handleStmtSendLongData
        if let Some(bound) = &bound_params[i] {
            params[i] = BinaryParam {
                tp: TypeBlob,
                val: bound.clone(),
                ..BinaryParam::default()
            };

            // The legacy logic is kept: if the `paramTypes` somehow didn't contain the type
            // information, it will be treated as BLOB type. We didn't return
            // `mysql.ErrMalformPacket` to keep compatibility with older versions, though
            // it's meaningless if every clients work properly.
            if (i << 1) + 1 < param_types.len() {
                // Only TEXT or BLOB type will be sent through `SEND_LONG_DATA`.
                let tp = param_types[i << 1];
                match tp {
                    TypeVarchar | TypeVarString | TypeString | TypeBit => {
                        params[i].tp = tp;
                        params[i].val = enc.decode_input(bound);
                    }
                    TypeBlob | TypeTinyBlob | TypeMediumBlob | TypeLongBlob => {
                        params[i].tp = tp;
                        params[i].val = bound.clone();
                    }
                    _ => {}
                }
            }
            continue;
        }

        // check nullBitMap to determine the NULL arguments.
        // ref https://dev.mysql.com/doc/internals/en/com-stmt-execute.html
        // notice: some client(e.g. mariadb) will set nullBitMap even if data had be sent via
        // ComStmtSendLongData, so this check need place after boundParam's check.
        if null_bitmap[i >> 3] & (1 << (i % 8)) > 0 {
            params[i] = BinaryParam {
                tp: TypeNull,
                ..BinaryParam::default()
            };
            continue;
        }

        if (i << 1) + 1 >= param_types.len() {
            return Err(ErrMalformPacket.clone());
        }

        let tp = param_types[i << 1];
        let is_unsigned = (param_types[(i << 1) + 1] & 0x80) > 0;
        let mut is_null = false;

        let mut decode_with_decoder = false;

        let length: u64;
        match tp {
            TypeNull => {
                length = 0;
                is_null = true;
            }
            TypeTiny => length = 1,
            TypeShort | TypeYear => length = 2,
            TypeInt24 | TypeLong | TypeFloat => length = 4,
            TypeLonglong | TypeDouble => length = 8,
            TypeDate | TypeTimestamp | TypeDatetime | TypeDuration => {
                if param_values.len() < pos + 1 {
                    return Err(ErrMalformPacket.clone());
                }
                length = u64::from(param_values[pos]);
                pos += 1;
            }
            TypeNewDecimal | TypeBlob | TypeTinyBlob | TypeMediumBlob | TypeLongBlob => {
                if param_values.len() < pos + 1 {
                    return Err(ErrMalformPacket.clone());
                }
                let (l, null, n) = parse_length_encoded_int(&param_values[pos..]);
                length = l;
                is_null = null;
                pos += n;
            }
            TypeUnspecified | TypeVarchar | TypeVarString | TypeString | TypeEnum | TypeSet
            | TypeGeometry | TypeBit => {
                if param_values.len() < pos + 1 {
                    return Err(ErrMalformPacket.clone());
                }
                let (l, null, n) = parse_length_encoded_int(&param_values[pos..]);
                length = l;
                is_null = null;
                pos += n;
                decode_with_decoder = true;
            }
            _ => {
                return Err(errUnknownFieldType
                    .gen_with_stack("stmt unknown field type %d", vec![Arg::from(i64::from(tp))]));
            }
        }

        // Go: `len(paramValues) < (pos + int(length))` in int arithmetic: a length above
        // MaxInt64 wraps negative, passes the check, and the slice expression panics.
        let end = (pos as i64).wrapping_add(length as i64);
        if (param_values.len() as i64) < end {
            return Err(ErrMalformPacket.clone());
        }
        if end < pos as i64 {
            panic!("slice bounds out of range [{pos}:{end}]");
        }
        let end = end as usize;
        params[i] = BinaryParam {
            tp,
            is_unsigned,
            is_null,
            val: param_values[pos..end].to_vec(),
        };
        if decode_with_decoder {
            params[i].val = enc.decode_input(&params[i].val);
        }
        pos += length as usize;
    }
    Ok(())
}
