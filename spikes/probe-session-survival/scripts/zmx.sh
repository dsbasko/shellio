#!/usr/bin/env bash
# Наборы zmx пробы P0.4: закреплённая ревизия zmx и его ghostty с патчами agterm, по одной комбинации
# патчей на набор (план P0.4, «Сборка zmx»). Значения — в zmx-pin рядом.
#
#   zmx.sh sets                     имена наборов из zmx-pin
#   zmx.sh --print-stamp <набор>    ожидаемый штамп набора; сеть и Zig не нужны
#   zmx.sh --verify <набор>         сверяет опубликованный набор со штампом и манифестом; только читает
#   zmx.sh ensure <набор>           под блокировкой набора: сверка, а при расхождении сборка и
#                                   атомарная публикация; сеть нужна только для исходников и Zig,
#                                   которых нет в кэше
#   zmx.sh version <набор>          сверка и одна строка `zmx version` набора: строки zmx и ghostty_vt
#
# Набор лежит в $SHELLIO_PROBE_SETS_DIR/<набор>/, по умолчанию target/probes/session-survival/<набор>/,
# блокировка — <набор>.lock рядом. Zig, git-копии исходников и пакеты Zig кэшируются в
# $SHELLIO_CACHE_DIR/zmx/, отдельно от Zig и исходников набора ghostty продукта.
# ZIG_MIRROR задаёт другой источник архива Zig с плоской раскладкой зеркал; сумма проверяется так же.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
probe="$(cd "$here/.." && pwd)"
root="$(cd "$probe/../.." && pwd)"
# shellcheck source=zmx-pin
source "$here/zmx-pin"
# shellcheck source=set.sh
source "$here/set.sh"

base="${SHELLIO_PROBE_SETS_DIR:-$root/target/probes/session-survival}"
cache="${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}/zmx"
patch_dir="$probe/zmx-patches"
ghostty_patch_dir="$patch_dir/ghostty"

zig_name="zig-aarch64-macos-$ZMX_ZIG_VERSION"
zig_archive="$cache/zig/dl/$zig_name.tar.xz"
zig_home="$cache/zig/$ZMX_ZIG_VERSION"
zig_url="${ZIG_MIRROR:-https://ziglang.org/download/$ZMX_ZIG_VERSION}/$zig_name.tar.xz"

usage() {
  echo "usage: $0 sets | --print-stamp <набор> | --verify <набор> | ensure <набор> | version <набор>" >&2
  exit 2
}

# --- наборы и комбинации ---

set_names() {
  sed -n 's/^ZMX_SET_\([A-Z0-9_]*\)=.*/\1/p' "$here/zmx-pin" | tr '[:upper:]_' '[:lower:]-'
}

set_var() {
  printf 'ZMX_SET_%s\n' "$(printf '%s' "$1" | tr '[:lower:]-' '[:upper:]_')"
}

