use std::fmt;

#[derive(Debug)]
pub struct ConfigError(pub String);

impl ConfigError {
    pub fn new(msg: impl Into<String>) -> Self {
        ConfigError(msg.into())
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}
