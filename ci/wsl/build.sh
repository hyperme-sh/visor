#!/usr/bin/env bash
set -euo pipefail
editor=$1
output=$2
stage=$(mktemp -d)
container=
image="visor-wsl-fixture:${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"
cleanup() {
    if [ -n "$container" ]; then docker rm "$container"; fi
    docker image rm "$image" >/dev/null 2>&1 || true
    rm -rf -- "$stage"
}
trap cleanup EXIT
cargo build --locked --no-default-features --features headless -p hypermesh-visor --bin hypermesh-visor
install -m 0755 target/debug/hypermesh-visor target/debug/hypermesh-companion "$stage"
strip "$stage/hypermesh-visor" "$stage/hypermesh-companion"
version=$(cat ci/wsl/vscode-version)
curl --proto '=https' --proto-redir '=https' --fail --silent --show-error --location --retry 3 --max-time 180 \
    "https://update.code.visualstudio.com/$version/server-linux-x64/stable" \
    --output "$stage/vscode-server.tar.gz"
cp ci/wsl/{Dockerfile,task.py} "$editor/dist/hypermesh-vscode.vsix" "$stage"
printf 'visor=%s\nvscode=%s\n' "$GITHUB_SHA" "$(git -C "$editor" rev-parse HEAD)" > "$stage/SOURCE_REVISIONS"
docker build -t "$image" "$stage"
container=$(docker create "$image")
docker export --output "$output/visor-wsl.tar" "$container"
