# shellcheck shell=bash
# Общие функции наборов пробы в target/probes/session-survival/: манифест, сверка, блокировка и
# атомарная публикация (ADR-0034, «Уточнено в P0.4»). Подключается через source, сам не исполняется.
#
# Набор — каталог с обычными файлами, манифестом manifest (SHA-256 и путь каждого файла) и штампом stamp
# (входы сборки). Публикация собирает набор целиком рядом с конечным каталогом и ставит его на место
# переименованием, поэтому прерванная публикация не оставляет наполовину заменённого набора.

die() {
  echo "error: $*" >&2
  exit 1
}

sha256_file() {
  local sum
  sum="$(shasum -a 256 <"$1")" || die "не удалось прочитать $1"
  printf '%s\n' "${sum%% *}"
}

# Каталог набора содержит только обычные файлы и каталоги с именами, которые манифест записывает однозначно.
set_check_entries() {
  local bad
  bad="$(cd "$1" && find . -mindepth 1 ! -type f ! -type d -print -quit)" || die "не удалось обойти $1"
  [[ -z "$bad" ]] || {
    echo "в $1 есть ${bad#./}, который не является обычным файлом или каталогом" >&2
    return 1
  }
  bad="$(cd "$1" && find . \( -name '*\\*' -o -name $'*\n*' \) -print -quit)" || die "не удалось обойти $1"
  [[ -z "$bad" ]] || {
    echo "в $1 есть имя с обратной косой чертой или переводом строки" >&2
    return 1
  }
}

# Строка манифеста — SHA-256 и путь от корня набора. Скрытые файлы входят, manifest и stamp в корне — нет.
set_manifest_of() {
  (cd "$1" &&
    find . -mindepth 1 ! -type d ! -path ./manifest ! -path ./stamp -print0 |
    LC_ALL=C sort -z |
      xargs -0 shasum -a 256 |
      sed 's|^\([0-9a-f]\{64\}\)  \./|\1  |')
}

set_manifest_paths() {
  printf '%s\n' "$1" | sed 's/^[0-9a-f]\{64\}  //' | LC_ALL=C sort
}

set_print_some() {
  local label="$1" items="$2" count
  [[ -n "$items" ]] || return 0
  count="$(printf '%s\n' "$items" | wc -l | tr -d ' ')"
  printf '%s\n' "$items" | sed -n "1,5s|^|  $label: |p" >&2
  if ((count > 5)); then
    echo "  $label: и ещё $((count - 5))" >&2
  fi
}

# Сверка каталога с манифестом: каждый файл на месте с тем же SHA-256, лишних и недостающих нет.
set_matches_manifest() {
  local dir="$1" expected actual missing extra gone changed
  set_check_entries "$dir" || return 1
  expected="$(cat "$2")" || die "не удалось прочитать $2"
  actual="$(set_manifest_of "$dir")" || die "не удалось посчитать SHA-256 файлов $dir"
  [[ "$actual" != "$expected" ]] || return 0
  missing="$(LC_ALL=C comm -23 <(set_manifest_paths "$expected") <(set_manifest_paths "$actual"))"
  extra="$(LC_ALL=C comm -13 <(set_manifest_paths "$expected") <(set_manifest_paths "$actual"))"
  # Строки манифеста, которых нет в каталоге, при пути на месте означают изменённое содержимое.
  gone="$(LC_ALL=C comm -23 <(printf '%s\n' "$expected" | LC_ALL=C sort) <(printf '%s\n' "$actual" | LC_ALL=C sort))"
  changed="$(LC_ALL=C comm -12 <(set_manifest_paths "$gone") <(set_manifest_paths "$actual"))"
  echo "набор $dir расходится с манифестом:" >&2
  set_print_some "нет файла" "$missing"
  set_print_some "лишний файл" "$extra"
  set_print_some "изменён" "$changed"
  return 1
}

