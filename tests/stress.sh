#!/usr/bin/env bash
# Exercise the audit's availability requirement against an empty static file.
# The server runs from a temporary copy of its config and web root, leaving the
# checkout and any server already running from it untouched.
#
# Usage:
#   tests/stress.sh [duration] [concurrency]
#   PORT=19000 tests/stress.sh 30s 25

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
REPO_ROOT="$(pwd)"

DURATION="${1:-20s}"
CONCURRENCY="${2:-25}"
PORT="${PORT:-18080}"
MIN_AVAILABILITY="99.5"

if ! [[ "$DURATION" =~ ^[1-9][0-9]*[sSmMhH]$ ]]; then
    echo "duration must be a positive number followed by s, m, or h (for example, 30s)"
    exit 2
fi
if ! [[ "$CONCURRENCY" =~ ^[1-9][0-9]*$ ]] || [ "$CONCURRENCY" -gt 1000 ]; then
    echo "concurrency must be an integer between 1 and 1000"
    exit 2
fi
if ! [[ "$PORT" =~ ^[0-9]+$ ]] || [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
    echo "PORT must be an integer between 1 and 65535"
    exit 2
fi
for dependency in cargo curl ss; do
    if ! command -v "$dependency" >/dev/null 2>&1; then
        echo "required command is not installed: $dependency"
        exit 2
    fi
done

BIN="$REPO_ROOT/target/release/localhost"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/localhost-stress.XXXXXX")"
SERVER_LOG="$TEST_ROOT/server.log"
URL="http://127.0.0.1:${PORT}/empty.html"
SIEGE_RC="$TEST_ROOT/siegerc"
SERVER_PID=""

# Called by the EXIT trap below.
# shellcheck disable=SC2329
cleanup() {
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null
        wait "$SERVER_PID" 2>/dev/null
    fi
    rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT

mkdir -p "$TEST_ROOT/config"
cp -a "$REPO_ROOT/www" "$TEST_ROOT/www"
cp "$REPO_ROOT/config/examples/single_server.conf" "$TEST_ROOT/config/stress.conf"
sed -i "s/127\.0\.0\.1:8080/127.0.0.1:${PORT}/" "$TEST_ROOT/config/stress.conf"
: >"$TEST_ROOT/www/empty.html"
printf 'verbose = false\ncolor = off\nlogging = false\n' >"$SIEGE_RC"

echo "Building release binary..."
(cd "$REPO_ROOT" && cargo build --release --quiet) || exit 1

(
    cd "$TEST_ROOT" || exit 1
    exec "$BIN" config/stress.conf
) >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

READY=false
for _ in $(seq 1 30); do
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        break
    fi
    if curl --noproxy '*' --max-time 1 -sS -o /dev/null "$URL" 2>/dev/null; then
        READY=true
        break
    fi
    sleep 0.1
done
if [ "$READY" != true ]; then
    echo "server failed to start on 127.0.0.1:${PORT}:"
    cat "$SERVER_LOG"
    exit 1
fi

RSS_BEFORE=$(awk '/VmRSS:/ {print $2}' "/proc/$SERVER_PID/status")
FD_BEFORE=$(find "/proc/$SERVER_PID/fd" -mindepth 1 -maxdepth 1 | wc -l)
LOAD_STATUS=0

if command -v siege >/dev/null 2>&1; then
    SIEGE_DURATION="${DURATION%?}$(printf '%s' "${DURATION: -1}" | tr 'smh' 'SMH')"
    echo "Running: siege -R $SIEGE_RC -b -c $CONCURRENCY -t $SIEGE_DURATION $URL"
    SIEGE_OUTPUT=$(siege -R "$SIEGE_RC" -b -c "$CONCURRENCY" -t "$SIEGE_DURATION" "$URL" 2>&1)
    SIEGE_STATUS=$?
    printf '%s\n' "$SIEGE_OUTPUT"

    AVAILABILITY=$(printf '%s\n' "$SIEGE_OUTPUT" | awk '/Availability:/ {value=$2; gsub(/[^0-9.]/, "", value)} END {print value}')
    if [ "$SIEGE_STATUS" -ne 0 ] || [ -z "$AVAILABILITY" ]; then
        echo "FAIL: siege did not complete with a readable availability result"
        LOAD_STATUS=1
    elif ! awk -v actual="$AVAILABILITY" -v minimum="$MIN_AVAILABILITY" 'BEGIN {exit !(actual >= minimum)}'; then
        echo "FAIL: availability ${AVAILABILITY}% is below ${MIN_AVAILABILITY}%"
        LOAD_STATUS=1
    else
        echo "PASS: availability ${AVAILABILITY}% meets the ${MIN_AVAILABILITY}% requirement"
    fi
else
    echo "siege is not installed; running the local Python fallback."
    echo "Install siege and rerun before the audit for the official measurement."
    if ! command -v python3 >/dev/null 2>&1; then
        echo "FAIL: neither siege nor python3 is installed"
        LOAD_STATUS=1
    else
        python3 - "127.0.0.1" "$PORT" "$CONCURRENCY" "$DURATION" "$MIN_AVAILABILITY" <<'PYEOF'
import concurrent.futures
import http.client
import sys
import time


host = sys.argv[1]
port = int(sys.argv[2])
concurrency = int(sys.argv[3])
duration = sys.argv[4].lower()
minimum = float(sys.argv[5])
units = {"s": 1, "m": 60, "h": 3600}
seconds = int(duration[:-1]) * units[duration[-1]]
deadline = time.monotonic() + seconds


def worker():
    ok = failed = 0
    while time.monotonic() < deadline:
        connection = http.client.HTTPConnection(host, port, timeout=5)
        try:
            connection.request("GET", "/empty.html", headers={"Connection": "close"})
            response = connection.getresponse()
            response.read()
            if response.status == 200:
                ok += 1
            else:
                failed += 1
        except (OSError, http.client.HTTPException):
            failed += 1
        finally:
            connection.close()
    return ok, failed


with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
    results = list(pool.map(lambda _: worker(), range(concurrency)))

successful = sum(result[0] for result in results)
failed = sum(result[1] for result in results)
total = successful + failed
availability = 100 * successful / total if total else 0.0
print(
    f"{total} requests, {successful} successful, {failed} failed: "
    f"{availability:.2f}% availability"
)
if total == 0 or availability < minimum:
    print(f"FAIL: availability is below {minimum:.1f}%")
    raise SystemExit(1)
print(f"PASS: availability meets the {minimum:.1f}% requirement")
PYEOF
        LOAD_STATUS=$?
    fi
fi

ESTABLISHED=-1
for _ in $(seq 1 20); do
    ESTABLISHED=$(ss -Htn state established "( sport = :${PORT} )" 2>/dev/null | wc -l)
    [ "$ESTABLISHED" -eq 0 ] && break
    sleep 0.1
done

HEALTH_CODE=$(curl --noproxy '*' --max-time 2 -sS -o /dev/null -w '%{http_code}' "$URL" 2>/dev/null)
RESULT=$LOAD_STATUS
if kill -0 "$SERVER_PID" 2>/dev/null; then
    RSS_AFTER=$(awk '/VmRSS:/ {print $2}' "/proc/$SERVER_PID/status")
    FD_AFTER=$(find "/proc/$SERVER_PID/fd" -mindepth 1 -maxdepth 1 | wc -l)
else
    RSS_AFTER="unavailable"
    FD_AFTER=-1
fi

echo
echo "Post-load checks"
echo "  RSS: ${RSS_BEFORE}kB -> ${RSS_AFTER}kB (observe over longer runs when checking leaks)"
echo "  open file descriptors: $FD_BEFORE -> $FD_AFTER"
echo "  lingering established connections: $ESTABLISHED"
echo "  final health response: $HEALTH_CODE"

if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "FAIL: server process exited during the test"
    RESULT=1
elif [ "$HEALTH_CODE" != 200 ]; then
    echo "FAIL: server did not answer after the load test"
    RESULT=1
elif [ "$ESTABLISHED" -ne 0 ]; then
    echo "FAIL: connections remained established after the settling period"
    RESULT=1
elif [ "$FD_AFTER" -gt "$FD_BEFORE" ]; then
    echo "FAIL: file descriptors remained open after the load test"
    RESULT=1
else
    echo "PASS: server stayed alive, responsive, and released its connections"
fi

exit "$RESULT"
