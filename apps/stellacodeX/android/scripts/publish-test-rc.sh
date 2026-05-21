#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/publish-test-rc.sh <version-name> [version-code]

Publishes an Android test rc using the existing dist channel:
  - updates app/build.gradle.kts versionCode/versionName
  - runs gradle :app:assembleRelease
  - copies app-release.apk to dist/stellacodex-android-test.apk
  - copies app-release.apk to dist/stellacodex-android-v<version-name>.apk

Examples:
  scripts/publish-test-rc.sh 0.2.0-rc.2
  scripts/publish-test-rc.sh 0.2.0-rc.2 66
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

version_name="${1:-}"
if [[ -z "$version_name" ]]; then
  usage >&2
  exit 2
fi
if [[ ! "$version_name" =~ -rc\.[0-9]+$ ]]; then
  echo "error: test releases must use an rc version name, got '$version_name'" >&2
  exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
project_dir="$(cd "$script_dir/.." && pwd)"
build_file="$project_dir/app/build.gradle.kts"
release_apk="$project_dir/app/build/outputs/apk/release/app-release.apk"
test_apk="$project_dir/dist/stellacodex-android-test.apk"
archive_apk="$project_dir/dist/stellacodex-android-v${version_name}.apk"

current_code="$(sed -nE 's/^[[:space:]]*versionCode[[:space:]]*=[[:space:]]*([0-9]+).*$/\1/p' "$build_file" | head -n1)"
if [[ -z "$current_code" ]]; then
  echo "error: could not read versionCode from $build_file" >&2
  exit 1
fi
version_code="${2:-$((current_code + 1))}"
if [[ ! "$version_code" =~ ^[0-9]+$ ]]; then
  echo "error: version-code must be numeric, got '$version_code'" >&2
  exit 2
fi
if (( version_code <= current_code )); then
  echo "error: version-code $version_code must be greater than current $current_code" >&2
  exit 2
fi

perl -0pi -e "s/versionCode\s*=\s*\d+/versionCode = $version_code/; s/versionName\s*=\s*\"[^\"]+\"/versionName = \"$version_name\"/" "$build_file"

cd "$project_dir"
gradle :app:assembleRelease

mkdir -p "$project_dir/dist"
cp "$release_apk" "$test_apk"
cp "$release_apk" "$archive_apk"

if command -v aapt >/dev/null 2>&1; then
  aapt dump badging "$test_apk" 2>/dev/null | sed -n '1p'
fi
sha256sum "$test_apk" "$archive_apk" "$release_apk"

echo "Published test rc $version_name ($version_code)"
echo "  $test_apk"
echo "  $archive_apk"
