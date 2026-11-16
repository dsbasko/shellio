#!/usr/bin/env bash
# Набор libghostty-vt пробы P0.4: статическая библиотека VT закреплённой ревизии поверхности shellio
# (план P0.4, «Сборка libghostty-vt»). Ревизия, версия, Zig и патчи — из scripts/ghostty/pin и
# scripts/ghostty-patches/, второго pin нет. Продуктовый vendor/ghostty/ и make ghostty не меняются.
#
#   vt.sh --print-stamp   ожидаемый штамп набора; сеть и Zig не нужны
#   vt.sh --verify        сверяет опубликованный набор со штампом и манифестом; только читает
#   vt.sh ensure          под блокировкой набора: сверка, а при расхождении сборка и атомарная
#                         публикация; сеть не нужна, если пакеты Zig уже в кэше
#   vt.sh version         сверка и одна строка: версия и ревизия ghostty набора
#
# Набор лежит в $SHELLIO_PROBE_SETS_DIR/vt/, по умолчанию target/probes/session-survival/vt/, блокировка —
# vt.lock рядом, рабочая копия исходников — .vt.build.* рядом, удаляется после сборки. Исходники
# ревизии только читаются из git-копии кэша $SHELLIO_CACHE_DIR/ghostty-src.git, которую ведёт
# make ghostty; копия кэша не меняется. Zig — из make setup, пакеты Zig — в $SHELLIO_CACHE_DIR/zig-global.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
probe="$(cd "$here/.." && pwd)"
root="$(cd "$probe/../.." && pwd)"
pin="$root/scripts/ghostty/pin"
patch_dir="$root/scripts/ghostty-patches"
# shellcheck source=../../../scripts/ghostty/pin
source "$pin"
# shellcheck source=set.sh
source "$here/set.sh"

base="${SHELLIO_PROBE_SETS_DIR:-$root/target/probes/session-survival}"
cache="${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}"
mirror="$cache/ghostty-src.git"
name=vt
dest="$base/$name"

usage() {
  echo "usage: $0 --print-stamp | --verify | ensure | version" >&2
  exit 2
}

# Флаги сборки входят в штамп. Без явного -Demit-xcframework в режиме lib-vt значение зависит от наличия
# xcodebuild, а без -Dlib-version-string версия берётся из git рабочей копии.
build_flags() {
  printf '%s\n' \
    -Demit-lib-vt=true \
    -Doptimize=ReleaseFast \
    -Demit-xcframework=false \
    "-Dlib-version-string=$GHOSTTY_VERSION_STRING"
}