# Патчи набора по порядку. Комбинация проверяется до штампа, загрузки и сборки: имя набора известно,
# патчи существуют, не повторяются, и каждый нужный патч стоит раньше того, кому он нужен.
set_patches() {
  local set="$1" var list patch rule needer needed seen=" " name
  [[ "$set" =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]] || die "имя набора «${set}» не из строчных букв, цифр и дефисов"
  var="$(set_var "$set")"
  grep -q "^$var=" "$here/zmx-pin" || die "неизвестный набор «${set}», известны: $(set_names | tr '\n' ' ')"
  list="${!var}"
  [[ -n "$list" ]] || die "у набора $set пустая комбинация патчей"
  for patch in $list; do
    [[ -f "$patch_dir/$patch.patch" ]] || die "набор $set: нет патча zmx-patches/$patch.patch"
    [[ "$seen" != *" $patch "* ]] || die "набор $set: патч $patch указан дважды"
    for rule in $ZMX_REQUIRES; do
      needer="${rule%%:*}"
      needed="${rule#*:}"
      [[ "$needer" == "$patch" ]] || continue
      [[ "$seen" == *" $needed "* ]] ||
        die "набор $set: патч $patch требует $needed раньше себя в комбинации; без него zmx не собирается"
    done
    seen+="$patch "
  done
  for name in $list; do printf '%s\n' "$name"; done
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

build_flags() {
  printf '%s\n' "-Doptimize=$ZMX_OPTIMIZE" "-Dtarget=$ZMX_TARGET"
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

ghostty_patches() {
  shopt -s nullglob
  local files=("$ghostty_patch_dir"/*.patch)
  shopt -u nullglob
  ((${#files[@]} > 0)) || return 0
  printf '%s\n' "${files[@]}"
}

# Входы сборки набора, известные до загрузки исходников: значения pin, комбинация, патчи, рецепт и Xcode.
compute_stamp() {
  local set="$1" own name value list patches=() ghostty=() names="" file recipe
  # Отказ внутри подстановки процесса скрипт не останавливает, поэтому комбинация читается заранее.
  list="$(set_patches "$set")"
  own="$(set_var "$set")"
  for name in $list; do patches+=("$patch_dir/$name.patch"); done
  while IFS= read -r file; do ghostty+=("$file"); done < <(ghostty_patches)

  stamp=""
  # Номер поднимается, когда меняется смысл сверки или набора без изменения входов сборки.
  field stamp_format 1
  field set "$set"
  while IFS= read -r name; do
    [[ "$name" != ZMX_SET_* || "$name" == "$own" ]] || continue
    value="${!name}"
    [[ -n "$value" ]] || die "в zmx-pin пусто значение $name"
    [[ "$name" != "$own" ]] || name=ZMX_SET
    field "$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]')" "$value"
  done < <(sed -n 's/^\([A-Z][A-Z0-9_]*\)=.*/\1/p' "$here/zmx-pin")

  field build_flags "$(build_flags | tr '\n' ' ' | sed 's/ $//')"
  field patches_sha256 "$(patches_digest "${patches[@]}")"
  for file in ${ghostty[@]+"${ghostty[@]}"}; do names+="${names:+ }$(basename "$file")"; done
  field ghostty_patches "${names:-none}"
  field ghostty_patches_sha256 "$(patches_digest ${ghostty[@]+"${ghostty[@]}"})"
  recipe="$(cat "$here/zmx.sh" "$here/set.sh" | shasum -a 256)" || die "не удалось посчитать дайджест рецепта"
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

# --- Zig ---

# Установленным считается только каталог с отметкой о сумме архива, из которого он распакован.
zig_installed() {
  [[ -x "$zig_home/zig" ]] &&
    [[ "$(cat "$zig_home/.archive-sha256" 2>/dev/null)" == "$ZMX_ZIG_SHA256" ]] &&
    [[ "$("$zig_home/zig" version 2>/dev/null)" == "$ZMX_ZIG_VERSION" ]]
}

fetch_zig_archive() {
  local part="$zig_archive.part" got
  if [[ -f "$zig_archive" && "$(sha256_file "$zig_archive")" == "$ZMX_ZIG_SHA256" ]]; then
    return 0
  fi
  rm -f "$zig_archive"
  mkdir -p "$(dirname "$zig_archive")"
  # Недокачанный архив дозагружается; источник без поддержки диапазонов отдаёт его заново с начала.
  if [[ ! -f "$part" || "$(sha256_file "$part")" != "$ZMX_ZIG_SHA256" ]]; then
    echo "загружаю $zig_url"
    if ! curl --proto '=https' --tlsv1.2 -fL --retry 5 -C - -o "$part" "$zig_url"; then
      rm -f "$part"
      curl --proto '=https' --tlsv1.2 -fL --retry 5 -o "$part" "$zig_url"
    fi
  fi
  got="$(sha256_file "$part")"
  if [[ "$got" != "$ZMX_ZIG_SHA256" ]]; then
    rm -f "$part"
    die "SHA-256 архива $zig_name не совпадает: ожидается $ZMX_ZIG_SHA256, получено $got"
  fi
  mv "$part" "$zig_archive"
}

install_zig() {
  local tmp ver
  [[ "$(uname -m)" == arm64 ]] || die "zmx пробы собирается только на arm64"
  zig_installed && return 0
  fetch_zig_archive
  tmp="$(mktemp -d "$cache/zig/.extract.XXXXXX")"
  tar -xJf "$zig_archive" -C "$tmp" || {
    rm -rf "$tmp"
    die "архив $zig_archive не распаковывается"
  }
  [[ -x "$tmp/$zig_name/zig" ]] || {
    rm -rf "$tmp"
    die "в архиве нет $zig_name/zig"
  }
  ver="$("$tmp/$zig_name/zig" version)"
  [[ "$ver" == "$ZMX_ZIG_VERSION" ]] || {
    rm -rf "$tmp"
    die "архив содержит Zig $ver, ожидается $ZMX_ZIG_VERSION"
  }
  printf '%s\n' "$ZMX_ZIG_SHA256" >"$tmp/$zig_name/.archive-sha256"
  # Каталог появляется под окончательным именем только целиком, прерванная распаковка установкой не считается.
  rm -rf "$zig_home"
  mv "$tmp/$zig_name" "$zig_home"
  rm -rf "$tmp"
  echo "Zig $ZMX_ZIG_VERSION для zmx установлен: $zig_home/zig"
}

# --- исходники ---

# Ревизия хранится в git-копии кэша под своим ref, рабочее дерево — чистая копия из неё.
checkout_rev() {
  local mirror="$1" repo="$2" rev="$3" dest="$4" ref="refs/shellio/pin/$3"
  [[ -d "$mirror" ]] || git init -q --bare "$mirror"
  if ! git -C "$mirror" rev-parse -q --verify "$ref^{commit}" >/dev/null; then
    if git -C "$mirror" cat-file -e "$rev^{commit}" 2>/dev/null; then
      git -C "$mirror" update-ref "$ref" "$rev"
    else
      echo "загружаю $repo $rev"
      git -C "$mirror" fetch -q --depth 1 "$repo" "+$rev:$ref" || die "не удалось загрузить $rev из $repo"
    fi
  fi
  git init -q "$dest"
  git -C "$dest" fetch -q --depth 1 "file://$mirror" "$ref"
  git -C "$dest" -c advice.detachedHead=false checkout -q FETCH_HEAD
  [[ "$(git -C "$dest" rev-parse HEAD)" == "$rev" ]] ||
    die "получена ревизия $(git -C "$dest" rev-parse HEAD), ожидается $rev"
  [[ -z "$(git -C "$dest" status --porcelain)" ]] || die "дерево $dest после checkout не чистое"
}

# Патчи накладываются по порядку на чистое дерево; каждый сначала проверяется на дереве после предыдущих.
apply_patches() {
  local tree="$1" what="$2" patch prev="чистое дерево"
  shift 2
  for patch in "$@"; do
    echo "патч $what $(basename "$patch")"
    git -C "$tree" apply --check --whitespace=nowarn "$patch" ||
      die "патч $(basename "$patch") не накладывается на $what (после: $prev)"
    git -C "$tree" apply --whitespace=nowarn "$patch"
    prev="$(basename "$patch")"
  done
}

# --- сборка и публикация ---

# Строки zmx и ghostty_vt вывода `zmx version`. Каталоги сокетов и журналов zmx создаёт даже для version,
# поэтому он идёт в пустом окружении с временным ZMX_DIR.
zmx_version_of() {
  local bin="$1" tmp out
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/shellio-probe-zmx-version.XXXXXX")"
  out="$(env -i HOME="$tmp" PATH=/usr/bin:/bin TMPDIR="$tmp" ZMX_DIR="$tmp/zmx" "$bin" version)" || {
    rm -rf "$tmp"
    echo "$bin version завершился ошибкой" >&2
    return 1
  }
  rm -rf "$tmp"
  printf '%s\n' "$out" | awk -F'\t+' '$1 == "zmx" || $1 == "ghostty_vt" { printf "%s%s %s", sep, $1, $2; sep = "; " } END { print "" }'
}

check_binary() {
  local bin="$1" archs minos want="${ZMX_TARGET#aarch64-macos.}" line
  archs="$(lipo -archs "$bin" 2>/dev/null)" || die "$bin не читается lipo"
  [[ "$archs" == arm64 ]] || die "zmx собран для $archs, ожидается arm64"
  minos="$(otool -l "$bin" | awk '$1 == "minos" { print $2 }' | sort -u | tr '\n' ' ')"
  [[ "$minos" == "$want " ]] || die "минимум macOS zmx: ${minos:-нет}, ожидается $want"
  line="$(zmx_version_of "$bin")" || die "собранный zmx не сообщает версию"
  [[ "$line" == "zmx $ZMX_VERSION_STRING"* ]] || die "собранный zmx сообщает «${line}», ожидается версия $ZMX_VERSION_STRING"
}

run_zig() {
  (cd "$work/zmx" &&
    env -u MACOSX_DEPLOYMENT_TARGET ZIG_GLOBAL_CACHE_DIR="$cache/zig-global" "$zig_home/zig" build "$@")
}

build_set() {
  local set="$1" out="$2" list patches=() ghostty=() name file flags=() started=$SECONDS
  list="$(set_patches "$set")"
  for name in $list; do patches+=("$patch_dir/$name.patch"); done
  while IFS= read -r file; do ghostty+=("$file"); done < <(ghostty_patches)
  while IFS= read -r file; do flags+=("$file"); done < <(build_flags)

  install_zig
  checkout_rev "$cache/zmx-src.git" "$ZMX_REPO" "$ZMX_REV" "$work/zmx"
  grep -q "ghostty-org/ghostty#$ZMX_GHOSTTY_REV\"" "$work/zmx/build.zig.zon" ||
    die "build.zig.zon zmx $ZMX_REV закрепляет не ghostty $ZMX_GHOSTTY_REV"
  # 0002 направляет зависимость zmx на ../zmx-ghostty, поэтому деревья — соседи с этими именами.
  checkout_rev "$cache/ghostty-src.git" "$ZMX_GHOSTTY_REPO" "$ZMX_GHOSTTY_REV" "$work/zmx-ghostty"
  apply_patches "$work/zmx-ghostty" "ghostty $ZMX_GHOSTTY_REV" ${ghostty[@]+"${ghostty[@]}"}
  apply_patches "$work/zmx" "zmx $ZMX_REV" "${patches[@]}"
  grep -q '\.path = "\.\./zmx-ghostty"' "$work/zmx/build.zig.zon" ||
    die "набор $set: после патчей zmx берёт ghostty не из рабочей копии ../zmx-ghostty с патчами"

  echo "загружаю зависимости build.zig.zon"
  run_zig --fetch "${flags[@]}" || die "не удалось загрузить зависимости zmx"
  echo "собираю zmx набора $set (Zig $ZMX_ZIG_VERSION, ${flags[*]})"
  run_zig "${flags[@]}" || die "сборка zmx набора $set завершилась ошибкой"

  [[ -x "$work/zmx/zig-out/bin/zmx" ]] || die "сборка не создала zig-out/bin/zmx"
  mkdir -p "$out/bin"
  install -m 0755 "$work/zmx/zig-out/bin/zmx" "$out/bin/zmx"
  cp "$work/zmx/LICENSE" "$out/LICENSE"
  cp "$work/zmx-ghostty/LICENSE" "$out/LICENSE.ghostty"
  check_binary "$out/bin/zmx"
  echo "zmx набора $set собран за $((SECONDS - started)) с"
}

cmd_verify() {
  local set="$1"
  compute_stamp "$set"
  set_verify "$base/$set" "$stamp" || die "набор $set не сходится, выполните make probe-session"
  echo "набор $set актуален: $base/$set"
}

cmd_ensure() {
  local set="$1" lock before reason
  # Имя набора входит в путь блокировки, а негодная комбинация отвергается до блокировки и загрузки.
  set_patches "$set" >/dev/null
  lock="$base/$set.lock"
  if ! set_locked "$lock"; then
    set_with_lock "$lock" "набор $set" "$here/zmx.sh" ensure "$set"
    return
  fi
  compute_stamp "$set"
  before="$stamp"
  if reason="$(set_verify "$base/$set" "$before" 2>&1)"; then
    echo "набор $set актуален: $base/$set"
    return 0
  fi
  echo "набор $set собирается заново: ${reason##*$'\n'}"
  set_clean_leftovers "$base" "$set"
  mkdir -p "$cache/zig"
  work="$(mktemp -d "$base/.$set.build.XXXXXX")"
  trap 'rm -rf "$work"' EXIT
  build_set "$set" "$work/set"
  # Штамп описывает входы, из которых собран набор, поэтому они не должны меняться за время сборки.
  compute_stamp "$set"
  if [[ "$stamp" != "$before" ]]; then
    diff -u -L "до сборки" -L "после сборки" <(printf '%s' "$before") <(printf '%s' "$stamp") >&2 || true
    die "входы сборки набора $set изменились за время сборки, набор не публикуется"
  fi
  set_publish "$work/set" "$base/$set" "$stamp"
  set_verify "$base/$set" "$stamp" || die "опубликованный набор $set не сходится"
  echo "набор $set опубликован: $base/$set ($(zmx_version_of "$base/$set/bin/zmx"))"
}

cmd_version() {
  local set="$1"
  compute_stamp "$set"
  set_verify "$base/$set" "$stamp" || die "набор $set не сходится, выполните make probe-session"
  zmx_version_of "$base/$set/bin/zmx" || die "zmx набора $set не сообщает версию"
}

case "${1:-}" in
  sets)
    [[ $# -eq 1 ]] || usage
    set_names
    ;;
  --print-stamp)
    [[ $# -eq 2 ]] || usage
    compute_stamp "$2"
    printf '%s' "$stamp"
    ;;
  --verify)
    [[ $# -eq 2 ]] || usage
    cmd_verify "$2"
    ;;
  ensure)
    [[ $# -eq 2 ]] || usage
    mkdir -p "$base"
    cmd_ensure "$2"
    ;;
  version)
    [[ $# -eq 2 ]] || usage
    cmd_version "$2"
    ;;
  *) usage ;;
esac
