#!/usr/bin/env bash
# Тест отказов check-bundle.sh на копиях собранного бандла.
#
# Копии переподписываются настоящей идентичностью Shellio Dev через sign.sh, поэтому тест нужен после
# make bundle и не входит в test-scripts. Сам target/bundle/shellio.app не меняется, тест это проверяет.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
check="$here/check-bundle.sh"
sign="$here/signing/sign.sh"

app="${1:-$root/target/bundle/shellio.app}"
[[ -d "$app" ]] || {
  echo "error: нет бандла $app: соберите его через make bundle" >&2
  exit 1
}

tmp="$(mktemp -d "${TMPDIR:-/tmp}/shellio-test-bundle.XXXXXX")"
tmp="$(cd "$tmp" && pwd -P)"
keychain="$tmp/empty.keychain-db"
cleanup() {
  [[ ! -f "$keychain" ]] || security delete-keychain "$keychain" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

out="$tmp/out"
checks=0
search_list_before="$(security list-keychains -d user)"
app_before="$(cd "$app" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 shasum -a 256 | shasum -a 256)"

fail() {
  echo "FAIL: $*" >&2
  [[ ! -s "$out" ]] || sed 's/^/  | /' "$out" >&2
  exit 1
}

ok() {
  checks=$((checks + 1))
  echo "ok: $*"
}

expect_ok() {
  local desc="$1"
  shift
  "$@" >"$out" 2>&1 || fail "$desc: команда завершилась ошибкой"
  ok "$desc"
}

# Отказ засчитывается только с ожидаемой причиной, чтобы случай не прошёл из-за чужой ошибки.
expect_fail() {
  local desc="$1" reason="$2"
  shift 2
  if "$@" >"$out" 2>&1; then
    fail "$desc: ожидался отказ"
  fi
  grep -qF -- "$reason" "$out" || fail "$desc: в выводе нет «${reason}»"
  ok "$desc"
}

copy_app() {
  local dir="$tmp/$1"
  mkdir -p "$dir"
  ditto "$app" "$dir/shellio.app"
  printf '%s\n' "$dir/shellio.app"
}

copy="$(copy_app baseline)"
expect_ok "копия бандла проходит check-bundle.sh" "$check" "$copy"

copy="$(copy_app no-terminfo)"
rm -rf "$copy/Contents/Resources/terminfo"
expect_ok "бандл без terminfo подписывается настоящей идентичностью" "$sign" "$copy"
expect_ok "подпись бандла без terminfo проходит codesign --verify --strict" codesign --verify --strict "$copy"
expect_fail "бандл без terminfo отвергается" "нет каталога terminfo" "$check" "$copy"

copy="$(copy_app adhoc)"
codesign --force --sign - "$copy" >"$out" 2>&1 || fail "ad-hoc подпись копии не удалась"
expect_ok "ad-hoc подпись проходит codesign --verify --strict" codesign --verify --strict "$copy"
expect_fail "бандл с ad-hoc подписью отвергается" "подпись бандла ad-hoc" "$check" "$copy"

copy="$(copy_app modified)"
printf '\n' >>"$copy/Contents/Resources/licenses/ghostty.txt"
expect_fail "файл, изменённый после подписи, отвергается" "не проходит codesign --verify --strict" "$check" "$copy"

copy="$(copy_app modified-binary)"
printf '\0' >>"$copy/Contents/MacOS/shellio"
expect_fail "бинарь, изменённый после подписи, отвергается" "не проходит codesign --verify --strict" "$check" "$copy"

copy="$(copy_app extra-resource)"
printf 'x\n' >"$copy/Contents/Resources/ghostty/extra"
expect_ok "бандл с лишним ресурсом подписывается настоящей идентичностью" "$sign" "$copy"
expect_fail "лишний ресурс отвергается" "ресурсы бандла не совпадают с манифестом" "$check" "$copy"

copy="$(copy_app changed-theme)"
theme="$(cd "$copy/Contents/Resources/ghostty/themes" && find . -type f | LC_ALL=C sort | head -1)"
[[ -n "$theme" ]] || fail "в бандле нет тем ghostty"
printf '\n' >>"$copy/Contents/Resources/ghostty/themes/$theme"
expect_ok "бандл с изменённой темой подписывается настоящей идентичностью" "$sign" "$copy"
expect_fail "ресурс, расходящийся с набором, отвергается" "ресурсы бандла не совпадают с манифестом" "$check" "$copy"

copy="$(copy_app bundle-id)"
plutil -replace CFBundleIdentifier -string io.github.dsbasko.other "$copy/Contents/Info.plist"
expect_ok "бандл с чужим bundle ID подписывается настоящей идентичностью" "$sign" "$copy"
expect_fail "чужой bundle ID отвергается" "CFBundleIdentifier" "$check" "$copy"

copy="$(copy_app no-identity)"
security create-keychain -p "kc-$(uuidgen)" "$keychain"
SHELLIO_SIGNING_KEYCHAIN="$keychain" expect_fail "sign.sh без идентичности отказывает" "нет идентичности подписи" "$sign" "$copy"
expect_ok "копия после отказа sign.sh сохраняет прежнюю подпись" "$check" "$copy"

[[ "$(security list-keychains -d user)" == "$search_list_before" ]] || fail "список поиска keychain изменился"
ok "список поиска keychain не изменился"
app_after="$(cd "$app" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 shasum -a 256 | shasum -a 256)"
[[ "$app_after" == "$app_before" ]] || fail "тест изменил $app"
ok "исходный бандл не изменился"

echo "test-bundle: $checks проверок прошли"
