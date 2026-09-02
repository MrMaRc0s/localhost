use crate::http::Method;
use std::collections::HashMap;
use std::net::IpAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Listen {
    pub host: IpAddr,
    pub port: u16,
}

/// One `route { ... }` block inside a server. Routes are matched by the
/// longest matching path prefix, same idea as an nginx `location` block —
/// no regexes needed per the spec.
#[derive(Debug, Clone)]
pub struct RouteConfig {
    pub path: String,
    pub methods: Vec<Method>,
    pub root: Option<String>,
    pub index: Option<String>,
    pub autoindex: bool,
    pub redirect: Option<(u16, String)>,
    /// file extension (with leading dot, e.g. ".py") -> interpreter path
    pub cgi: HashMap<String, String>,
    pub upload_store: Option<String>,
}

impl RouteConfig {
    pub fn new(path: impl Into<String>) -> Self {
        RouteConfig {
            path: path.into(),
            methods: vec![Method::Get, Method::Head],
            root: None,
            index: None,
            autoindex: false,
            redirect: None,
            cgi: HashMap::new(),
            upload_store: None,
        }
    }

    pub fn allows(&self, method: Method) -> bool {
        self.methods.contains(&method)
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listens: Vec<Listen>,
    pub server_names: Vec<String>,
    pub root: String,
    pub index: Option<String>,
    pub client_max_body_size: usize,
    pub error_pages: HashMap<u16, String>,
    pub routes: Vec<RouteConfig>,
}

impl ServerConfig {
    pub fn matches_host(&self, host: &str) -> bool {
        let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
        self.server_names
            .iter()
            .any(|name| name.to_ascii_lowercase() == host)
    }

    /// Longest-prefix route match, breaking ties on path-segment boundaries
    /// so `/test` doesn't accidentally match a request for `/testing`.
    pub fn find_route(&self, path: &str) -> Option<&RouteConfig> {
        self.routes
            .iter()
            .filter(|r| route_prefix_matches(&r.path, path))
            .max_by_key(|r| r.path.len())
    }

    pub fn error_page(&self, code: u16) -> Option<&str> {
        self.error_pages.get(&code).map(String::as_str)
    }
}

fn route_prefix_matches(route_path: &str, request_path: &str) -> bool {
    if route_path == "/" {
        return true;
    }
    if !request_path.starts_with(route_path) {
        return false;
    }
    let rest = &request_path[route_path.len()..];
    rest.is_empty() || rest.starts_with('/')
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub servers: Vec<ServerConfig>,
}

impl Config {
    /// All distinct (host, port) pairs that need a listening socket.
    pub fn all_listens(&self) -> Vec<Listen> {
        let mut out: Vec<Listen> = Vec::new();
        for server in &self.servers {
            for listen in &server.listens {
                if !out.contains(listen) {
                    out.push(*listen);
                }
            }
        }
        out
    }

    /// Resolve which virtual server should handle a request that arrived on
    /// `listen`, given the (optional) Host header. The first server
    /// declared for a given host:port is the default when the Host header
    /// doesn't match any server_name — same rule as nginx.
    pub fn resolve<'a>(&'a self, listen: Listen, host_header: Option<&str>) -> Option<&'a ServerConfig> {
        let candidates: Vec<&ServerConfig> = self
            .servers
            .iter()
            .filter(|s| s.listens.iter().any(|l| listens_match(*l, listen)))
            .collect();

        if candidates.is_empty() {
            return None;
        }

        if let Some(host) = host_header {
            if let Some(exact) = candidates.iter().find(|s| s.matches_host(host)) {
                return Some(exact);
            }
        }

        candidates.into_iter().next()
    }
}

fn listens_match(configured: Listen, actual: Listen) -> bool {
    if configured.port != actual.port {
        return false;
    }
    configured.host.is_unspecified() || configured.host == actual.host
}
