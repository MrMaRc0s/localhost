use super::error::ConfigError;
use super::types::{Config, Listen, RouteConfig, ServerConfig};
use crate::http::Method;
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

    let flush = |current: &mut String, line: usize, tokens: &mut Vec<Tok>| {
        if !current.is_empty() {
            tokens.push(Tok { text: std::mem::take(current), line });
        }
    };

    for ch in src.chars() {
        match ch {
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
                    server.server_names.extend(self.until_semicolon()?);
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
                    for word in self.until_semicolon()? {
                        let method = Method::parse(&word);
                        if method == Method::Other {
                            return Err(ConfigError::new(format!(
                                "line {}: unsupported HTTP method `{}`",
                                line, word
                            )));
                        }
                        methods.push(method);
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
                    route.cgi.insert(values[0].clone(), values[1].clone());
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
        Ok(Listen { host, port })
    } else {
        let port: u16 = value
            .parse()
            .map_err(|_| ConfigError::new(format!("line {}: invalid `listen` value `{}`", line, value)))?;
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
    Ok(base * multiplier)
}
