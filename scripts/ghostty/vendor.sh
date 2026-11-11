#!/usr/bin/env bash
# Штамп, сверка и публикация набора libghostty (ADR-0003).
#
#   vendor.sh --print-stamp      ожидаемый штамп набора; только читает, сеть не нужна
#   vendor.sh --verify           сверяет опубликованный набор со штампом и манифестом; только читает
#   vendor.sh publish <каталог> <штамп>
#                                публикует набор из каталога сборки build.sh; только шаг make ghostty.
#                                <штамп> — вывод --print-stamp, снятый до сборки: если входы изменились
#                                за время сборки, публикация отказывает
#
# Корень набора — $SHELLIO_GHOSTTY_VENDOR, по умолчанию vendor/ghostty. Публикация требует маркер
# SHELLIO_GHOSTTY_LOCKED=1, который выставляет lock.sh. Маркер — соглашение, а не доказательство захвата.
# SHELLIO_GHOSTTY_TEST_ABORT_BEFORE_STAMP=1 прерывает публикацию перед записью штампа; только для test-publish.sh.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
# shellcheck source=pin
source "$here/pin"

vendor="${SHELLIO_GHOSTTY_VENDOR:-$root/vendor/ghostty}"
cache="${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}"
patch_dir="$root/scripts/ghostty-patches"

die() {
  echo "error: $*" >&2
  exit 1
}

usage() {
  echo "usage: $0 --print-stamp|--verify|publish <каталог> <штамп>" >&2
  exit 2
}

stamp=""

field() {
  [[ "$2" != *$'\n'* ]] || die "значение $1 штампа занимает несколько строк"
  stamp+="$1=$2"$'\n'
}

# Проба даёт одну непустую строку или останавливает штамп; пустых значений в штампе не бывает.
probe() {
  local key="$1" hint="$2" out
  shift 2
  out="$("$@" 2>/dev/null)" || die "$* не выполняется: $hint"
  [[ -n "$out" ]] || die "$* вернул пустой ответ: $hint"
  field "$key" "$out"
}

sha256_of() {
  local sum
  sum="$(shasum -a 256 <"$1")" || die "не удалось прочитать $1"
  digest="${sum%% *}"
}

