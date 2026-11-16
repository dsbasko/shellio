#!/usr/bin/env bash
# Генерирует привязки пробы P0.4 к libghostty-vt из include/ghostty/vt.h опубликованного набора vt.
#
#   vt-bindings.sh          переписывает src/vt/bindings.rs
#   vt-bindings.sh --check  генерирует во временный файл и сравнивает с файлом в репозитории, не переписывая его
#
# Набор сверяет вызывающий run.sh. Clang, SDK и rustfmt берутся явно, как в scripts/ghostty/bindings.sh:
# привязки зависят только от заголовков набора, трёх списков ниже, закреплённых bindgen и Rust и Xcode.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
probe="$(cd "$here/.." && pwd)"
root="$(cd "$probe/../.." && pwd)"
# shellcheck source=../../../scripts/ghostty/pin
source "$root/scripts/ghostty/pin"

set_dir="${SHELLIO_PROBE_SETS_DIR:-$root/target/probes/session-survival}/vt"
target_file="$probe/src/vt/bindings.rs"

# Функции. Семейства терминала, formatter, снимка и render state входят целиком, остальные — по имени.
# Сознательно не используются, хотя входят в семейства:
# - ghostty_formatter_format и _format_buf, ghostty_snapshot_encode и _encode_buf,
#   ghostty_terminal_continuation_write и _buf: обёртка берёт варианты _alloc с освобождением через
#   ghostty_free, одна функция на операцию;
# - ghostty_snapshot_decoder_new с GhosttyReader, _ready и _next: снимок пробы целиком в памяти,
#   инкрементальное восстановление истории проверяет хост в задаче 12;
# - ghostty_render_*: клетки обёртка читает через grid_ref, render state нужен рендереру, а не оракулу;
# - *_get_multi, ghostty_terminal_grid_ref_track, _compress, _scroll_viewport, _vt_write_until.
# Вне списка: kitty graphics, search, selection, парсеры OSC и SGR, paste, focus, unicode, sys и wasm.
functions=(
  'ghostty_terminal_.*'
  'ghostty_formatter_.*'
  'ghostty_snapshot_.*'
  'ghostty_render_.*'
  ghostty_grid_ref_cell
  ghostty_grid_ref_row
  ghostty_grid_ref_graphemes
  ghostty_grid_ref_hyperlink_uri
  ghostty_grid_ref_style
  ghostty_cell_get
  ghostty_row_get
  ghostty_key_encoder_new
  ghostty_key_encoder_free
  ghostty_key_encoder_setopt
  ghostty_key_encoder_setopt_from_terminal
  ghostty_key_encoder_encode
  ghostty_key_event_new
  ghostty_key_event_free
  ghostty_key_event_set_action
  ghostty_key_event_set_key
  ghostty_key_event_set_mods
  ghostty_key_event_set_consumed_mods
  ghostty_key_event_set_utf8
  ghostty_key_event_set_unshifted_codepoint
  ghostty_mouse_encoder_new
  ghostty_mouse_encoder_free
  ghostty_mouse_encoder_setopt
  ghostty_mouse_encoder_setopt_from_terminal
  ghostty_mouse_encoder_encode
  ghostty_mouse_event_new
  ghostty_mouse_event_free
  ghostty_mouse_event_set_action
  ghostty_mouse_event_set_button
  ghostty_mouse_event_set_mods
  ghostty_mouse_event_set_position
  ghostty_build_info
  ghostty_free
)
# Типы, которые передаются через void* в ghostty_terminal_set, _get, setopt и _build_info и поэтому из
# сигнатур функций не выводятся: колбэки эффектов и их полезная нагрузка, типы данных get, перечисления
# опций и флагов.
types=(
  GhosttyTerminalWritePtyFn
  GhosttyTerminalBellFn
  GhosttyTerminalTitleChangedFn
  GhosttyTerminalXtversionFn
  GhosttyTerminalSizeFn
  GhosttyTerminalColorSchemeFn
  GhosttyTerminalDeviceAttributesFn
  GhosttyTerminalClipboardWriteFn
  GhosttyTerminalClipboardReadFn
  GhosttyClipboardWrite
  GhosttyClipboardWriteReply
  GhosttyClipboardWriteResult
  GhosttyClipboardRead
  GhosttyClipboardReadReply
  GhosttyClipboardReadResult
  GhosttyClipboardContent
  GhosttyClipboardLocation
  GhosttyDeviceAttributes
  GhosttySizeReportSize
  GhosttyColorScheme
  GhosttyColorRgb
  GhosttyTerminalScreen
  GhosttyTerminalModeConfig
  GhosttyMode
  GhosttyStyle
  GhosttyStyleColorTag
  GhosttySgrUnderline
  GhosttyKittyKeyFlags
  GhosttyCellContentTag
  GhosttyCellWide
  GhosttyKeyEncoderOption
  GhosttyMouseEncoderOption
  GhosttyMouseEncoderSize
  GhosttyMouseTrackingMode
  GhosttyMouseFormat
  GhosttySnapshotDecoderOption
  GhosttyBuildInfo
  GhosttyOptimizeMode
)
# Константы-макросы: модификаторы клавиш и флаги клавиатуры Kitty. Режимы GHOSTTY_MODE_* и
# GHOSTTY_INIT_SIZED — макросы с вызовом функции, bindgen их не выдаёт; обёртка задаёт их сама.
vars=(
  'GHOSTTY_MODS_.*'
  'GHOSTTY_KITTY_KEY_.*'
)

