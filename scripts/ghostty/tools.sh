#!/usr/bin/env bash
# Инструменты сборки libghostty: Zig архивом точной версии с закреплённым SHA-256, Metal toolchain Xcode
# и bindgen-cli точной версии для привязок shellio-sys-ghostty.
#
#   tools.sh zig           ставит Zig из pin в кэш, если его там нет
#   tools.sh metal         ставит Metal toolchain, если `xcrun metal` не исполняется
#   tools.sh bindgen       ставит bindgen-cli в кэш через `cargo install --locked`, если его там нет
#   tools.sh zig-path      печатает путь к установленному Zig или завершается ошибкой
#   tools.sh bindgen-path  печатает путь к установленному bindgen или завершается ошибкой
#
# ZIG_MIRROR задаёт другой источник архива с плоской раскладкой зеркал Zig; сумма проверяется так же.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
# shellcheck source=pin
source "$here/pin"

cache="${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}"
zig_name="zig-aarch64-macos-$ZIG_VERSION"
zig_archive="$cache/zig/dl/$zig_name.tar.xz"
zig_home="$cache/zig/$ZIG_VERSION"
zig_url="${ZIG_MIRROR:-https://ziglang.org/download/$ZIG_VERSION}/$zig_name.tar.xz"

# Версия bindgen не входит в pin: она не влияет на сборку набора, а pin целиком входит в его штамп.
BINDGEN_CLI_VERSION=0.73.2
bindgen_home="$cache/bindgen-cli/$BINDGEN_CLI_VERSION"

die() {
  echo "error: $*" >&2
  exit 1
}

sha256() {
  shasum -a 256 "$1" | cut -d' ' -f1
}

# Установленным считается только каталог с отметкой о сумме архива, из которого он распакован.
zig_installed() {
  [[ -x "$zig_home/zig" ]] &&
    [[ "$(cat "$zig_home/.archive-sha256" 2>/dev/null)" == "$ZIG_SHA256" ]] &&
    [[ "$("$zig_home/zig" version 2>/dev/null)" == "$ZIG_VERSION" ]]
}

fetch_zig_archive() {
  local part="$zig_archive.part" got
  if [[ -f "$zig_archive" && "$(sha256 "$zig_archive")" == "$ZIG_SHA256" ]]; then
    return 0
  fi
  rm -f "$zig_archive"
  mkdir -p "$(dirname "$zig_archive")"
  # Недокачанный архив дозагружается, а уже полный не запрашивается повторно.
  # Источник без поддержки диапазонов отдаёт архив заново с начала.
  if [[ ! -f "$part" || "$(sha256 "$part")" != "$ZIG_SHA256" ]]; then
    echo "загружаю $zig_url"
    if ! curl --proto '=https' --tlsv1.2 -fL --retry 5 -C - -o "$part" "$zig_url"; then
      rm -f "$part"
      curl --proto '=https' --tlsv1.2 -fL --retry 5 -o "$part" "$zig_url"
    fi
  fi
  got="$(sha256 "$part")"
  if [[ "$got" != "$ZIG_SHA256" ]]; then
    rm -f "$part"
    die "SHA-256 архива $zig_name не совпадает: ожидается $ZIG_SHA256, получено $got"
  fi
  mv "$part" "$zig_archive"
}

install_zig() {
  local tmp ver
  [[ "$(uname -m)" == arm64 ]] || die "набор ghostty собирается только на arm64"
  if zig_installed; then
    echo "Zig $ZIG_VERSION уже установлен: $zig_home/zig"
    return 0
  fi
  fetch_zig_archive
  tmp="$(mktemp -d "$cache/zig/.extract.XXXXXX")"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT
  tar -xJf "$zig_archive" -C "$tmp"
  [[ -x "$tmp/$zig_name/zig" ]] || die "в архиве нет $zig_name/zig"
  ver="$("$tmp/$zig_name/zig" version)"
  [[ "$ver" == "$ZIG_VERSION" ]] || die "архив содержит Zig $ver, ожидается $ZIG_VERSION"
  printf '%s\n' "$ZIG_SHA256" >"$tmp/$zig_name/.archive-sha256"
  # Каталог появляется под окончательным именем только целиком, прерванная распаковка установкой не считается.
  rm -rf "$zig_home"
  mv "$tmp/$zig_name" "$zig_home"
  echo "Zig $ZIG_VERSION установлен: $zig_home/zig"
}

install_metal() {
  if xcrun metal --version >/dev/null 2>&1; then
    echo "Metal toolchain установлен: $(xcrun metal --version 2>&1 | head -n 1)"
    return 0
  fi
  # Без первого запуска Xcode `-downloadComponent` не загружает свои модули.
  if ! xcodebuild -checkFirstLaunchStatus >/dev/null 2>&1; then
    echo "выполняю первый запуск Xcode"
    xcodebuild -runFirstLaunch
  fi
  echo "загружаю Metal toolchain"
  xcodebuild -downloadComponent MetalToolchain
  xcrun metal --version >/dev/null 2>&1 || die "Metal toolchain не исполняется после установки"
  echo "Metal toolchain установлен: $(xcrun metal --version 2>&1 | head -n 1)"
}

bindgen_installed() {
  [[ "$("$bindgen_home/bin/bindgen" --version 2>/dev/null)" == "bindgen $BINDGEN_CLI_VERSION" ]]
}

install_bindgen() {
  local tmp
  if bindgen_installed; then
    echo "bindgen-cli $BINDGEN_CLI_VERSION уже установлен: $bindgen_home/bin/bindgen"
    return 0
  fi
  mkdir -p "$cache/bindgen-cli"
  tmp="$(mktemp -d "$cache/bindgen-cli/.install.XXXXXX")"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT
  # Toolchain для cargo install берётся из rust-toolchain.toml корня репозитория.
  (cd "$root" && "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" install --locked --version "$BINDGEN_CLI_VERSION" --root "$tmp" bindgen-cli)
  [[ "$("$tmp/bin/bindgen" --version)" == "bindgen $BINDGEN_CLI_VERSION" ]] ||
    die "установленный bindgen не сообщает версию $BINDGEN_CLI_VERSION"
  rm -rf "$bindgen_home"
  mv "$tmp" "$bindgen_home"
  echo "bindgen-cli $BINDGEN_CLI_VERSION установлен: $bindgen_home/bin/bindgen"
}

case "${1:-}" in
  zig) install_zig ;;
  metal) install_metal ;;
  bindgen) install_bindgen ;;
  zig-path)
    zig_installed || die "Zig $ZIG_VERSION не установлен в $zig_home, выполните make setup"
    printf '%s\n' "$zig_home/zig"
    ;;
  bindgen-path)
    bindgen_installed || die "bindgen-cli $BINDGEN_CLI_VERSION не установлен в $bindgen_home, выполните make setup"
    printf '%s\n' "$bindgen_home/bin/bindgen"
    ;;
  *)
    echo "usage: $0 zig|metal|bindgen|zig-path|bindgen-path" >&2
    exit 2
    ;;
esac
