use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use super::epoll::{self, Epoll};
use super::EventCtx;
use crate::cgi::{self, CgiProcess, ReadOutcome, WriteOutcome};
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
/// A CGI script that hasn't produced a complete response within this long
/// is killed rather than left to hold the connection (and a process
/// table entry) open forever.
const CGI_TIMEOUT: Duration = Duration::from_secs(15);

enum State {
    ReadingHead,
    ReadingBody { request: Request, body: BodyReader },
    RunningCgi(CgiProcess),
    Writing { buf: Vec<u8>, written: usize, close_after: bool },
}

pub struct Connection {
    pub fd: RawFd,
    stream: TcpStream,
    listen: Listen,
    peer_addr: String,
    read_buf: Vec<u8>,
    state: State,
    last_activity: Instant,
    /// Decided when a request is dispatched, applied once its response
    /// (static or CGI-produced) is actually ready to write.
    pending_keep_alive: bool,
    pending_for_head: bool,
}

impl Connection {
    pub fn new(fd: RawFd, stream: TcpStream, listen: Listen) -> Self {
        let peer_addr = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "0.0.0.0".to_string());
        Connection {
            fd,
            stream,
            listen,
            peer_addr,
            read_buf: Vec::new(),
            state: State::ReadingHead,
            last_activity: Instant::now(),
            pending_keep_alive: false,
            pending_for_head: false,
        }
    }

    /// Handles one epoll notification. `event.fd` may be this connection's
    /// own client socket, or one of its CGI child's pipe fds — the main
    /// loop resolves either back to this Connection via its owner map.
    /// Returns `true` when the connection is finished and should be
    /// dropped (after `cleanup` is called on it).
    pub fn on_event(&mut self, config: &Config, ctx: &mut EventCtx, event: &epoll::Event) -> bool {
        self.last_activity = Instant::now();

        if event.fd == self.fd {
            // EPOLLHUP/EPOLLERR on the client socket really does mean
            // "nothing more to do here".
            if event.error {
                return true;
            }
            if event.readable && self.read_once(config, ctx) {
                return true;
            }
            if event.writable && self.write_once(config, ctx) {
                return true;
            }
            return false;
        }

        // Not the client socket: must be one of this connection's CGI
        // pipes. A pipe's read end reports EPOLLHUP the moment the child
        // closes it — normal end-of-output, not a fatal error — so it
        // must still be routed into a real read() (whose Ok(0) is what
        // actually detects EOF) rather than tearing the connection down
        // here before the response has even been produced. `event.fd`
        // itself (not the readable/writable/error bits, which EPOLLHUP
        // can set on either pipe) says which of the two this is.
        let is_stdin = matches!(&self.state, State::RunningCgi(cgi) if Some(event.fd) == cgi.stdin_fd());
        let is_stdout = matches!(&self.state, State::RunningCgi(cgi) if event.fd == cgi.stdout_fd());
        if is_stdin {
            self.cgi_write_once(ctx, event.fd);
        }
        if is_stdout {
            self.cgi_read_once(config, ctx, event.fd);
        }
        false
    }

    pub fn is_timed_out(&self, now: Instant) -> bool {
        let limit = match self.state {
            State::ReadingHead if self.read_buf.is_empty() => IDLE_TIMEOUT,
            State::RunningCgi(_) => CGI_TIMEOUT,
            _ => REQUEST_TIMEOUT,
        };
        now.duration_since(self.last_activity) > limit
    }

    /// Called by the main loop's sweep when `is_timed_out` fires.
    pub fn handle_timeout(&mut self, _config: &Config, ctx: &mut EventCtx) -> bool {
        match &mut self.state {
            State::ReadingHead if self.read_buf.is_empty() => true,
            State::Writing { .. } => true,
            State::RunningCgi(cgi) => {
                deregister_cgi(cgi, ctx);
                cgi.kill();
                let state = std::mem::replace(&mut self.state, State::ReadingHead);
                if let State::RunningCgi(cgi) = state {
                    ctx.reap.push(cgi.into_child());
                }
                let resp = crate::error_page::build(None, 504);
                self.begin_write(resp, false, false, ctx.epoll);
                false
            }
            _ => {
                let resp = crate::error_page::build(None, 408);
                self.begin_write(resp, false, false, ctx.epoll);
                false
            }
        }
    }

    /// Deregisters and, if a CGI child is still running, kills it — used
    /// when the connection is being dropped for a reason unrelated to
    /// timeout (client hung up, socket error) so we never abandon a
    /// child process or leak its pipe fds from the epoll/owner registry.
    pub fn cleanup(&mut self, ctx: &mut EventCtx) {
        ctx.pipes.retain(|_, owner| *owner != self.fd);
        if let State::RunningCgi(mut cgi) = std::mem::replace(&mut self.state, State::ReadingHead) {
            cgi.kill();
            ctx.reap.push(cgi.into_child());
        }
    }

    fn read_once(&mut self, config: &Config, ctx: &mut EventCtx) -> bool {
        let mut chunk = [0u8; READ_CHUNK];
        match self.stream.read(&mut chunk) {
            Ok(0) => true, // peer closed the connection
            Ok(n) => self.feed(&chunk[..n], config, ctx),
            Err(e) if would_block(&e) => false,
            Err(_) => true,
        }
    }

    fn write_once(&mut self, config: &Config, ctx: &mut EventCtx) -> bool {
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
                let _ = ctx.epoll.modify(self.fd, epoll::READABLE);
                if self.read_buf.is_empty() {
                    false
                } else {
                    self.try_parse_head(config, ctx)
                }
            }
            None => false,
        }
    }

    fn feed(&mut self, data: &[u8], config: &Config, ctx: &mut EventCtx) -> bool {
        let state = std::mem::replace(&mut self.state, State::ReadingHead);
        match state {
            State::ReadingHead => {
                self.read_buf.extend_from_slice(data);
                self.try_parse_head(config, ctx)
            }
            State::ReadingBody { request, mut body } => match body.feed(data) {
                Ok(()) => {
                    if body.is_done() {
                        self.read_buf = body.take_trailing();
                        let mut request = request;
                        request.body = body.into_body();
                        self.dispatch_and_respond(request, config, ctx);
                    } else {
                        self.state = State::ReadingBody { request, body };
                    }
                    false
                }
                Err(e) => self.fail_body_error(e, Some(&request), config, ctx),
            },
            other => {
                // Mid-CGI or mid-write: a stray extra read event is a
                // no-op, not corruption. Restore state and move on.
                self.state = other;
                false
            }
        }
    }

    fn try_parse_head(&mut self, config: &Config, ctx: &mut EventCtx) -> bool {
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
                                self.dispatch_and_respond(req, config, ctx);
                            } else {
                                self.state = State::ReadingBody { request: req, body };
                            }
                            false
                        }
                        Err(e) => self.fail_body_error(e, Some(&req), config, ctx),
                    },
                    Err(e) => self.fail_body_error(e, Some(&req), config, ctx),
                }
            }
            Err(e) => self.fail_parse_error(e, ctx),
        }
    }

    fn dispatch_and_respond(&mut self, req: Request, config: &Config, ctx: &mut EventCtx) {
        let keep_alive = req.keep_alive();
        let for_head = req.method == Method::Head;
        let host = req.header("host").map(str::to_string);
        let server = config.resolve(self.listen, host.as_deref());

        let server = match server {
            Some(s) => s,
            None => {
                let resp = crate::error_page::build(None, 400);
                self.begin_write(resp, false, false, ctx.epoll);
                return;
            }
        };

        match crate::router::dispatch(server, &req) {
            Dispatch::Response(r) => self.begin_write(r, keep_alive, for_head, ctx.epoll),
            Dispatch::Cgi(cgi_req) => {
                self.pending_keep_alive = keep_alive;
                self.pending_for_head = for_head;
                self.start_cgi(cgi_req, &req, server, ctx);
            }
        }
    }

    fn start_cgi(&mut self, cgi_req: crate::router::CgiRequest, req: &Request, server: &crate::config::ServerConfig, ctx: &mut EventCtx) {
        match cgi::spawn(&cgi_req, req, server, self.listen, &self.peer_addr) {
            Ok(process) => {
                if let Some(stdin_fd) = process.stdin_fd() {
                    if ctx.epoll.add(stdin_fd, epoll::WRITABLE).is_ok() {
                        ctx.pipes.insert(stdin_fd, self.fd);
                    }
                }
                let stdout_fd = process.stdout_fd();
                if ctx.epoll.add(stdout_fd, epoll::READABLE).is_ok() {
                    ctx.pipes.insert(stdout_fd, self.fd);
                }
                self.state = State::RunningCgi(process);
            }
            Err(_) => {
                let resp = crate::error_page::build(Some(server), 500);
                self.begin_write(resp, self.pending_keep_alive, self.pending_for_head, ctx.epoll);
            }
        }
    }

    fn cgi_write_once(&mut self, ctx: &mut EventCtx, fd: RawFd) {
        let State::RunningCgi(cgi) = &mut self.state else { return };
        match cgi.write_stdin_once() {
            WriteOutcome::Pending => {}
            WriteOutcome::Done | WriteOutcome::Error => {
                ctx.epoll.remove(fd);
                ctx.pipes.remove(&fd);
            }
        }
    }

    fn cgi_read_once(&mut self, config: &Config, ctx: &mut EventCtx, fd: RawFd) {
        let State::RunningCgi(cgi) = &mut self.state else { return };
        match cgi.read_stdout_once() {
            ReadOutcome::Pending => {}
            ReadOutcome::Eof | ReadOutcome::Error => {
                ctx.epoll.remove(fd);
                ctx.pipes.remove(&fd);
                self.finish_cgi(config, ctx);
            }
        }
    }

    fn finish_cgi(&mut self, _config: &Config, ctx: &mut EventCtx) {
        let state = std::mem::replace(&mut self.state, State::ReadingHead);
        let State::RunningCgi(mut cgi) = state else {
            self.state = state;
            return;
        };

        deregister_cgi(&mut cgi, ctx);

        let response = if cgi.output.is_empty() {
            crate::error_page::build(None, 502)
        } else {
            cgi::parse_output(&cgi.output)
        };

        if !cgi.try_reap() {
            ctx.reap.push(cgi.into_child());
        }

        self.begin_write(response, self.pending_keep_alive, self.pending_for_head, ctx.epoll);
    }

    fn begin_write(&mut self, response: Response, keep_alive: bool, for_head: bool, epoll: &Epoll) {
        let buf = response.serialize(keep_alive, for_head);
        self.state = State::Writing { buf, written: 0, close_after: !keep_alive };
        let _ = epoll.modify(self.fd, epoll::WRITABLE);
    }

    fn fail_body_error(&mut self, err: BodyError, req: Option<&Request>, config: &Config, ctx: &mut EventCtx) -> bool {
        let code = match err {
            BodyError::TooLarge => 413,
            BodyError::Malformed(_) => 400,
        };
        let server = config.resolve(self.listen, req.and_then(|r| r.header("host")));
        let resp = crate::error_page::build(server, code);
        self.begin_write(resp, false, false, ctx.epoll);
        false
    }

    fn fail_parse_error(&mut self, err: ParseError, ctx: &mut EventCtx) -> bool {
        let code = match err {
            ParseError::UriTooLong => 414,
            ParseError::HeadTooLarge => 400,
            ParseError::VersionNotSupported => 505,
            ParseError::BadRequest(_) => 400,
        };
        let resp = crate::error_page::build(None, code);
        self.begin_write(resp, false, false, ctx.epoll);
        false
    }
}

fn deregister_cgi(cgi: &mut CgiProcess, ctx: &mut EventCtx) {
    if let Some(fd) = cgi.stdin_fd() {
        ctx.epoll.remove(fd);
        ctx.pipes.remove(&fd);
    }
    let stdout_fd = cgi.stdout_fd();
    ctx.epoll.remove(stdout_fd);
    ctx.pipes.remove(&stdout_fd);
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
