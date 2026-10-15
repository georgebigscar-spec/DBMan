use std::fmt;

/// An error reported by a driver, with the server's message as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbError {
    pub message: String,
}

impl DbError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    /// Wraps any displayable error.
    pub fn from_display(e: impl fmt::Display) -> Self {
        Self::new(e.to_string())
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DbError {}

pub type DbResult<T> = Result<T, DbError>;
