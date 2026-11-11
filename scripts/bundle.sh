#!/usr/bin/env bash
# Собирает и подписывает target/bundle/shellio.app из release-сборки и опубликованного набора ghostty.
#
# Раскладка: Contents/MacOS/shellio, Contents/Info.plist, Contents/Resources/ghostty/ и terminfo/ рядом
# с ним (ADR-0003), Contents/Resources/licenses/ghostty.txt. Подпись — scripts/signing/sign.sh.
# Набор только читается; блокировку держит цель make bundle.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
# rustup и Cargo ищут rust-toolchain.toml и .cargo/config.toml от текущего каталога, а не от --manifest-path.
cd "$root"

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

# Путь бинаря берётся из сообщения Cargo о собранном артефакте: при build.target или CARGO_BUILD_TARGET он лежит
# в каталоге цели, а не в release/, и там может остаться старый бинарь.
messages="$("$cargo" build --release --locked -p shellio --manifest-path "$root/Cargo.toml" \
  --message-format=json-render-diagnostics)" || die "cargo build завершился ошибкой"
exe=""
while IFS= read -r line; do
  [[ "$line" == *'"reason":"compiler-artifact"'* && "$line" != *'"executable":null'* ]] || continue
  [[ "$(plutil -extract target.name raw -o - - <<<"$line")" == shellio ]] || continue
  [[ "$(plutil -extract target.kind.0 raw -o - - <<<"$line")" == bin ]] || continue
  [[ -z "$exe" ]] || die "Cargo сообщил больше одного бинаря shellio"
  exe="$(plutil -extract executable raw -o - - <<<"$line")" || die "в сообщении Cargo о бинаре shellio нет пути"
done <<<"$messages"
[[ -n "$exe" ]] || die "Cargo не сообщил о собранном бинаре shellio"
[[ -f "$exe" ]] || die "после сборки нет $exe"

"$root/scripts/ghostty/vendor.sh" --verify

# Бандл лежит в target/ корня и при другом каталоге сборки Cargo: там его ищут check-bundle.sh и test-bundle.sh.
out="$root/target/bundle"
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
