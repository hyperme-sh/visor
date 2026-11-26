#!/usr/bin/env bash
# Disposable, network-isolated worker image qualification.
set -euo pipefail
name="visor-cloud-worker-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}-$(uname -m)"
volume="$(docker volume create "$name")"
trap 'docker rm -f "$name" >/dev/null 2>&1 || true; docker volume rm "$volume" >/dev/null' EXIT

docker run --rm --network none --user 0 --entrypoint /bin/sh \
    --mount "type=volume,source=$volume,target=/var/lib/visor" visor-cloud-worker:ci \
    -ec 'chown 1000:1000 /var/lib/visor && chmod 0700 /var/lib/visor'

boot() {
    docker run --detach --name "$name" --network none --read-only \
        --tmpfs /run/visor:mode=0700,uid=1000,gid=1000 \
        --tmpfs /tmp:mode=1777 --mount "type=volume,source=$volume,target=/var/lib/visor" "$1" >/dev/null
    for attempt in {1..30}; do
        if docker exec "$name" /usr/bin/hypermesh-visor-headless worker-status; then
            return
        fi
        sleep 1
    done
    docker logs "$name"
    return 1
}

stop_worker() {
    docker stop --time 25 "$name" >/dev/null
    [[ "$(docker inspect --format '{{.State.ExitCode}}' "$name")" == 0 ]]
    docker rm "$name" >/dev/null
}

boot visor-cloud-worker:ci
docker exec "$name" /usr/bin/hypermesh-visor-headless worker-status | jq -e '.tools | keys == ["bash"]'
docker exec "$name" sh -eu -c '
    test "$(id -u)" = 1000
    test ! -e "$HYPERMESH_SESSION_SECRET_FILE"
    test -z "${HYPERMESH_BOT_CONFIG:-}"
    if command -v git || command -v shellcheck; then
        exit 1
    fi
    if apt-get --simulate --no-download install visor-ci-unavailable-package; then
        exit 1
    fi
    echo workspace-fixture > "$HOME/workspace-marker"
'
stop_worker

boot visor-cloud-worker-coding:ci
docker exec "$name" /usr/bin/hypermesh-visor-headless worker-status | jq -e '.tools | keys == ["bash", "git", "shellcheck"]'
docker exec "$name" bash -euc '
    test "$(cat "$HOME/workspace-marker")" = workspace-fixture
    git init --quiet "$HOME/repo"
    printf "#!/bin/sh\nprintf \"%%s\\\\n\" hello\n" > "$HOME/repo/fixture.sh"
    shellcheck "$HOME/repo/fixture.sh"
    test "$(git -C "$HOME/repo" rev-parse --is-inside-work-tree)" = true
'
stop_worker

# Removing the pack switches the executable image back to base.
boot visor-cloud-worker:ci
docker exec "$name" /usr/bin/hypermesh-visor-headless worker-status | jq -e '.tools | keys == ["bash"]'
docker exec "$name" sh -eu -c '
    if command -v git || command -v shellcheck; then
        exit 1
    fi
    test "$(cat "$HOME/workspace-marker")" = workspace-fixture
    test -f "$HOME/repo/fixture.sh"
'
stop_worker
