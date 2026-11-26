#!/usr/bin/env bash
set -euo pipefail
case "$EXPECTED_ARCH" in
    amd64) test "$(uname -m)" = x86_64 ;;
    arm64) test "$(uname -m)" = aarch64 ;;
    *) exit 2 ;;
esac
if [[ $# == 1 && "$1" == visor-container:ci ]]; then
    export VISOR_IMAGE="$1"
else
    [[ "$PUBLISHED_TEST_INDEX" =~ ^sha256:[a-f0-9]{64}$ ]]
    export VISOR_IMAGE="ghcr.io/hyperme-sh/hypermesh-visor@$PUBLISHED_TEST_INDEX"
    docker pull "$VISOR_IMAGE"
fi
compose=(docker compose -p "visor-plugin-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}" -f container/compose.yaml)
scratch="$(mktemp -d)"
cleanup() {
    "${compose[@]}" down --volumes --remove-orphans
    rm -rf -- "$scratch"
}
trap cleanup EXIT

test "$(docker image inspect --format '{{.Architecture}}' "$VISOR_IMAGE")" = "$EXPECTED_ARCH"
revision="$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$VISOR_IMAGE")"
[[ "$revision" =~ ^[a-f0-9]{40}$ ]]
printf 'Visor image %s %s from %s\n' "$EXPECTED_ARCH" "$VISOR_IMAGE" "$revision"
if ! "${compose[@]}" up -d --pull never --wait --wait-timeout 90 writer; then
    "${compose[@]}" logs writer
    exit 1
fi
id="$("${compose[@]}" ps -q writer)"
test "$(docker inspect --format '{{.HostConfig.NetworkMode}}' "$id")" = none
docker inspect --format 'Network={{.HostConfig.NetworkMode}} ReadOnly={{.HostConfig.ReadonlyRootfs}} CapDrop={{json .HostConfig.CapDrop}} SecurityOpt={{json .HostConfig.SecurityOpt}} AppArmor={{.AppArmorProfile}}' "$id"
"${compose[@]}" exec -T writer python3 - < ci/container/published-github-issue-probe.py

# If the real daemon can run the plugin sandbox, exercise issue reads with the
# existing loopback provider contract in that same container. The helper is
# built from the image revision and is the only file copied into its tmpfs.
mkdir "$scratch/source"
git archive "$revision" | tar -xf - -C "$scratch/source"
(
    cd "$scratch/source"
    CARGO_PROFILE_TEST_DEBUG=0 cargo test --locked --no-default-features --features headless \
        --lib --no-run --message-format=json
) | python3 -c '
import json, sys
for line in sys.stdin:
    row = json.loads(line)
    if row.get("reason") == "compiler-artifact" and row.get("executable") and row["target"].get("kind") == ["lib"]:
        print(row["executable"])
' > "$scratch/test-binary"
test "$(wc -l < "$scratch/test-binary")" = 1
read -r binary < "$scratch/test-binary"
"${compose[@]}" exec -T writer sh -ec 'umask 077; cat > /tmp/plugin-ci-contract; chmod 0700 /tmp/plugin-ci-contract' < "$binary"
bundle=/usr/local/share/hypermesh-visor/plugins/github-issue.visor-plugin
digest="$("${compose[@]}" exec -T writer sha256sum "$bundle" | cut -d ' ' -f 1)"
"${compose[@]}" exec -T -e "VISOR_TEST_BUNDLED_GITHUB_ISSUE=$digest" writer \
    /tmp/plugin-ci-contract --ignored --exact --nocapture \
    api::tests::plugin_contract::github_issue_plugin_uses_the_selected_account_and_public_repository
