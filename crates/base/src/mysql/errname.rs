//! go-file: pkg/parser/mysql/errname.go
//!
//! The table `MySQLErrName` is in `errname_gen.rs` (generated).

/// ErrMessage is a error message with the format specifier.
///
/// Go builds messages at package initialisation (sometimes with `fmt.Sprintf`); the port's
/// messages are literals (generated tables, prototypes), hence `&'static`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrMessage {
    pub raw: &'static str,
    pub redact_arg_pos: &'static [usize],
}

impl ErrMessage {
    /// `Message` in a constant context (the generated tables).
    // glue: const constructor for the generated tables (same as Message)
    pub const fn new(raw: &'static str, redact_arg_pos: &'static [usize]) -> ErrMessage {
        ErrMessage {
            raw,
            redact_arg_pos,
        }
    }
}

/// Message creates a error message with the format specifier.
// go: Message L1
pub const fn message(message: &'static str, redact_args: &'static [usize]) -> ErrMessage {
    ErrMessage {
        raw: message,
        redact_arg_pos: redact_args,
    }
}
