use super::error::ConfigError;
use super::types::{Config, Listen};
use std::collections::HashSet;

/// Semantic checks that go beyond what the grammar itself can enforce.
/// Structural checks (missing `listen`, malformed values, ...) already
/// happen while parsing; this pass catches cross-block inconsistencies
/// that would leave the server unable to route a request unambiguously.
///
/// Note this is deliberately about *configuration* ambiguity, not about
/// `bind(2)` failing at runtime (e.g. a port already held by another
/// process): that's a per-listener concern handled when we actually open
/// sockets, and a failure there must not take down the other, valid
/// listeners. See `server::listener`.
pub fn validate(config: &Config) -> Result<(), ConfigError> {
    for server in &config.servers {
        let mut seen = HashSet::new();
        for listen in &server.listens {
            if !seen.insert(*listen) {
                return Err(ConfigError::new(format!(
                    "duplicate `listen {}` within the same server block",
                    format_listen(listen)
                )));
            }
        }
    }

    for listen in config.all_listens() {
        let servers_on_listen: Vec<&super::types::ServerConfig> = config
            .servers
            .iter()
            .filter(|s| s.listens.contains(&listen))
            .collect();

        let default_claimants = servers_on_listen
            .iter()
            .filter(|s| s.server_names.is_empty())
            .count();
        if default_claimants > 1 {
            return Err(ConfigError::new(format!(
                "{}: {} server blocks declare no server_name — cannot tell which one is the default",
                format_listen(&listen),
                default_claimants
            )));
        }

        let mut names_seen: HashSet<String> = HashSet::new();
        for server in &servers_on_listen {
            for name in &server.server_names {
                let lower = name.to_ascii_lowercase();
                if !names_seen.insert(lower) {
                    return Err(ConfigError::new(format!(
                        "{}: server_name `{}` is declared on more than one server block",
                        format_listen(&listen),
                        name
                    )));
                }
            }
        }
    }

    Ok(())
}

fn format_listen(l: &Listen) -> String {
    format!("{}:{}", l.host, l.port)
}
