use super::date::http_date_now;
use super::status::reason_phrase;

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response { status, headers: Vec::new(), body: Vec::new() }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn html(status: u16, body: impl Into<String>) -> Self {
        let body = body.into().into_bytes();
        Response::new(status).header("Content-Type", "text/html; charset=utf-8").body(body)
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        let body = body.into().into_bytes();
        Response::new(status).header("Content-Type", "text/plain; charset=utf-8").body(body)
    }

    pub fn redirect(status: u16, location: &str) -> Self {
        let body = format!(
            "<html><body>{} — <a href=\"{}\">{}</a></body></html>",
            reason_phrase(status),
            location,
            location
        );
        Response::html(status, body).header("Location", location)
    }

    pub fn no_content(status: u16) -> Self {
        Response::new(status)
    }

    /// Serializes the full HTTP/1.1 response. `for_head` drops the body
    /// bytes (a HEAD response must not have one) while still reporting the
    /// Content-Length the equivalent GET would have sent.
    pub fn serialize(&self, keep_alive: bool, for_head: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(256 + self.body.len());
        out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", self.status, reason_phrase(self.status)).as_bytes());
        out.extend_from_slice(format!("Date: {}\r\n", http_date_now()).as_bytes());
        out.extend_from_slice(b"Server: localhost/0.1\r\n");

        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("content-length") || name.eq_ignore_ascii_case("connection") {
                continue; // we own these two, always computed below
            }
            out.extend_from_slice(format!("{}: {}\r\n", name, value).as_bytes());
        }

        out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        out.extend_from_slice(format!("Connection: {}\r\n", if keep_alive { "keep-alive" } else { "close" }).as_bytes());
        out.extend_from_slice(b"\r\n");

        if !for_head {
            out.extend_from_slice(&self.body);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_status_line_and_headers() {
        let resp = Response::html(200, "<h1>hi</h1>");
        let bytes = resp.serialize(true, false);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 11\r\n"));
        assert!(text.contains("Connection: keep-alive\r\n"));
        assert!(text.ends_with("<h1>hi</h1>"));
    }

    #[test]
    fn head_response_has_no_body_bytes() {
        let resp = Response::html(200, "<h1>hi</h1>");
        let bytes = resp.serialize(true, true);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("Content-Length: 11\r\n"));
        assert!(!text.contains("<h1>"));
    }

    #[test]
    fn connection_close_when_not_keep_alive() {
        let resp = Response::new(200);
        let text = String::from_utf8(resp.serialize(false, false)).unwrap();
        assert!(text.contains("Connection: close\r\n"));
    }

    #[test]
    fn caller_cannot_smuggle_a_second_content_length() {
        let resp = Response::new(200).header("Content-Length", "999").body(b"ok".to_vec());
        let text = String::from_utf8(resp.serialize(true, false)).unwrap();
        assert_eq!(text.matches("Content-Length").count(), 1);
        assert!(text.contains("Content-Length: 2\r\n"));
    }
}
