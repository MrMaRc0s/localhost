#!/usr/bin/env bash
# End-to-end audit checks against real server processes. The web roots and
# configs are copied to a temporary directory, so uploads and permission tests
# never modify the checkout.
#
# Usage:
#   tests/integration.sh
#   PORT_BASE=19000 tests/integration.sh   # when the default ports are busy

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
REPO_ROOT="$(pwd)"

PORT_MAIN="${PORT_BASE:-8080}"
if ! [[ "$PORT_MAIN" =~ ^[0-9]+$ ]] || [ "$PORT_MAIN" -lt 1 ] || [ "$PORT_MAIN" -gt 65533 ]; then
    echo "PORT_BASE must be an integer between 1 and 65533"
    exit 2
fi
PORT_SECOND=$((PORT_MAIN + 1))
PORT_THIRD=$((PORT_MAIN + 2))
BASE_URL="http://127.0.0.1:${PORT_MAIN}"

BIN="$REPO_ROOT/target/release/localhost"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/localhost-integration.XXXXXX")"
SERVER_LOG="$TEST_ROOT/server.log"
PASS=0
FAIL=0
SERVER_PID=""
BLOCKER_PID=""

cp -a "$REPO_ROOT/config" "$REPO_ROOT/www" "$REPO_ROOT/www-second" "$TEST_ROOT/"
while IFS= read -r config_file; do
    sed -i \
        -e "s/8082/${PORT_THIRD}/g" \
        -e "s/8081/${PORT_SECOND}/g" \
        -e "s/8080/${PORT_MAIN}/g" \
        "$config_file"
