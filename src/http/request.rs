use super::method::Method;

const MAX_HEAD_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub enum ParseError {
    /// Head is malformed in a way that can never be fixed by more bytes.
    BadRequest(String),
    UriTooLong,
    HeadTooLarge,
    VersionNotSupported,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub raw_method: String,
    pub target: String,
    pub path: String,
    pub query: Option<String>,
    pub version: (u8, u8),
    /// (lowercase name, value), duplicates preserved in arrival order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Returns every occurrence of a header. Message-framing headers need
    /// this rather than `header()`: silently using the first of two
    /// Content-Length or Host fields can make two HTTP recipients disagree
    /// about where one request ends and the next begins.
    pub fn header_values<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .filter_map(move |(key, value)| (key == &name).then_some(value.as_str()))
    }

    pub fn keep_alive(&self) -> bool {
        match self.header("connection").map(|v| v.to_ascii_lowercase()) {
            Some(v) if v.contains("close") => false,
            Some(v) if v.contains("keep-alive") => true,
            _ => self.version >= (1, 1),
        }
    }

    /// Parses the request-line + header block from `buf`. Returns
    /// `Ok(None)` when the terminating blank line hasn't arrived yet (the
    /// caller should keep reading), `Ok(Some((request, consumed)))` once a
    /// full head is available, and `Err` for a head that is malformed
    /// beyond repair regardless of how many more bytes show up.
    pub fn parse_head(buf: &[u8]) -> Result<Option<(Request, usize)>, ParseError> {
        let head_end = match find_double_crlf(buf) {
            Some(pos) => pos,
            None => {
                if buf.len() > MAX_HEAD_BYTES {
                    return Err(ParseError::HeadTooLarge);
                }
                return Ok(None);
            }
        };

        let separator_len = if buf[head_end..].starts_with(b"\r\n\r\n") {
            4
        } else {
            2
        };
        if head_end > MAX_HEAD_BYTES {
            return Err(ParseError::HeadTooLarge);
        }
        let head_bytes = &buf[..head_end];
        let head_str = std::str::from_utf8(head_bytes)
            .map_err(|_| ParseError::BadRequest("head is not valid UTF-8".into()))?;

        let mut lines = head_str.split("\r\n").flat_map(|l| l.split('\n'));

        let request_line = lines
            .next()
            .ok_or_else(|| ParseError::BadRequest("empty request".into()))?;
        let mut parts = request_line.split(' ').filter(|s| !s.is_empty());
        let raw_method = parts
            .next()
            .ok_or_else(|| ParseError::BadRequest("missing method".into()))?;
        let target = parts
            .next()
            .ok_or_else(|| ParseError::BadRequest("missing target".into()))?;
        let http_version = parts
            .next()
            .ok_or_else(|| ParseError::BadRequest("missing HTTP version".into()))?;
        if parts.next().is_some() {
            return Err(ParseError::BadRequest("malformed request line".into()));
        }

        if target.len() > 8192 {
            return Err(ParseError::UriTooLong);
        }
        if raw_method.is_empty() || !raw_method.as_bytes().iter().copied().all(is_token_byte) {
            return Err(ParseError::BadRequest("malformed method".into()));
        }
        if !target.starts_with('/') {
            return Err(ParseError::BadRequest(
                "request target must be origin-form".into(),
            ));
        }

        let version = parse_version(http_version)?;

        let (path_raw, query) = match target.split_once('?') {
            Some((p, q)) => (p, Some(q.to_string())),
            None => (target, None),
        };
        let path = decode_percent(path_raw)
            .ok_or_else(|| ParseError::BadRequest("invalid percent-encoding".into()))?;

        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if line.starts_with(' ') || line.starts_with('\t') {
                return Err(ParseError::BadRequest(
                    "obsolete header line folding is not supported".into(),
                ));
            }
            let (name, value) = line.split_once(':').ok_or_else(|| {
                ParseError::BadRequest(format!("malformed header line `{}`", line))
            })?;
            if name.is_empty() || !name.as_bytes().iter().copied().all(is_token_byte) {
                return Err(ParseError::BadRequest("malformed header name".into()));
            }
            if value
                .as_bytes()
                .iter()
                .any(|byte| (*byte < b' ' && *byte != b'\t') || *byte == 0x7f)
            {
                return Err(ParseError::BadRequest("malformed header value".into()));
            }
            headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
        }

        if version == (1, 1) {
            let mut hosts = headers.iter().filter(|(name, _)| name == "host");
            let host = hosts
                .next()
                .ok_or_else(|| ParseError::BadRequest("HTTP/1.1 request is missing Host".into()))?;
            if host.1.is_empty() || hosts.next().is_some() {
                return Err(ParseError::BadRequest(
                    "HTTP/1.1 request must contain exactly one non-empty Host".into(),
                ));
            }
        }

        let request = Request {
            method: Method::parse(raw_method),
            raw_method: raw_method.to_string(),
            target: target.to_string(),
            path,
            query,
            version,
            headers,
            body: Vec::new(),
        };

        Ok(Some((request, head_end + separator_len)))
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    if let Some(pos) = windows_find(buf, b"\r\n\r\n") {
        return Some(pos);
    }
    // Be lenient with bare-LF clients/tools (e.g. some `nc` scripts).
    windows_find(buf, b"\n\n")
}

