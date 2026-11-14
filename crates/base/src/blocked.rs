//! Boundary stubs that callers can recognise (constitution §4.3, VS1 plan §4.3).
//!
//! glue: VS1-02 (no Go counterpart).
//!
//! A temporary boundary stub (`blocked-by:<block>`) returns [`blocked`]: an error with
//! MySQL code 1235 (`ER_NOT_SUPPORTED_YET`), so a client that sees it gets TiDB's "not
//! supported" answer, and with the RFC code text `blocked-by:<block>`, so the VS1 router can
//! tell it from the errors ported from Go ([`blocked_by`]) and send the statement to the PoC
//! before any executor was opened. Ported Go errors are returned to the client as they are;
//! only this error triggers a fallback.

use crate::errors::{self, Error, IntoErrCode, mysql_error_code, rfc_code_text};
use crate::mysql::ErrNotSupportedYet;

/// The RFC code text prefix of a boundary-stub error.
const PREFIX: &str = "blocked-by:";

/// The error of a temporary boundary stub: `what` cannot run until `block` (a ROADMAP block
/// ID, optionally with a part, e.g. `D10.2`) is ported. The message is TiDB's 1235 text
/// with the block appended: `This version of TiDB doesn't yet support '<what>
/// (blocked-by:<block>)'`.
// glue: VS1-02 (boundary stub error, constitution §4.3)
#[track_caller]
pub fn blocked(block: &str, what: &str) -> Error {
    // The message has no arguments, so it is never formatted: `%` in `what` is literal.
    let msg = format!("This version of TiDB doesn't yet support '{what} ({PREFIX}{block})'");
    errors::normalize(
        msg,
        [
            rfc_code_text(format!("{PREFIX}{block}")),
            mysql_error_code(ErrNotSupportedYet.into_err_code()),
        ],
    )
}

/// The block an error of [`blocked`] names, or `None` for any other error (in particular
/// every error ported from Go, including Go's own 1235 errors).
// glue: VS1-02
pub fn blocked_by(err: &Error) -> Option<&str> {
    if err.code() != ErrNotSupportedYet.into_err_code() {
        return None;
    }
    match err.rfc_code() {
        // Borrowed: the code text set by `blocked` (an empty code text gives the number).
        std::borrow::Cow::Borrowed(text) => text.strip_prefix(PREFIX),
        std::borrow::Cow::Owned(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbterror::plannererrors::ErrNotSupportedYet as GoNotSupported;
    use crate::gofmt::Arg;

    #[test]
    fn blocked_is_recognised() {
        let e = blocked("D10.2", "an index path");
        assert_eq!(blocked_by(&e), Some("D10.2"));
        assert_eq!(e.code(), 1235);
        assert_eq!(
            String::from_utf8(e.get_msg()).unwrap(),
            "This version of TiDB doesn't yet support 'an index path (blocked-by:D10.2)'"
        );
        // No formatting: a `%` in the text stays.
        let e = blocked("D08.6", "LIKE 'a%'");
        assert!(String::from_utf8(e.get_msg()).unwrap().contains("'a%'"));
    }

    #[test]
    fn go_errors_are_not_blocked() {
        let go = GoNotSupported.gen_with_stack_by_args(vec![Arg::from("x")]);
        assert_eq!(go.code(), 1235);
        assert_eq!(blocked_by(&go), None);
        assert_eq!(blocked_by(&errors::new("blocked-by:D01.1")), None);
    }
}
