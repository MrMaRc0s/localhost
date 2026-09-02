#!/usr/bin/env bash
#
# End-to-end integration tests for `localhost`, driven entirely with curl
# against real running instances of the server — no mocking. Mirrors the
# audit checklist section by section so each check here maps to a
# specific "does it work" question an auditor would ask.
#
# Usage: tests/integration.sh   (run from the repo root, or anywhere —
# it cd's to the repo root itself)

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO_ROOT="$(pwd)"

BIN="$REPO_ROOT/target/release/localhost"
PASS=0
FAIL=0
SERVER_PID=""

# ---------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------

log_pass() { PASS=$((PASS + 1)); printf '  \033[32mPASS\033[0m  %s\n' "$1"; }
log_fail() { FAIL=$((FAIL + 1)); printf '  \033[31mFAIL\033[0m  %s\n' "$1"; }
section()  { printf '\n\033[1m%s\033[0m\n' "$1"; }

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
    local config="$1"
    "$BIN" "$config" >"$REPO_ROOT/tests/.server.log" 2>&1 &
    SERVER_PID=$!
    sleep 0.4
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "server failed to start with $config:"
        cat "$REPO_ROOT/tests/.server.log"
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

trap stop_server EXIT

code() { curl -sS -o /dev/null -w '%{http_code}' "$@"; }

# ---------------------------------------------------------------------
# build
# ---------------------------------------------------------------------

echo "Building release binary..."
(cd "$REPO_ROOT" && cargo build --release --quiet) || { echo "build failed"; exit 1; }

echo "Running unit tests..."
(cd "$REPO_ROOT" && cargo test --release --bin localhost --quiet) || { echo "unit tests failed"; exit 1; }

# ---------------------------------------------------------------------
# configuration file scenarios
# ---------------------------------------------------------------------

section "Configuration: single server, single port"
start_server config/examples/single_server.conf
assert_eq "GET / on single-server config" 200 "$(code http://127.0.0.1:8080/)"
stop_server

section "Configuration: multiple servers, different ports"
start_server config/examples/multi_port.conf
assert_eq "GET / on :8080" 200 "$(code http://127.0.0.1:8080/)"
assert_eq "GET / on :8082" 200 "$(code http://127.0.0.1:8082/)"
stop_server

section "Configuration: duplicate port without server_name is rejected"
DUP_OUT=$("$BIN" config/examples/duplicate_port.conf 2>&1)
assert_contains "startup fails with a clear message" "$DUP_OUT" "configuration error"

section "Configuration: client_max_body_size (413)"
start_server config/examples/tiny_body_limit.conf
assert_eq "short body accepted" 201 "$(code -X POST --data "short" http://127.0.0.1:8080/uploads/a.txt)"
assert_eq "long body rejected with 413" 413 "$(code -X POST --data "this body is definitely longer than sixteen bytes" http://127.0.0.1:8080/uploads/b.txt)"
rm -f "$REPO_ROOT/www/uploads/a.txt" "$REPO_ROOT/www/uploads/b.txt"
stop_server

# ---------------------------------------------------------------------
# main default.conf: routes, vhosts, methods, cookies, CGI
# ---------------------------------------------------------------------

start_server config/default.conf

section "Configuration: multiple servers, different hostnames, same port"
assert_contains "default vhost serves www/index.html" "$(curl -sS http://127.0.0.1:8080/)" "localhost"
assert_contains "second.localhost serves a different site" "$(curl -sS --resolve second.localhost:8080:127.0.0.1 http://second.localhost:8080/)" "second.localhost"

section "Configuration: second listen port on the same server"
assert_eq "GET / on :8081" 200 "$(code http://127.0.0.1:8081/)"

section "Configuration: routes and custom error pages"
assert_eq "GET /files/ (autoindex on)" 200 "$(code http://127.0.0.1:8080/files/)"
assert_contains "autoindex lists sample files" "$(curl -sS http://127.0.0.1:8080/files/)" "notes.txt"
assert_eq "GET /old-page redirects (301)" 301 "$(code http://127.0.0.1:8080/old-page)"
assert_contains "custom 404 page is served, not the generic fallback" "$(curl -sS http://127.0.0.1:8080/nope)" "custom error page"

