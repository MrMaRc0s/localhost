//! Thin, safe-ish wrapper around Linux's `epoll(7)` — the I/O
//! multiplexing primitive this whole server is built around. `std` has no
//! epoll API, and we're not allowed to reach for `mio`/`tokio`/`nix`, so
//! this talks to the three syscalls directly through `libc`.
use std::io;
use std::os::unix::io::RawFd;

pub const READABLE: u32 = libc::EPOLLIN as u32;
pub const WRITABLE: u32 = libc::EPOLLOUT as u32;

pub struct Event {
    pub fd: RawFd,
    pub readable: bool,
    pub writable: bool,
    pub error: bool,
}

pub struct Epoll {
    fd: RawFd,
}

impl Epoll {
    pub fn new() -> io::Result<Self> {
        // epoll_create1(0) — the size hint epoll_create(2) took is ignored
        // by the kernel; epoll_create1 drops it and takes flags instead.
        let fd = unsafe { libc::epoll_create1(0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Epoll { fd })
    }

    pub fn add(&self, fd: RawFd, events: u32) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_ADD, fd, events)
    }

    pub fn modify(&self, fd: RawFd, events: u32) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_MOD, fd, events)
    }

    /// Deregisters `fd`. Safe to call even if `fd` was already closed or
    /// never added — both cases are reported by the kernel as `ENOENT`/`EBADF`
    /// and simply ignored, since the caller's intent ("stop watching this
    /// fd") is already satisfied.
    pub fn remove(&self, fd: RawFd) {
        let _ = unsafe { libc::epoll_ctl(self.fd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
    }

    fn ctl(&self, op: libc::c_int, fd: RawFd, events: u32) -> io::Result<()> {
        let mut ev = libc::epoll_event { events, u64: fd as u64 };
        let ret = unsafe { libc::epoll_ctl(self.fd, op, fd, &mut ev) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Blocks (this is the *only* blocking call in the whole event loop)
    /// until at least one registered fd is ready, or `timeout_ms`
    /// elapses. One call here services every fd — every client
    /// connection, every listening socket, every CGI pipe — in a single
    /// process, single thread.
    pub fn wait(&self, capacity: usize, timeout_ms: i32) -> io::Result<Vec<Event>> {
        let mut raw: Vec<libc::epoll_event> = vec![libc::epoll_event { events: 0, u64: 0 }; capacity];
        let n = unsafe { libc::epoll_wait(self.fd, raw.as_mut_ptr(), raw.len() as i32, timeout_ms) };
        if n < 0 {
            let err = io::Error::last_os_error();
            // A signal interrupting the wait isn't a real error — the
            // caller just loops back into epoll_wait.
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(Vec::new());
            }
            return Err(err);
        }
        let mut out = Vec::with_capacity(n as usize);
        for ev in raw.into_iter().take(n as usize) {
            out.push(Event {
                fd: ev.u64 as RawFd,
                readable: ev.events & (libc::EPOLLIN as u32) != 0,
                writable: ev.events & (libc::EPOLLOUT as u32) != 0,
                error: ev.events & (libc::EPOLLERR as u32 | libc::EPOLLHUP as u32) != 0,
            });
        }
        Ok(out)
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_readable_pipe() {
        let mut fds = [0 as RawFd; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (read_fd, write_fd) = (fds[0], fds[1]);

        let epoll = Epoll::new().unwrap();
        epoll.add(read_fd, READABLE).unwrap();

        // Nothing written yet: a short wait should time out with no events.
        let events = epoll.wait(8, 20).unwrap();
        assert!(events.is_empty());

        assert_eq!(unsafe { libc::write(write_fd, b"x".as_ptr() as *const _, 1) }, 1);

        let events = epoll.wait(8, 1000).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].fd, read_fd);
        assert!(events[0].readable);

        epoll.remove(read_fd);
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
    }
}
