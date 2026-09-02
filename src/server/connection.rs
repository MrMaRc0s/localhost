use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use super::epoll::{self, Epoll};
use crate::config::{Config, Listen};
use crate::http::body::{determine_mode, BodyError, BodyReader};
use crate::http::request::ParseError;
use crate::http::{Method, Request, Response};
use crate::router::Dispatch;

const READ_CHUNK: usize = 64 * 1024;
/// A request that hasn't finished arriving within this long is abandoned
/// with a 408 rather than left to occupy a connection slot forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an idle keep-alive connection is held open waiting for the
/// next pipelined request before we just close it.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

enum State {
    ReadingHead,
    ReadingBody { request: Request, body: BodyReader },
    Writing { buf: Vec<u8>, written: usize, close_after: bool },
}

pub struct Connection {
    pub fd: RawFd,
    stream: TcpStream,
    listen: Listen,
    read_buf: Vec<u8>,
    state: State,
    last_activity: Instant,
}

impl Connection {
    pub fn new(fd: RawFd, stream: TcpStream, listen: Listen) -> Self {
        Connection { fd, stream, listen, read_buf: Vec::new(), state: State::ReadingHead, last_activity: Instant::now() }
    }

    /// Handles one epoll notification for this connection's fd. Returns
    /// `true` when the connection is finished and should be dropped.
    pub fn on_event(&mut self, config: &Config, epoll: &Epoll, event: &epoll::Event) -> bool {
        self.last_activity = Instant::now();

        if event.error {
            return true;
        }
        if event.readable {
            if self.read_once(config, epoll) {
                return true;
            }
        }
        if event.writable {
            if self.write_once(config, epoll) {
                return true;
            }
        }
        false
    }

    pub fn is_timed_out(&self, now: Instant) -> bool {
        let limit = match self.state {
            State::ReadingHead if self.read_buf.is_empty() => IDLE_TIMEOUT,
            _ => REQUEST_TIMEOUT,
        };
        now.duration_since(self.last_activity) > limit
    }

    /// Called by the sweep in the main loop when `is_timed_out` fires.
    /// A request that was actually in flight gets a 408; a connection
    /// that was just idly waiting for the next pipelined request is
    /// closed without ceremony, exactly like nginx's `keepalive_timeout`.
    pub fn handle_timeout(&mut self, config: &Config, epoll: &Epoll) -> bool {
        match &self.state {
            State::ReadingHead if self.read_buf.is_empty() => true,
            State::Writing { .. } => true,
            _ => {
                let resp = crate::error_page::build(None, 408);
                self.begin_write(resp, false, false, config, epoll);
                false
            }
        }
    }

    fn read_once(&mut self, config: &Config, epoll: &Epoll) -> bool {
        let mut chunk = [0u8; READ_CHUNK];
        match self.stream.read(&mut chunk) {
            Ok(0) => true, // peer closed the connection
            Ok(n) => self.feed(&chunk[..n], config, epoll),
            Err(e) if would_block(&e) => false,
            Err(_) => true,
        }
    }

    fn write_once(&mut self, config: &Config, epoll: &Epoll) -> bool {
        let outcome = match &mut self.state {
            State::Writing { buf, written, close_after } => match self.stream.write(&buf[*written..]) {
                Ok(0) => return true,
                Ok(n) => {
                    *written += n;
                    if *written < buf.len() {
                        return false; // more to write; wait for the next writable event
                    }
                    Some(*close_after)
                }
                Err(e) if would_block(&e) => return false,
                Err(_) => return true,
            },
            _ => None,
        };

        match outcome {
            Some(true) => true,
            Some(false) => {
                self.state = State::ReadingHead;
                let _ = epoll.modify(self.fd, epoll::READABLE);
                if self.read_buf.is_empty() {
                    false
                } else {
                    self.try_parse_head(config, epoll)
                }
            }
            None => false,
        }
    }

