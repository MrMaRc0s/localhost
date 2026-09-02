#!/usr/bin/env python3
"""Demo CGI script for the `localhost` server.

Reads the CGI/1.1 meta-variables the server sets in the environment,
reads exactly CONTENT_LENGTH bytes of body from stdin (the server has
already closed stdin once that's all of it — chunked or not, by this
point it's just bytes), and prints a CGI response: a header block,
a blank line, then the body.
"""
import html
import os
import sys

method = os.environ.get("REQUEST_METHOD", "GET")
query = os.environ.get("QUERY_STRING", "")
path_info = os.environ.get("PATH_INFO", "")
session_id = os.environ.get("SESSION_ID", "(none)")
session_visits = os.environ.get("SESSION_VISITS", "0")
remote_addr = os.environ.get("REMOTE_ADDR", "?")
content_length = int(os.environ.get("CONTENT_LENGTH", "0") or "0")

body = sys.stdin.buffer.read(content_length) if content_length > 0 else b""
try:
    body_text = body.decode("utf-8")
except UnicodeDecodeError:
    body_text = repr(body)

http_headers = sorted(
    (k[5:].replace("_", "-").title(), v)
    for k, v in os.environ.items()
    if k.startswith("HTTP_")
)

rows = "\n".join(f"<tr><th>{html.escape(k)}</th><td>{html.escape(v)}</td></tr>" for k, v in http_headers)

print("Content-Type: text/html; charset=utf-8\r")
print("\r")
print(f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>CGI demo</title>
<style>
  body {{ font-family: -apple-system, sans-serif; background: #14161a; color: #e8e8e8; padding: 2rem; }}
  h1 {{ color: #6cb2ff; }}
  table {{ border-collapse: collapse; margin: 1rem 0; }}
  th, td {{ text-align: left; padding: 0.3rem 0.8rem 0.3rem 0; border-bottom: 1px solid #2a2d33; }}
  th {{ color: #9aa0a6; font-weight: 500; }}
  code {{ background: #1c1f26; padding: 0.15rem 0.4rem; border-radius: 4px; }}
  a {{ color: #6cb2ff; }}
</style>
</head>
<body>
<h1>Hello from Python CGI</h1>
<table>
  <tr><th>REQUEST_METHOD</th><td>{html.escape(method)}</td></tr>
  <tr><th>QUERY_STRING</th><td>{html.escape(query)}</td></tr>
  <tr><th>PATH_INFO</th><td>{html.escape(path_info)}</td></tr>
  <tr><th>REMOTE_ADDR</th><td>{html.escape(remote_addr)}</td></tr>
  <tr><th>SESSION_ID</th><td><code>{html.escape(session_id)}</code></td></tr>
  <tr><th>SESSION_VISITS</th><td>{html.escape(session_visits)} (server-tracked, cookie-based)</td></tr>
  <tr><th>CONTENT_LENGTH</th><td>{content_length}</td></tr>
  {rows}
</table>
<p>Body received ({len(body)} bytes): <code>{html.escape(body_text)}</code></p>
<p><a href="/">&larr; back</a> &middot; reload this page to watch SESSION_VISITS climb.</p>
</body>
</html>""")
