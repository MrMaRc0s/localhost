use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::config::{Listen, ServerConfig};
use crate::http::{Request, Response};
use crate::router::CgiRequest;

/// A running CGI child process together with the request body still
/// waiting to be written to its stdin and whatever it has produced on
/// stdout so far. Every read/write against `stdin`/`stdout` here is a
/// single non-blocking syscall driven by the connection's own epoll
/// events — nothing in this struct ever blocks the event loop.
pub struct CgiProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    input: Vec<u8>,
    input_written: usize,
    pub output: Vec<u8>,
}

pub enum WriteOutcome {
    Pending,
    Done,
    Error,
}

pub enum ReadOutcome {
    Pending,
    Eof,
    Error,
}

impl CgiProcess {
    pub fn stdin_fd(&self) -> Option<RawFd> {
        self.stdin.as_ref().map(|s| s.as_raw_fd())
    }

    pub fn stdout_fd(&self) -> RawFd {
        self.stdout.as_raw_fd()
    }

    /// One write() of whatever's left of the request body. Once it's all
    /// gone, dropping `stdin` closes the pipe — that's the EOF the CGI/1.1
    /// contract expects to mark the end of the request body.
    pub fn write_stdin_once(&mut self) -> WriteOutcome {
        let stdin = match self.stdin.as_mut() {
            Some(s) => s,
            None => return WriteOutcome::Done,
        };
        match stdin.write(&self.input[self.input_written..]) {
            Ok(n) => {
                self.input_written += n;
                if self.input_written >= self.input.len() {
                    self.stdin = None;
                    WriteOutcome::Done
                } else {
                    WriteOutcome::Pending
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => WriteOutcome::Pending,
            Err(_) => {
                self.stdin = None;
                WriteOutcome::Error
            }
        }
    }

    /// One read() of whatever the CGI has produced so far.
    pub fn read_stdout_once(&mut self) -> ReadOutcome {
        let mut chunk = [0u8; 32 * 1024];
        match self.stdout.read(&mut chunk) {
            Ok(0) => ReadOutcome::Eof,
            Ok(n) => {
                self.output.extend_from_slice(&chunk[..n]);
                ReadOutcome::Pending
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => ReadOutcome::Pending,
            Err(_) => ReadOutcome::Error,
        }
    }

    /// Non-blocking reap attempt. Returns true once the child is gone.
    pub fn try_reap(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Used when a CGI request times out: no graceful way to ask an
    /// arbitrary script to stop, so it's killed outright. Only *sends*
    /// the signal — never blocks on `wait()`; the caller hands the child
    /// to the reap list so the next non-blocking `try_reap()` sweep
    /// collects it once it actually exits.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    pub fn into_child(self) -> Child {
        self.child
    }
}

pub fn spawn(
    cgi_req: &CgiRequest,
    req: &Request,
    server: &ServerConfig,
    listen: Listen,
    remote_addr: &str,
    session_id: &str,
    session_visits: u64,
) -> std::io::Result<CgiProcess> {
    let mut cmd = Command::new(&cgi_req.interpreter);
    // "CGI expects the file to process as first argument."
    cmd.arg(&cgi_req.script_path);
    // Run from the script's own directory so relative paths it opens
    // resolve the way the script author expects.
    cmd.current_dir(&cgi_req.script_dir);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());

    for (key, value) in build_env(cgi_req, req, server, listen, remote_addr, session_id, session_visits) {
        cmd.env(key, value);
    }

    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("stdout was piped");

    set_nonblocking(stdout.as_raw_fd())?;

    let input = req.body.clone();
    if input.is_empty() {
        // No body to send: dropping stdin immediately signals EOF, and we
        // never need to register it with epoll at all.
        stdin = None;
    } else if let Some(s) = &stdin {
        set_nonblocking(s.as_raw_fd())?;
    }

    Ok(CgiProcess { child, stdin, stdout, input, input_written: 0, output: Vec::new() })
}

fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Standard CGI/1.1 meta-variables (RFC 3875) plus one HTTP_* variable per
/// request header, which is how a CGI script reads cookies, user-agent,
/// etc. Everything the script needs to know about the request that isn't
/// the body itself travels through the environment, never argv.
fn build_env(
    cgi_req: &CgiRequest,
    req: &Request,
    server: &ServerConfig,
    listen: Listen,
    remote_addr: &str,
    session_id: &str,
    session_visits: u64,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    env.push(("GATEWAY_INTERFACE".to_string(), "CGI/1.1".to_string()));
    env.push(("SERVER_PROTOCOL".to_string(), format!("HTTP/{}.{}", req.version.0, req.version.1)));
    env.push(("SERVER_SOFTWARE".to_string(), "localhost/0.1".to_string()));
    env.push(("SERVER_PORT".to_string(), listen.port.to_string()));

    let server_name = req
        .header("host")
        .map(|h| h.split(':').next().unwrap_or(h).to_string())
        .unwrap_or_else(|| listen.host.to_string());
    env.push(("SERVER_NAME".to_string(), server_name));

    env.push(("REQUEST_METHOD".to_string(), req.raw_method.clone()));
    // Not part of RFC 3875 either, but ubiquitous enough in real CGI/PHP
    // scripts (they expect it for building self-referential links) that
    // it's worth setting alongside the standard variables above.
    env.push(("REQUEST_URI".to_string(), req.target.clone()));

    let script_name_len = req.path.len().saturating_sub(cgi_req.path_info.len());
    env.push(("SCRIPT_NAME".to_string(), req.path[..script_name_len].to_string()));
    env.push(("PATH_INFO".to_string(), cgi_req.path_info.clone()));
    if !cgi_req.path_info.is_empty() {
        env.push(("PATH_TRANSLATED".to_string(), format!("{}{}", server.root, cgi_req.path_info)));
    }

    env.push(("QUERY_STRING".to_string(), req.query.clone().unwrap_or_default()));
    env.push(("REMOTE_ADDR".to_string(), remote_addr.to_string()));
    // Not part of RFC 3875, but a widely relied-on php-cgi convention that
    // costs nothing to also set for other interpreters.
    env.push(("REDIRECT_STATUS".to_string(), "200".to_string()));
    // Also not standard CGI: the server already resolved (or minted) this
    // request's session before running the script, so hand it over
    // directly rather than making every script re-parse HTTP_COOKIE — the
    // id is available here even on the very first request, before the
    // client has seen the Set-Cookie this response is about to carry.
    env.push(("SESSION_ID".to_string(), session_id.to_string()));
    env.push(("SESSION_VISITS".to_string(), session_visits.to_string()));

    if let Some(ct) = req.header("content-type") {
        env.push(("CONTENT_TYPE".to_string(), ct.to_string()));
    }
    env.push(("CONTENT_LENGTH".to_string(), req.body.len().to_string()));

    for (name, value) in &req.headers {
        if name == "content-type" || name == "content-length" {
            continue;
        }
        let key = format!("HTTP_{}", name.to_ascii_uppercase().replace('-', "_"));
        env.push((key, value.clone()));
    }

    env
}

/// Translates raw CGI output (a CGI-header block, blank line, then body —
/// RFC 3875 §6) into an HTTP response. A script that forgets to speak CGI
/// at all (no header block) still renders as something useful: its whole
/// output becomes the body of a 200 rather than a hard failure.
pub fn parse_output(raw: &[u8]) -> Response {
    let split = find(raw, b"\r\n\r\n")
        .map(|p| (p, 4))
        .or_else(|| find(raw, b"\n\n").map(|p| (p, 2)));

    let (header_block, body) = match split {
        Some((pos, sep_len)) => match std::str::from_utf8(&raw[..pos]) {
            Ok(h) => (h, raw[pos + sep_len..].to_vec()),
            Err(_) => return Response::new(200).header("Content-Type", "text/html; charset=utf-8").body(raw.to_vec()),
        },
        None => return Response::new(200).header("Content-Type", "text/html; charset=utf-8").body(raw.to_vec()),
    };

    let mut status: u16 = 200;
    let mut headers = Vec::new();
    let mut has_content_type = false;

    for line in header_block.split(['\r', '\n']) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            if let Some(code) = value.split_whitespace().next().and_then(|c| c.parse().ok()) {
                status = code;
            }
        } else {
            if name.eq_ignore_ascii_case("content-type") {
                has_content_type = true;
            }
            headers.push((name.to_string(), value.to_string()));
        }
    }

    let mut resp = Response::new(status);
    for (name, value) in headers {
        resp = resp.header(&name, value);
    }
    if !has_content_type {
        resp = resp.header("Content-Type", "text/html; charset=utf-8");
    }
    resp.body(body)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_and_headers() {
        let raw = b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\nnope";
        let resp = parse_output(raw);
        assert_eq!(resp.status, 404);
        assert_eq!(resp.body, b"nope");
        assert!(resp.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v == "text/plain"));
    }

    #[test]
    fn defaults_to_200_and_html_when_headers_omit_them() {
        let raw = b"X-Demo: yes\r\n\r\n<p>hi</p>";
        let resp = parse_output(raw);
        assert_eq!(resp.status, 200);
        assert!(resp.headers.iter().any(|(k, v)| k == "Content-Type" && v.starts_with("text/html")));
    }

    #[test]
    fn falls_back_to_raw_body_when_script_ignores_cgi_headers() {
        let raw = b"just plain text, no header block";
        let resp = parse_output(raw);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, raw);
    }
}
