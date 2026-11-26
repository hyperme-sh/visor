#!/usr/bin/env bash
# Exercise the exact package in the native image with Visor's real sandbox.
set -euo pipefail
image="$1"
path=/usr/local/share/hypermesh-visor/plugins/github-issue.visor-plugin
scratch="$(mktemp -d)"
container=''
staged=0
cleanup() {
    if [[ -n "$container" ]]; then
        docker rm "$container" >/dev/null
    fi
    if [[ "$staged" == 1 ]]; then
        sudo rm -f -- "$path"
    fi
    rm -rf -- "$scratch"
}
trap cleanup EXIT

docker run --rm --network none --read-only --cap-drop ALL \
    --security-opt no-new-privileges --entrypoint /bin/sh "$image" \
    -ec 'test -x /usr/bin/bwrap; test -r "$1"; test ! -w "$1"' sh "$path"
container="$(docker create "$image")"
docker cp "$container:$path" - > "$scratch/plugin.tar"
python3 - "$scratch/plugin.tar" "$scratch/github-issue.visor-plugin" <<'PY'
import shutil
import sys
import tarfile

with tarfile.open(sys.argv[1], "r:") as archive:
    members = archive.getmembers()
    assert len(members) == 1
    saved = members[0]
    assert saved.isfile() and saved.uid == 0 and saved.mode == 0o444
    assert 0 < saved.size <= 12 * 1024 * 1024
    with archive.extractfile(saved) as src, open(sys.argv[2], "xb") as dst:
        shutil.copyfileobj(src, dst)
PY

if ! command -v bwrap >/dev/null; then
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends bubblewrap
fi
if [[ "$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || true)" == 1 ]]; then
    profile=/usr/share/apparmor/extra-profiles/bwrap-userns-restrict
    if [[ ! -r "$profile" ]]; then
        sudo apt-get update
        sudo apt-get install -y --no-install-recommends apparmor-profiles
    fi
    sudo apparmor_parser --replace "$profile"
fi

# Put the image's bytes at the same system path on this disposable native runner.
# The contract starts with an empty user store and skips the plugin install API.
test ! -e "$path" && test ! -L "$path"
sudo install -D -o root -g root -m 0444 "$scratch/github-issue.visor-plugin" "$path"
staged=1
digest="$(sha256sum "$scratch/github-issue.visor-plugin" | cut -d ' ' -f 1)"
VISOR_TEST_BUNDLED_GITHUB_ISSUE="$digest" cargo test --locked --no-default-features --features headless --lib \
    api::tests::plugin_contract::github_issue_plugin_uses_the_selected_account_and_public_repository \
    -- --ignored --exact --nocapture
