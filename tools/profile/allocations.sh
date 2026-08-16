#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo"

env -u RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER= cargo bench --bench allocations --no-run
profile=$(find target/release/deps -maxdepth 1 -type f -name 'allocations-*' -perm -111 | head -1)
test -n "$profile"

mkdir -p target/profile
for repeat in $(seq 1 "${PROFILE_REPEATS:-3}"); do
    echo "repeat=$repeat"
    "$profile" get-many 100
    "$profile" get-many-1-sorted-hits 100
    "$profile" get-many-16-sorted-hits 100
    "$profile" get-many-256-sorted-hits 100
    "$profile" get-many-1024-sorted-hits 100
    "$profile" get-many-256-random-hits 100
    "$profile" get-many-256-sorted-misses 100
    "$profile" get-many-256-random-mixed 100
    "$profile" scan 2
    "$profile" scan-stream 2
    "$profile" scan-tombstone 2
    "$profile" scan-stream-tombstone 2
    "$profile" apply 100
    "$profile" apply-1-distinct 10
    "$profile" apply-256-distinct 10
    "$profile" apply-1024-distinct 10
    "$profile" apply-256-repeated 10
    "$profile" apply-256-delete 10
    "$profile" apply-256-mixed 10
    "$profile" cow-low 25
    "$profile" cow-high 25
    "$profile" hash 1000
    "$profile" decode 1000
done | tee target/profile/allocations.txt
