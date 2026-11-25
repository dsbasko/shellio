#!/usr/bin/env bash
# Браузер съёмщика паспортов (ADR-0037): Chrome for Testing chrome-headless-shell точной версии из pin.
#
#   tools.sh chrome       ставит браузер в кэш, если его там нет
#   tools.sh chrome-path  печатает путь к установленному браузеру или завершается ошибкой
#
# Браузер ставят только make passports и make conformance-tools; make setup, make check и CI его не загружают.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=pin
source "$here/pin"

cache="${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}"
chrome_name="chrome-headless-shell-mac-arm64"
chrome_archive="$cache/chrome-headless-shell/dl/$chrome_name-$CHROME_VERSION.zip"
chrome_home="$cache/chrome-headless-shell/$CHROME_VERSION"
chrome_bin="$chrome_home/chrome-headless-shell"

die() {
  echo "error: $*" >&2
  exit 1
}

sha256() {
  shasum -a 256 "$1" | cut -d' ' -f1
}

length() {
  stat -f %z "$1"
}

archive_ok() {
  [[ -f "$1" && "$(length "$1")" == "$CHROME_LENGTH" && "$(sha256 "$1")" == "$CHROME_SHA256" ]]
}

# Установленным считается только каталог с отметкой о сумме архива, из которого он распакован.
chrome_installed() {
  [[ -x "$chrome_bin" ]] &&
    [[ "$(cat "$chrome_home/.archive-sha256" 2>/dev/null)" == "$CHROME_SHA256" ]] &&
    "$chrome_bin" --version 2>/dev/null | grep -qF "$CHROME_VERSION"
}

fetch_chrome_archive() {
  local part="$chrome_archive.part" got
  if archive_ok "$chrome_archive"; then
    return 0
  fi
  rm -f "$chrome_archive"
  mkdir -p "$(dirname "$chrome_archive")"
  # Недокачанный архив дозагружается, а уже полный не запрашивается повторно.
  # Источник без поддержки диапазонов отдаёт архив заново с начала.
  if ! archive_ok "$part"; then
    echo "загружаю $CHROME_URL"
    if ! curl --proto '=https' --tlsv1.2 -fL --retry 5 -C - -o "$part" "$CHROME_URL"; then
      rm -f "$part"
      curl --proto '=https' --tlsv1.2 -fL --retry 5 -o "$part" "$CHROME_URL"
    fi
  fi
  got="$(length "$part")"
  if [[ "$got" != "$CHROME_LENGTH" ]]; then
    rm -f "$part"
    die "длина архива $chrome_name не совпадает: ожидается $CHROME_LENGTH, получено $got"
  fi
  got="$(sha256 "$part")"
  if [[ "$got" != "$CHROME_SHA256" ]]; then
    rm -f "$part"
    die "SHA-256 архива $chrome_name не совпадает: ожидается $CHROME_SHA256, получено $got"
  fi
  mv "$part" "$chrome_archive"
}

install_chrome() {
  local tmp ver
  [[ "$(uname -m)" == arm64 ]] || die "закреплён браузер только для mac-arm64"
  if chrome_installed; then
    echo "chrome-headless-shell $CHROME_VERSION уже установлен: $chrome_bin"
    return 0
  fi
  fetch_chrome_archive
  mkdir -p "$cache/chrome-headless-shell"
  tmp="$(mktemp -d "$cache/chrome-headless-shell/.extract.XXXXXX")"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT
  ditto -x -k "$chrome_archive" "$tmp"
  [[ -x "$tmp/$chrome_name/chrome-headless-shell" ]] || die "в архиве нет $chrome_name/chrome-headless-shell"
  # Карантин снимается только с распакованного каталога своего кэша; другие атрибуты и настройки не меняются.
  xattr -r -d com.apple.quarantine "$tmp/$chrome_name" 2>/dev/null || true
  ver="$("$tmp/$chrome_name/chrome-headless-shell" --version 2>/dev/null || true)"
  [[ "$ver" == *"$CHROME_VERSION"* ]] || die "архив сообщает версию «$ver», ожидается $CHROME_VERSION"
  printf '%s\n' "$CHROME_SHA256" >"$tmp/$chrome_name/.archive-sha256"
  # Каталог появляется под окончательным именем только целиком, прерванная распаковка установкой не считается.
  rm -rf "$chrome_home"
  mv "$tmp/$chrome_name" "$chrome_home"
  echo "chrome-headless-shell $CHROME_VERSION установлен: $chrome_bin"
}

case "${1:-}" in
  chrome) install_chrome ;;
  chrome-path)
    chrome_installed ||
      die "chrome-headless-shell $CHROME_VERSION не установлен в $chrome_home, выполните make conformance-tools"
    printf '%s\n' "$chrome_bin"
    ;;
  *)
    echo "usage: $0 chrome|chrome-path" >&2
    exit 2
    ;;
esac