patches() {
  shopt -s nullglob
  local files=("$patch_dir"/*.patch)
  shopt -u nullglob
  ((${#files[@]} > 0)) || return 0
  printf '%s\n' "${files[@]}"
}

# Дайджест списка «имя  SHA-256» патчей по порядку.
patches_digest() {
  local list="" patch digest
  for patch in "$@"; do
    list+="$(basename "$patch")  $(sha256_file "$patch")"$'\n'
  done
  digest="$(printf '%s' "$list" | shasum -a 256)" || die "не удалось посчитать дайджест патчей"
  printf '%s\n' "${digest%% *}"
}

# --- штамп ---

stamp=""

field() {
  [[ "$2" != *$'\n'* ]] || die "значение $1 штампа занимает несколько строк"
  stamp+="$1=$2"$'\n'
}

probe_field() {
  local key="$1" hint="$2" out
  shift 2
  out="$("$@" 2>/dev/null)" || die "$* не выполняется: $hint"
  [[ -n "$out" ]] || die "$* вернул пустой ответ: $hint"
  field "$key" "$out"
}

# Входы сборки, известные до загрузки исходников: значения pin, флаги, патчи, рецепт и Xcode.
compute_stamp() {
  local key value files=() file names="" recipe
  while IFS= read -r file; do files+=("$file"); done < <(patches)

  stamp=""
  # Номер поднимается, когда меняется смысл сверки или набора без изменения входов сборки.
  field stamp_format 1
  field set "$name"
  while IFS= read -r key; do
    value="${!key}"
    [[ -n "$value" ]] || die "в scripts/ghostty/pin пусто значение $key"
    field "$(printf '%s' "$key" | tr '[:upper:]' '[:lower:]')" "$value"
  done < <(sed -n 's/^\([A-Z][A-Z0-9_]*\)=.*/\1/p' "$pin")

  field build_flags "$(build_flags | tr '\n' ' ' | sed 's/ $//')"
  for file in ${files[@]+"${files[@]}"}; do names+="${names:+ }$(basename "$file")"; done
  field patches "${names:-none}"
  field patches_sha256 "$(patches_digest ${files[@]+"${files[@]}"})"
  recipe="$(cat "$here/vt.sh" "$here/set.sh" | shasum -a 256)" || die "не удалось посчитать дайджест рецепта"
  field recipe_sha256 "${recipe%% *}"

  probe_field arch "нет uname" uname -m
  if [[ -n "${DEVELOPER_DIR:-}" ]]; then
    field developer_dir "$DEVELOPER_DIR"
  else
    probe_field developer_dir "выберите Xcode через xcode-select" xcode-select -p
  fi
  probe_field sdk_version "нет SDK macOS" xcrun --show-sdk-version
  probe_field sdk_build "нет SDK macOS" xcrun --show-sdk-build-version
}

# --- сборка ---

# Рабочая копия ревизии из git-копии кэша. Копия кэша только читается: ref ревизии заводит make ghostty.
checkout_source() {
  local src="$1" ref="refs/shellio/pin/$GHOSTTY_REV"
  git -C "$mirror" rev-parse -q --verify "$ref^{commit}" >/dev/null 2>&1 ||
    die "в $mirror нет ревизии ghostty $GHOSTTY_REV, выполните make ghostty"
  git init -q "$src"
  git -C "$src" fetch -q --depth 1 "file://$mirror" "$ref" || die "не удалось получить $GHOSTTY_REV из $mirror"
  git -C "$src" -c advice.detachedHead=false checkout -q FETCH_HEAD
  [[ "$(git -C "$src" rev-parse HEAD)" == "$GHOSTTY_REV" ]] ||
    die "получена ревизия $(git -C "$src" rev-parse HEAD), ожидается $GHOSTTY_REV"
  [[ -z "$(git -C "$src" status --porcelain)" ]] || die "дерево $src после checkout не чистое"
}

# Патчи накладываются по порядку имён; каждый сначала проверяется на дереве после предыдущих.
apply_patches() {
  local src="$1" patch prev="чистое дерево"
  shift
  for patch in "$@"; do
    echo "патч $(basename "$patch")"
    git -C "$src" apply --check "$patch" ||
      die "патч $(basename "$patch") не накладывается на $GHOSTTY_REV (после: $prev)"
    git -C "$src" apply "$patch"
    prev="$(basename "$patch")"
  done
}

check_archive() {
  local lib="$1" archs minos symbols
  ar -t "$lib" >/dev/null || die "архив $lib не читается ar"
  archs="$(lipo -archs "$lib")" || die "архив $lib не читается lipo"
  [[ "$archs" == arm64 ]] || die "архив собран для $archs, ожидается arm64"
  minos="$(otool -l "$lib" | awk '$1 == "minos" { print $2 }' | sort -u | tr '\n' ' ')"
  [[ "$minos" == "$GHOSTTY_MIN_MACOS " ]] ||
    die "минимум macOS объектов архива: ${minos:-нет}, ожидается $GHOSTTY_MIN_MACOS"
  # Вывод nm читается целиком: grep -q под pipefail обрывал бы nm сигналом SIGPIPE.
  symbols="$(nm -g "$lib" 2>/dev/null)" || die "архив $lib не читается nm"
  grep -q ' T _ghostty_terminal_new$' <<<"$symbols" || die "в архиве нет _ghostty_terminal_new"
}

build_set() {
  local work="$1" out="$2" src="$1/ghostty" zig files=() file flags=() started=$SECONDS
  zig="$("$root/scripts/ghostty/tools.sh" zig-path)"
  while IFS= read -r file; do files+=("$file"); done < <(patches)
  while IFS= read -r file; do flags+=("$file"); done < <(build_flags)

  checkout_source "$src"
  apply_patches "$src" ${files[@]+"${files[@]}"}
  # Минимум macOS у ghostty собственный, родительское окружение его не переопределяет.
  echo "загружаю зависимости build.zig.zon"
  (cd "$src" && env -u MACOSX_DEPLOYMENT_TARGET ZIG_GLOBAL_CACHE_DIR="$cache/zig-global" "$zig" build --fetch "${flags[@]}") ||
    die "не удалось загрузить зависимости ghostty; часть из них раздаёт deps.files.ghostty.org"
  echo "собираю libghostty-vt ($("$zig" version), ${flags[*]})"
  (cd "$src" && env -u MACOSX_DEPLOYMENT_TARGET ZIG_GLOBAL_CACHE_DIR="$cache/zig-global" "$zig" build "${flags[@]}") ||
    die "сборка libghostty-vt завершилась ошибкой"

  [[ -f "$src/zig-out/lib/libghostty-vt.a" ]] || die "сборка не создала zig-out/lib/libghostty-vt.a"
  [[ -f "$src/zig-out/include/ghostty/vt.h" ]] || die "сборка не установила include/ghostty/vt.h"
  # Установленные заголовки совпадают с исходниками ревизии, иначе привязки описывали бы не тот API.
  diff -r "$src/include/ghostty/vt" "$src/zig-out/include/ghostty/vt" >/dev/null &&
    cmp -s "$src/include/ghostty/vt.h" "$src/zig-out/include/ghostty/vt.h" ||
    die "установленные заголовки VT отличаются от include/ghostty исходников"
  mkdir -p "$out/lib" "$out/include/ghostty"
  cp "$src/zig-out/lib/libghostty-vt.a" "$out/lib/libghostty-vt.a"
  cp "$src/zig-out/include/ghostty/vt.h" "$out/include/ghostty/vt.h"
  cp -R "$src/zig-out/include/ghostty/vt" "$out/include/ghostty/vt"
  cp "$src/LICENSE" "$out/LICENSE"
  check_archive "$out/lib/libghostty-vt.a"
  echo "libghostty-vt собран за $((SECONDS - started)) с"
}

# --- команды ---

cmd_verify() {
  compute_stamp
  set_verify "$dest" "$stamp" || die "набор $name не сходится, выполните make probe-session"
  echo "набор $name актуален: $dest"
}

cmd_ensure() {
  local lock="$base/$name.lock" before reason work
  if ! set_locked "$lock"; then
    set_with_lock "$lock" "набор $name" "$here/vt.sh" ensure
    return
  fi
  compute_stamp
  before="$stamp"
  if reason="$(set_verify "$dest" "$before" 2>&1)"; then
    echo "набор $name актуален: $dest"
    return 0
  fi
  echo "набор $name собирается заново: ${reason##*$'\n'}"
  set_clean_leftovers "$base" "$name"
  work="$(mktemp -d "$base/.$name.build.XXXXXX")"
  # shellcheck disable=SC2064
  trap "rm -rf '$work'" EXIT
  build_set "$work" "$work/set"
  # Штамп описывает входы, из которых собран набор, поэтому они не должны меняться за время сборки.
  compute_stamp
  if [[ "$stamp" != "$before" ]]; then
    diff -u -L "до сборки" -L "после сборки" <(printf '%s' "$before") <(printf '%s' "$stamp") >&2 || true
    die "входы сборки набора $name изменились за время сборки, набор не публикуется"
  fi
  set_publish "$work/set" "$dest" "$stamp"
  set_verify "$dest" "$stamp" || die "опубликованный набор $name не сходится"
  echo "набор $name опубликован: $dest (ghostty $GHOSTTY_VERSION_STRING, $GHOSTTY_REV)"
}

cmd_version() {
  compute_stamp
  set_verify "$dest" "$stamp" || die "набор $name не сходится, выполните make probe-session"
  printf 'ghostty-vt %s (%s)\n' "$GHOSTTY_VERSION_STRING" "${GHOSTTY_REV:0:12}"
}

[[ $# -eq 1 ]] || usage
case "$1" in
  --print-stamp)
    compute_stamp
    printf '%s' "$stamp"
    ;;
  --verify) cmd_verify ;;
  ensure)
    mkdir -p "$base"
    cmd_ensure
    ;;
  version) cmd_version ;;
  *) usage ;;
esac