done < <(printf '%s\n' "$TEST_ROOT"/config/*.conf "$TEST_ROOT"/config/examples/*.conf)

log_pass() { PASS=$((PASS + 1)); printf '  \033[32mPASS\033[0m  %s\n' "$1"; }
log_fail() { FAIL=$((FAIL + 1)); printf '  \033[31mFAIL\033[0m  %s\n' "$1"; }
section() { printf '\n\033[1m%s\033[0m\n' "$1"; }
http_curl() { curl --noproxy '*' --max-time 5 "$@"; }

assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        log_pass "$desc (got $actual)"
    else
        log_fail "$desc (expected $expected, got $actual)"
    fi
}

assert_contains() {
    local desc="$1" haystack="$2" needle="$3"
    if printf '%s' "$haystack" | grep -qF "$needle"; then
        log_pass "$desc"
    else
        log_fail "$desc (did not find \"$needle\")"
    fi
}

start_server() {
    local config="$1" health_port="${2:-$PORT_MAIN}"
    (
        cd "$TEST_ROOT" || exit 1
        exec "$BIN" "$config"
    ) >"$SERVER_LOG" 2>&1 &
    SERVER_PID=$!

    local ready=false
    for _ in $(seq 1 20); do
        if ! kill -0 "$SERVER_PID" 2>/dev/null; then
            break
        fi
        if http_curl --max-time 1 -sS -o /dev/null "http://127.0.0.1:${health_port}/" 2>/dev/null; then
            ready=true
            break
        fi
        sleep 0.1
    done
    if [ "$ready" != true ]; then
        echo "server failed to start with $config:"
        cat "$SERVER_LOG"
        exit 1
    fi
}

stop_server() {
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null
        wait "$SERVER_PID" 2>/dev/null
    fi
    SERVER_PID=""
}

stop_blocker() {
    if [ -n "$BLOCKER_PID" ] && kill -0 "$BLOCKER_PID" 2>/dev/null; then
        kill "$BLOCKER_PID" 2>/dev/null
        wait "$BLOCKER_PID" 2>/dev/null
    fi
    BLOCKER_PID=""
}

cleanup() {
    stop_server
    stop_blocker
    rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT

code() { http_curl -sS -o /dev/null -w '%{http_code}' "$@"; }
raw_code() { python3 "$REPO_ROOT/tests/raw_http.py" 127.0.0.1 "$PORT_MAIN"; }

echo "Building release binary..."
(cd "$REPO_ROOT" && cargo build --release --quiet) || { echo "build failed"; exit 1; }

echo "Running unit tests..."
(cd "$REPO_ROOT" && cargo test --release --bin localhost --quiet) || { echo "unit tests failed"; exit 1; }

section "Configuration: single server, single port"
start_server config/examples/single_server.conf
assert_eq "GET / on single-server config" 200 "$(code "$BASE_URL/")"
stop_server

section "Configuration: multiple servers on different ports"
start_server config/examples/multi_port.conf
assert_eq "GET / on :$PORT_MAIN" 200 "$(code "$BASE_URL/")"
assert_eq "GET / on :$PORT_THIRD" 200 "$(code "http://127.0.0.1:${PORT_THIRD}/")"
stop_server

section "Runtime bind failure does not disable a valid listener"
python3 - "$PORT_MAIN" <<'PYEOF' >"$TEST_ROOT/blocker.log" 2>&1 &
import socket
import sys
import time

with socket.socket() as blocker:
    blocker.bind(("127.0.0.1", int(sys.argv[1])))
    blocker.listen()
    time.sleep(30)
PYEOF
BLOCKER_PID=$!
sleep 0.2
if ! kill -0 "$BLOCKER_PID" 2>/dev/null; then
    echo "could not reserve the test port"
    cat "$TEST_ROOT/blocker.log"
    exit 1
fi
start_server config/examples/multi_port.conf "$PORT_THIRD"
assert_eq "second listener still serves after first bind fails" 200 "$(code "http://127.0.0.1:${PORT_THIRD}/")"
assert_contains "failed listener is logged" "$(cat "$SERVER_LOG")" "could not bind"
stop_server
stop_blocker

section "Configuration conflicts are rejected"
DUP_OUT=$(cd "$TEST_ROOT" && "$BIN" config/examples/duplicate_port.conf 2>&1)
DUP_STATUS=$?
assert_eq "duplicate default listen exits unsuccessfully" 1 "$DUP_STATUS"
assert_contains "startup reports a configuration error" "$DUP_OUT" "configuration error"

section "Configuration: client body limit"
start_server config/examples/tiny_body_limit.conf
assert_eq "short body accepted" 201 "$(code -X POST --data 'short' "$BASE_URL/uploads/a.txt")"
assert_eq "long body rejected with 413" 413 "$(code -X POST --data 'this body is definitely longer than sixteen bytes' "$BASE_URL/uploads/b.txt")"
stop_server

section "CGI startup failure produces custom 500 without a crash"
cp "$TEST_ROOT/config/default.conf" "$TEST_ROOT/config/bad_cgi.conf"
sed -i 's/cgi \.py python3;/cgi .py \/definitely\/missing\/python;/' "$TEST_ROOT/config/bad_cgi.conf"
start_server config/bad_cgi.conf
assert_eq "missing CGI interpreter returns 500" 500 "$(code "$BASE_URL/cgi-bin/hello.py")"
assert_contains "custom 500 page is served" "$(http_curl -sS "$BASE_URL/cgi-bin/hello.py")" "custom error page"
assert_eq "server remains available after CGI spawn failure" 200 "$(code "$BASE_URL/")"
stop_server

start_server config/default.conf

section "Virtual hosts and multiple listening ports"
assert_contains "default vhost serves www" "$(http_curl -sS "$BASE_URL/")" "localhost"
assert_contains "Host selects second virtual server" "$(http_curl -sS --resolve "second.localhost:${PORT_MAIN}:127.0.0.1" "http://second.localhost:${PORT_MAIN}/")" "second.localhost"
assert_eq "same server answers on second port" 200 "$(code "http://127.0.0.1:${PORT_SECOND}/")"

section "Routes, indexes, listings, redirects and error pages"
assert_eq "directory root serves its index" 200 "$(code "$BASE_URL/")"
assert_eq "autoindex enabled route" 200 "$(code "$BASE_URL/files/")"
assert_contains "autoindex lists sample file" "$(http_curl -sS "$BASE_URL/files/")" "notes.txt"
assert_eq "directory listing disabled route" 403 "$(code "$BASE_URL/error_pages/")"
assert_contains "custom 403 page is served" "$(http_curl -sS "$BASE_URL/error_pages/")" "custom error page"
assert_eq "GET redirect" 301 "$(code "$BASE_URL/old-page")"
assert_eq "missing resource" 404 "$(code "$BASE_URL/nope")"
assert_contains "custom 404 page is served" "$(http_curl -sS "$BASE_URL/nope")" "custom error page"

section "Methods and response headers"
HEADERS=$(http_curl -sSI "$BASE_URL/index.html")
EXPECTED_LENGTH=$(wc -c <"$TEST_ROOT/www/index.html")
HEAD_LENGTH=$(printf '%s' "$HEADERS" | awk 'BEGIN { IGNORECASE=1 } /^Content-Length:/ { gsub("\r", "", $2); print $2 }')
assert_eq "HEAD static file" 200 "$(code -I "$BASE_URL/index.html")"
assert_eq "HEAD reports GET Content-Length" "$EXPECTED_LENGTH" "$HEAD_LENGTH"
assert_contains "response identifies the server" "$HEADERS" "Server: localhost/0.1"
assert_eq "POST cannot bypass redirect method list" 405 "$(code -X POST "$BASE_URL/old-page")"
REDIRECT_METHOD_HEADERS=$(http_curl -sS -D - -o /dev/null -X POST "$BASE_URL/old-page")
assert_contains "405 carries Allow header" "$REDIRECT_METHOD_HEADERS" "Allow: GET, HEAD"
assert_eq "DELETE where not permitted" 405 "$(code -X DELETE "$BASE_URL/index.html")"

section "POST upload, GET round-trip and DELETE"
assert_eq "POST creates a resource" 201 "$(code -X POST --data 'hello' "$BASE_URL/uploads/roundtrip.txt")"
assert_eq "GET uploaded resource" 200 "$(code "$BASE_URL/uploads/roundtrip.txt")"
assert_eq "uploaded content is intact" hello "$(http_curl -sS "$BASE_URL/uploads/roundtrip.txt")"
assert_eq "DELETE uploaded resource" 204 "$(code -X DELETE "$BASE_URL/uploads/roundtrip.txt")"
assert_eq "GET after DELETE" 404 "$(code "$BASE_URL/uploads/roundtrip.txt")"

section "Multipart upload integrity and malformed body handling"
TMP_UPLOAD="$TEST_ROOT/upload-source.bin"
head -c 4096 /dev/urandom >"$TMP_UPLOAD"
assert_eq "browser-style multipart upload" 201 "$(code -F "file=@${TMP_UPLOAD};filename=binary.bin" "$BASE_URL/uploads/")"
http_curl -sS "$BASE_URL/uploads/binary.bin" -o "$TEST_ROOT/upload-roundtrip.bin"
if cmp -s "$TMP_UPLOAD" "$TEST_ROOT/upload-roundtrip.bin"; then
    log_pass "multipart upload round-trips byte-identical"
else
    log_fail "multipart upload round-trips byte-identical"
fi
assert_eq "DELETE multipart upload" 204 "$(code -X DELETE "$BASE_URL/uploads/binary.bin")"
BAD_MULTIPART=$'--unfinished\r\nContent-Disposition: form-data; name="file"; filename="partial.txt"\r\n\r\ndata'
assert_eq "missing multipart terminator" 400 "$(code -H 'Content-Type: multipart/form-data; boundary=unfinished' --data-binary "$BAD_MULTIPART" "$BASE_URL/uploads/")"
assert_eq "malformed multipart creates no partial file" 404 "$(code "$BASE_URL/uploads/partial.txt")"

section "Malformed HTTP framing never takes down the server"
assert_eq "garbage request line" 400 "$(printf 'GARBAGE\r\n\r\n' | raw_code)"
assert_eq "HTTP/1.1 missing Host" 400 "$(printf 'GET / HTTP/1.1\r\nConnection: close\r\n\r\n' | raw_code)"
assert_eq "duplicate Host" 400 "$(printf 'GET / HTTP/1.1\r\nHost: one\r\nHost: two\r\nConnection: close\r\n\r\n' | raw_code)"
assert_eq "duplicate Content-Length" 400 "$(printf 'POST /uploads/a HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n' | raw_code)"
assert_eq "Content-Length with chunked coding" 400 "$(printf 'POST /uploads/a HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n' | raw_code)"
assert_eq "unsupported transfer coding" 501 "$(printf 'POST /uploads/a HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: gzip\r\nConnection: close\r\n\r\n' | raw_code)"
OVERSIZED_CODE=$({ printf 'GET / HTTP/1.1\r\nHost: localhost\r\nX-Fill: '; head -c 17000 /dev/zero | tr '\0' a; printf '\r\n\r\n'; } | raw_code)
assert_eq "oversized request headers" 431 "$OVERSIZED_CODE"
assert_eq "server still answers after malformed traffic" 200 "$(code "$BASE_URL/")"

section "Filesystem root confinement"
cp "$REPO_ROOT/README.md" "$TEST_ROOT/outside.txt"
cp "$TEST_ROOT/www/cgi-bin/hello.py" "$TEST_ROOT/outside.py"
ln -s "$TEST_ROOT/outside.txt" "$TEST_ROOT/www/files/escape.txt"
ln -s "$TEST_ROOT/outside.txt" "$TEST_ROOT/www/uploads/escape.txt"
ln -s "$TEST_ROOT/outside.py" "$TEST_ROOT/www/cgi-bin/escape.py"
OUTSIDE_BEFORE=$(sha256sum "$TEST_ROOT/outside.txt" | awk '{print $1}')
assert_eq "GET cannot follow escaping symlink" 403 "$(code "$BASE_URL/files/escape.txt")"
assert_eq "POST cannot overwrite through symlink" 403 "$(code -X POST --data replacement "$BASE_URL/uploads/escape.txt")"
assert_eq "DELETE cannot follow escaping symlink" 403 "$(code -X DELETE "$BASE_URL/uploads/escape.txt")"
assert_eq "CGI cannot execute escaping symlink" 403 "$(code "$BASE_URL/cgi-bin/escape.py/path")"
OUTSIDE_AFTER=$(sha256sum "$TEST_ROOT/outside.txt" | awk '{print $1}')
assert_eq "outside file remains unchanged" "$OUTSIDE_BEFORE" "$OUTSIDE_AFTER"

section "Sessions and cookies"
JAR="$TEST_ROOT/cookies.txt"
FIRST_HEADERS=$(http_curl -sS -i -c "$JAR" "$BASE_URL/cgi-bin/hello.py")
SECOND_BODY=$(http_curl -sS -b "$JAR" -c "$JAR" "$BASE_URL/cgi-bin/hello.py")
assert_contains "first visit sets session cookie" "$FIRST_HEADERS" "Set-Cookie: localhost_session="
assert_contains "session cookie is HttpOnly" "$FIRST_HEADERS" "HttpOnly"
assert_contains "session cookie uses SameSite=Lax" "$FIRST_HEADERS" "SameSite=Lax"
assert_contains "replayed cookie advances server-side state" "$SECOND_BODY" "SESSION_VISITS</th><td>2"

section "CGI GET, PATH_INFO, chunked and unchunked POST"
assert_eq "CGI GET" 200 "$(code "$BASE_URL/cgi-bin/hello.py")"
assert_contains "CGI receives QUERY_STRING" "$(http_curl -sS "$BASE_URL/cgi-bin/hello.py?a=1")" "QUERY_STRING</th><td>a=1"
assert_contains "CGI receives PATH_INFO" "$(http_curl -sS "$BASE_URL/cgi-bin/hello.py/a/b")" "PATH_INFO</th><td>/a/b"
assert_contains "CGI receives Content-Length body" "$(http_curl -sS -X POST --data 'unchunked-body' "$BASE_URL/cgi-bin/hello.py")" "unchunked-body"
assert_contains "CGI receives decoded chunked body" "$(http_curl -sS -X POST -H 'Transfer-Encoding: chunked' --data 'chunked-body' "$BASE_URL/cgi-bin/hello.py")" "chunked-body"

stop_server

echo
echo "-----------------------------------------"
echo "  $PASS passed, $FAIL failed"
echo "-----------------------------------------"
[ "$FAIL" -eq 0 ]