# Входы сборки, известные до загрузки исходников: значения pin, флаги, патчи, рецепт и Xcode.
compute_stamp() {
  local name value flags patch names="" list="" digest xcode out

  # Номер поднимается, когда меняется смысл сверки или набора без изменения входов сборки.
  field stamp_format 1
  while IFS= read -r name; do
    value="${!name}"
    [[ -n "$value" ]] || die "в pin пусто значение $name"
    field "$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]')" "$value"
  done < <(sed -n 's/^\([A-Z][A-Z0-9_]*\)=.*/\1/p' "$here/pin")

  flags="$("$here/build.sh" --print-flags)" || die "build.sh --print-flags завершился ошибкой"
  field build_flags "${flags//$'\n'/ }"

  shopt -s nullglob
  for patch in "$patch_dir"/*.patch; do
    sha256_of "$patch"
    names+="${names:+ }$(basename "$patch")"
    list+="$(basename "$patch")  $digest"$'\n'
  done
  shopt -u nullglob
  field patches "${names:-none}"
  digest="$(printf '%s' "$list" | shasum -a 256)" || die "не удалось посчитать дайджест патчей"
  field patches_sha256 "${digest%% *}"
  sha256_of "$here/build.sh"
  field build_sh_sha256 "$digest"

  probe arch "нет uname" uname -m
  if [[ -n "${DEVELOPER_DIR:-}" ]]; then
    field developer_dir "$DEVELOPER_DIR"
  else
    probe developer_dir "выберите Xcode через xcode-select" xcode-select -p
  fi
  xcode="$(xcodebuild -version 2>/dev/null)" || die "xcodebuild -version не выполняется: выберите Xcode через xcode-select"
  field xcodebuild_version "${xcode//$'\n'/; }"
  probe sdk_version "нет SDK macOS" xcrun --show-sdk-version
  probe sdk_build "нет SDK macOS" xcrun --show-sdk-build-version
  out="$(xcrun metal --version 2>/dev/null)" || die "Metal toolchain не исполняется, выполните make setup"
  field metal_version "${out%%$'\n'*}"
}

# Каталог набора содержит только обычные файлы и каталоги с именами, которые манифест записывает однозначно.
check_entries() {
  local bad
  bad="$(cd "$1" && find . -mindepth 1 ! -type f ! -type d -print -quit)" || die "не удалось обойти $1"
  [[ -z "$bad" ]] || die "в $1 есть ${bad#./}, который не является обычным файлом или каталогом"
  bad="$(cd "$1" && find . \( -name '*\\*' -o -name $'*\n*' \) -print -quit)" || die "не удалось обойти $1"
  [[ -z "$bad" ]] || die "в $1 есть имя с обратной косой чертой или переводом строки"
}

# Строка манифеста — SHA-256 и путь от корня набора. Скрытые файлы входят, manifest и stamp в корне — нет.
manifest_of() {
  (cd "$1" &&
    find . -mindepth 1 ! -type d ! -path ./manifest ! -path ./stamp -print0 |
    LC_ALL=C sort -z |
      xargs -0 shasum -a 256 |
      sed 's|^\([0-9a-f]\{64\}\)  \./|\1  |')
}

manifest_paths() {
  printf '%s\n' "$1" | sed 's/^[0-9a-f]\{64\}  //' | LC_ALL=C sort
}

print_some() {
  local label="$1" items="$2" count
  [[ -n "$items" ]] || return 0
  count="$(printf '%s\n' "$items" | wc -l | tr -d ' ')"
  printf '%s\n' "$items" | sed -n "1,5s|^|  $label: |p" >&2
  if ((count > 5)); then
    echo "  $label: и ещё $((count - 5))" >&2
  fi
}

# Сверка каталога с манифестом: каждый файл на месте с тем же SHA-256, лишних и недостающих нет.
matches_manifest() {
  local dir="$1" expected actual missing extra gone changed
  check_entries "$dir"
  expected="$(cat "$2")" || die "не удалось прочитать $2"
  actual="$(manifest_of "$dir")" || die "не удалось посчитать SHA-256 файлов $dir"
  [[ "$actual" != "$expected" ]] || return 0
  missing="$(LC_ALL=C comm -23 <(manifest_paths "$expected") <(manifest_paths "$actual"))"
  extra="$(LC_ALL=C comm -13 <(manifest_paths "$expected") <(manifest_paths "$actual"))"
  # Строки манифеста, которых нет в каталоге, при пути на месте означают изменённое содержимое.
  gone="$(LC_ALL=C comm -23 <(printf '%s\n' "$expected" | LC_ALL=C sort) <(printf '%s\n' "$actual" | LC_ALL=C sort))"
  changed="$(LC_ALL=C comm -12 <(manifest_paths "$gone") <(manifest_paths "$actual"))"
  echo "набор $dir расходится с манифестом:" >&2
  print_some "нет файла" "$missing"
  print_some "лишний файл" "$extra"
  print_some "изменён" "$changed"
  return 1
}

verify() {
  [[ -d "$vendor" ]] || die "набор ghostty не опубликован в $vendor, выполните make ghostty"
  [[ -f "$vendor/stamp" ]] || die "в наборе $vendor нет штампа: публикация не завершена, выполните make ghostty"
  compute_stamp
  if ! cmp -s <(printf '%s' "$stamp") "$vendor/stamp"; then
    diff -u -L ожидается -L "$vendor/stamp" <(printf '%s' "$stamp") "$vendor/stamp" >&2 || true
    die "штамп набора $vendor не совпадает с ожидаемым, выполните make ghostty"
  fi
  [[ -f "$vendor/manifest" ]] || die "в наборе $vendor нет манифеста, выполните make ghostty"
  matches_manifest "$vendor" "$vendor/manifest" || die "набор $vendor изменён после публикации, выполните make ghostty"
  echo "набор ghostty $GHOSTTY_VERSION_STRING ($GHOSTTY_REV) актуален: $vendor"
}

# Заголовок собранного дерева восстанавливается из кэша исходников build.sh: ревизия и те же патчи по порядку.
check_header() {
  local build="$1" mirror="$cache/ghostty-src.git" ref="refs/shellio/pin/$GHOSTTY_REV" patch
  git --git-dir="$mirror" rev-parse -q --verify "$ref^{commit}" >/dev/null ||
    die "в кэше $mirror нет исходников $GHOSTTY_REV, заголовок не с чем сверить"
  git init -q "$work/tree"
  mkdir -p "$work/tree/include"
  git --git-dir="$mirror" show "$ref:include/ghostty.h" >"$work/tree/include/ghostty.h" ||
    die "в $GHOSTTY_REV нет include/ghostty.h"
  shopt -s nullglob
  for patch in "$patch_dir"/*.patch; do
    git -C "$work/tree" apply --include=include/ghostty.h "$patch" ||
      die "патч $(basename "$patch") не накладывается на include/ghostty.h"
  done
  shopt -u nullglob
  cmp -s "$work/tree/include/ghostty.h" "$build/include/ghostty.h" ||
    die "include/ghostty.h каталога сборки отличается от include/ghostty.h собранного дерева $GHOSTTY_REV с патчами"
}

check_build_dir() {
  local build="$1" file dir
  for file in lib/libghostty.a include/ghostty.h share/terminfo/78/xterm-ghostty LICENSE; do
    [[ -f "$build/$file" && -s "$build/$file" ]] || die "в каталоге сборки нет $file"
  done
  for dir in share/ghostty/shell-integration share/ghostty/themes share/terminfo; do
    [[ -d "$build/$dir" && -n "$(ls -A "$build/$dir")" ]] || die "в каталоге сборки пуст $dir"
  done
  for file in manifest stamp; do
    [[ ! -e "$build/$file" ]] || die "в каталоге сборки есть $file, это имя занято набором"
  done
  check_entries "$build"
  ar -t "$build/lib/libghostty.a" >/dev/null 2>&1 || die "архив lib/libghostty.a не читается ar"
  check_header "$build"
}

publish() {
  local build before="$2" vendor_abs vendor_name vendor_parent repo home manifest
  [[ "${SHELLIO_GHOSTTY_LOCKED:-}" == 1 ]] ||
    die "публикация выполняется только целью make ghostty под блокировкой набора (нет SHELLIO_GHOSTTY_LOCKED=1)"
  [[ -d "$1" ]] || die "каталог сборки $1 не найден"
  [[ -f "$before" ]] || die "нет штампа $before, снятого до сборки"
  [[ -n "$vendor" ]] || die "пустой путь набора"
  vendor_name="$(basename "$vendor")"
  [[ "$vendor_name" != / && "$vendor_name" != . && "$vendor_name" != .. ]] ||
    die "путь набора $vendor должен оканчиваться именем каталога"
  mkdir -p "$(dirname "$vendor")"
  # Пути сравниваются физическими: лишняя косая черта или ссылка в пути не обходит проверки ниже.
  build="$(cd "$1" && pwd -P)"
  vendor_parent="$(cd "$(dirname "$vendor")" && pwd -P)"
  # У каталога прямо под / родитель равен /, без среза получилось бы //имя и проверки ниже не совпали бы.
  vendor_abs="${vendor_parent%/}/$vendor_name"
  repo="$(cd "$root" && pwd -P)"
  home="$HOME"
  [[ ! -d "$home" ]] || home="$(cd "$home" && pwd -P)"
  # Публикация удаляет корень набора целиком, поэтому он не может накрывать репозиторий, HOME или файл блокировки.
  case "$repo/" in "$vendor_abs/"*) die "набор нельзя публиковать в $vendor_abs: он содержит репозиторий" ;; esac
  case "$home/" in "$vendor_abs/"*) die "набор нельзя публиковать в $vendor_abs: он содержит HOME" ;; esac
  [[ "$vendor_abs" != "$repo/vendor" ]] || die "набор нельзя публиковать в $vendor_abs: там лежит файл блокировки"
  case "$build/" in "$vendor_abs/"*) die "каталог сборки $build лежит внутри набора $vendor_abs" ;; esac
  case "$vendor_abs/" in "$build/"*) die "набор $vendor_abs лежит внутри каталога сборки $build" ;; esac

  # Штамп описывает входы, из которых собран набор, поэтому они не должны меняться за время сборки.
  compute_stamp
  if ! cmp -s <(printf '%s' "$stamp") "$before"; then
    diff -u -L "до сборки" -L "после сборки" "$before" <(printf '%s' "$stamp") >&2 || true
    die "входы сборки изменились за время сборки, набор не публикуется: выполните make ghostty заново"
  fi
  work="$(mktemp -d "${TMPDIR:-/tmp}/shellio-vendor.XXXXXX")"
  trap 'rm -rf "$work"' EXIT

  echo "шаг 1: проверяю каталог сборки $build"
  check_build_dir "$build"
  manifest="$(manifest_of "$build")" || die "не удалось посчитать SHA-256 файлов $build"

  echo "шаг 2: заменяю содержимое $vendor_abs"
  rm -f "$vendor_abs/stamp"
  rm -rf "$vendor_abs"
  mkdir -p "$vendor_abs"
  cp -R "$build/." "$vendor_abs/"
  printf '%s\n' "$manifest" >"$vendor_abs/manifest"
  matches_manifest "$vendor_abs" "$vendor_abs/manifest" ||
    die "опубликованный набор $vendor_abs не совпадает с манифестом сборки"

  if [[ "${SHELLIO_GHOSTTY_TEST_ABORT_BEFORE_STAMP:-}" == 1 ]]; then
    die "публикация прервана перед записью штампа (SHELLIO_GHOSTTY_TEST_ABORT_BEFORE_STAMP)"
  fi

  echo "шаг 3: записываю штамп"
  printf '%s' "$stamp" >"$vendor_abs/stamp"
  echo "набор ghostty $GHOSTTY_VERSION_STRING опубликован: $vendor_abs"
}

case "${1:-}" in
  --print-stamp)
    [[ $# -eq 1 ]] || usage
    compute_stamp
    printf '%s' "$stamp"
    ;;
  --verify)
    [[ $# -eq 1 ]] || usage
    verify
    ;;
  publish)
    [[ $# -eq 3 ]] || usage
    publish "$2" "$3"
    ;;
  *) usage ;;
esac
