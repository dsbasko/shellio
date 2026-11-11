#!/usr/bin/env bash
# Линкует весь архив набора ghostty по link-libs и проверяет, что зависимости только системные.
#
# Архив подключается через -force_load и без -dead_strip: так неразрешённый символ в любом объекте архива
# роняет линковку, даже если код, который его использует, приложению не нужен.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

vendor="${SHELLIO_GHOSTTY_VENDOR:-$root/vendor/ghostty}"

die() {
  echo "error: $*" >&2
  exit 1
}

# Минимум тот же, что у приложения: его задаёт .cargo/config.toml корня workspace.
min_macos="$(sed -n 's/^MACOSX_DEPLOYMENT_TARGET = { value = "\([0-9.]*\)".*/\1/p' "$root/.cargo/config.toml")"
[[ -n "$min_macos" ]] || die "в .cargo/config.toml нет MACOSX_DEPLOYMENT_TARGET"

args=()
while IFS= read -r line; do
  [[ -n "$line" && "$line" != \#* ]] || continue
  kind="${line%%=*}"
  name="${line#*=}"
  [[ "$kind" != "$line" && -n "$name" ]] || die "строка link-libs без вида: $line"
  case "$kind" in
    static)
      [[ -f "$vendor/lib/lib$name.a" ]] || die "в наборе нет lib/lib$name.a"
      args+=("-Wl,-force_load,$vendor/lib/lib$name.a")
      ;;
    framework) args+=(-framework "$name") ;;
    dylib) args+=("-l$name") ;;
    *) die "неизвестный вид $kind в link-libs: $line" ;;
  esac
done <"$here/link-libs"

work="$(mktemp -d "${TMPDIR:-/tmp}/shellio-link-audit.XXXXXX")"
trap 'rm -rf "$work"' EXIT
printf 'int main(void) { return 0; }\n' >"$work/main.c"

echo "линкую весь архив набора $vendor"
env -u MACOSX_DEPLOYMENT_TARGET xcrun clang -arch arm64 "-mmacosx-version-min=$min_macos" \
  "$work/main.c" "${args[@]}" -o "$work/audit" || die "архив набора не линкуется по link-libs"

deps="$(otool -L "$work/audit" | tail -n +2 | awk '{ print $1 }')"
[[ -n "$deps" ]] || die "otool -L не показал зависимостей"
foreign="$(printf '%s\n' "$deps" | grep -v -e '^/System/' -e '^/usr/lib/' || true)"
[[ -z "$foreign" ]] || die "связка зависит от библиотек вне /System и /usr/lib: $(printf '%s' "$foreign" | tr '\n' ' ')"
echo "архив линкуется целиком, зависимостей: $(printf '%s\n' "$deps" | wc -l | tr -d ' '), все в /System и /usr/lib"
