#!/usr/bin/env bash
set -euo pipefail

patches=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
jobs=${BUILD_JOBS:-4}

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

cmake -S "$scratch/kwin" -B "$scratch/kwin-build" -G Ninja \
    -DBUILD_TESTING=OFF -DCMAKE_BUILD_TYPE=Release '-DCMAKE_CXX_FLAGS_RELEASE=-O1 -DNDEBUG' \
    -DKWIN_BUILD_KCMS=OFF -DKWIN_BUILD_RUNNERS=OFF -DKWIN_BUILD_LOCAL_TAKEOVER_TEST=ON
cmake --build "$scratch/kwin-build" --target eis eisLocalTakeoverContract --parallel "$jobs"
dbus-run-session -- "$scratch/kwin-build/bin/eisLocalTakeoverContract"

cmake -S "$scratch/portal-kde" -B "$scratch/portal-kde-build" -G Ninja \
    -DBUILD_TESTING=OFF -DCMAKE_BUILD_TYPE=Release
cmake --build "$scratch/portal-kde-build" --target xdg-desktop-portal-kde --parallel "$jobs"

meson setup "$scratch/portal-build" "$scratch/portal" \
    -Ddocumentation=disabled -Dman-pages=disabled -Dtests=enabled
meson compile -C "$scratch/portal-build" --jobs "$jobs"
BUILDDIR="$scratch/portal-build" "$scratch/portal/tests/run-test.sh" \
    "$scratch/portal/tests/test_remotedesktop.py" -q -k 'version or connect_to_eis'