fn windows_find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// RFC 9110's `tchar`: the bytes allowed in a method or field name.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn parse_version(s: &str) -> Result<(u8, u8), ParseError> {
    let rest = s
        .strip_prefix("HTTP/")
        .ok_or(ParseError::VersionNotSupported)?;
    let (major, minor) = rest
        .split_once('.')
        .ok_or(ParseError::VersionNotSupported)?;
    let major: u8 = major.parse().map_err(|_| ParseError::VersionNotSupported)?;
    let minor: u8 = minor.parse().map_err(|_| ParseError::VersionNotSupported)?;
    if major != 1 || (minor != 0 && minor != 1) {
        return Err(ParseError::VersionNotSupported);
    }
    Ok((major, minor))
}

/// Percent-decodes a URI component. Returns `None` on an invalid escape.
pub fn decode_percent(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                let hex_str = std::str::from_utf8(hex).ok()?;
                let byte = u8::from_str_radix(hex_str, 16).ok()?;
                out.push(byte);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_get() {
        let raw = b"GET /index.html?x=1 HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n";
        let (req, consumed) = Request::parse_head(raw).unwrap().unwrap();
        assert_eq!(consumed, raw.len());
        assert_eq!(req.method, Method::Get);
        assert_eq!(req.path, "/index.html");
        assert_eq!(req.query.as_deref(), Some("x=1"));
        assert_eq!(req.header("host"), Some("example.com"));
        assert!(!req.keep_alive());
    }

    #[test]
    fn incomplete_head_returns_none() {
        let raw = b"GET / HTTP/1.1\r\nHost: example.com\r\n";
        assert!(Request::parse_head(raw).unwrap().is_none());
    }

    #[test]
    fn rejects_missing_target() {
        let raw = b"GET HTTP/1.1\r\n\r\n";
        assert!(matches!(
            Request::parse_head(raw),
            Err(ParseError::BadRequest(_))
        ));
    }

    #[test]
    fn rejects_unsupported_version() {
        let raw = b"GET / HTTP/2.0\r\n\r\n";
        assert!(matches!(
            Request::parse_head(raw),
            Err(ParseError::VersionNotSupported)
        ));
    }

    #[test]
    fn decodes_percent_encoded_path() {
        let raw = b"GET /a%20b HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let (req, _) = Request::parse_head(raw).unwrap().unwrap();
        assert_eq!(req.path, "/a b");
    }

    #[test]
    fn http_1_0_defaults_to_close() {
        let raw = b"GET / HTTP/1.0\r\n\r\n";
        let (req, _) = Request::parse_head(raw).unwrap().unwrap();
        assert!(!req.keep_alive());
    }

    #[test]
    fn http_1_1_defaults_to_keep_alive() {
        let raw = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let (req, _) = Request::parse_head(raw).unwrap().unwrap();
        assert!(req.keep_alive());
    }

    #[test]
    fn rejects_http_1_1_without_host() {
        let raw = b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n";
        assert!(matches!(
            Request::parse_head(raw),
            Err(ParseError::BadRequest(_))
        ));
    }

    #[test]
    fn rejects_duplicate_host_headers() {
        let raw = b"GET / HTTP/1.1\r\nHost: a.test\r\nHost: b.test\r\n\r\n";
        assert!(matches!(
            Request::parse_head(raw),
            Err(ParseError::BadRequest(_))
        ));
    }

    #[test]
    fn rejects_oversized_complete_head() {
        let value = "x".repeat(MAX_HEAD_BYTES);
        let raw = format!(
            "GET / HTTP/1.1\r\nHost: example.com\r\nX-Fill: {}\r\n\r\n",
            value
        );
        assert!(matches!(
            Request::parse_head(raw.as_bytes()),
            Err(ParseError::HeadTooLarge)
        ));
    }

    #[test]
    fn rejects_invalid_header_name() {
        let raw = b"GET / HTTP/1.1\r\nHost: example.com\r\nBad(Name): value\r\n\r\n";
        assert!(matches!(
            Request::parse_head(raw),
            Err(ParseError::BadRequest(_))
        ));
    }
}
