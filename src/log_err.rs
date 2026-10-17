//! Logging an error that a caller cannot propagate, instead of dropping it with `let _ =`.
//!
//! `let _ = result` throws an error away without a trace, and the workspace denies it
//! (`clippy::let_underscore_must_use`). Where the caller has nowhere to send the error (a
//! `Drop`, a best-effort cleanup, a send to a channel whose reader may already be gone), one of
//! these logs it with a fixed context string and hands back the success value, if any.
//!
//! Pick the level by whether the failure is news. `log_warn` is for a failure someone should
//! see: a file that could not be written, a socket that could not be closed. `log_debug` is for
//! a failure that is expected in normal operation, such as a `watch` send with no receiver left
//! at shutdown; logging that at WARN would fill the log with noise on every clean exit.
//!
//! The context is a constant phrase naming what was attempted. Variable data goes in fields, not
//! in the context string, so one kind of failure stays one kind of log line.

use std::fmt::Display;

/// Log an `Err` with a fixed context string instead of dropping it.
pub trait LogErr<T> {
    /// Log an `Err` at WARN and return `None`; return `Some(value)` for `Ok`.
    fn log_warn(self, context: &'static str) -> Option<T>;
    /// Log an `Err` at DEBUG and return `None`; return `Some(value)` for `Ok`.
    fn log_debug(self, context: &'static str) -> Option<T>;
}

impl<T, E: Display> LogErr<T> for Result<T, E> {
    fn log_warn(self, context: &'static str) -> Option<T> {
        match self {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::warn!(context, error = %error, "error not propagated");
                None
            }
        }
    }

    fn log_debug(self, context: &'static str) -> Option<T> {
        match self {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::debug!(context, error = %error, "error not propagated");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LogErr;

    #[test]
    fn an_ok_result_returns_its_value() {
        let result: Result<u8, String> = Ok(7);
        assert_eq!(result.log_warn("reading a test value"), Some(7));
    }

    #[test]
    fn an_err_logged_at_warn_returns_none() {
        let result: Result<u8, String> = Err("boom".to_owned());
        assert_eq!(result.log_warn("reading a test value"), None);
    }

    #[test]
    fn an_err_logged_at_debug_returns_none() {
        let result: Result<u8, String> = Err("gone".to_owned());
        assert_eq!(result.log_debug("sending to a closed channel"), None);
    }
}
