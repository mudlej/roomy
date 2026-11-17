//! One error type for the engine: a message meant to be shown to the user.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error { message: message.into() }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Adds "what roomy was doing" in front of a lower-level error.
pub trait Context<T> {
    fn context(self, what: impl fmt::Display) -> Result<T>;
}

impl<T, E: fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, what: impl fmt::Display) -> Result<T> {
        self.map_err(|e| Error::new(format!("{what}: {e}")))
    }
}

/// Builds an [`Error`] from a format string.
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => { $crate::Error::new(format!($($arg)*)) };
}
