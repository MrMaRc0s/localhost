pub mod connection;
pub mod epoll;
pub mod listener;

use std::collections::HashMap;
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::Instant;

use crate::config::{Config, Listen};
use connection::Connection;
use epoll::Epoll;

const MAX_EVENTS: usize = 1024;
/// Also doubles as how often we sweep for timed-out connections, since
/// that's the only other thing the loop needs to do besides react to I/O.
const POLL_TIMEOUT_MS: i32 = 1000;

pub fn run(config: Config) -> std::io::Result<()> {
    // We do our own error handling on every write()/read() return value;
    // a `SIGPIPE` from writing to a socket the client already closed
    // should surface as an `EPIPE` error there, not take the process
    // down (std normally leaves this ignored already, but we make it
    // explicit since we also touch raw fds directly via libc elsewhere).
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let epoll = Epoll::new()?;
    let listeners = listener::bind_all(&config, &epoll)?;
    let mut connections: HashMap<RawFd, Connection> = HashMap::new();

    loop {
        let events = epoll.wait(MAX_EVENTS, POLL_TIMEOUT_MS)?;

        for ev in &events {
            if let Some((listen, tcp)) = listeners.get(&ev.fd) {
                accept_one(*listen, tcp, &epoll, &mut connections);
                continue;
            }

            let should_close = match connections.get_mut(&ev.fd) {
                Some(conn) => conn.on_event(&config, &epoll, ev),
                None => continue, // stale event for an fd we already dropped
            };
            if should_close {
                epoll.remove(ev.fd);
                connections.remove(&ev.fd);
            }
        }

        sweep_timeouts(&config, &epoll, &mut connections);
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

fn sweep_timeouts(config: &Config, epoll: &Epoll, connections: &mut HashMap<RawFd, Connection>) {
    let now = Instant::now();
    let timed_out: Vec<RawFd> = connections.iter().filter(|(_, c)| c.is_timed_out(now)).map(|(fd, _)| *fd).collect();

    for fd in timed_out {
        let finished = match connections.get_mut(&fd) {
            Some(conn) => conn.handle_timeout(config, epoll),
            None => true,
        };
        if finished {
            epoll.remove(fd);
            connections.remove(&fd);
        }
    }
}
