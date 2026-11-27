#!/usr/bin/env bash
set -euo pipefail

patches=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
jobs=${BUILD_JOBS:-4}
export LC_ALL=C.UTF-8

if [[ -n ${VISOR_TAKEOVER_EXPORT_DIR:-} ]]; then
    repo=$(cd "$patches/../.." && pwd)
    candidate_sha=$(git -c "safe.directory=$repo" -C "$repo" rev-parse --verify HEAD)
    if [[ ! $candidate_sha =~ ^[0-9a-f]{40}$ || $candidate_sha != "${VISOR_TAKEOVER_SOURCE_SHA:-}" ]]; then
        echo "The ARM64 bundle requires an exact matching Visor source commit" >&2
        exit 1
    fi
    test "$(dpkg --print-architecture)" = arm64
fi

checkout() {
    local name=$1 repository=$2 revision=$3
    local source="$scratch/$name"
    git init -q "$source"
    git -C "$source" fetch --quiet --depth 1 "$repository" "$revision"
    git -C "$source" -c advice.detachedHead=false checkout --quiet --detach FETCH_HEAD
    test "$(git -C "$source" rev-parse HEAD)" = "$revision"
    git -C "$source" apply --check "$patches/$name.patch"
    git -C "$source" apply "$patches/$name.patch"
}

checkout kwin https://github.com/KDE/kwin.git b8de4329447824b1b1e7a36b3a57acfd069f1423
checkout portal-kde https://github.com/KDE/xdg-desktop-portal-kde.git 6eb375f48e7330b7e5c57143f3014978b31cab3c
checkout portal https://github.com/flatpak/xdg-desktop-portal.git 23a76c392170dbbd26230f85ef56c3a57e52b857

meson setup "$scratch/portal-build" "$scratch/portal" \
    -Ddocumentation=disabled -Dman-pages=disabled -Dtests=enabled
meson compile -C "$scratch/portal-build" --jobs "$jobs"
BUILDDIR="$scratch/portal-build" "$scratch/portal/tests/run-test.sh" \
    "$scratch/portal/tests/test_remotedesktop.py" -q -k 'version or connect_to_eis'

cmake -S "$scratch/portal-kde" -B "$scratch/portal-kde-build" -G Ninja \
    -DBUILD_TESTING=OFF -DCMAKE_DISABLE_FIND_PACKAGE_Git=TRUE -DCMAKE_BUILD_TYPE=Release
cmake --build "$scratch/portal-kde-build" --target xdg-desktop-portal-kde --parallel "$jobs"

cmake -S "$scratch/kwin" -B "$scratch/kwin-build" -G Ninja \
    -DBUILD_TESTING=OFF -DCMAKE_DISABLE_FIND_PACKAGE_Git=TRUE -DCMAKE_BUILD_TYPE=Release \
    '-DCMAKE_CXX_FLAGS_RELEASE=-O0 -DNDEBUG' -DKWIN_BUILD_KCMS=OFF -DKWIN_BUILD_RUNNERS=OFF \
    -DKWIN_BUILD_LOCAL_TAKEOVER_TEST=ON
cmake --build "$scratch/kwin-build" --target eis eisLocalTakeoverContract --parallel "$jobs"
dbus-run-session -- "$scratch/kwin-build/bin/eisLocalTakeoverContract"

if [[ -n ${VISOR_TAKEOVER_EXPORT_DIR:-} ]]; then
    cmake --build "$scratch/kwin-build" --target kwin_wayland --parallel "$jobs"

    stage="$scratch/visor-kde-candidate"
    mkdir "$stage"
    install -m 0755 "$scratch/kwin-build/bin/kwin_wayland" "$stage/"
    install -m 0755 "$scratch/kwin-build/bin/libkwin.so.6" "$stage/"
    install -m 0755 "$scratch/kwin-build/bin/kwin/plugins/eis.so" "$stage/"
    install -m 0755 "$scratch/portal-kde-build/bin/xdg-desktop-portal-kde" "$stage/"
    install -m 0755 "$scratch/portal-build/src/xdg-desktop-portal" "$stage/"
    for component in "$stage"/*; do
        readelf -h "$component" | grep -E 'Machine:[[:space:]]+AArch64$'
    done
    {
        printf 'visor %s\n' "$candidate_sha"
        for name in kwin portal-kde portal; do
            component_sha=$(git -C "$scratch/$name" rev-parse --verify HEAD)
            printf '%s %s\n' "$name" "$component_sha"
        done
    } > "$stage/SOURCE"
    (cd "$patches" && sha256sum kwin.patch portal-kde.patch portal.patch) > "$stage/PATCHES.sha256"
    (cd "$stage" && sha256sum SOURCE PATCHES.sha256 kwin_wayland libkwin.so.6 eis.so xdg-desktop-portal-kde xdg-desktop-portal > SHA256SUMS)
    (cd "$stage" && sha256sum -c SHA256SUMS)

    mkdir -p "$VISOR_TAKEOVER_EXPORT_DIR"
    export_dir=$(cd "$VISOR_TAKEOVER_EXPORT_DIR" && pwd)
    tar --sort=name --owner=0 --group=0 --numeric-owner -C "$scratch" \
        -czf "$export_dir/visor-kde-candidate-arm64.tar.gz" visor-kde-candidate
    (cd "$export_dir" && sha256sum visor-kde-candidate-arm64.tar.gz > visor-kde-candidate-arm64.tar.gz.sha256)
fi
