#!/usr/bin/env bash
# Собирает и подписывает target/bundle/shellio.app из release-сборки и опубликованного набора ghostty.
#
# Раскладка: Contents/MacOS/shellio, Contents/Info.plist, Contents/Resources/ghostty/ и terminfo/ рядом
# с ним (ADR-0003), Contents/Resources/licenses/ghostty.txt. Подпись — scripts/signing/sign.sh.
# Набор только читается; блокировку держит цель make bundle.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"

vendor="${SHELLIO_GHOSTTY_VENDOR:-$root/vendor/ghostty}"
cargo="${CARGO:-$HOME/.cargo/bin/cargo}"

die() {
  echo "error: $*" >&2
  exit 1
}

version="$(awk '
  /^\[/ { in_section = ($0 == "[workspace.package]") }
  in_section && /^version *= *"/ { sub(/^version *= *"/, ""); sub(/".*/, ""); print; exit }
' "$root/Cargo.toml")"
[[ -n "$version" ]] || die "в [workspace.package] Cargo.toml нет version"

"$cargo" build --release --locked -p shellio --manifest-path "$root/Cargo.toml"

target_dir="$("$cargo" metadata --format-version 1 --no-deps --locked --manifest-path "$root/Cargo.toml" |
  plutil -extract target_directory raw -o - -)" || die "cargo metadata не назвал каталог сборки"
exe="$target_dir/release/shellio"
[[ -f "$exe" ]] || die "после сборки нет $exe"

"$root/scripts/ghostty/vendor.sh" --verify

out="$target_dir/bundle"
app="$out/shellio.app"
mkdir -p "$out"
staging="$(mktemp -d "$out/.staging.XXXXXX")"
trap 'rm -rf "$staging"' EXIT

stage="$staging/shellio.app"
contents="$stage/Contents"
resources="$contents/Resources"
mkdir -p "$contents/MacOS" "$resources/licenses"

# Каталоги копируются целиком, со скрытыми файлами вроде shell-integration/zsh/.zshenv, и без
# расширенных атрибутов: codesign отвергает Finder info и resource fork.
copy() {
  ditto --norsrc --noextattr --noacl "$1" "$2" || die "не удалось скопировать $1"
}
copy "$exe" "$contents/MacOS/shellio"
copy "$vendor/share/ghostty" "$resources/ghostty"
copy "$vendor/share/terminfo" "$resources/terminfo"
copy "$vendor/LICENSE" "$resources/licenses/ghostty.txt"

plist="$contents/Info.plist"
copy "$root/apps/shellio/Info.plist" "$plist"
plutil -replace CFBundleShortVersionString -string "$version" "$plist"
plutil -replace CFBundleVersion -string "$version" "$plist"
plutil -lint "$plist" >/dev/null || die "Info.plist бандла не проходит plutil -lint"

"$root/scripts/signing/sign.sh" "$stage"

rm -rf "$app"
mv "$stage" "$app"
echo "бандл $app собран и подписан"