section "Methods: GET / POST / DELETE"
assert_eq "GET static file" 200 "$(code http://127.0.0.1:8080/index.html)"
assert_eq "POST upload creates a resource (201)" 201 "$(code -X POST --data "hello" http://127.0.0.1:8080/uploads/roundtrip.txt)"
assert_eq "GET the uploaded file back" 200 "$(code http://127.0.0.1:8080/uploads/roundtrip.txt)"
assert_contains "uploaded content is byte-for-byte intact" "$(curl -sS http://127.0.0.1:8080/uploads/roundtrip.txt)" "hello"
assert_eq "DELETE the uploaded file" 204 "$(code -X DELETE http://127.0.0.1:8080/uploads/roundtrip.txt)"
assert_eq "GET after DELETE is 404" 404 "$(code http://127.0.0.1:8080/uploads/roundtrip.txt)"
assert_eq "DELETE where not permitted is 405" 405 "$(code -X DELETE http://127.0.0.1:8080/index.html)"

section "Multipart file upload (browser-style) round-trips uncorrupted"
TMP_UPLOAD="$(mktemp)"
head -c 4096 /dev/urandom | base64 >"$TMP_UPLOAD"
curl -sS -F "file=@${TMP_UPLOAD};filename=binary.txt" http://127.0.0.1:8080/uploads/ >/dev/null
curl -sS http://127.0.0.1:8080/uploads/binary.txt -o "${TMP_UPLOAD}.roundtrip"
if diff -q "$TMP_UPLOAD" "${TMP_UPLOAD}.roundtrip" >/dev/null 2>&1; then
    log_pass "multipart upload round-trips byte-identical"
else
    log_fail "multipart upload round-trips byte-identical"
fi
curl -sS -X DELETE http://127.0.0.1:8080/uploads/binary.txt >/dev/null
rm -f "$TMP_UPLOAD" "${TMP_UPLOAD}.roundtrip"

section "Malformed / wrong requests never take the server down"
assert_eq "garbage request line -> 400" 400 "$(printf 'GARBAGE\r\n\r\n' | { exec 3<>/dev/tcp/127.0.0.1/8080; cat >&3; timeout 1 cat <&3; } | head -1 | grep -o '[0-9]\{3\}')"
assert_eq "server still answers normally afterwards" 200 "$(code http://127.0.0.1:8080/)"

section "Sessions and cookies"
JAR="$(mktemp)"
SET_COOKIE_1=$(curl -sS -i -c "$JAR" http://127.0.0.1:8080/ | grep -ic "set-cookie: localhost_session=")
SET_COOKIE_2=$(curl -sS -i -b "$JAR" -c "$JAR" http://127.0.0.1:8080/ | grep -ic "set-cookie: localhost_session=")
assert_eq "first visit sets a session cookie" 1 "$SET_COOKIE_1"
assert_eq "second visit (cookie replayed) does not re-issue one" 0 "$SET_COOKIE_2"
rm -f "$JAR"

section "CGI: GET, PATH_INFO, chunked and unchunked POST"
assert_eq "CGI GET" 200 "$(code http://127.0.0.1:8080/cgi-bin/hello.py)"
assert_contains "CGI reflects QUERY_STRING" "$(curl -sS "http://127.0.0.1:8080/cgi-bin/hello.py?a=1")" "QUERY_STRING</th><td>a=1"
assert_contains "CGI (Content-Length body) reflects the body" "$(curl -sS -X POST --data "unchunked-body" http://127.0.0.1:8080/cgi-bin/hello.py)" "unchunked-body"
assert_contains "CGI (chunked body) reflects the body" "$(curl -sS -X POST -H 'Transfer-Encoding: chunked' --data "chunked-body" http://127.0.0.1:8080/cgi-bin/hello.py)" "chunked-body"

stop_server

# ---------------------------------------------------------------------
# summary
# ---------------------------------------------------------------------

echo
echo "-----------------------------------------"
echo "  $PASS passed, $FAIL failed"
echo "-----------------------------------------"
rm -f "$REPO_ROOT/tests/.server.log"
[ "$FAIL" -eq 0 ]
