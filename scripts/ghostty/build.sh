#!/usr/bin/env bash
# Собирает закреплённую ревизию libghostty с патчами и раскладывает набор в пустой каталог.
#
#   build.sh <каталог>     загрузка, патчи, сборка и извлечение набора
#   build.sh --print-flags флаги `zig build`, по одному в строке; сеть не нужна
#
# Публикацию в vendor/ghostty/, штамп и манифест ведёт vendor.sh. Исходники ревизии
# кэшируются в $SHELLIO_CACHE_DIR/ghostty-src.git, пакеты Zig — в $SHELLIO_CACHE_DIR/zig-global.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
# shellcheck source=pin
source "$here/pin"

cache="${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}"
patch_dir="$root/scripts/ghostty-patches"

die() {
  echo "error: $*" >&2
  exit 1
}

build_flags() {
  printf '%s\n' \
    -Doptimize=ReleaseFast \
    -Demit-xcframework=true \
    -Dxcframework-target=native \
    -Demit-macos-app=false \
    -Dsentry=false \
    "-Dversion-string=$GHOSTTY_VERSION_STRING"
}

usage() {
  echo "usage: $0 <каталог>|--print-flags" >&2
  exit 2
}

case "${1:-}" in
  --print-flags)
    build_flags
    exit 0
    ;;
  "" | -*) usage ;;
esac
[[ $# -eq 1 ]] || usage

out="$1"
if [[ -e "$out" && -n "$(ls -A "$out")" ]]; then
  die "каталог $out не пуст"
fi
mkdir -p "$out"
out="$(cd "$out" && pwd)"

zig="$("$here/tools.sh" zig-path)"
xcrun metal --version >/dev/null 2>&1 || die "Metal toolchain не исполняется, выполните make setup"

flags=()
while IFS= read -r flag; do flags+=("$flag"); done < <(build_flags)

work="$(mktemp -d "${TMPDIR:-/tmp}/shellio-ghostty.XXXXXX")"
trap 'rm -rf "$work"' EXIT
src="$work/ghostty"
started=$SECONDS

# Ревизия хранится в локальном кэше под своим ref, сборка идёт в чистой копии из него.
fetch_source() {
  local mirror="$cache/ghostty-src.git" ref="refs/shellio/pin/$GHOSTTY_REV"
  [[ -d "$mirror" ]] || git init -q --bare "$mirror"
  if ! git -C "$mirror" rev-parse -q --verify "$ref^{commit}" >/dev/null; then
    if git -C "$mirror" cat-file -e "$GHOSTTY_REV^{commit}" 2>/dev/null; then
      git -C "$mirror" update-ref "$ref" "$GHOSTTY_REV"
    else
      echo "загружаю ghostty $GHOSTTY_REV"
      git -C "$mirror" fetch -q --depth 1 "$GHOSTTY_REPO" "+$GHOSTTY_REV:$ref"
    fi
  fi
  git init -q "$src"
  git -C "$src" fetch -q --depth 1 "file://$mirror" "$ref"
  git -C "$src" -c advice.detachedHead=false checkout -q FETCH_HEAD
  [[ "$(git -C "$src" rev-parse HEAD)" == "$GHOSTTY_REV" ]] ||
    die "получена ревизия $(git -C "$src" rev-parse HEAD), ожидается $GHOSTTY_REV"
}

# Патчи накладываются по порядку имён; каждый сначала проверяется на дереве после предыдущих.
apply_patches() {
  local patch
  shopt -s nullglob
  for patch in "$patch_dir"/*.patch; do
    echo "патч $(basename "$patch")"
    git -C "$src" apply --check "$patch" || die "патч $(basename "$patch") не накладывается на $GHOSTTY_REV"
    git -C "$src" apply "$patch"
  done
  shopt -u nullglob
}

# Минимум macOS у ghostty собственный, родительское окружение его не переопределяет.
run_zig() {
  (cd "$src" && env -u MACOSX_DEPLOYMENT_TARGET ZIG_GLOBAL_CACHE_DIR="$cache/zig-global" "$zig" build "$@")
}

build() {
  echo "загружаю зависимости build.zig.zon"
  run_zig --fetch "${flags[@]}" ||
    die "не удалось загрузить зависимости ghostty; часть из них раздаёт deps.files.ghostty.org"
  echo "собираю libghostty ($("$zig" version), ${flags[*]})"
  run_zig "${flags[@]}"
}

extract() {
  local xcf="$src/macos/GhosttyKit.xcframework/macos-arm64" share="$src/zig-out/share"
  [[ -f "$xcf/libghostty-internal.a" ]] || die "сборка не создала $xcf/libghostty-internal.a"
  if [[ -f "$xcf/Headers/ghostty.h" ]]; then
    cmp -s "$xcf/Headers/ghostty.h" "$src/include/ghostty.h" ||
      die "ghostty.h в xcframework отличается от include/ghostty.h исходников"
  fi
  mkdir -p "$out/lib" "$out/include" "$out/share/ghostty"
  cp "$xcf/libghostty-internal.a" "$out/lib/libghostty.a"
  cp "$src/include/ghostty.h" "$out/include/ghostty.h"
  cp -R "$share/ghostty/shell-integration" "$out/share/ghostty/shell-integration"
  cp -R "$share/ghostty/themes" "$out/share/ghostty/themes"
  cp -R "$share/terminfo" "$out/share/terminfo"
  cp "$src/LICENSE" "$out/LICENSE"
}

verify_layout() {
  local lib="$out/lib/libghostty.a" dir archs minos
  for dir in share/ghostty/shell-integration share/ghostty/themes share/terminfo; do
    [[ -n "$(ls -A "$out/$dir" 2>/dev/null)" ]] || die "в наборе пуст $dir"
  done
  [[ -s "$out/include/ghostty.h" ]] || die "в наборе нет include/ghostty.h"
  [[ -s "$out/share/terminfo/78/xterm-ghostty" ]] || die "в наборе нет share/terminfo/78/xterm-ghostty"
  [[ -s "$out/LICENSE" ]] || die "в наборе нет LICENSE"
  ar -t "$lib" >/dev/null || die "архив $lib не читается ar"
  archs="$(lipo -archs "$lib")"
  [[ "$archs" == arm64 ]] || die "архив собран для $archs, ожидается arm64"
  minos="$(otool -l "$lib" | awk '$1 == "minos" { print $2 }' | sort -u | tr '\n' ' ')"
  [[ "$minos" == "$GHOSTTY_MIN_MACOS " ]] ||
    die "минимум macOS объектов архива: ${minos:-нет}, ожидается $GHOSTTY_MIN_MACOS"
}

fetch_source
apply_patches
build
extract
verify_layout
echo "набор ghostty собран в $out за $((SECONDS - started)) с"
