#!/usr/bin/env bash
set -euo pipefail

case "$EXPECTED_ARCH" in
    amd64) test "$(uname -m)" = x86_64 ;;
    arm64) test "$(uname -m)" = aarch64 ;;
    *) exit 2 ;;
esac
if [[ ! "$PUBLISHED_TEST_INDEX" =~ ^sha256:[a-f0-9]{64}$ || ! "$LIVE_GITHUB_ISSUE_ACCOUNT" =~ ^[1-9][0-9]{0,19}$ ]]; then
    echo 'Set the published TEST index digest and numeric GitHub account ID.' >&2
    exit 2
fi
if [[ ! "$LIVE_GITHUB_ISSUE_TOKEN" =~ ^github_pat_[A-Za-z0-9_]{20,255}$ ]]; then
    echo 'Set VISOR_LIVE_GITHUB_ISSUE_TOKEN to a fine-grained Issues read token.' >&2
    exit 2
fi
# Keep the credential out of subprocess environments and container metadata.
token="$LIVE_GITHUB_ISSUE_TOKEN"
unset LIVE_GITHUB_ISSUE_TOKEN

export VISOR_IMAGE="ghcr.io/hyperme-sh/hypermesh-visor@$PUBLISHED_TEST_INDEX"
docker pull "$VISOR_IMAGE"
test "$(docker image inspect --format '{{.Architecture}}' "$VISOR_IMAGE")" = "$EXPECTED_ARCH"
test "$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$VISOR_IMAGE")" = "$GITHUB_SHA"
printf 'Visor image %s %s from %s\n' "$EXPECTED_ARCH" "$VISOR_IMAGE" "$GITHUB_SHA"

scratch="$(mktemp -d)"
export VISOR_GITHUB_ISSUE_RELAY_SOCKET="$scratch/github.sock"
compose=(docker compose -p "visor-live-plugin-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"
    -f container/compose.yaml -f container/compose.github-issues.yaml)
relay_pid=
cleanup() {
    if [[ -n "$relay_pid" ]]; then
        kill "$relay_pid" 2>/dev/null || true
        wait "$relay_pid" || true
    fi
    "${compose[@]}" down --volumes --remove-orphans || true
    rm -rf -- "$scratch"
}
trap cleanup EXIT

python3 container/github-issue-relay.py "$VISOR_GITHUB_ISSUE_RELAY_SOCKET" &
relay_pid=$!
for _ in {1..50}; do
    if [[ -S "$VISOR_GITHUB_ISSUE_RELAY_SOCKET" ]]; then break; fi
    kill -0 "$relay_pid"
    sleep 0.1
done
test -S "$VISOR_GITHUB_ISSUE_RELAY_SOCKET"
sudo chown 1000:1000 "$VISOR_GITHUB_ISSUE_RELAY_SOCKET"

if ! "${compose[@]}" up -d --pull never --wait --wait-timeout 90 writer; then
    "${compose[@]}" logs writer
    exit 1
fi
id="$("${compose[@]}" ps -q writer)"
test "$(docker inspect --format '{{.HostConfig.NetworkMode}}' "$id")" = none
probe=/home/bot/.local/state/hypermesh/github-issue-probe.py
"${compose[@]}" exec -T writer sh -ec 'umask 077; cat > "$1"' sh "$probe" \
    < ci/container/published-github-issue-probe.py
printf '%s' "$token" | "${compose[@]}" exec -T writer python3 "$probe" "$LIVE_GITHUB_ISSUE_ACCOUNT"
