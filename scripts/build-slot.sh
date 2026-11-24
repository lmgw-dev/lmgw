#!/usr/bin/env bash
# Usage: scripts/build-slot.sh <command...>   run a build command holding one of the machine's two build slots
#
# Two cargo builds at once, 16 jobs each, are what a large build machine holds; more unsplit ones
# can run it out of memory. The slots are flock locks; /tmp/lmgw-cargo-build.lock is also the lock that
# scripts/csd-spike.sh and older scripts take on their own, so they count as one slot. The command
# gets 16 jobs and 16 test threads unless the caller sets CARGO_BUILD_JOBS or NEXTEST_TEST_THREADS.
# flock -o keeps the lock off the command's children (sccache's server would hold it for minutes).
# Example: scripts/build-slot.sh bash ci/check.sh > target/ci.log 2>&1
set -uo pipefail
if [ "$#" -eq 0 ]; then sed -n '2p' "$0" >&2; exit 2; fi
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-16}"
export NEXTEST_TEST_THREADS="${NEXTEST_TEST_THREADS:-16}"
while :; do
    for lock in /tmp/lmgw-cargo-build.lock /tmp/lmgw-cargo-build-2.lock; do
        flock -o -n -E 75 "$lock" "$@"
        rc=$?
        [ "$rc" -ne 75 ] && exit "$rc"
    done
    sleep 5
done
