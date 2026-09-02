# localhost

A single-threaded, single-process HTTP/1.1 server written from scratch in
Rust, built around one `epoll` event loop. Serves static files, runs CGI
scripts, handles file uploads, and tracks sessions with cookies — using
only `std` and `libc` for the raw syscalls Rust doesn't wrap natively.
No `tokio`, no `nix`, no async runtime of any kind.

## Quick start

```sh
cargo run --release -- config/default.conf
```

Then visit `http://127.0.0.1:8080/`. `www/index.html` links to every
feature demo (autoindex, uploads, the CGI script, a redirect, a custom
404 page). To see the second virtual host:

```sh
curl --resolve second.localhost:8080:127.0.0.1 http://second.localhost:8080/
```

More configs under [`config/examples/`](config/examples/) each isolate one
scenario (single server, multiple ports, a duplicate-port config that's
supposed to fail, a tiny `client_max_body_size`) — see the comment at the
top of each file.

## How it works

One process, one thread, one `epoll` instance. `server::run` in
[`src/server/mod.rs`](src/server/mod.rs:36) binds every listener, then
loops on a single `epoll_wait` ([`src/server/epoll.rs`](src/server/epoll.rs:63))
that reports readiness for *everything* at once — every client socket,
every listening socket, every CGI child's pipes. Each fd gets exactly one
`read()`/`write()`/`accept()` per readiness notification
([`Connection::on_event`](src/server/connection.rs:74)); a connection's
epoll interest is switched between `EPOLLIN`/`EPOLLOUT` with `epoll_ctl(MOD)`
as it moves between reading a request and writing a response, so it's
never registered for both at once.

A request goes through:

1. **Parse the head** — [`src/http/request.rs`](src/http/request.rs) reads
   off a growing buffer incrementally, so a request split across many
   `read()`s (or several pipelined requests in one buffer) is handled
   without blocking.
2. **Read the body** — [`src/http/body.rs`](src/http/body.rs) decodes
   `Content-Length` or `chunked` framing the same way, enforcing
   `client_max_body_size` as bytes arrive rather than only up front.