# Сверка опубликованного набора: штамп совпадает с ожидаемым $2, файлы — с манифестом. Только читает;
# причину расхождения пишет в stderr и возвращает 1.
set_verify() {
  local dir="$1" stamp="$2"
  if [[ ! -d "$dir" ]]; then
    echo "набор не опубликован в $dir" >&2
    return 1
  fi
  if [[ ! -f "$dir/stamp" ]]; then
    echo "в наборе $dir нет штампа" >&2
    return 1
  fi
  if ! cmp -s <(printf '%s' "$stamp") "$dir/stamp"; then
    diff -u -L ожидается -L "$dir/stamp" <(printf '%s' "$stamp") "$dir/stamp" >&2 || true
    echo "штамп набора $dir не совпадает с ожидаемым" >&2
    return 1
  fi
  if [[ ! -f "$dir/manifest" ]]; then
    echo "в наборе $dir нет манифеста" >&2
    return 1
  fi
  set_matches_manifest "$dir" "$dir/manifest" || {
    echo "набор $dir изменён после публикации" >&2
    return 1
  }
}

# Выполняет команду под блокировкой файла $1 с маркером SHELLIO_PROBE_SET_LOCKED=<файл>. Занятая
# блокировка не ожидается: вызов сразу завершается отказом с именем $2. Файл блокировки не удаляется.
set_with_lock() {
  local lock="$1" label="$2" started status=0
  shift 2
  mkdir -p "$(dirname "$lock")"
  # Код 75 занятой блокировки совпадает с EX_TEMPFAIL команды, поэтому команда сначала удаляет отметку о старте.
  started="$(mktemp "${TMPDIR:-/tmp}/shellio-probe-set-lock.XXXXXX")"
  SHELLIO_PROBE_SET_LOCKED="$lock" /usr/bin/lockf -s -k -t 0 "$lock" \
    /bin/sh -c 'rm -f "$1" && shift && exec "$@"' sh "$started" "$@" || status=$?
  if [[ -e "$started" ]]; then
    rm -f "$started"
    if [[ $status -eq 75 ]]; then
      die "$label занят: блокировку $lock держит другой процесс, повторите после его завершения"
    fi
    die "не удалось взять блокировку $lock (lockf завершился с кодом $status)"
  fi
  return "$status"
}

set_locked() {
  [[ "${SHELLIO_PROBE_SET_LOCKED:-}" == "$1" ]]
}

# Убирает остатки прерванных сборок и публикаций набора с именем $2 в каталоге $1.
set_clean_leftovers() {
  local base="$1" name="$2" left
  shopt -s nullglob
  for left in "$base/.$name".build.* "$base/.$name".new.* "$base/.$name".old.*; do
    rm -rf "$left"
  done
  shopt -u nullglob
}

# Публикует набор из каталога $1 в $2: манифест, сверка копии, штамп $3 последним, затем замена
# переименованием. Каталог $1 переносится в .<имя>.new.<pid> рядом с $2, поэтому оба на одном томе.
# SHELLIO_PROBE_SET_TEST_ABORT_BEFORE_SWAP=1 прерывает публикацию перед заменой; только для тестов.
set_publish() {
  local build="$1" dest="$2" stamp="$3" base name staged old manifest
  base="$(dirname "$dest")"
  name="$(basename "$dest")"
  staged="$base/.$name.new.$$"
  old="$base/.$name.old.$$"
  set_check_entries "$build" || die "каталог сборки $build нельзя опубликовать"
  [[ ! -e "$build/manifest" && ! -e "$build/stamp" ]] || die "в каталоге сборки есть manifest или stamp, эти имена заняты набором"
  rm -rf "$staged" "$old"
  mv "$build" "$staged"
  manifest="$(set_manifest_of "$staged")" || die "не удалось посчитать SHA-256 файлов $staged"
  printf '%s\n' "$manifest" >"$staged/manifest"
  set_matches_manifest "$staged" "$staged/manifest" || die "набор $staged не совпадает со своим манифестом"
  printf '%s' "$stamp" >"$staged/stamp"
  if [[ "${SHELLIO_PROBE_SET_TEST_ABORT_BEFORE_SWAP:-}" == 1 ]]; then
    die "публикация $name прервана перед заменой (SHELLIO_PROBE_SET_TEST_ABORT_BEFORE_SWAP)"
  fi
  if [[ -e "$dest" ]]; then
    mv "$dest" "$old"
  fi
  mv "$staged" "$dest"
  rm -rf "$old"
}
