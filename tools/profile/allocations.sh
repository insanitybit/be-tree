#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"

env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo bench --bench allocations --no-run
profile=$(find target/release/deps -maxdepth 1 -type f -name 'allocations-*' -perm -111 | head -1)
test -n "$profile"

mkdir -p target/profile
{
    "$profile" get-many 100
    "$profile" scan 2
    "$profile" scan-stream 2
    "$profile" apply 100
    "$profile" hash 1000
    "$profile" decode 1000
} | tee target/profile/allocations.txt
