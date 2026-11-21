//! go-file: pkg/planner/core/expression_codec_fn.go
//!
//! `tidbCodecFuncHelper`, the key codec behind the builtins `tidb_decode_key(hex_string)`,
//! `tidb_encode_record_key(database_name, table_name, handle/pk columns...)` and
//! `tidb_encode_index_key(database_name, table_name, index_name, index columns..., handle/pk
//! columns...)` (Go's `core_init.go` stores the three entry points into function variables
//! of package `expression`).
//!
//! The builtins themselves (`tidbDecodeKeyFunctionClass` and the two encode classes) are
//! not ported yet: the expression crate registers them as blocked by the info builtins
//! (D08.9), so nothing can call these helpers. They are boundary stubs until those builtins
//! are ported, and are ported together with them (they need the info schema's
//! `TableByName`, the privilege checker and the partition lookup).

use tidb_base::Error;
use tidb_base::blocked::blocked;

/// tidbCodecFuncHelper contains some utililty functions for
///   - tidb_decode_key(hex_string)
///   - tidb_encode_record_key(database_name, table_name, handle/pk columns...)
///   - tidb_encode_index_key(database_name, table_name, index_name, index columns..., handle/pk columns...)
///
/// define an individual struct instead of a bunch of un-exported functions
/// to avoid polluting the global scope of current package.
// glue: stub of tidbCodecFuncHelper.encodeHandleFromRow, blocked-by:D08.9 (the tidb_decode_key / tidb_encode_*_key builtins that call the helpers are not ported)
// glue: stub of tidbCodecFuncHelper.findCommonOrPartitionedTable, blocked-by:D08.9 (the tidb_decode_key / tidb_encode_*_key builtins that call the helpers are not ported)
// glue: stub of tidbCodecFuncHelper.extractTablePartition, blocked-by:D08.9 (the tidb_decode_key / tidb_encode_*_key builtins that call the helpers are not ported)
// glue: stub of tidbCodecFuncHelper.buildHandle, blocked-by:D08.9 (the tidb_decode_key / tidb_encode_*_key builtins that call the helpers are not ported)
// glue: stub of tidbCodecFuncHelper.encodeIndexKeyFromRow, blocked-by:D08.9 (the tidb_decode_key / tidb_encode_*_key builtins that call the helpers are not ported)
// glue: stub of tidbCodecFuncHelper.decodeKeyFromString, blocked-by:D08.9 (the tidb_decode_key / tidb_encode_*_key builtins that call the helpers are not ported)
// glue: stub of tidbCodecFuncHelper.decodeRecordKey, blocked-by:D08.9 (the tidb_decode_key / tidb_encode_*_key builtins that call the helpers are not ported)
// glue: stub of tidbCodecFuncHelper.decodeIndexKey, blocked-by:D08.9 (the tidb_decode_key / tidb_encode_*_key builtins that call the helpers are not ported)
// glue: stub of tidbCodecFuncHelper.decodeTableKey, blocked-by:D08.9 (the tidb_decode_key / tidb_encode_*_key builtins that call the helpers are not ported)
// glue: stub of tidbCodecFuncHelper.datumToJSONObject, blocked-by:D08.9 (the tidb_decode_key / tidb_encode_*_key builtins that call the helpers are not ported)
pub fn tidb_codec_func_helper() -> Result<(), Error> {
    Err(blocked(
        "D08.9",
        "tidbCodecFuncHelper (tidb_decode_key / tidb_encode_record_key / tidb_encode_index_key)",
    ))
}
