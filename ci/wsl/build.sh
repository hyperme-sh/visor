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
(
    cd .ci-fixtures/wsl-host
    CGO_ENABLED=0 go build -trimpath -mod=readonly -o "$stage/host-agent" ./cmd/agent
    CGO_ENABLED=0 go build -trimpath -mod=readonly -o "$stage/host-cli" ./cmd/hypermesh-host
)
(
    cd .ci-fixtures/wsl-conductor
    CGO_ENABLED=0 go build -trimpath -mod=readonly -o "$stage/conductor" ./cmd/conductor
)
cp ci/wsl/{Dockerfile,task.py,ssh_host.py,preview_app.py,editor.mjs,local_preview.sh} "$editor/dist/hypermesh-vscode.vsix" "$stage"
printf 'visor=%s\nvscode=%s\nhost=%s\nconductor=%s\n' "$GITHUB_SHA" "$(git -C "$editor" rev-parse HEAD)" \
    "$(git -C .ci-fixtures/wsl-host rev-parse HEAD)" "$(git -C .ci-fixtures/wsl-conductor rev-parse HEAD)" > "$stage/SOURCE_REVISIONS"
docker build --target runtime -t "$image" "$stage"
container=$(docker create "$image")
docker export --output "$output/visor-wsl.tar" "$container"

local_image="visor-local-preview:$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT"
curl --proto '=https' --proto-redir '=https' --fail --silent --show-error --location --retry 3 --max-time 180 \
    "https://update.code.visualstudio.com/$version/linux-x64/stable" --output "$stage/code-linux.tar.gz"
install -m 0755 "$(command -v node)" "$stage/node"
docker build --target local-preview -t "$local_image" "$stage"
