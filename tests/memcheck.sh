#!/usr/bin/env bash
# Run the complete Rust test binary under Valgrind. Definite and indirect
# leaks are treated as failures; Valgrind may classify a small allocation in
# Rust's test-thread runtime as "possibly lost" even after every test exits.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

for dependency in cargo jq valgrind; do
    if ! command -v "$dependency" >/dev/null 2>&1; then
        echo "required command is not installed: $dependency"
        exit 2
    fi
done

echo "Building the Rust test binary..."
TEST_BIN=$(
    cargo test --no-run --bin localhost --message-format=json |
        jq -r 'select(.reason == "compiler-artifact" and .target.test == true and .executable != null) | .executable' |
        tail -n 1
)

if [ -z "$TEST_BIN" ] || [ ! -x "$TEST_BIN" ]; then
    echo "could not locate the compiled test binary"
    exit 1
fi

exec valgrind \
    --leak-check=full \
    --show-leak-kinds=definite,indirect \
    --errors-for-leak-kinds=definite,indirect \
    --error-exitcode=99 \
    "$TEST_BIN" --test-threads=1
