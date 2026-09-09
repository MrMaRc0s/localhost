use super::error::ConfigError;
use super::types::{Config, Listen, RouteConfig, ServerConfig};
use crate::http::Method;
use std::collections::HashSet;
use std::net::IpAddr;
use std::str::FromStr;

const DEFAULT_MAX_BODY: usize = 1024 * 1024; // 1 MiB

pub fn parse_file(path: &str) -> Result<Config, ConfigError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::new(format!("cannot read config file `{}`: {}", path, e)))?;
    parse_str(&raw)
}

pub fn parse_str(src: &str) -> Result<Config, ConfigError> {
    let tokens = tokenize(src);
    let mut parser = Parser { tokens: &tokens, pos: 0 };
    let mut config = Config::default();

    while !parser.at_end() {
        let (word, line) = parser.next_tok()?;
        if word != "server" {
            return Err(ConfigError::new(format!(
                "line {}: expected `server` block, found `{}`",
                line, word
            )));
        }
        config.servers.push(parser.parse_server_block()?);
    }

    if config.servers.is_empty() {
        return Err(ConfigError::new("config file defines no `server` blocks"));
    }

    super::validate::validate(&config)?;
    Ok(config)
}

// ---------------------------------------------------------------------
// Tokenizer: `{`, `}`, `;` are always standalone tokens; everything else
// is whitespace-separated. No comment handling is needed per the spec.
// ---------------------------------------------------------------------

struct Tok {
    text: String,
    line: usize,
}

fn tokenize(src: &str) -> Vec<Tok> {
    let mut tokens = Vec::new();
    let mut line = 1usize;
    let mut current = String::new();
    // The spec doesn't require comment support, but a config file with
    // no way to leave a note next to a directive is painful to author by
    // hand, so `#` through end-of-line is stripped like a shell/nginx
    // comment before anything else sees it.
    let mut in_comment = false;

    let flush = |current: &mut String, line: usize, tokens: &mut Vec<Tok>| {
        if !current.is_empty() {
            tokens.push(Tok { text: std::mem::take(current), line });
        }
    };

    for ch in src.chars() {
        if in_comment {
            if ch == '\n' {
                in_comment = false;
                line += 1;
            }
            continue;
        }
        match ch {
            '#' => {
                flush(&mut current, line, &mut tokens);
                in_comment = true;
            }
            '\n' => {
                flush(&mut current, line, &mut tokens);
                line += 1;
            }
            c if c.is_whitespace() => flush(&mut current, line, &mut tokens),
            '{' | '}' | ';' => {
                flush(&mut current, line, &mut tokens);
                tokens.push(Tok { text: ch.to_string(), line });
            }
            c => current.push(c),
        }
    }
    flush(&mut current, line, &mut tokens);
    tokens
}

