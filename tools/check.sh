#!/bin/bash
# Everything a change has to pass before it's committed: the tests,
# unoptimised and optimised, then clippy. Prints only each test binary's
# result and any failure; exits non-zero if anything failed.
#
#   tools/check.sh            all of it (about 10 minutes)
#   tools/check.sh release    the optimised tests and clippy only
set -u
cd "$(dirname "$0")/.."
status=0
log=$(mktemp)
trap 'rm -f "$log"' EXIT

run_tests() {
    echo "== tests ($1)"
    if ! cargo test -q --workspace --no-fail-fast ${2:+"$2"} >"$log" 2>&1; then
        status=1
    fi
    grep -E "^test result|FAILED|panicked|^error" "$log" | grep -v "^test result: ok. 0 passed"
}

[ "${1:-}" = release ] || run_tests debug
run_tests release --release

echo "== clippy"
if ! cargo clippy -q --workspace --all-targets 2>&1 | tee "$log" | tail -20; then
    status=1
fi
[ -s "$log" ] && status=1

[ $status = 0 ] && echo "== all passed" || echo "== something failed"
exit $status
