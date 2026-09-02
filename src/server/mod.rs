pub mod connection;
pub mod epoll;
pub mod listener;

use std::collections::HashMap;
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, RawFd};
use std::process::Child;
use std::time::Instant;

use crate::config::{Config, Listen};
use crate::session::SessionStore;
use connection::Connection;
use epoll::Epoll;

const MAX_EVENTS: usize = 1024;
/// Also doubles as how often we sweep for timed-out connections and
/// reap finished CGI children, since those are the only other things the
/// loop needs to do besides react to I/O.
const POLL_TIMEOUT_MS: i32 = 1000;

/// Everything a `Connection` needs to touch outside of itself while
/// handling one event: the shared epoll instance, the registry mapping a
/// CGI pipe fd back to the client fd that owns it (so the main loop can
/// route a pipe's readiness event to the right `Connection`), and the
/// list of CGI children that have been killed/finished but not yet
/// reaped. Bundled into one struct purely so connection.rs doesn't have
/// to thread three separate `&mut` parameters through every method.
pub struct EventCtx<'a> {
    pub epoll: &'a Epoll,
    pub pipes: &'a mut HashMap<RawFd, RawFd>,
    pub reap: &'a mut Vec<Child>,
    pub sessions: &'a mut SessionStore,
}

pub fn run(config: Config) -> std::io::Result<()> {
    // We check every syscall's return value ourselves; a SIGPIPE from
    // writing to a socket the client already closed should surface as an
    // EPIPE error there, not take the process down (std leaves this
    // ignored by default, but we make it explicit since we also touch
    // raw fds directly via libc elsewhere).
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let epoll = Epoll::new()?;
    let listeners = listener::bind_all(&config, &epoll)?;
    let mut connections: HashMap<RawFd, Connection> = HashMap::new();
    let mut pipes: HashMap<RawFd, RawFd> = HashMap::new();
    let mut reap: Vec<Child> = Vec::new();
    let mut sessions = SessionStore::new();

    loop {
        let events = epoll.wait(MAX_EVENTS, POLL_TIMEOUT_MS)?;

        for ev in &events {
            if let Some((listen, tcp)) = listeners.get(&ev.fd) {
                accept_one(*listen, tcp, &epoll, &mut connections);
                continue;
            }

            // A CGI pipe fd resolves to the client fd that owns it; a
            // plain client-socket event resolves to itself.
            let owner_fd = pipes.get(&ev.fd).copied().unwrap_or(ev.fd);

            let should_close = {
                let mut ctx = EventCtx { epoll: &epoll, pipes: &mut pipes, reap: &mut reap, sessions: &mut sessions };
                match connections.get_mut(&owner_fd) {
                    Some(conn) => guard_panic(owner_fd, || conn.on_event(&config, &mut ctx, ev)),
                    None => continue, // stale event for a connection we already dropped
                }
            };

            if should_close {
                if let Some(mut conn) = connections.remove(&owner_fd) {
                    let mut ctx = EventCtx { epoll: &epoll, pipes: &mut pipes, reap: &mut reap, sessions: &mut sessions };
                    conn.cleanup(&mut ctx);
                }
                epoll.remove(owner_fd);
            }
        }

        reap.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
        sessions.sweep_expired();
        sweep_timeouts(&config, &epoll, &mut pipes, &mut reap, &mut sessions, &mut connections);
    }
}

/// Runs `f` (which handles exactly one connection) behind
/// `catch_unwind`, so a bug that panics while serving one client — a
/// malformed request tripping an unhandled edge case, say — takes down
/// only that one connection instead of the whole process. `AssertUnwindSafe`
/// is sound here specifically because the caller always discards the
/// connection (closes the fd, drops it from the map) whenever this
/// returns via the panic path, so a partially-mutated `Connection` is
/// never touched again.
fn guard_panic<F: FnOnce() -> bool>(fd: RawFd, f: F) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(should_close) => should_close,
        Err(_) => {
            eprintln!("localhost: recovered from an internal error handling fd {} — closing that connection", fd);
            true
        }
    }
}

fn accept_one(listen: Listen, tcp: &TcpListener, epoll: &Epoll, connections: &mut HashMap<RawFd, Connection>) {
    // One accept() per readiness notification, same discipline as every
    // other fd: if more connections are still queued, the listener stays
    // readable and epoll_wait reports it again on the next loop tick.
    match tcp.accept() {
        Ok((stream, _peer_addr)) => {
            if let Err(e) = stream.set_nonblocking(true) {
                eprintln!("localhost: could not set accepted socket non-blocking: {}", e);
                return;
            }
            let fd = stream.as_raw_fd();
            if let Err(e) = epoll.add(fd, epoll::READABLE) {
                eprintln!("localhost: could not register accepted socket with epoll: {}", e);
                return;
            }
            connections.insert(fd, Connection::new(fd, stream, listen));
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(e) => eprintln!("localhost: accept() failed: {}", e),
    }
}

fn sweep_timeouts(
    config: &Config,
    epoll: &Epoll,
    pipes: &mut HashMap<RawFd, RawFd>,
    reap: &mut Vec<Child>,
    sessions: &mut SessionStore,
    connections: &mut HashMap<RawFd, Connection>,
) {
    let now = Instant::now();
    let timed_out: Vec<RawFd> = connections.iter().filter(|(_, c)| c.is_timed_out(now)).map(|(fd, _)| *fd).collect();

    for fd in timed_out {
        let finished = {
            let mut ctx = EventCtx { epoll, pipes, reap, sessions };
            match connections.get_mut(&fd) {
                Some(conn) => guard_panic(fd, || conn.handle_timeout(config, &mut ctx)),
                None => true,
            }
        };
        if finished {
            if let Some(mut conn) = connections.remove(&fd) {
                let mut ctx = EventCtx { epoll, pipes, reap, sessions };
                conn.cleanup(&mut ctx);
            }
            epoll.remove(fd);
        }
    }
}