3. **Route** — [`src/router.rs`](src/router.rs) matches the longest route
   prefix, checks the method, and resolves to either a `Response` (static
   file, redirect, directory listing, upload, delete — everything that's
   local disk I/O and doesn't need epoll) or a `Dispatch::Cgi`.
4. **CGI, if matched** — [`src/cgi.rs`](src/cgi.rs) forks the interpreter,
   registers its pipes with the *same* epoll instance, and the connection
   drives them exactly like a client socket until the child's stdout hits
   EOF.
5. **Write the response** — [`src/http/response.rs`](src/http/response.rs)
   owns `Content-Length`/`Connection` so a handler can't desync them from
   the real body or keep-alive state.

Sessions ([`src/session.rs`](src/session.rs)) and panic recovery
([`guard_panic`](src/server/mod.rs:97)) are cross-cutting — every request
gets a session resolved before routing, and every per-connection call is
wrapped so a bug handling one client can't take the process down.

## Configuration file

Nginx-flavored, no regex support (routes match by longest path prefix).
`#` to end-of-line is a comment. See
[`config/default.conf`](config/default.conf) for a fully worked example.

```
server {
    listen 127.0.0.1:8080;       # host:port, or just a port (binds 0.0.0.0)
    listen 127.0.0.1:8081;       # a server can listen on several
    server_name example.com;     # matched against the Host header; the
                                  # first server with no server_name for a
                                  # given listen is the default
    root ./www;
    index index.html;
    client_max_body_size 5m;     # bytes, or a k/m/g suffix

    error_page 404 /error_pages/404.html;
    error_page 500 502 /error_pages/5xx.html;   # one path, several codes

    route /uploads {
        methods GET POST DELETE; # default is GET, HEAD if omitted
        root ./www/uploads;      # overrides the server root for this route
        upload_store ./www/uploads;  # where POST bodies get saved (defaults to root)
        autoindex on;             # directory listing when there's no index file
    }

    route /old {
        return 301 /new;         # redirect; nothing else in the block matters
    }

    route /cgi-bin {
        methods GET POST;
        root ./www/cgi-bin;
        cgi .py python3;         # extension -> interpreter
    }
}
```

Two server blocks may share a `listen` only if something disambiguates
them (`server_name`) — see
[`src/config/validate.rs`](src/config/validate.rs:15) and
[`config/examples/duplicate_port.conf`](config/examples/duplicate_port.conf)
for the rejected case. A `listen` that fails to `bind()` at runtime (port
already held by something else) is logged and skipped rather than
aborting the other, valid listeners —
[`src/server/listener.rs`](src/server/listener.rs:13).

## Testing

```sh
cargo test                 # unit tests (parser, HTTP parsing, routing, uploads, sessions, epoll...)
tests/integration.sh       # end-to-end: real server instances + curl, the full audit checklist
tests/stress.sh [duration] [concurrency]   # siege -b, or a fallback load generator if siege isn't installed
```

`tests/integration.sh` starts and tears down six different server
instances in sequence (to avoid port clashes) and asserts against real
HTTP responses — virtual hosting, methods and status codes, a byte-for-byte
diff on a multipart upload round-trip, a garbage request line followed by
proof the server still answers normally, session-cookie issuance vs.
reuse, and CGI over both chunked and unchunked POST bodies.

## Audit crib sheet

Answers to the functional-review questions, each pointing at the actual
code rather than just asserting it.

**How does an HTTP server work?** Accepts a TCP connection, reads bytes
until it has a full request (a start line, headers, optionally a body),
maps that request to a resource or handler, and writes back a status
line, headers, and a body. Everything else — routing, CGI, uploads — is
detail on top of that request/response cycle.

**Which function is used for I/O multiplexing, and how does it work?**
`epoll` via raw `libc` calls in [`src/server/epoll.rs`](src/server/epoll.rs)
— `epoll_create1` once at startup, `epoll_ctl` to register/modify/remove
interest in a fd's readability or writability, `epoll_wait` to block until
at least one registered fd is ready. It's level-triggered (the default),
so a fd with more data than one `read()` drained simply gets reported
ready again on the next `epoll_wait` — no need to loop until `EWOULDBLOCK`.

**Is the server using only one `epoll_wait` to read requests and write
answers?** Yes — [`server::run`](src/server/mod.rs:36) has exactly one
`epoll.wait(...)` call, inside the single `loop`, and every fd (listeners,
every client socket, every CGI pipe) is registered on the same `Epoll`
instance created once at the top of `run`.

**Why does that matter, and how was it achieved?** One multiplexer means
one thread can genuinely serve every connection without blocking on any
single one — the alternative (a `select`/`epoll_wait` per connection, or
one thread per connection) either burns a thread per client or requires
locking shared state across threads. It's achieved structurally: there is
only one `Epoll` value in the whole program (`server::run`'s local
variable), and every listener/client/CGI-pipe fd is registered onto it —
there's no code path that could construct a second one.

**Is there only one read or write per client per `epoll_wait`?** Yes.
[`Connection::on_event`](src/server/connection.rs:74) is called once per
fd per event batch, and for the client-socket branch calls at most one of
`read_once`/`write_once` — never both, since a connection is registered
for exactly one of `EPOLLIN`/`EPOLLOUT` at a time
([`begin_write`](src/server/connection.rs) switches it with
`epoll_ctl(MOD)` when moving from reading to writing and back).
[`read_once`](src/server/connection.rs:156) and
[`write_once`](src/server/connection.rs:166) each make exactly one
`stream.read()`/`stream.write()` syscall.

**Are I/O return values checked, and is the client removed on error?**
Every `read`/`write`/`accept` result is matched explicitly — `Ok(0)`
(peer closed), `Ok(n)`, `WouldBlock` (not an error, just "nothing to do
yet"), or any other `Err` (closes the connection). See the `match` in
`read_once`/`write_once` in
[`src/server/connection.rs`](src/server/connection.rs:156). A `true`
return from `on_event` tells `server::run` to `epoll_ctl(DEL)` the fd and
drop the `Connection` (and, if it owned a CGI child, kill and reap it —
[`Connection::cleanup`](src/server/connection.rs:148)).

**Is reading/writing always done through epoll?** Yes for every socket
and pipe fd in the program — client sockets, listeners, CGI stdin/stdout.
Local disk reads for static files are the one exception, and
deliberately so: a regular file is always "ready" as far as epoll is
concerned (it isn't something a network peer can leave half-written), so
registering it with epoll would be theater, not correctness — this
matches how reference implementations like nginx treat disk I/O too.

**What if an error is returned on a socket — is the client removed?**
Yes, see the `read_once`/`write_once`/`on_event` answer above:
[`if event.error`](src/server/connection.rs:80) (EPOLLERR/EPOLLHUP on the
*client* socket) and any I/O `Err` other than `WouldBlock` both return
`true`, which removes the connection.

**Does it manage GET, POST, DELETE?** Yes —
[`src/router.rs`](src/router.rs) branches on method after resolving the
route: GET/HEAD serve a file or directory listing, POST either runs a
matched CGI script or saves an upload
([`src/upload.rs`](src/upload.rs)), DELETE removes the named file. A
method not in a route's `methods` list gets `405` with a correct `Allow`
header.

**File uploads?** [`upload::save`](src/upload.rs) handles both a
browser's `multipart/form-data` (parses the boundary, extracts each
part's filename and bytes) and a plain `curl --data-binary` upload.

**Sessions and cookies?** [`src/session.rs`](src/session.rs) — every
request gets a session resolved via
[`SessionStore::touch`](src/session.rs:41) before routing even happens; a
new session gets a `Set-Cookie` queued and attached to whichever response
ends up being written, static or CGI.

**Never crashes?** [`guard_panic`](src/server/mod.rs:97) wraps every
per-connection call in `catch_unwind`, so a bug handling one client drops
that connection instead of the process — verified live by temporarily
injecting a panic into the read path and confirming the server kept
accepting new connections afterward (see the commit history). `SIGPIPE`
is explicitly ignored at startup so writing to an already-closed socket
surfaces as a normal `EPIPE` `Err` instead of a signal.

## What's deliberately out of scope

- **Regex routes** — the spec says routes won't need them; matching is
  longest-prefix.
- **HTTPS/TLS** — not asked for; would mean either hand-rolling a TLS
  stack or reaching for a crate the "no server-feature crates" rule would
  make questionable either way.
- **More than one CGI interpreter** — the spec asks for one; the config
  format supports mapping any extension to any interpreter, so adding a
  second is a config-only change, not a code change.