struct Parser<'a> {
    tokens: &'a [Tok],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.pos).map(|t| t.text.as_str())
    }

    fn next_tok(&mut self) -> Result<(String, usize), ConfigError> {
        let tok = self
            .tokens
            .get(self.pos)
            .ok_or_else(|| ConfigError::new("unexpected end of config file"))?;
        self.pos += 1;
        Ok((tok.text.clone(), tok.line))
    }

    fn expect(&mut self, expected: &str) -> Result<(), ConfigError> {
        let (word, line) = self.next_tok()?;
        if word != expected {
            return Err(ConfigError::new(format!(
                "line {}: expected `{}`, found `{}`",
                line, expected, word
            )));
        }
        Ok(())
    }

    /// Collects tokens up to (and consuming) the next `;`.
    fn until_semicolon(&mut self) -> Result<Vec<String>, ConfigError> {
        let mut out = Vec::new();
        loop {
            let (word, line) = self.next_tok()?;
            if word == ";" {
                return Ok(out);
            }
            if word == "{" || word == "}" {
                return Err(ConfigError::new(format!(
                    "line {}: unexpected `{}` inside directive (missing `;`?)",
                    line, word
                )));
            }
            out.push(word);
        }
    }

    fn single_value(&mut self, directive: &str, line: usize) -> Result<String, ConfigError> {
        let values = self.until_semicolon()?;
        if values.len() != 1 {
            return Err(ConfigError::new(format!(
                "line {}: `{}` expects exactly one value, got {}",
                line,
                directive,
                values.len()
            )));
        }
        Ok(values.into_iter().next().unwrap())
    }

    fn parse_server_block(&mut self) -> Result<ServerConfig, ConfigError> {
        self.expect("{")?;

        let mut server = ServerConfig {
            listens: Vec::new(),
            server_names: Vec::new(),
            root: ".".to_string(),
            index: None,
            client_max_body_size: DEFAULT_MAX_BODY,
            error_pages: std::collections::HashMap::new(),
            routes: Vec::new(),
        };

        loop {
            let peeked = self.peek().ok_or_else(|| ConfigError::new("unterminated `server` block"))?;
            if peeked == "}" {
                self.pos += 1;
                break;
            }

            let (directive, line) = self.next_tok()?;
            match directive.as_str() {
                "listen" => {
                    for value in self.until_semicolon()? {
                        server.listens.push(parse_listen(&value, line)?);
                    }
                }
                "server_name" => {
                    let names = self.until_semicolon()?;
                    if names.is_empty() {
                        return Err(ConfigError::new(format!(
                            "line {}: `server_name` expects at least one name",
                            line
                        )));
                    }
                    server.server_names.extend(names);
                }
                "root" => {
                    server.root = self.single_value("root", line)?;
                }
                "index" => {
                    server.index = Some(self.single_value("index", line)?);
                }
                "client_max_body_size" => {
                    let raw = self.single_value("client_max_body_size", line)?;
                    server.client_max_body_size = parse_size(&raw, line)?;
                }
                "error_page" => {
                    let values = self.until_semicolon()?;
                    if values.len() < 2 {
                        return Err(ConfigError::new(format!(
                            "line {}: `error_page` needs at least one status code and a path",
                            line
                        )));
                    }
                    let (codes, path) = values.split_at(values.len() - 1);
                    let path = path[0].clone();
                    for code in codes {
                        let code: u16 = code.parse().map_err(|_| {
                            ConfigError::new(format!("line {}: invalid status code `{}`", line, code))
                        })?;
                        if !(400..=599).contains(&code) {
                            return Err(ConfigError::new(format!(
                                "line {}: `error_page` status must be between 400 and 599, found {}",
                                line, code
                            )));
                        }
                        if server.error_pages.contains_key(&code) {
                            return Err(ConfigError::new(format!(
                                "line {}: duplicate `error_page` mapping for status {}",
                                line, code
                            )));
                        }
                        server.error_pages.insert(code, path.clone());
                    }
                }
                "route" => {
                    let (path, _) = self.next_tok()?;
                    self.expect("{")?;
                    server.routes.push(self.parse_route_block(path)?);
                }
                other => {
                    return Err(ConfigError::new(format!(
                        "line {}: unknown server directive `{}`",
                        line, other
                    )));
                }
            }
        }

        if server.listens.is_empty() {
            return Err(ConfigError::new("server block has no `listen` directive"));
        }

        Ok(server)
    }

    fn parse_route_block(&mut self, path: String) -> Result<RouteConfig, ConfigError> {
        let mut route = RouteConfig::new(path);
        let mut methods_set = false;

        loop {
            let peeked = self.peek().ok_or_else(|| ConfigError::new("unterminated `route` block"))?;
            if peeked == "}" {
                self.pos += 1;
                break;
            }

            let (directive, line) = self.next_tok()?;
            match directive.as_str() {
                "methods" => {
                    let mut methods = Vec::new();
                    let mut seen = HashSet::new();
                    for word in self.until_semicolon()? {
                        let method = Method::parse(&word);
                        if method == Method::Other {
                            return Err(ConfigError::new(format!(
                                "line {}: unsupported HTTP method `{}`",
                                line, word
                            )));
                        }
                        if !seen.insert(method) {
                            return Err(ConfigError::new(format!(
                                "line {}: duplicate HTTP method `{}`",
                                line, word
                            )));
                        }
                        methods.push(method);
                    }
                    if methods.is_empty() {
                        return Err(ConfigError::new(format!(
                            "line {}: `methods` expects at least one HTTP method",
                            line
                        )));
                    }
                    // HEAD has the same representation as GET without the
                    // response body. HTTP/1.1 general-purpose servers are
                    // expected to support both, so configuring GET implies it.
                    if methods.contains(&Method::Get) && !methods.contains(&Method::Head) {
                        methods.push(Method::Head);
                    }
                    route.methods = methods;
                    methods_set = true;
                }
                "root" => route.root = Some(self.single_value("root", line)?),
                "index" => route.index = Some(self.single_value("index", line)?),
                "autoindex" => {
                    let value = self.single_value("autoindex", line)?;
                    route.autoindex = match value.as_str() {
                        "on" => true,
                        "off" => false,
                        other => {
                            return Err(ConfigError::new(format!(
                                "line {}: `autoindex` must be `on` or `off`, found `{}`",
                                line, other
                            )))
                        }
                    };
                }
                "return" => {
                    let values = self.until_semicolon()?;
                    if values.len() != 2 {
                        return Err(ConfigError::new(format!(
                            "line {}: `return` expects `<status> <target>`",
                            line
                        )));
                    }
                    let code: u16 = values[0].parse().map_err(|_| {
                        ConfigError::new(format!("line {}: invalid redirect status `{}`", line, values[0]))
                    })?;
                    if !(300..=399).contains(&code) {
                        return Err(ConfigError::new(format!(
                            "line {}: `return` status must be between 300 and 399, found {}",
                            line, code
                        )));
                    }
                    route.redirect = Some((code, values[1].clone()));
                }
                "cgi" => {
                    let values = self.until_semicolon()?;
                    if values.len() != 2 {
                        return Err(ConfigError::new(format!(
                            "line {}: `cgi` expects `<extension> <interpreter path>`",
                            line
                        )));
                    }
                    let extension = &values[0];
                    if !extension.starts_with('.') || extension.len() == 1 || extension.contains('/') {
                        return Err(ConfigError::new(format!(
                            "line {}: CGI extension `{}` must look like `.py`",
                            line, extension
                        )));
                    }
                    if route.cgi.insert(extension.clone(), values[1].clone()).is_some() {
                        return Err(ConfigError::new(format!(
                            "line {}: duplicate CGI mapping for extension `{}`",
                            line, extension
                        )));
                    }
                }
                "upload_store" => {
                    route.upload_store = Some(self.single_value("upload_store", line)?);
                }
                other => {
                    return Err(ConfigError::new(format!(
                        "line {}: unknown route directive `{}`",
                        line, other
                    )));
                }
            }
        }

        if !methods_set {
            // RouteConfig::new() default already covers this, kept explicit
            // for readers: an un-configured route only answers GET/HEAD.
        }

        Ok(route)
    }
}

