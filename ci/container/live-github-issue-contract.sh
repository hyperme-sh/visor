#!/usr/bin/env bash
set -euo pipefail

case "$EXPECTED_ARCH" in
    amd64) [[ "$(uname -m)" == x86_64 ]] ;;
    arm64) [[ "$(uname -m)" == aarch64 ]] ;;
    *) exit 2 ;;
esac
if [[ ! "$PUBLISHED_TEST_INDEX" =~ ^sha256:[a-f0-9]{64}$ || ! "$LIVE_GITHUB_ISSUE_ACCOUNT" =~ ^[1-9][0-9]{0,19}$ ]]; then
    echo 'Set the published TEST index digest and numeric GitHub account ID.' >&2
    exit 2
fi
export VISOR_IMAGE="ghcr.io/hyperme-sh/hypermesh-visor@$PUBLISHED_TEST_INDEX"
docker pull "$VISOR_IMAGE"
[[ "$(docker image inspect --format '{{.Architecture}}' "$VISOR_IMAGE")" == "$EXPECTED_ARCH" ]]
[[ "$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$VISOR_IMAGE")" == "$GITHUB_SHA" ]]
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
[[ -S "$VISOR_GITHUB_ISSUE_RELAY_SOCKET" ]]
sudo chown 1000:1000 "$VISOR_GITHUB_ISSUE_RELAY_SOCKET"

if ! "${compose[@]}" up -d --pull never --wait --wait-timeout 90 writer; then
    "${compose[@]}" logs writer
    exit 1
fi
id="$("${compose[@]}" ps -q writer)"
[[ "$(docker inspect --format '{{.HostConfig.NetworkMode}}' "$id")" == none ]]
probe=/home/bot/.local/state/hypermesh/github-issue-probe.py
"${compose[@]}" exec -T writer sh -ec 'umask 077; cat > "$1"' sh "$probe" \
    < ci/container/published-github-issue-probe.py

artifacts="repos/$GITHUB_REPOSITORY/actions/artifacts"
envelope_name="github-issue-envelopes-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"
echo 'Waiting for the token encrypted to this job’s ephemeral public key.'
artifact_id=
for _ in {1..36}; do
    artifact_id="$(gh api --method GET "$artifacts" -f "name=$envelope_name" | jq -r \
        --arg revision "$GITHUB_SHA" '[.artifacts[] | select(.expired == false and .workflow_run.head_sha == $revision and .workflow_run.head_branch == "main")] | first | .id // empty')"
    if [[ -n "$artifact_id" ]]; then break; fi
    sleep 10
done
if [[ ! "$artifact_id" =~ ^[0-9]+$ ]]; then
    echo 'No encrypted token arrived for this run attempt.' >&2
    exit 2
fi
gh api "$artifacts/$artifact_id/zip" > "$scratch/envelopes.zip"
# Hold the plaintext in this shell until it is piped into the session vault.
token="$(unzip -p "$scratch/envelopes.zip" "$EXPECTED_ARCH" | base64 --decode | \
    openssl pkeyutl -decrypt -inkey "$RUNNER_TEMP/github-issue-key.pem" \
        -pkeyopt rsa_padding_mode:oaep -pkeyopt rsa_oaep_md:sha256 -pkeyopt rsa_mgf1_md:sha256)"
if [[ ! "$token" =~ ^github_pat_[A-Za-z0-9_]{20,255}$ ]]; then
    echo 'The encrypted token must contain a fine-grained GitHub Issues credential.' >&2
    exit 2
fi
printf '%s' "$token" | "${compose[@]}" exec -T writer python3 "$probe" "$LIVE_GITHUB_ISSUE_ACCOUNT"
