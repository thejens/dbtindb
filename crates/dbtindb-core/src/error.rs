use std::fmt;

/// A dbtindb error: a message meant for the person running the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl Error {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<minijinja::Error> for Error {
    fn from(e: minijinja::Error) -> Self {
        Self(e.to_string())
    }
}
