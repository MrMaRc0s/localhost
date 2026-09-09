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
        let mut seen_listens = HashSet::new();
        for listen in &server.listens {
            if !seen_listens.insert(*listen) {
                return Err(ConfigError::new(format!(
                    "duplicate `listen {}` within the same server block",
                    format_listen(listen)
                )));
            }
        }

        let mut seen_names = HashSet::new();
        for name in &server.server_names {
            let normalized = name.to_ascii_lowercase();
            if !seen_names.insert(normalized) {
                return Err(ConfigError::new(format!(
                    "duplicate server_name `{}` within the same server block",
                    name
                )));
            }
        }

        let mut seen_routes = HashSet::new();
        for route in &server.routes {
            if !route.path.starts_with('/') {
                return Err(ConfigError::new(format!(
                    "route `{}` must start with `/`",
                    route.path
                )));
            }
            if !seen_routes.insert(&route.path) {
                return Err(ConfigError::new(format!(
                    "duplicate route `{}` within the same server block",
                    route.path
                )));
            }
        }
    }

    reject_overlapping_listeners(config)?;

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

/// `0.0.0.0:PORT` and `127.0.0.1:PORT` are not independent sockets:
/// the wildcard bind already owns that concrete address. Rejecting this
/// at parse time avoids an order-dependent partial startup. Virtual hosts
/// remain valid because they share the exact same Listen value and socket.
fn reject_overlapping_listeners(config: &Config) -> Result<(), ConfigError> {
    let listens = config.all_listens();
    for (index, left) in listens.iter().enumerate() {
        for right in &listens[index + 1..] {
            if left.port == right.port
                && left.host != right.host
                && (left.host.is_unspecified() || right.host.is_unspecified())
            {
                return Err(ConfigError::new(format!(
                    "listeners `{}` and `{}` overlap because one address is a wildcard",
                    format_listen(left),
                    format_listen(right)
                )));
            }
        }
    }
    Ok(())
}

fn format_listen(l: &Listen) -> String {
    format!("{}:{}", l.host, l.port)
}
