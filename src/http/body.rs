use super::request::Request;

#[derive(Debug)]
pub enum BodyError {
    TooLarge,
    Malformed(String),
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    None,
    ContentLength(usize),
    Chunked,
}

/// Decides how (or whether) a request carries a body, without reading any
/// of it yet. Rejects the ambiguous case of both `Content-Length` and
/// `Transfer-Encoding: chunked` being present (RFC 9112 §6.1 — a classic
/// request-smuggling vector) and rejects a declared length over the
/// configured `client_max_body_size` up front, before we bother reading
/// any of it.
pub fn determine_mode(req: &Request, max_size: usize) -> Result<BodyReader, BodyError> {
    let chunked = req.is_chunked();
    let content_length = req.content_length();

    if chunked && content_length.is_some() {
        return Err(BodyError::Malformed(
            "request has both Content-Length and Transfer-Encoding".into(),
        ));
    }

    let mode = if chunked {
        Mode::Chunked
    } else if let Some(len) = content_length {
        if len > max_size {
            return Err(BodyError::TooLarge);
        }
        Mode::ContentLength(len)
    } else {
        Mode::None
    };

    Ok(BodyReader::new(mode, max_size))
}

/// Incrementally decodes a request body (fixed-length or chunked) as bytes
/// trickle in from a non-blocking socket read. `feed` is meant to be
/// called once per `read()` the connection state machine performs; any
/// bytes left over once the body is complete (e.g. the start of the next
/// pipelined request) are held in `trailing` for the caller to reclaim.
#[derive(Debug)]
pub struct BodyReader {
    mode: Mode,
    max_size: usize,
    body: Vec<u8>,
    trailing: Vec<u8>,
    chunk_buf: Vec<u8>,
    done: bool,
}

impl BodyReader {
    fn new(mode: Mode, max_size: usize) -> Self {
        let done = matches!(mode, Mode::None);
        BodyReader { mode, max_size, body: Vec::new(), trailing: Vec::new(), chunk_buf: Vec::new(), done }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn into_body(self) -> Vec<u8> {
        self.body
    }

    /// Bytes read but not part of this body (start of a pipelined next
    /// request). Only meaningful once `is_done()`.
    pub fn take_trailing(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.trailing)
    }

    pub fn feed(&mut self, input: &[u8]) -> Result<(), BodyError> {
        if self.done {
            self.trailing.extend_from_slice(input);
            return Ok(());
        }
        match self.mode {
            Mode::None => {
                self.done = true;
                self.trailing.extend_from_slice(input);
                Ok(())
            }
            Mode::ContentLength(len) => self.feed_content_length(len, input),
            Mode::Chunked => self.feed_chunked(input),
        }
    }

    fn feed_content_length(&mut self, len: usize, input: &[u8]) -> Result<(), BodyError> {
        let need = len - self.body.len();
        let take = need.min(input.len());
        self.body.extend_from_slice(&input[..take]);
        if self.body.len() >= len {
            self.done = true;
            self.trailing.extend_from_slice(&input[take..]);
        }
        Ok(())
    }

