#!/usr/bin/env bash
# Проверяет подписанный бандл shellio.app.
#
#   check-bundle.sh [<бандл>]         подпись, requirement, Info.plist, раскладка, бинарь и самопроверка
#   check-bundle.sh --open [<бандл>]  самопроверка через open -W -n с временным SHELLIO_STATE_DIR и проверка вывода
#
# Бандл по умолчанию — target/bundle/shellio.app. Ресурсы сверяются с манифестом опубликованного набора,
# версия ghostty — с его штампом; набор только читается, блокировку держит цель make.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"

BUNDLE_ID="io.github.dsbasko.shellio"
vendor="${SHELLIO_GHOSTTY_VENDOR:-$root/vendor/ghostty}"
requirement="$root/scripts/signing/requirement.txt"

# Каталоги, которые самопроверка получает временными; после выхода они должны остаться пустыми.
ISOLATED=(HOME XDG_CONFIG_HOME XDG_STATE_HOME XDG_CACHE_HOME XDG_DATA_HOME SHELLIO_STATE_DIR)

die() {
  echo "error: $*" >&2
  exit 1
}

mode=check
if [[ "${1:-}" == "--open" ]]; then
  mode=open
  shift
fi
[[ $# -le 1 ]] || {
  echo "usage: $0 [--open] [<бандл>]" >&2
  exit 2
}
app="${1:-$root/target/bundle/shellio.app}"
[[ -d "$app" ]] || die "нет бандла $app: соберите его через make bundle"
app="$(cd "$app" && pwd -P)"
contents="$app/Contents"
resources="$contents/Resources"
exe="$contents/MacOS/shellio"
plist="$contents/Info.plist"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/shellio-check-bundle.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

# Ожидаемые значения не бывают пустыми: пустое совпало бы с отсутствующим ключом.
app_version="$(awk '
  /^\[/ { in_section = ($0 == "[workspace.package]") }
  in_section && /^version *= *"/ { sub(/^version *= *"/, ""); sub(/".*/, ""); print; exit }
' "$root/Cargo.toml")"
[[ -n "$app_version" ]] || die "в [workspace.package] Cargo.toml нет version"
min_macos="$(sed -n 's/^MACOSX_DEPLOYMENT_TARGET = { value = "\([0-9.]*\)".*/\1/p' "$root/.cargo/config.toml")"
[[ -n "$min_macos" ]] || die "в .cargo/config.toml нет MACOSX_DEPLOYMENT_TARGET"
[[ -f "$vendor/stamp" ]] || die "нет штампа набора $vendor/stamp"
ghostty_version="$(sed -n 's/^ghostty_version_string=//p' "$vendor/stamp")"
[[ -n "$ghostty_version" ]] || die "в штампе $vendor/stamp нет ghostty_version_string"

# Значение ключа plist или JSON; пустая строка, если ключа нет.
extract() {
  plutil -extract "$1" raw -o - "$2" 2>/dev/null || true
}

expect_equal() {
  local what="$1" got="$2" want="$3"
  [[ "$got" == "$want" ]] || die "$what: «${got}», ожидалось «${want}»"
}

check_signature() {
  codesign --verify --strict --verbose=1 "$app" 2>"$tmp/verify.log" ||
    die "подпись бандла не проходит codesign --verify --strict: $(tr '\n' ' ' <"$tmp/verify.log")"
}

check_signer() {
  local details sha1 leaf got want

  details="$(codesign -d --verbose=4 "$app" 2>&1)" || die "codesign -d не читает подпись бандла"
  if grep -q '^Signature=adhoc$' <<<"$details" || grep -Eq '^CodeDirectory .*flags=0x[0-9a-f]*\([^)]*adhoc' <<<"$details"; then
    die "подпись бандла ad-hoc, а не идентичностью Shellio Dev"
  fi
  expect_equal "identifier подписи" "$(sed -n 's/^Identifier=//p' <<<"$details")" "$BUNDLE_ID"

  [[ -f "$requirement" ]] || die "нет $requirement"
  sha1="$(sed -n 's/.*certificate leaf = H"\([0-9a-fA-F]\{40\}\)".*/\1/p' "$requirement" | tr '[:upper:]' '[:lower:]')"
  [[ "$sha1" =~ ^[0-9a-f]{40}$ ]] || die "в $requirement нет SHA-1 сертификата"

  codesign -d --extract-certificates="$tmp/cert" "$app" 2>/dev/null ||
    die "codesign не извлекает сертификаты подписи"
  [[ -f "$tmp/cert0" ]] || die "в подписи бандла нет сертификата"
  leaf="$(shasum -a 1 <"$tmp/cert0")"
  expect_equal "SHA-1 сертификата подписи" "${leaf%% *}" "$sha1"
  codesign --verify --strict -R "=certificate leaf = H\"$sha1\"" "$app" 2>/dev/null ||
    die "подпись бандла не удовлетворяет requirement certificate leaf = H\"$sha1\""

  got="$(codesign -d -r- "$app" 2>&1 | sed -n 's/^designated => /&/p')"
  want="$(csreq -r "$requirement" -t)" || die "csreq не читает $requirement"
  expect_equal "designated requirement подписи" "$got" "$want"
}

check_plist() {
  plutil -lint "$plist" >/dev/null || die "Info.plist бандла не проходит plutil -lint"
  expect_equal "CFBundleIdentifier" "$(extract CFBundleIdentifier "$plist")" "$BUNDLE_ID"
  expect_equal "CFBundleExecutable" "$(extract CFBundleExecutable "$plist")" "shellio"
  expect_equal "CFBundlePackageType" "$(extract CFBundlePackageType "$plist")" "APPL"
  expect_equal "LSMinimumSystemVersion" "$(extract LSMinimumSystemVersion "$plist")" "$min_macos"
  expect_equal "CFBundleShortVersionString" "$(extract CFBundleShortVersionString "$plist")" "$app_version"
  expect_equal "CFBundleVersion" "$(extract CFBundleVersion "$plist")" "$app_version"
}

# Список «SHA-256  путь» обычных файлов каталога в порядке LC_ALL=C; другие элементы отвергаются.
list_files() {
  local dir="$1" odd
  odd="$(cd "$dir" && find . ! -type f ! -type d)"
  [[ -z "$odd" ]] || die "в $dir не обычные файлы: $(printf '%s' "$odd" | tr '\n' ' ')"
  (cd "$dir" && find . -type f -print0 | xargs -0 shasum -a 256) | sed 's|  \./|  |' | LC_ALL=C sort -k2
}

check_layout() {
  local entries

  entries="$(cd "$contents" && find . -mindepth 1 -maxdepth 1 | LC_ALL=C sort | tr '\n' ' ')"
  expect_equal "содержимое Contents" "$entries" "./Info.plist ./MacOS ./Resources ./_CodeSignature "
  entries="$(cd "$contents/MacOS" && find . -mindepth 1 | tr '\n' ' ')"
  expect_equal "содержимое Contents/MacOS" "$entries" "./shellio "
  [[ -f "$exe" ]] || die "нет исполняемого файла $exe"

  [[ -d "$resources/ghostty" ]] || die "нет каталога ресурсов ghostty $resources/ghostty"
  [[ -d "$resources/terminfo" ]] || die "нет каталога terminfo $resources/terminfo рядом с ресурсами ghostty"
  [[ -f "$resources/terminfo/78/xterm-ghostty" ]] || die "нет файла $resources/terminfo/78/xterm-ghostty"
  [[ -f "$resources/licenses/ghostty.txt" ]] || die "нет лицензии ghostty $resources/licenses/ghostty.txt"

  # Ресурсы бандла — ровно ресурсы набора и его лицензия, с теми же хэшами.
  [[ -f "$vendor/manifest" ]] || die "нет манифеста набора $vendor/manifest"
  awk '
    { sum = $1; path = substr($0, length($1) + 3) }
    path ~ /^share\/ghostty\// { print sum "  ghostty/" substr(path, 15); next }
    path ~ /^share\/terminfo\// { print sum "  terminfo/" substr(path, 16); next }
    path == "LICENSE" { print sum "  licenses/ghostty.txt" }
  ' "$vendor/manifest" | LC_ALL=C sort -k2 >"$tmp/expected"
  list_files "$resources" >"$tmp/actual"
  if ! diff -u "$tmp/expected" "$tmp/actual" >"$tmp/resources.diff"; then
    echo "ресурсы бандла расходятся с набором (- набор, + бандл):" >&2
    tail -n +3 "$tmp/resources.diff" | grep '^[-+]' | head -20 >&2 || true
    die "ресурсы бандла не совпадают с манифестом набора $vendor/manifest"
  fi
}

check_binary() {
  local build minos deps foreign

  expect_equal "архитектуры бинаря" "$(lipo -archs "$exe")" "arm64"
  build="$(xcrun vtool -show-build "$exe")" || die "vtool не читает $exe"
  minos="$(awk '$1 == "minos" { print $2; exit }' <<<"$build")"
  expect_equal "minos бинаря" "$minos" "$min_macos"

  deps="$(otool -L "$exe" | tail -n +2 | awk '{ print $1 }')"
  [[ -n "$deps" ]] || die "otool -L не показал зависимостей бинаря"
  foreign="$(printf '%s\n' "$deps" | grep -v -e '^/System/' -e '^/usr/lib/' || true)"
  [[ -z "$foreign" ]] || die "бинарь зависит от библиотек вне /System и /usr/lib: $(printf '%s' "$foreign" | tr '\n' ' ')"
}

# Строку самопроверки проверяют оба способа запуска: прямой и через open.
check_report() {
  local out="$1" err="$2"

  [[ ! -s "$err" ]] || die "самопроверка писала в stderr: $(tr '\n' ' ' <"$err")"
  [[ -s "$out" ]] || die "самопроверка ничего не вывела"
  [[ "$(wc -l <"$out" | tr -d ' ')" == 1 ]] || die "самопроверка вывела не одну строку: $(cat "$out")"
  expect_equal "app_version самопроверки" "$(extract app_version "$out")" "$app_version"
  expect_equal "ghostty_version самопроверки" "$(extract ghostty_version "$out")" "$ghostty_version"
  expect_equal "ghostty_build_mode самопроверки" "$(extract ghostty_build_mode "$out")" ReleaseFast
  expect_equal "ghostty_resources самопроверки" "$(extract ghostty_resources "$out")" "$resources/ghostty"
  expect_equal "terminfo самопроверки" "$(extract terminfo "$out")" "$resources/terminfo"
}

expect_empty() {
  local name="$1" dir="$2" entries
  entries="$(find "$dir" -mindepth 1 | tr '\n' ' ')"
  [[ -z "$entries" ]] || die "самопроверка оставила файлы в $name: $entries"
}

check_self_check() {
  local name status=0 env_args=()

  for name in "${ISOLATED[@]}"; do
    mkdir -p "$tmp/isolated/$name"
    env_args+=("$name=$tmp/isolated/$name")
  done
  (cd "$tmp" && env -i "${env_args[@]}" "$exe" --self-check >"$tmp/stdout" 2>"$tmp/stderr") || status=$?
  [[ $status -eq 0 ]] || die "самопроверка завершилась с кодом $status: $(tr '\n' ' ' <"$tmp/stderr")"
  check_report "$tmp/stdout" "$tmp/stderr"
  for name in "${ISOLATED[@]}"; do
    expect_empty "$name" "$tmp/isolated/$name"
  done
}

# open не передаёт код выхода приложения, поэтому итог судится по выводу и пустому каталогу состояния.
# Самопроверка выходит раньше, чем open начинает её ждать, и open пишет «Unable to block on application»
# в свой stderr; его вывод показывается только при ошибке самого open. Если самопроверка вышла раньше, чем
# open подписался на её завершение, kevent отвечает ESRCH и open возвращает 1, хотя приложение
# отработало: такой отказ тоже судится по выводу.
OPEN_EXITED_EARLY="initial call to kevent() failed: No such process"

check_open() {
  local state="$tmp/state" status=0

  mkdir -p "$state"
  : >"$tmp/stdout"
  : >"$tmp/stderr"
  open -W -n --env "SHELLIO_STATE_DIR=$state" --stdout "$tmp/stdout" --stderr "$tmp/stderr" "$app" \
    --args --self-check >"$tmp/open.log" 2>&1 || status=$?
  if [[ $status -ne 0 ]] && ! { [[ $status -eq 1 ]] && grep -qF "$OPEN_EXITED_EARLY" "$tmp/open.log"; }; then
    die "open завершился с кодом $status: $(tr '\n' ' ' <"$tmp/open.log")"
  fi
  check_report "$tmp/stdout" "$tmp/stderr"
  expect_empty SHELLIO_STATE_DIR "$state"
}

check_signature
case "$mode" in
  check)
    check_signer
    check_plist
    check_layout
    check_binary
    check_self_check
    echo "бандл $app прошёл проверку: подпись Shellio Dev, requirement, раскладка, бинарь и самопроверка"
    ;;
  open)
    check_open
    echo "бандл $app запущен через open, самопроверка успешна: $(cat "$tmp/stdout")"
    ;;
esac
