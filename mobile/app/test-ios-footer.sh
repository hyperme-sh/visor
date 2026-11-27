#!/usr/bin/env bash
set -euo pipefail

device=$(xcrun simctl list devices available -j | python3 -c '
import json
import sys

for devices in json.load(sys.stdin)["devices"].values():
    for device in devices:
        if device["name"].startswith("iPhone") and device["isAvailable"]:
            print(device["udid"])
            sys.exit(0)
sys.exit("No available iPhone simulator")
')
destination="platform=iOS Simulator,id=$device"
derived="$RUNNER_TEMP/visor-capacitor-apple-iphonesimulator"
project=ios/App/App.xcodeproj
xcrun simctl bootstatus "$device" -b
xcodebuild -project "$project" -scheme App -configuration Release \
  -destination "$destination" -derivedDataPath "$derived" \
  ARCHS=arm64 ONLY_ACTIVE_ARCH=YES CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO \
  build-for-testing -quiet

app="$derived/Build/Products/Release-iphonesimulator/App.app"
bundle_id=$(plutil -extract CFBundleIdentifier raw "$app/Info.plist")
xcrun simctl install "$device" "$app"
installed=$(xcrun simctl get_app_container "$device" "$bundle_id" app)
version=$(plutil -extract CFBundleShortVersionString raw "$installed/Info.plist")
build=$(plutil -extract CFBundleVersion raw "$installed/Info.plist")
test -n "$version"
test -n "$build"
export TEST_RUNNER_VISOR_APP_ID="$bundle_id"
export TEST_RUNNER_VISOR_EXPECTED_FOOTER="Version $version · Build $build"

xcodebuild -project "$project" -scheme App -configuration Release \
  -destination "$destination" -derivedDataPath "$derived" \
  -parallel-testing-enabled NO -only-testing:PhoneFooterUITests test-without-building -quiet
printf 'Verified installed iOS footer: %s\n' "$TEST_RUNNER_VISOR_EXPECTED_FOOTER" | tee -a "$GITHUB_STEP_SUMMARY"
