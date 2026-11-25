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
expect_ok "копия бандла запускается через open" "$check" --open "$copy"

# Подменённый open запускает бинарь бандла напрямую с аргументами после --args и завершается кодом и
# сообщением из FAKE_OPEN_*: так воспроизводится гонка, в которой приложение выходит раньше, чем open
# подписывается на его завершение.
# FAKE_OPEN_STDOUT подменяет вывод приложения, FAKE_OPEN_STDERR дописывается в его stderr,
# FAKE_OPEN_TOUCH_STATE=1 оставляет файл в SHELLIO_STATE_DIR.
fake_open="$tmp/fake-open"
mkdir -p "$fake_open"
cat >"$fake_open/open" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
envs=()
app_args=()
app=""
out=/dev/null
err=/dev/null
while [[ $# -gt 0 ]]; do
  case "$1" in
    -W | -n) shift ;;
    --env) envs+=("$2") && shift 2 ;;
    --stdout) out="$2" && shift 2 ;;
    --stderr) err="$2" && shift 2 ;;
    --args) shift && app_args=("$@") && break ;;
    -*) echo "fake open: неизвестный аргумент $1" >&2 && exit 2 ;;
    *)
      [[ -z "$app" ]] || { echo "fake open: лишний аргумент $1" >&2 && exit 2; }
      app="$1" && shift
      ;;
  esac
done
[[ -n "$app" ]] || { echo "fake open: не задан бандл" >&2 && exit 2; }
# Без самопроверки бинарь стал бы владельцем и не вышел бы: прогон завис бы, а не упал.
[[ "${app_args[*]-}" == --self-check ]] || { echo "fake open: ожидались --args --self-check" >&2 && exit 2; }
if [[ "$FAKE_OPEN_RUN" == 1 ]]; then
  env ${envs[@]+"${envs[@]}"} "$app/Contents/MacOS/shellio" "${app_args[@]}" >"$out" 2>"$err" || true
fi
[[ -z "${FAKE_OPEN_STDOUT:-}" ]] || printf '%s\n' "$FAKE_OPEN_STDOUT" >"$out"
[[ -z "${FAKE_OPEN_STDERR:-}" ]] || printf '%s\n' "$FAKE_OPEN_STDERR" >>"$err"
if [[ "${FAKE_OPEN_TOUCH_STATE:-}" == 1 ]]; then
  for kv in ${envs[@]+"${envs[@]}"}; do
    [[ "$kv" != SHELLIO_STATE_DIR=* ]] || : >"${kv#SHELLIO_STATE_DIR=}/state"
  done
fi
echo "$FAKE_OPEN_MESSAGE" >&2
exit "$FAKE_OPEN_STATUS"
EOF
chmod +x "$fake_open/open"

early="Unable to block on applications (initial call to kevent() failed: No such process)"
PATH="$fake_open:$PATH" FAKE_OPEN_RUN=1 FAKE_OPEN_STATUS=1 FAKE_OPEN_MESSAGE="$early" \
  expect_ok "код 1 open после раннего выхода приложения судится по выводу" "$check" --open "$copy"
PATH="$fake_open:$PATH" FAKE_OPEN_RUN=0 FAKE_OPEN_STATUS=1 FAKE_OPEN_MESSAGE="$early" \
  expect_fail "ранний выход без вывода приложения отвергается" "самопроверка ничего не вывела" "$check" --open "$copy"
PATH="$fake_open:$PATH" FAKE_OPEN_RUN=1 FAKE_OPEN_STATUS=1 FAKE_OPEN_MESSAGE="LSOpenURLsWithRole() failed" \
  expect_fail "другой отказ open отвергается" "open завершился с кодом 1" "$check" --open "$copy"
PATH="$fake_open:$PATH" FAKE_OPEN_RUN=1 FAKE_OPEN_STATUS=2 FAKE_OPEN_MESSAGE="$early" \
  expect_fail "код open кроме 1 отвергается и с сообщением о раннем выходе" "open завершился с кодом 2" "$check" --open "$copy"
PATH="$fake_open:$PATH" FAKE_OPEN_RUN=1 FAKE_OPEN_STATUS=0 FAKE_OPEN_MESSAGE= FAKE_OPEN_STDERR="warning" \
  expect_fail "вывод приложения в stderr отвергается" "самопроверка писала в stderr" "$check" --open "$copy"
PATH="$fake_open:$PATH" FAKE_OPEN_RUN=0 FAKE_OPEN_STATUS=0 FAKE_OPEN_MESSAGE= FAKE_OPEN_STDOUT=$'{}\n{}' \
  expect_fail "вывод не одной строкой отвергается" "вывела не одну строку" "$check" --open "$copy"
PATH="$fake_open:$PATH" FAKE_OPEN_RUN=0 FAKE_OPEN_STATUS=0 FAKE_OPEN_MESSAGE= FAKE_OPEN_STDOUT='{"app_version":"0.0.0"}' \
  expect_fail "неверное значение в выводе отвергается" "app_version самопроверки" "$check" --open "$copy"
