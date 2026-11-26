#!/usr/bin/env bash
set -euo pipefail
umask 077
architecture="$1"
machine="$2"
revision="$3"
output="$4"
[[ "$(uname -m)" == "$machine" ]]
mkdir -p "$output"/{base,coding-v1}
temporary="$(mktemp -d)"
container=""
cleanup() {
    if [[ -n "$container" ]]; then
        docker rm "$container" >/dev/null
    fi
    rm -rf "$temporary"
}
trap cleanup EXIT

build() {
    docker build --file ci/headless/Dockerfile --target "$1" \
        --build-arg "EXPECTED_ARCH=$machine" --build-arg "SOURCE_REVISION=$revision" --tag "$2" .
}
inventory() {
    container="$(docker create "$1")"
    docker cp "$container:/usr/share/visor-worker/." "$output/$2/"
    docker rm "$container" >/dev/null
    container=""
    docker inspect "$1" > "$temporary/$2.json"
}

build cloud-worker visor-cloud-worker:ci
inventory visor-cloud-worker:ci base
build cloud-coding visor-cloud-worker-coding:ci
inventory visor-cloud-worker-coding:ci coding-v1

# An identical selected pack and locked inputs can reuse the build safely.
before="$(docker image inspect --format '{{.Id}}' visor-cloud-worker-coding:ci)"
build cloud-coding visor-cloud-worker-coding:ci
[[ "$(docker image inspect --format '{{.Id}}' visor-cloud-worker-coding:ci)" == "$before" ]]

docker image save visor-cloud-worker:ci | gzip -n -1 > "$output/visor-cloud-worker-$architecture.tar.gz"
python3 ci/cloud-worker/manifest.py build "$output" "$temporary/base.json" "$temporary/coding-v1.json" "$architecture" "$revision"
python3 ci/cloud-worker/manifest.py verify "$output"

# Exercise checksum refusal without changing the archive or retaining a bad manifest.
mv "$output/manifest.json" "$temporary/manifest.json"
jq '.archive.sha256 = "invalid"' "$temporary/manifest.json" > "$output/manifest.json"
if python3 ci/cloud-worker/manifest.py verify "$output" > "$temporary/verify" 2>&1; then
    echo 'Worker archive accepted an incorrect checksum' >&2
    exit 1
fi
grep -F 'worker archive checksum mismatch' "$temporary/verify"
mv "$temporary/manifest.json" "$output/manifest.json"
