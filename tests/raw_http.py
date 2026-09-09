#!/usr/bin/env python3
"""Send one raw HTTP request from stdin and print its response status code."""

import socket
import sys


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: raw_http.py HOST PORT", file=sys.stderr)
        return 2

    host = sys.argv[1]
    try:
        port = int(sys.argv[2])
    except ValueError:
        print(f"invalid port: {sys.argv[2]}", file=sys.stderr)
        return 2

    request = sys.stdin.buffer.read()
    try:
        with socket.create_connection((host, port), timeout=3) as client:
            client.sendall(request)
            client.shutdown(socket.SHUT_WR)
            response = b""
            while b"\n" not in response and len(response) < 4096:
                chunk = client.recv(4096)
                if not chunk:
                    break
                response += chunk
    except OSError as error:
        print(f"raw request failed: {error}", file=sys.stderr)
        return 2

    first_line = response.splitlines()[0].split() if response else []
    if len(first_line) < 2 or not first_line[0].startswith(b"HTTP/"):
        print("server returned no valid HTTP status line", file=sys.stderr)
        return 2

    try:
        print(int(first_line[1]))
    except ValueError:
        print("server returned a malformed status code", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
