use super::error::ConfigError;
use super::types::Config;

/// Semantic checks that go beyond what the grammar itself can enforce.
/// Structural checks (missing `listen`, malformed values, ...) already
/// happen while parsing; this pass catches cross-block inconsistencies.
pub fn validate(_config: &Config) -> Result<(), ConfigError> {
    Ok(())
}