    fn feed(&mut self, data: &[u8], config: &Config, epoll: &Epoll) -> bool {
        let state = std::mem::replace(&mut self.state, State::ReadingHead);
        match state {
            State::ReadingHead => {
                self.read_buf.extend_from_slice(data);
                self.try_parse_head(config, epoll)
            }
            State::ReadingBody { request, mut body } => match body.feed(data) {
                Ok(()) => {
                    if body.is_done() {
                        self.read_buf = body.take_trailing();
                        let mut request = request;
                        request.body = body.into_body();
                        self.dispatch_and_respond(request, config, epoll);
                    } else {
                        self.state = State::ReadingBody { request, body };
                    }
                    false
                }
                Err(e) => self.fail_body_error(e, Some(&request), config, epoll),
            },
            other @ State::Writing { .. } => {
                // We only register for EPOLLIN while reading; a stray
                // read event mid-write is a no-op, not corruption.
                self.state = other;
                false
            }
        }
    }

    fn try_parse_head(&mut self, config: &Config, epoll: &Epoll) -> bool {
        match Request::parse_head(&self.read_buf) {
            Ok(None) => false,
            Ok(Some((req, consumed))) => {
                let remaining: Vec<u8> = self.read_buf.split_off(consumed);
                self.read_buf.clear();

                let max_body = max_body_size(config, self.listen, &req);
                match determine_mode(&req, max_body) {
                    Ok(mut body) => match body.feed(&remaining) {
                        Ok(()) => {
                            if body.is_done() {
                                self.read_buf = body.take_trailing();
                                let mut req = req;
                                req.body = body.into_body();
                                self.dispatch_and_respond(req, config, epoll);
                            } else {
                                self.state = State::ReadingBody { request: req, body };
                            }
                            false
                        }
                        Err(e) => self.fail_body_error(e, Some(&req), config, epoll),
                    },
                    Err(e) => self.fail_body_error(e, Some(&req), config, epoll),
                }
            }
            Err(e) => self.fail_parse_error(e, config, epoll),
        }
    }

    fn dispatch_and_respond(&mut self, req: Request, config: &Config, epoll: &Epoll) {
        let keep_alive = req.keep_alive();
        let host = req.header("host").map(str::to_string);
        let server = config.resolve(self.listen, host.as_deref());

        let response = match server {
            Some(server) => match crate::router::dispatch(server, &req) {
                Dispatch::Response(r) => r,
                // CGI execution is wired up through the event loop's own
                // pipe fds in a later commit; for now report what's true.
                Dispatch::Cgi(_) => crate::error_page::build(Some(server), 501),
            },
            None => crate::error_page::build(None, 400),
        };

        let for_head = req.method == Method::Head;
        self.begin_write(response, keep_alive, for_head, config, epoll);
    }

    fn begin_write(&mut self, response: Response, keep_alive: bool, for_head: bool, _config: &Config, epoll: &Epoll) {
        let buf = response.serialize(keep_alive, for_head);
        self.state = State::Writing { buf, written: 0, close_after: !keep_alive };
        let _ = epoll.modify(self.fd, epoll::WRITABLE);
    }

    fn fail_body_error(&mut self, err: BodyError, req: Option<&Request>, config: &Config, epoll: &Epoll) -> bool {
        let code = match err {
            BodyError::TooLarge => 413,
            BodyError::Malformed(_) => 400,
        };
        let server = config.resolve(self.listen, req.and_then(|r| r.header("host")));
        let resp = crate::error_page::build(server, code);
        self.begin_write(resp, false, false, config, epoll);
        false
    }

    fn fail_parse_error(&mut self, err: ParseError, config: &Config, epoll: &Epoll) -> bool {
        let code = match err {
            ParseError::UriTooLong => 414,
            ParseError::HeadTooLarge => 400,
            ParseError::VersionNotSupported => 505,
            ParseError::BadRequest(_) => 400,
        };
        let resp = crate::error_page::build(None, code);
        self.begin_write(resp, false, false, config, epoll);
        false
    }
}

fn would_block(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock
}

fn max_body_size(config: &Config, listen: Listen, req: &Request) -> usize {
    config
        .resolve(listen, req.header("host"))
        .map(|s| s.client_max_body_size)
        .unwrap_or(1024 * 1024)
}
