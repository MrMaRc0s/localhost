# localhost

A single-threaded, single-process HTTP/1.1 server written from scratch in Rust,
built around one `epoll` event loop. No `tokio`, no `nix` — just `std` and
`libc` for the raw syscalls Rust doesn't expose natively.

Project write-up, configuration reference and audit notes live in
[`docs/`](docs/) (added as the implementation lands).

## Quick start

```sh
cargo run --release -- config/default.conf
```

Then visit `http://127.0.0.1:8080/`.

## Status

Work in progress — see commit history for the build-up from config parsing
through the event loop, static file serving, CGI, and sessions.
