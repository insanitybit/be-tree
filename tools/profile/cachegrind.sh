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
        } > /profile/metadata.txt
        run() {
            scenario=$1
            iterations=$2
            destination=$3
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
            run get-many 100 "get-many.$repeat.cachegrind"
            run scan 2 "scan.$repeat.cachegrind"
            run scan-stream 2 "scan-stream.$repeat.cachegrind"
            run apply 25 "apply.$repeat.cachegrind"
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
