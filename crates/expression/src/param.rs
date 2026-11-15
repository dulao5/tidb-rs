//! go-file: pkg/param/binary_params.go
//!
//! The binary-protocol parameters of `COM_STMT_EXECUTE` as the server decodes them
//! (`parseBinaryParams`, D03.1) and [`crate::util::exec_binary_param`] turns into
//! constants. Go keeps them in their own package so that the server and the expression
//! package can share them; here they live next to their only consumer.

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::ClassServer;
use tidb_base::errno;

/// ErrUnknownFieldType is returned when datatype of the binary param is unknown
pub static ErrUnknownFieldType: LazyLock<Error> =
    LazyLock::new(|| ClassServer.new_std(errno::ErrUnknownFieldType));

/// BinaryParam stores the information decoded from the binary protocol
/// It can be further parsed into `expression.Expression` through the `ExecArgs` function in this package
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BinaryParam {
    pub tp: u8,
    pub is_unsigned: bool,
    pub is_null: bool,
    pub val: Vec<u8>,
}
