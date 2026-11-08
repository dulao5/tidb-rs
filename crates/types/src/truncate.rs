//! go-file: pkg/types/truncate.go
//!
//! `Context.HandleTruncate` (block D01.4, ported with D01.1).

use tidb_base::errno;

use crate::context::Context;
use crate::errors::Error;

impl Context {
    /// HandleTruncate ignores or returns the error based on the Context state.
    ///
    /// Go takes and returns an `error`; here `Ok(())` is `nil`.
    // go: Context.HandleTruncate L1
    pub fn handle_truncate(&self, err: Result<(), Error>) -> Result<(), Error> {
        // TODO: At present we have not checked whether the error can be ignored or treated as warning.
        // We will do that later, and then append WarnDataTruncated instead of the error itself.
        let Err(err) = err else {
            return Ok(());
        };

        let err = tidb_base::errors::cause(&err).clone();
        let code = err.code();
        let is = |c: u16| code == c as tidb_base::errors::ErrCode;
        if !err.is_normalized()
            || (!is(errno::ErrTruncatedWrongValue)
                && !is(errno::ErrDataTooLong)
                && !is(errno::ErrTruncatedWrongValueForField)
                && !is(errno::ErrWarnDataOutOfRange)
                && !is(errno::ErrDataOutOfRange)
                && !is(errno::ErrBadNumber)
                && !is(errno::ErrWrongValueForType)
                && !is(errno::ErrDatetimeFunctionOverflow)
                && !is(errno::WarnDataTruncated)
                && !is(errno::ErrIncorrectDatetimeValue))
        {
            return Err(err);
        }

        if self.flags().ignore_truncate_err() {
            return Ok(());
        }
        if self.flags().truncate_as_warning() {
            self.append_warning(err);
            return Ok(());
        }
        Err(err)
    }
}
