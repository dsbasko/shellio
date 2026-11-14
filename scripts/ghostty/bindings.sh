#!/usr/bin/env bash
# Генерирует привязки shellio-sys-ghostty из include/ghostty.h опубликованного набора.
#
#   bindings.sh          переписывает crates/shellio-sys-ghostty/src/bindings.rs
#   bindings.sh --check  генерирует во временный файл и сравнивает с файлом в репозитории, не переписывая его
#
# Набор сверяет вызывающая цель make. Clang, SDK и rustfmt берутся явно, а не из окружения: тогда привязки
# зависят только от ghostty.h, закреплённых bindgen и Rust и выбранного Xcode.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
# shellcheck source=pin
source "$here/pin"

vendor="${SHELLIO_GHOSTTY_VENDOR:-$root/vendor/ghostty}"
target_file="$root/crates/shellio-sys-ghostty/src/bindings.rs"

# Встраивание поверхности, которое проверяет проба P0.3. Инспектор, разбиения, quicklook, ghostty_benchmark_cli,
# ghostty_set_window_background_blur и чтение пользовательской конфигурации не входят. Типы bindgen тянет из сигнатур.
functions=(
  ghostty_info
  ghostty_init
  ghostty_string_free
  ghostty_config_new
  ghostty_config_free
  ghostty_config_load_file
  ghostty_config_finalize
  ghostty_config_diagnostics_count
  ghostty_config_get_diagnostic
  ghostty_app_new
  ghostty_app_free
  ghostty_app_tick
  ghostty_app_userdata
  ghostty_app_set_focus
  ghostty_app_keyboard_changed
  ghostty_app_update_config
  ghostty_app_set_color_scheme
  ghostty_surface_config_new
  ghostty_surface_new
  ghostty_surface_free
  ghostty_surface_userdata
  ghostty_surface_update_config
  ghostty_surface_process_exited
  ghostty_surface_refresh
  ghostty_surface_draw
  ghostty_surface_set_content_scale
  ghostty_surface_set_focus
  ghostty_surface_set_occlusion
  ghostty_surface_set_size
  ghostty_surface_size
  ghostty_surface_foreground_pid
  ghostty_surface_tty_name
  ghostty_surface_set_color_scheme
  ghostty_surface_key_translation_mods
  ghostty_surface_key
  ghostty_surface_key_is_binding
  ghostty_surface_text
  ghostty_surface_paste_text
  ghostty_surface_preedit
  ghostty_surface_mouse_captured
  ghostty_surface_mouse_button
  ghostty_surface_mouse_pos
  ghostty_surface_mouse_scroll
  ghostty_surface_mouse_pressure
  ghostty_surface_ime_point
  ghostty_surface_request_close
  ghostty_surface_binding_action
  ghostty_surface_complete_clipboard_request
  ghostty_surface_deny_clipboard_request
  ghostty_surface_has_selection
  ghostty_surface_read_selection
  ghostty_surface_read_text
  ghostty_surface_free_text
  ghostty_surface_set_display_id
)
# Фазу инерции прокрутки libghostty принимает упакованной в int, поэтому до перечисления сигнатуры не доходят.
types=(
  ghostty_input_mouse_momentum_e
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
  local out="$1" bindgen clang sdk rustfmt rust_version edition min_macos header="$vendor/include/ghostty.h" name
  local allowlist=()
  [[ -f "$header" ]] || die "в наборе нет include/ghostty.h, выполните make ghostty"
  bindgen="$("$here/tools.sh" bindgen-path)"
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

  {
    echo "// Привязки к libghostty для shellio-sys-ghostty. Файл генерирует make bindings, вручную не правится."
    echo "// Источник: include/ghostty.h ghostty $GHOSTTY_VERSION_STRING ($GHOSTTY_REV)."
    echo
    LIBCLANG_PATH="$(dirname "$clang")/../lib" RUSTFMT="$rustfmt" "$bindgen" "$header" \
      --rust-target "$rust_version" \
      --rust-edition "$edition" \
      "${allowlist[@]}" \
      --formatter rustfmt \
      -- -target "arm64-apple-macos$min_macos" -isysroot "$sdk"
  } >"$out" || die "bindgen не сгенерировал привязки"
}

case "${1:-}" in
  "")
    tmp="$(mktemp "${TMPDIR:-/tmp}/shellio-bindings.XXXXXX")"
    trap 'rm -f "$tmp"' EXIT
    generate "$tmp"
    if cmp -s "$tmp" "$target_file"; then
      echo "привязки не изменились: $target_file"
    else
      cp "$tmp" "$target_file"
      echo "привязки обновлены: $target_file"
    fi
    ;;
  --check)
    tmp="$(mktemp "${TMPDIR:-/tmp}/shellio-bindings.XXXXXX")"
    trap 'rm -f "$tmp"' EXIT
    generate "$tmp"
    [[ -f "$target_file" ]] || die "нет $target_file, выполните make bindings"
    if ! cmp -s "$tmp" "$target_file"; then
      diff -u -L "$target_file" -L сгенерировано "$target_file" "$tmp" >&2 || true
      die "привязки в репозитории расходятся с ghostty.h набора, выполните make bindings"
    fi
    echo "привязки совпадают с ghostty.h набора: $target_file"
    ;;
  *)
    echo "usage: $0 [--check]" >&2
    exit 2
    ;;
esac