fn parse_listen(value: &str, line: usize) -> Result<Listen, ConfigError> {
    if let Some((host, port)) = value.rsplit_once(':') {
        let host = if host == "localhost" {
            IpAddr::from_str("127.0.0.1").unwrap()
        } else {
            IpAddr::from_str(host)
                .map_err(|_| ConfigError::new(format!("line {}: invalid host `{}`", line, host)))?
        };
        let port: u16 = port
            .parse()
            .map_err(|_| ConfigError::new(format!("line {}: invalid port `{}`", line, port)))?;
        if port == 0 {
            return Err(ConfigError::new(format!(
                "line {}: port 0 is not a usable configured listener",
                line
            )));
        }
        Ok(Listen { host, port })
    } else {
        let port: u16 = value
            .parse()
            .map_err(|_| ConfigError::new(format!("line {}: invalid `listen` value `{}`", line, value)))?;
        if port == 0 {
            return Err(ConfigError::new(format!(
                "line {}: port 0 is not a usable configured listener",
                line
            )));
        }
        Ok(Listen { host: IpAddr::from_str("0.0.0.0").unwrap(), port })
    }
}

fn parse_size(value: &str, line: usize) -> Result<usize, ConfigError> {
    let lower = value.to_ascii_lowercase();
    let (digits, multiplier) = if let Some(stripped) = lower.strip_suffix('k') {
        (stripped, 1024)
    } else if let Some(stripped) = lower.strip_suffix('m') {
        (stripped, 1024 * 1024)
    } else if let Some(stripped) = lower.strip_suffix('g') {
        (stripped, 1024 * 1024 * 1024)
    } else {
        (lower.as_str(), 1)
    };
    let base: usize = digits
        .parse()
        .map_err(|_| ConfigError::new(format!("line {}: invalid size `{}`", line, value)))?;
    base.checked_mul(multiplier)
        .ok_or_else(|| ConfigError::new(format!("line {}: size `{}` is too large", line, value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_server_block() {
        let cfg = parse_str("server { listen 8080; route / { methods GET; } }").unwrap();
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].listens[0].port, 8080);
        assert_eq!(cfg.servers[0].routes[0].path, "/");
    }

    #[test]
    fn strips_hash_comments() {
        let src = "# top-level note\nserver {\n  listen 8080; # inline note\n  route / { methods GET; }\n}\n";
        let cfg = parse_str(src).unwrap();
        assert_eq!(cfg.servers[0].listens[0].port, 8080);
    }

    #[test]
    fn parses_client_max_body_size_suffixes() {
        let cfg = parse_str("server { listen 8080; client_max_body_size 2m; route / { methods GET; } }").unwrap();
        assert_eq!(cfg.servers[0].client_max_body_size, 2 * 1024 * 1024);
    }

    #[test]
    fn parses_error_page_with_multiple_codes() {
        let cfg = parse_str("server { listen 8080; error_page 500 502 /5xx.html; route / { methods GET; } }").unwrap();
        assert_eq!(cfg.servers[0].error_pages.get(&500).map(String::as_str), Some("/5xx.html"));
        assert_eq!(cfg.servers[0].error_pages.get(&502).map(String::as_str), Some("/5xx.html"));
    }

    #[test]
    fn rejects_server_block_without_listen() {
        let err = parse_str("server { route / { methods GET; } }").unwrap_err();
        assert!(err.0.contains("listen"));
    }

    #[test]
    fn rejects_duplicate_default_servers_on_same_listen() {
        let src = "server { listen 8080; route / { methods GET; } } server { listen 8080; route / { methods GET; } }";
        let err = parse_str(src).unwrap_err();
        assert!(err.0.contains("default"));
    }

    #[test]
    fn allows_virtual_hosts_disambiguated_by_server_name() {
        let src = "server { listen 8080; server_name a.test; route / { methods GET; } } \
                   server { listen 8080; server_name b.test; route / { methods GET; } }";
        assert!(parse_str(src).is_ok());
    }

    #[test]
    fn rejects_port_zero() {
        assert_error_contains(
            "server { listen 0; route / { methods GET; } }",
            "port 0",
        );
    }

    #[test]
    fn rejects_overflowing_body_size() {
        let src = format!(
            "server {{ listen 8080; client_max_body_size {}m; route / {{ methods GET; }} }}",
            usize::MAX
        );
        assert_error_contains(&src, "too large");
    }

    #[test]
    fn rejects_relative_and_duplicate_routes() {
        assert_error_contains(
            "server { listen 8080; route files { methods GET; } }",
            "must start with `/`",
        );
        assert_error_contains(
            "server { listen 8080; route /files { methods GET; } route /files { methods POST; } }",
            "duplicate route",
        );
    }

    #[test]
    fn rejects_empty_and_duplicate_method_lists() {
        assert_error_contains(
            "server { listen 8080; route / { methods; } }",
            "at least one HTTP method",
        );
        assert_error_contains(
            "server { listen 8080; route / { methods GET GET; } }",
            "duplicate HTTP method",
        );
    }

    #[test]
    fn configuring_get_also_enables_head() {
        let config = parse_str("server { listen 8080; route / { methods GET; } }").unwrap();
        let methods = &config.servers[0].routes[0].methods;
        assert!(methods.contains(&Method::Get));
        assert!(methods.contains(&Method::Head));
    }

    #[test]
    fn rejects_non_redirect_return_status() {
        assert_error_contains(
            "server { listen 8080; route / { return 200 /elsewhere; } }",
            "between 300 and 399",
        );
    }

    #[test]
    fn rejects_invalid_or_duplicate_cgi_extensions() {
        assert_error_contains(
            "server { listen 8080; route / { cgi py python3; } }",
            "must look like `.py`",
        );
        assert_error_contains(
            "server { listen 8080; route / { cgi .py python3; cgi .py other-python; } }",
            "duplicate CGI mapping",
        );
    }

    #[test]
    fn rejects_invalid_or_duplicate_error_page_codes() {
        assert_error_contains(
            "server { listen 8080; error_page 200 /error.html; route / { methods GET; } }",
            "between 400 and 599",
        );
        assert_error_contains(
            "server { listen 8080; error_page 404 /first.html; error_page 404 /second.html; route / { methods GET; } }",
            "duplicate `error_page`",
        );
    }

    #[test]
    fn rejects_duplicate_server_names() {
        assert_error_contains(
            "server { listen 8080; server_name example.test EXAMPLE.test; route / { methods GET; } }",
            "duplicate server_name",
        );
    }

    #[test]
    fn rejects_overlapping_wildcard_and_concrete_listeners() {
        let src = "server { listen 8080; server_name wildcard.test; route / { methods GET; } } \
                   server { listen 127.0.0.1:8080; server_name loopback.test; route / { methods GET; } }";
        assert_error_contains(src, "overlap");
    }

    fn assert_error_contains(src: &str, expected: &str) {
        let error = parse_str(src).expect_err("configuration should be rejected");
        assert!(
            error.0.contains(expected),
            "expected `{}` in configuration error `{}`",
            expected,
            error.0
        );
    }
}
