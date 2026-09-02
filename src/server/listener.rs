use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener};
use std::os::unix::io::{AsRawFd, RawFd};

use super::epoll::{Epoll, READABLE};
use crate::config::{Config, Listen};

/// Binds every distinct (host, port) the config needs. A single failed
/// `bind()` (port already in use by something else, address not
/// available, ...) is logged and skipped rather than aborting the whole
/// server — other, valid listeners must keep working. Only if *nothing*
/// could be bound do we give up, since there'd be nothing left to serve.
pub fn bind_all(config: &Config, epoll: &Epoll) -> std::io::Result<HashMap<RawFd, (Listen, TcpListener)>> {
    let mut listeners = HashMap::new();

    for listen in config.all_listens() {
        let addr = SocketAddr::new(listen.host, listen.port);
        match TcpListener::bind(addr) {
            Ok(tcp) => {
                if let Err(e) = tcp.set_nonblocking(true) {
                    eprintln!("localhost: failed to set {} non-blocking: {}", addr, e);
                    continue;
                }
                let fd = tcp.as_raw_fd();
                if let Err(e) = epoll.add(fd, READABLE) {
                    eprintln!("localhost: failed to register listener {} with epoll: {}", addr, e);
                    continue;
                }
                println!("localhost: listening on {}", addr);
                listeners.insert(fd, (listen, tcp));
            }
            Err(e) => {
                eprintln!("localhost: could not bind {}: {} (skipping this listener)", addr, e);
            }
        }
    }

    if listeners.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "no listener could be bound — check the config file and for conflicting processes",
        ));
    }

    Ok(listeners)
}