die() {
  echo "error: $*" >&2
  exit 1
}

value_of() {
  local value
  value="$(sed -n "$2" "$1")"
  [[ -n "$value" ]] || die "$3"
  printf '%s' "$value"
}

generate() {
  local out="$1" bindgen clang sdk rustfmt rust_version edition min_macos header="$set_dir/include/ghostty/vt.h" name
  local allowlist=()
  [[ -f "$header" ]] || die "в наборе нет include/ghostty/vt.h, выполните make probe-session"
  bindgen="$("$root/scripts/ghostty/tools.sh" bindgen-path)"
  clang="$(xcrun --find clang)" || die "xcrun не нашёл clang: выберите Xcode через xcode-select"
  sdk="$(xcrun --sdk macosx --show-sdk-path)" || die "нет SDK macOS"
  rustfmt="$(cd "$root" && "${CARGO_HOME:-$HOME/.cargo}/bin/rustup" which rustfmt)" || die "rustup не нашёл rustfmt, выполните make setup"
  rust_version="$(value_of "$root/rust-toolchain.toml" 's/^channel = "\([0-9.]*\)"$/\1/p' "в rust-toolchain.toml нет channel")"
  edition="$(value_of "$root/Cargo.toml" 's/^edition = "\([0-9]*\)"$/\1/p' "в Cargo.toml нет edition")"
  min_macos="$(value_of "$root/.cargo/config.toml" 's/^MACOSX_DEPLOYMENT_TARGET = { value = "\([0-9.]*\)".*/\1/p' \
    "в .cargo/config.toml нет MACOSX_DEPLOYMENT_TARGET")"
  for name in "${functions[@]}"; do
    allowlist+=(--allowlist-function "^$name\$")
  done
  for name in "${types[@]}"; do
    allowlist+=(--allowlist-type "^$name\$")
  done
  for name in "${vars[@]}"; do
    allowlist+=(--allowlist-var "^$name\$")
  done

  {
    echo "// Привязки пробы P0.4 к libghostty-vt. Файл генерирует run.sh bindings, вручную не правится."
    echo "// Источник: include/ghostty/vt.h набора vt, ghostty $GHOSTTY_VERSION_STRING ($GHOSTTY_REV)."
    echo
    LIBCLANG_PATH="$(dirname "$clang")/../lib" RUSTFMT="$rustfmt" "$bindgen" "$header" \
      --rust-target "$rust_version" \
      --rust-edition "$edition" \
      --no-doc-comments \
      --no-layout-tests \
      "${allowlist[@]}" \
      --formatter rustfmt \
      -- -target "arm64-apple-macos$min_macos" -isysroot "$sdk" -I "$set_dir/include" -DGHOSTTY_STATIC
  } >"$out" || die "bindgen не сгенерировал привязки"
}

case "${1:-}" in
  "")
    tmp="$(mktemp "${TMPDIR:-/tmp}/shellio-vt-bindings.XXXXXX")"
    trap 'rm -f "$tmp"' EXIT
    generate "$tmp"
    mkdir -p "$(dirname "$target_file")"
    if cmp -s "$tmp" "$target_file"; then
      echo "привязки не изменились: $target_file"
    else
      cp "$tmp" "$target_file"
      echo "привязки обновлены: $target_file"
    fi
    ;;
  --check)
    tmp="$(mktemp "${TMPDIR:-/tmp}/shellio-vt-bindings.XXXXXX")"
    trap 'rm -f "$tmp"' EXIT
    generate "$tmp"
    [[ -f "$target_file" ]] || die "нет $target_file, выполните spikes/probe-session-survival/run.sh bindings"
    if ! cmp -s "$tmp" "$target_file"; then
      diff -u -L "$target_file" -L сгенерировано "$target_file" "$tmp" >&2 || true
      die "привязки расходятся с заголовками набора vt и списками vt-bindings.sh, выполните spikes/probe-session-survival/run.sh bindings"
    fi
    echo "привязки совпадают с заголовками набора vt: $target_file"
    ;;
  *)
    echo "usage: $0 [--check]" >&2
    exit 2
    ;;
esac