    fn feed_chunked(&mut self, input: &[u8]) -> Result<(), BodyError> {
        self.chunk_buf.extend_from_slice(input);

        loop {
            let size_line_end = match find(&self.chunk_buf, b"\r\n") {
                Some(pos) => pos,
                None => {
                    if self.chunk_buf.len() > 64 {
                        return Err(BodyError::Malformed("chunk size line too long".into()));
                    }
                    break;
                }
            };

            let size_line = &self.chunk_buf[..size_line_end];
            let size_str = size_line
                .split(|&b| b == b';')
                .next()
                .and_then(|s| std::str::from_utf8(s).ok())
                .map(str::trim)
                .unwrap_or("");
            let chunk_size = usize::from_str_radix(size_str, 16)
                .map_err(|_| BodyError::Malformed(format!("invalid chunk size `{}`", size_str)))?;

            let data_start = size_line_end + 2;

            if chunk_size == 0 {
                // Last chunk: `0\r\n` followed by an (optional) trailer
                // section, terminated by a blank line.
                if self.chunk_buf.len() < data_start + 2 {
                    break;
                }
                let terminator = match find(&self.chunk_buf[data_start..], b"\r\n\r\n") {
                    Some(pos) => data_start + pos + 4,
                    None if self.chunk_buf[data_start..].starts_with(b"\r\n") => data_start + 2,
                    None => break,
                };
                self.trailing = self.chunk_buf.split_off(terminator);
                self.chunk_buf.clear();
                self.done = true;
                break;
            }

            if self.body.len() + chunk_size > self.max_size {
                return Err(BodyError::TooLarge);
            }

            let chunk_end = data_start + chunk_size;
            if self.chunk_buf.len() < chunk_end + 2 {
                break;
            }
            if &self.chunk_buf[chunk_end..chunk_end + 2] != b"\r\n" {
                return Err(BodyError::Malformed("chunk data missing trailing CRLF".into()));
            }

            self.body.extend_from_slice(&self.chunk_buf[data_start..chunk_end]);
            self.chunk_buf.drain(..chunk_end + 2);
        }

        Ok(())
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_length_single_feed() {
        let mut r = BodyReader::new(Mode::ContentLength(5), 1024);
        r.feed(b"hello").unwrap();
        assert!(r.is_done());
        assert_eq!(r.body(), b"hello");
    }

    #[test]
    fn content_length_split_across_feeds() {
        let mut r = BodyReader::new(Mode::ContentLength(5), 1024);
        r.feed(b"he").unwrap();
        assert!(!r.is_done());
        r.feed(b"llo").unwrap();
        assert!(r.is_done());
        assert_eq!(r.body(), b"hello");
    }

    #[test]
    fn content_length_keeps_trailing_pipelined_bytes() {
        let mut r = BodyReader::new(Mode::ContentLength(5), 1024);
        r.feed(b"helloGET / HTTP/1.1").unwrap();
        assert!(r.is_done());
        assert_eq!(r.body(), b"hello");
        assert_eq!(r.take_trailing(), b"GET / HTTP/1.1");
    }

    #[test]
    fn chunked_basic() {
        let mut r = BodyReader::new(Mode::Chunked, 1024);
        r.feed(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap();
        assert!(r.is_done());
        assert_eq!(r.body(), b"Wikipedia");
    }

    #[test]
    fn chunked_split_across_feeds() {
        let mut r = BodyReader::new(Mode::Chunked, 1024);
        r.feed(b"4\r\nWi").unwrap();
        assert!(!r.is_done());
        r.feed(b"ki\r\n0\r\n\r\n").unwrap();
        assert!(r.is_done());
        assert_eq!(r.body(), b"Wiki");
    }

    #[test]
    fn chunked_rejects_oversized_body() {
        let mut r = BodyReader::new(Mode::Chunked, 3);
        let err = r.feed(b"4\r\nWiki\r\n0\r\n\r\n").unwrap_err();
        assert!(matches!(err, BodyError::TooLarge));
    }

    #[test]
    fn chunked_with_trailer_headers() {
        let mut r = BodyReader::new(Mode::Chunked, 1024);
        r.feed(b"3\r\nfoo\r\n0\r\nX-Trailer: yes\r\n\r\n").unwrap();
        assert!(r.is_done());
        assert_eq!(r.body(), b"foo");
    }

    #[test]
    fn rejects_content_length_and_chunked_together() {
        use crate::http::Method;
        let req = Request {
            method: Method::Post,
            raw_method: "POST".into(),
            target: "/".into(),
            path: "/".into(),
            query: None,
            version: (1, 1),
            headers: vec![
                ("content-length".into(), "5".into()),
                ("transfer-encoding".into(), "chunked".into()),
            ],
            body: Vec::new(),
        };
        let err = determine_mode(&req, 1024).unwrap_err();
        assert!(matches!(err, BodyError::Malformed(_)));
    }
}
