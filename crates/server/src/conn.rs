//! go-file: pkg/server/conn.go
//!
//! The part of `clientConn` that turns a result set into packets (`writeResultSet`,
//! `writeChunks`, `writeColumnInfo`) and sets up the encoders. The connection itself
//! (packets, handshake, the command loop) is still the PoC's (`tidb-mysql-protocol`), so
//! the packets go into a [`ResultPackets`] that the PoC writes: column definitions, then
//! rows. The EOF / OK packets around them and the flush are the PoC connection's, which
//! knows the client's capabilities (spec D03.1 §3).

use tidb_base::Error;
use tidb_session::Session;

use crate::column::{Info, ResultEncoder, dump_binary_row, dump_text_row};
use crate::resultset::ResultSet;
use crate::util::InputDecoder;

/// The payloads of a result set's packets, in order: one per column definition, then one
/// per row. The column-count packet is the length of `columns`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultPackets {
    pub columns: Vec<Vec<u8>>,
    pub rows: Vec<Vec<u8>>,
    /// The column definitions as sent (for the router's metadata checks).
    pub infos: Vec<Info>,
}

/// `initResultEncoder`: an encoder for `@@character_set_results` (empty if it cannot be
/// read, as Go).
// go: clientConn.initResultEncoder L1
pub fn init_result_encoder(se: &mut Session) -> ResultEncoder {
    let chs = match se
        .vars_mut()
        .get_session_or_global_system_var("character_set_results")
    {
        Ok(chs) => chs,
        Err(err) => {
            tidb_base::log_warn!(
                "get character_set_results system variable failed",
                "error" => &err,
            );
            String::new()
        }
    };
    ResultEncoder::new(&chs)
}

/// `initInputEncoder`: a decoder for `@@character_set_client`.
// go: clientConn.initInputEncoder L1
pub fn init_input_encoder(se: &mut Session) -> InputDecoder {
    let chs = match se
        .vars_mut()
        .get_session_or_global_system_var("character_set_client")
    {
        Ok(chs) => chs,
        Err(err) => {
            tidb_base::log_warn!(
                "get character_set_client system variable failed",
                "error" => &err,
            );
            String::new()
        }
    };
    InputDecoder::new(&chs)
}

/// writeResultSet writes data into a result set and uses rs.Next to get row data back.
/// If binary is true, the data would be encoded in BINARY format.
///
/// `rs_encoder` is Go's `cc.rsEncoder`: set here from `@@character_set_results` and kept
/// by the connection afterwards (Go's `writePrepare` dumps column definitions with the
/// encoder of the previous result set, or the default one before the first).
///
/// Go recovers the panics of memory-quota and kill errors here; in Rust those are errors
/// already (A1), so there is nothing to recover. The cursor branch
/// (`SERVER_STATUS_CURSOR_EXISTS`, `writeChunksWithFetchSize`) is never taken: the VS1
/// router keeps cursors on the PoC (plan §4.3 step 0).
// go: clientConn.writeResultSet L2 (no cursor branch: cursors stay on the PoC; quota panics are errors, A1)
pub async fn write_result_set(
    se: &mut Session,
    rs: &mut ResultSet,
    binary: bool,
    rs_encoder: &mut Option<ResultEncoder>,
    out: &mut ResultPackets,
) -> Result<bool, Error> {
    // log-skip: clientConn.writeResultSet:2364 quota/kill panics are Result::Err in Rust (A1), no panic recovery boundary here
    let enc = rs_encoder.insert(init_result_encoder(se));
    let r = write_chunks(se, rs, binary, enc, out).await;
    enc.clean();
    r
}

/// writeColumnInfo: one column definition packet per column.
// go: clientConn.writeColumnInfo L1
pub fn write_column_info(columns: &[Info], enc: &mut ResultEncoder, out: &mut ResultPackets) {
    for v in columns {
        let mut data = Vec::with_capacity(128);
        v.dump(&mut data, Some(enc));
        out.columns.push(data);
    }
}

/// writeChunks writes data from a Chunk, which filled data by a ResultSet, into a
/// connection. The first return value indicates whether error occurs at the first call of
/// ResultSet.Next.
// go: clientConn.writeChunks L2 (an error drops Go's firstNext flag: the TiFlash retry is not ported; statement detail timings: D14.1)
pub async fn write_chunks(
    se: &mut Session,
    rs: &mut ResultSet,
    binary: bool,
    enc: &mut ResultEncoder,
    out: &mut ResultPackets,
) -> Result<bool, Error> {
    let mut req = rs.new_chunk()?;
    let mut got_column_info = false;
    let mut first_next = true;
    loop {
        // Here server.tidbResultSet implements Next method.
        // Go returns `(firstNext, err)`: whether the first Next failed, for the TiFlash
        // retry of `handleStmt`, which the new stack does not have.
        rs.next(se, &mut req).await?;
        if !got_column_info {
            // We need to call Next before we get columns.
            // Otherwise, we will get incorrect columns info.
            write_column_info(rs.columns(se)?, enc, out);
            got_column_info = true;
        }
        let row_count = req.num_rows();
        if row_count == 0 {
            break;
        }
        first_next = false;
        let columns = rs.columns(se)?;
        for i in 0..row_count {
            let mut data = Vec::with_capacity(64);
            if binary {
                dump_binary_row(&mut data, columns, &req.get_row(i), Some(enc))?;
            } else {
                dump_text_row(&mut data, columns, &req.get_row(i), Some(enc))?;
            }
            out.rows.push(data);
        }
    }
    // glue: the column definitions as sent, for the router (moved out of the result set,
    // whose `Columns` cache Go keeps until the result set is closed; nothing reads it after
    // the last `Next`).
    out.infos = rs.take_columns();
    rs.finish(se).await?;
    Ok(first_next)
}
