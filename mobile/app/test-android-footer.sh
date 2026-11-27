#!/usr/bin/env bash
set -euo pipefail

apk=$(realpath "${1:?Pass the unsigned Visor release APK}")
test -f "$apk"
work=$(mktemp -d "${RUNNER_TEMP:?Use the CI temporary directory}/visor-android-footer.XXXXXX")
export ANDROID_AVD_HOME="$work"
sdk="$ANDROID_HOME/cmdline-tools/latest/bin"
emulator="$ANDROID_HOME/emulator/emulator"
adb=("$ANDROID_HOME/platform-tools/adb" -s emulator-5554)
package=sh.hyperme.visor.mobile
system_image='system-images;android-35;google_apis;x86_64'
emulator_pid=

# Called by the EXIT trap even when the install or check fails.
# shellcheck disable=SC2329
cleanup() {
  status=$?
  if (( status != 0 )) && [[ -n "$emulator_pid" ]]; then
    timeout 10s "${adb[@]}" logcat -d -s AndroidRuntime chromium Capacitor || true
    tail -80 "$work/emulator.log"
  fi
  if [[ -n "$emulator_pid" ]]; then
    kill "$emulator_pid" 2>/dev/null || true
    wait "$emulator_pid" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap 'cleanup' EXIT

if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
  sudo chgrp "$(id -gn)" /dev/kvm
  sudo chmod g+rw /dev/kvm
fi
"$sdk/sdkmanager" emulator "$system_image"
printf 'no\n' | "$sdk/avdmanager" create avd --name visor-footer --package "$system_image"
"$emulator" -avd visor-footer -port 5554 -accel on -no-window -no-audio \
  -no-boot-anim -no-snapshot -gpu swiftshader_indirect >"$work/emulator.log" 2>&1 &
emulator_pid=$!

timeout 180s "${adb[@]}" wait-for-device
for ((attempt=0; attempt<90; attempt++)); do
  if [[ $("${adb[@]}" shell getprop sys.boot_completed) == 1 ]]; then break; fi
  sleep 2
done
test "$("${adb[@]}" shell getprop sys.boot_completed)" = 1
"${adb[@]}" shell getprop ro.product.cpu.abilist | grep -Eq '(^|,)arm64-v8a(,|$)'
"${adb[@]}" shell input keyevent KEYCODE_WAKEUP
"${adb[@]}" shell wm dismiss-keyguard
"${adb[@]}" shell wm size 1080x1920
"${adb[@]}" shell wm density 360

# Use a throwaway key to install the same release APK built by the previous CI step.
keytool -genkeypair -keystore "$work/ci.p12" -storepass android -keypass android \
  -alias visor-ci -dname 'CN=Visor CI' -keyalg RSA -keysize 2048 -validity 1
"$ANDROID_HOME/build-tools/35.0.0/apksigner" sign --ks "$work/ci.p12" \
  --ks-key-alias visor-ci --ks-pass pass:android --out "$work/visor.apk" "$apk"
"${adb[@]}" install "$work/visor.apk"
metadata=$("${adb[@]}" shell dumpsys package "$package")
version=$(sed -n 's/^[[:blank:]]*versionName=\([^[:space:]][^[:space:]]*\)$/\1/p' <<< "$metadata")
build=$(sed -n 's/^[[:blank:]]*versionCode=\([0-9][0-9]*\) .*/\1/p' <<< "$metadata")
test -n "$version"
[[ "$build" =~ ^[0-9]+$ ]]
expected="Version $version · Build $build"

"${adb[@]}" shell am start -W -n "$package/.MainActivity"
for ((attempt=0; attempt<10; attempt++)); do
  if timeout 15s "${adb[@]}" shell uiautomator dump --compressed /sdcard/visor-footer.xml &&
     "${adb[@]}" pull /sdcard/visor-footer.xml "$work/ui.xml" >/dev/null &&
     python3 - "$work/ui.xml" "$package" "$expected" <<'PY'
import re
import sys
from xml.etree import ElementTree

path, package, expected = sys.argv[1:]
for node in ElementTree.parse(path).iter("node"):
    if node.get("package") != package or expected not in (node.get("text"), node.get("content-desc")):
        continue
    bounds = re.fullmatch(r"\[(\d+),(\d+)\]\[(\d+),(\d+)\]", node.get("bounds", ""))
    if bounds:
        left, top, right, bottom = map(int, bounds.groups())
        if left < right and top < bottom:
            sys.exit(0)
sys.exit(1)
PY
  then
    printf 'Verified installed Android footer: %s\n' "$expected" | tee -a "$GITHUB_STEP_SUMMARY"
    exit 0
  fi
  "${adb[@]}" shell input swipe 540 1510 540 670 350
done
printf 'Android footer did not show %s\n' "$expected" >&2
[[ ! -f "$work/ui.xml" ]] || cat "$work/ui.xml"
exit 1
