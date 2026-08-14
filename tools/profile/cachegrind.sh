#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
image=be-tree-cachegrind:rust-1.91
output="$repo/target/profile"
mkdir -p "$output"

docker build --quiet --tag "$image" "$repo/tools/profile" >/dev/null
docker run --rm \
    --volume "$repo:/source:ro" \
    --volume "$output:/profile" \
    --volume be-tree-cachegrind-target:/target \
    --volume be-tree-cachegrind-registry:/usr/local/cargo/registry \
    --volume be-tree-cachegrind-git:/usr/local/cargo/git \
    --workdir /source \
    --env CARGO_TARGET_DIR=/target \
    --env PROFILE_REPEATS="${PROFILE_REPEATS:-3}" \
    "$image" bash -euo pipefail -c '
        cargo bench --bench profile --no-run
        profile=$(find /target/release/deps -maxdepth 1 -type f -name "profile-*" -perm -111 | head -1)
        test -n "$profile"
        source_digest=$(find src benches Cargo.toml Cargo.lock -type f -print0 \
            | sort -z | xargs -0 sha256sum | sha256sum | cut -d" " -f1)
        {
            rustc -Vv
            valgrind --version
            uname -m
            echo "cache_model=I1:32768,8,64 D1:32768,8,64 LL:8388608,16,64"
            echo "repeats=$PROFILE_REPEATS"
            echo "source_digest=$source_digest"
            # Fixture dimensions, from benches/profile/support.rs constants.
            echo "fixture=keys:10000,inline_value_bytes:22,commit_width:256,store:MemStore"
        } > /profile/metadata.txt
        : > /profile/commands.txt
        run() {
            scenario=$1
            iterations=$2
            destination=$3
            echo "$profile $scenario $iterations > $destination" >> /profile/commands.txt
            valgrind --tool=cachegrind --branch-sim=yes \
                --quiet \
                --error-exitcode=99 \
                --I1=32768,8,64 \
                --D1=32768,8,64 \
                --LL=8388608,16,64 \
                --log-file="/profile/$destination.log" \
                --cachegrind-out-file="/profile/$destination" \
                "$profile" "$scenario" "$iterations" >/dev/null
        }
        for repeat in $(seq 1 "$PROFILE_REPEATS"); do
            run setup 1 "setup.$repeat.cachegrind"
            run setup-get-many 1 "setup-get-many.$repeat.cachegrind"
            run setup-hash 1 "setup-hash.$repeat.cachegrind"
            run setup-decode 1 "setup-decode.$repeat.cachegrind"
            # Cold shapes read 16 DISTINCT strided keys (at most the fixture leaf count), so every
            # measured leaf load is a real miss; rereading one key 100 times would be 99% warm.
            for shape in get-cold-short get-cold-long; do
                run "setup-$shape" 1 "setup-$shape.$repeat.cachegrind"
                run "$shape" 16 "${shape}.$repeat.cachegrind"
            done
            for shape in get-hot-short get-hot-long; do
                run "setup-$shape" 1 "setup-$shape.$repeat.cachegrind"
                run "$shape" 100 "${shape}.$repeat.cachegrind"
            done
            run get-many 100 "get-many.$repeat.cachegrind"
            for shape in \
                get-many-1-sorted-hits get-many-1-sorted-misses get-many-1-sorted-mixed \
                get-many-1-random-hits get-many-1-random-misses get-many-1-random-mixed \
                get-many-16-sorted-hits get-many-16-sorted-misses get-many-16-sorted-mixed \
                get-many-16-random-hits get-many-16-random-misses get-many-16-random-mixed \
                get-many-256-sorted-hits get-many-256-sorted-misses get-many-256-sorted-mixed \
                get-many-256-random-hits get-many-256-random-misses get-many-256-random-mixed \
                get-many-1024-sorted-hits get-many-1024-sorted-misses get-many-1024-sorted-mixed \
                get-many-1024-random-hits get-many-1024-random-misses get-many-1024-random-mixed; do
                run "setup-$shape" 1 "setup-$shape.$repeat.cachegrind"
                run "$shape" 100 "${shape}.$repeat.cachegrind"
            done
            run scan 2 "scan.$repeat.cachegrind"
            run scan-stream 2 "scan-stream.$repeat.cachegrind"
            for shape in scan-2 scan-32 scan-256 scan-stream-2 scan-stream-32 scan-stream-256; do
                run setup 1 "setup-$shape.$repeat.cachegrind"
                run "$shape" 2 "${shape}.$repeat.cachegrind"
            done
            for shape in scan-tombstone scan-stream-tombstone; do
                run "setup-$shape" 1 "setup-$shape.$repeat.cachegrind"
                run "$shape" 2 "${shape}.$repeat.cachegrind"
            done
            run apply 25 "apply.$repeat.cachegrind"
            for shape in apply-1-distinct apply-256-distinct apply-1024-distinct apply-256-repeated apply-256-delete apply-256-mixed; do
                run setup 1 "setup-$shape.$repeat.cachegrind"
                run "$shape" 10 "${shape}.$repeat.cachegrind"
            done
            for shape in cow-low cow-high; do
                # The setup twin builds the SAME prebuilt batches as the measured run and applies
                # none of them, so the delta is exactly the chained commits.
                run "setup-$shape" 25 "setup-$shape.$repeat.cachegrind"
                run "$shape" 25 "$shape.$repeat.cachegrind"
            done
            run hash 1000 "hash.$repeat.cachegrind"
            run decode 1000 "decode.$repeat.cachegrind"
        done
        annotate_delta() {
            baseline=$1
            scenario=$2
            label=$3
            cg_diff "/profile/$baseline" "/profile/$scenario" \
                > "/profile/$label.delta.cachegrind"
            cg_annotate --auto=yes --show=Ir,D1mr,D1mw,Bcm --threshold=0.5 \
                "/profile/$label.delta.cachegrind" \
                > "/profile/$label.annotate.txt"
        }
        annotate_delta setup-get-many.1.cachegrind get-many.1.cachegrind get-many
        annotate_delta setup.1.cachegrind scan.1.cachegrind scan
        annotate_delta setup.1.cachegrind scan-stream.1.cachegrind scan-stream
        annotate_delta setup.1.cachegrind apply.1.cachegrind apply
        annotate_delta setup-hash.1.cachegrind hash.1.cachegrind hash
        annotate_delta setup-decode.1.cachegrind decode.1.cachegrind decode
    '

python3 "$repo/tools/profile/summarize.py" "$output"
