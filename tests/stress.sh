#!/usr/bin/env bash
#
# Starts the server on a throwaway config and runs `siege` against it,
# per the audit: "Use siege with a GET method on an empty page,
# availability should be at least 99.5%."
#
# If `siege` isn't installed, falls back to a pure-Python concurrent load
# generator so you can still sanity-check availability without it —
# useful for iterating locally, but siege is what the audit actually asks
# for, so install it (`apt install siege` / `brew install siege`) before
# the real thing.
#
# Usage: tests/stress.sh [duration] [concurrency]
#   tests/stress.sh 30s 25

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

DURATION="${1:-20s}"
CONCURRENCY="${2:-25}"
URL="http://127.0.0.1:8080/"

cargo build --release --quiet

./target/release/localhost config/examples/single_server.conf >tests/.stress-server.log 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null; rm -f tests/.stress-server.log' EXIT
sleep 0.4

if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "server failed to start:"; cat tests/.stress-server.log; exit 1
fi

RSS_BEFORE=$(grep VmRSS "/proc/$SERVER_PID/status" | awk '{print $2}')
FD_BEFORE=$(ls "/proc/$SERVER_PID/fd" | wc -l)

if command -v siege >/dev/null 2>&1; then
    echo "Running: siege -b -c $CONCURRENCY -t $DURATION $URL"
    siege -b -c "$CONCURRENCY" -t "$DURATION" "$URL"
else
    echo "siege is not installed — falling back to a Python concurrent load generator."
    echo "(install siege and re-run for the number the audit actually wants)"
    python3 - "$URL" "$CONCURRENCY" "$DURATION" <<'PYEOF'
import sys, time, concurrent.futures, urllib.request

url, concurrency, duration = sys.argv[1], int(sys.argv[2]), sys.argv[3]
seconds = int(duration.rstrip("s"))
deadline = time.time() + seconds

def worker(_):
    ok = err = 0
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=5) as r:
                ok += (r.status == 200)
        except Exception:
            err += 1
    return ok, err

with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as ex:
    results = list(ex.map(worker, range(concurrency)))

ok = sum(r[0] for r in results)
err = sum(r[1] for r in results)
total = ok + err
avail = 100 * ok / total if total else 0
print(f"\n{total} requests, {ok} ok, {err} failed -> {avail:.2f}% availability over ~{seconds}s")
PYEOF
fi

sleep 1
RSS_AFTER=$(grep VmRSS "/proc/$SERVER_PID/status" | awk '{print $2}')
FD_AFTER=$(ls "/proc/$SERVER_PID/fd" | wc -l)
ESTABLISHED=$(ss -tn state established '( sport = :8080 )' 2>/dev/null | tail -n +2 | wc -l)

echo
echo "RSS before/after: ${RSS_BEFORE}kB -> ${RSS_AFTER}kB"
echo "open fds before/after: $FD_BEFORE -> $FD_AFTER"
echo "lingering ESTABLISHED connections on :8080: $ESTABLISHED (should settle near 0)"
kill -0 "$SERVER_PID" 2>/dev/null && echo "server process is still alive after the run"