PATH="$fake_open:$PATH" FAKE_OPEN_RUN=1 FAKE_OPEN_STATUS=0 FAKE_OPEN_MESSAGE= FAKE_OPEN_TOUCH_STATE=1 \
  expect_fail "файл в SHELLIO_STATE_DIR отвергается" "оставила файлы в SHELLIO_STATE_DIR" "$check" --open "$copy"

copy="$(copy_app no-terminfo)"
rm -rf "$copy/Contents/Resources/terminfo"
expect_ok "бандл без terminfo подписывается настоящей идентичностью" "$sign" "$copy"
expect_ok "подпись бандла без terminfo проходит codesign --verify --strict" codesign --verify --strict "$copy"
expect_fail "бандл без terminfo отвергается" "нет каталога terminfo" "$check" "$copy"

copy="$(copy_app adhoc)"
codesign --force --sign - "$copy" >"$out" 2>&1 || fail "ad-hoc подпись копии не удалась"
expect_ok "ad-hoc подпись проходит codesign --verify --strict" codesign --verify --strict "$copy"
expect_fail "бандл с ad-hoc подписью отвергается" "подпись бандла ad-hoc" "$check" "$copy"

# Настоящий сертификат, но requirement подписи не тот, что в requirement.txt.
copy="$(copy_app other-requirement)"
sha1="$(sed -n 's/.*certificate leaf = H"\([0-9a-fA-F]\{40\}\)".*/\1/p' "$here/signing/requirement.txt")"
codesign --force --sign "$sha1" ${SHELLIO_SIGNING_KEYCHAIN:+--keychain "$SHELLIO_SIGNING_KEYCHAIN"} \
  --identifier io.github.dsbasko.shellio --timestamp=none \
  -r '=designated => identifier "io.github.dsbasko.shellio"' "$copy" >"$out" 2>&1 ||
  fail "подпись копии с другим requirement не удалась"
expect_fail "подпись с другим requirement отвергается" "designated requirement подписи" "$check" "$copy"

copy="$(copy_app extra-executable)"
# Вложенный код подписывается до бандла; копия основного бинаря привязана к Info.plist и не годится.
cp /usr/bin/true "$copy/Contents/MacOS/helper"
expect_ok "лишний исполняемый файл подписывается настоящей идентичностью" "$sign" "$copy/Contents/MacOS/helper"
expect_ok "бандл с лишним файлом в Contents/MacOS подписывается настоящей идентичностью" "$sign" "$copy"
expect_fail "лишний файл в Contents/MacOS отвергается" "содержимое Contents/MacOS" "$check" "$copy"

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

copy="$(copy_app no-font)"
rm "$copy/Contents/Resources/Fonts/JetBrainsMono-Regular.ttf"
expect_ok "бандл без шрифта подписывается настоящей идентичностью" "$sign" "$copy"
expect_fail "бандл без шрифта отвергается: продукт молча взял бы запасной" "нет шрифта" "$check" "$copy"

# Подмена — годный шрифт под чужим именем: по раскладке и подписи её не видно, только по SHA-256.
copy="$(copy_app replaced-font)"
cp "$copy/Contents/Resources/Fonts/JetBrainsMono-Bold.ttf" "$copy/Contents/Resources/Fonts/JetBrainsMono-Regular.ttf"
expect_ok "бандл с подменённым шрифтом подписывается настоящей идентичностью" "$sign" "$copy"
expect_fail "подменённый шрифт отвергается" "подменён: SHA-256" "$check" "$copy"

copy="$(copy_app extra-font)"
cp "$copy/Contents/Resources/Fonts/JetBrainsMono-Bold.ttf" "$copy/Contents/Resources/Fonts/Extra.ttf"
expect_ok "бандл с лишним шрифтом подписывается настоящей идентичностью" "$sign" "$copy"
expect_fail "лишний шрифт отвергается" "ресурсы бандла не совпадают с манифестом" "$check" "$copy"

copy="$(copy_app no-font-licence)"
rm "$copy/Contents/Resources/licenses/JetBrainsMono-OFL.txt"
expect_ok "бандл без лицензии шрифта подписывается настоящей идентичностью" "$sign" "$copy"
expect_fail "бандл без лицензии шрифта отвергается" "нет лицензии шрифта" "$check" "$copy"

copy="$(copy_app no-fonts-path)"
plutil -remove ATSApplicationFontsPath "$copy/Contents/Info.plist"
expect_ok "бандл без ATSApplicationFontsPath подписывается настоящей идентичностью" "$sign" "$copy"
expect_fail "бандл без ATSApplicationFontsPath отвергается" "ATSApplicationFontsPath" "$check" "$copy"

copy="$(copy_app bundle-id)"
plutil -replace CFBundleIdentifier -string io.github.dsbasko.other "$copy/Contents/Info.plist"
expect_ok "бандл с чужим bundle ID подписывается настоящей идентичностью" "$sign" "$copy"
expect_fail "чужой bundle ID отвергается" "CFBundleIdentifier" "$check" "$copy"

copy="$(copy_app no-ui-element)"
plutil -remove LSUIElement "$copy/Contents/Info.plist"
expect_ok "бандл без LSUIElement подписывается настоящей идентичностью" "$sign" "$copy"
expect_fail "бандл без LSUIElement отвергается: второй запуск появился бы в Dock" "LSUIElement" "$check" "$copy"

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
