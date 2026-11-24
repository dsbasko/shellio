#!/usr/bin/env bash
# Латинский источник ввода на время команды (план P1.5, «Правила выбора», «Источник ввода через JXA»; ADR-0036).
#
#   with-latin-input.sh <команда> [<аргумент>…]
#
# Сценарии make e2e набирают текст через System Events по раскладке и требуют латинский источник ввода. Включённый
# ABC, US или British обёртка не трогает. Иначе она запоминает точный ID текущего источника, выбирает первый
# установленный латинский, проверяет выбор чтением, запускает команду, а после неё возвращает исходный ID и проверяет
# его чтением. Возврат идёт в общей уборке штатного выхода, SIGINT и SIGTERM: сигнал передаётся группе процессов
# команды, обёртка ждёт её выхода и только потом возвращает источник.
#
# Источник читается и выбирается мостом ObjC в JXA (osascript -l JavaScript, Carbon: TISCopyCurrentKeyboardInputSource,
# TISCreateInputSourceList, TISSelectInputSource). osascript и defaults берутся из PATH. Команда получает пустой
# stdin и свою группу процессов.
#
# Каталог прогона — target/e2e/input-source/<время>-<pid>/, его заменяет SHELLIO_INPUT_SOURCE_DIR: before — ID
# источника до команды, after — после уборки, log — журнал.
#
# Коды: код команды, если источник не менялся или возвращён; 130 и 143 — команда прервана SIGINT или SIGTERM;
# 1 — источник не прочитан, не переключён (команда не запускалась) или не восстановлен; 2 — неверный вызов.
# Тесты — crates/shellio-e2e/tests/input_source.rs.
set -Eeuo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

# Латинские источники ввода, как в предусловии сценариев: keystroke набирает текст по раскладке.
LATIN_LAYOUTS=(com.apple.keylayout.ABC com.apple.keylayout.US com.apple.keylayout.British
  com.apple.keylayout.British-PC)
# Опрос выбора и возврата: число чтений и пауза между ними, секунды.
TRIES=30
POLL=0.1
# Предусловие сценариев читает источник из этой настройки, система пишет её после выбора не сразу.
PREFS_DOMAIN=com.apple.HIToolbox
PREFS_KEY=AppleCurrentKeyboardLayoutInputSourceID

# Режимы: current — ID текущего источника; list — ID включённых источников клавиатуры, которые можно выбрать;
# select <id> — выбрать источник. Константа kTISProperty… — указатель, поэтому второй аргумент свойства — void *.
read -r -d '' JXA <<'EOF' || true
function run(argv) {
  ObjC.import('Carbon');
  ObjC.bindFunction('TISCopyCurrentKeyboardInputSource', ['id', []]);
  ObjC.bindFunction('TISCreateInputSourceList', ['id', ['id', 'bool']]);
  ObjC.bindFunction('TISSelectInputSource', ['int', ['id']]);
  ObjC.bindFunction('TISGetInputSourceProperty', ['id', ['id', 'void *']]);
  function prop(source, key) {
    return $.TISGetInputSourceProperty(source, key).js;
  }
  function sources() {
    const list = $.TISCreateInputSourceList($(), false);
    const out = [];
    for (let i = 0; i < list.count; i++) {
      const source = list.objectAtIndex(i);
      if (prop(source, $.kTISPropertyInputSourceCategory) === 'TISCategoryKeyboardInputSource' &&
        prop(source, $.kTISPropertyInputSourceIsSelectCapable)) {
        out.push(source);
      }
    }
    return out;
  }
  const mode = argv[0];
  if (mode === 'current') {
    const id = prop($.TISCopyCurrentKeyboardInputSource(), $.kTISPropertyInputSourceID);
    if (!id) throw new Error('ID текущего источника не прочитан');
    return id;
  }
  if (mode === 'list') {
    return sources().map(s => prop(s, $.kTISPropertyInputSourceID)).join('\n');
  }
  if (mode === 'select') {
    const hit = sources().filter(s => prop(s, $.kTISPropertyInputSourceID) === argv[1])[0];
    if (!hit) throw new Error('источник ' + argv[1] + ' не включён');
    const status = $.TISSelectInputSource(hit);
    if (status !== 0) throw new Error('TISSelectInputSource: код ' + status);
    return argv[1];
  }
  throw new Error('неизвестный режим ' + mode);
}
EOF

usage() {
  echo "usage: $0 <команда> [<аргумент>…]" >&2
  exit 2
}

[[ $# -ge 1 ]] || usage

dir="${SHELLIO_INPUT_SOURCE_DIR:-$root/target/e2e/input-source/$(date +%Y%m%d-%H%M%S)-$$}"
mkdir -p "$dir"
: >"$dir/log"

before=""
switched=0
child=""
signal=""
# bash 3.2 после ошибки set -u отдаёт ловушке EXIT код 0: код выхода ведётся в переменной.
result=1
SEEN=""

log() {
  printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >>"$dir/log"
}

say() {
  echo "with-latin-input: $*" >&2
  log "$*"
}

fail() {
  say "$1"
  result=1
  exit 1
}

tis() {
  osascript -l JavaScript -e "$JXA" "$@"
}

is_latin() {
  local name
  for name in "${LATIN_LAYOUTS[@]}"; do
    [[ "$1" != "$name" ]] || return 0
  done
  return 1
}

# Текущий источник по мосту стал $1; иначе последнее прочитанное — в SEEN.
wait_current() {
  local want="$1" i got=""
  for ((i = 0; i < TRIES; i++)); do
    got="$(tis current 2>>"$dir/log")" || got=""
    [[ "$got" != "$want" ]] || return 0
    sleep "$POLL"
  done
  SEEN="$got"
  return 1
}

# Настройка, которую читает предусловие сценариев, догнала выбор. Не догнала за срок — запись в журнале: о
# предусловии судит сам сценарий.
wait_prefs() {
  local want="$1" i got=""
  for ((i = 0; i < TRIES; i++)); do
    got="$(defaults read "$PREFS_DOMAIN" "$PREFS_KEY" 2>/dev/null)" || got=""
    [[ "$got" != "$want" ]] || return 0
    sleep "$POLL"
  done
  log "настройка $PREFS_DOMAIN $PREFS_KEY — ${got:-не прочитана}, а не $want"
}

restore() {
  local out
  if ! out="$(tis select "$before" 2>&1)"; then
    say "источник ввода не восстановлен: выбор $before отказал: $(printf '%s' "$out" | tr '\n' ' ')"
    return 1
  fi
  if ! wait_current "$before"; then
    say "источник ввода не восстановлен: текущий ${SEEN:-не прочитан}, а не $before"
    return 1
  fi
  wait_prefs "$before"
  log "источник ввода восстановлен: $before"
}

on_signal() {
  signal="$1"
  [[ -n "$child" ]] || return 0
  kill -s "$1" -- "-$child" 2>/dev/null || kill -s "$1" "$child" 2>/dev/null || true
}

finish() {
  local status="$result" after
  # Повторный сигнал не прерывает возврат источника.
  trap '' INT TERM
  set +eu
  if ((switched)); then
    restore || status=1
  fi
  if [[ -n "$before" ]]; then
    after="$(tis current 2>>"$dir/log")"
    printf '%s\n' "${after:-не прочитан}" >"$dir/after"
  fi
  exit "$status"
}

trap finish EXIT
trap 'on_signal INT' INT
trap 'on_signal TERM' TERM

before="$(tis current 2>"$dir/current.stderr")" || {
  before=""
  fail "источник ввода не прочитан: osascript отказал: $(tr '\n' ' ' <"$dir/current.stderr")"
}
[[ -n "$before" ]] || fail "источник ввода не прочитан: мост вернул пустой ID"
printf '%s\n' "$before" >"$dir/before"

if is_latin "$before"; then
  log "источник ввода $before — латинский, не меняется"
else
  installed="$(tis list 2>>"$dir/log")" || fail "источник ввода не переключён: список источников не прочитан"
  target=""
  for name in "${LATIN_LAYOUTS[@]}"; do
    if grep -qxF "$name" <<<"$installed"; then
      target="$name"
      break
    fi
  done
  [[ -n "$target" ]] ||
    fail "источник ввода не переключён: нет установленного латинского источника (ABC, US или British), текущий — $before"
  switched=1
  out="$(tis select "$target" 2>&1)" ||
    fail "источник ввода не переключён: выбор $target отказал: $(printf '%s' "$out" | tr '\n' ' ')"
  wait_current "$target" || fail "источник ввода не переключён: текущий ${SEEN:-не прочитан}, а не $target"
  wait_prefs "$target"
  say "источник ввода $before заменён на $target на время команды"
fi

sigcode() {
  if [[ "$1" == INT ]]; then
    echo 130
  else
    echo 143
  fi
}

if [[ -n "$signal" ]]; then
  log "сигнал $signal до запуска команды: команда не запускалась"
  result="$(sigcode "$signal")"
  exit "$result"
fi

# Своя группа процессов: сигнал обёртки доходит до всех потомков команды, а не только до первого.
set -m
"$@" </dev/null &
child=$!
set +m
[[ -z "$signal" ]] || on_signal "$signal"

code=0
while :; do
  code=0
  wait "$child" 2>/dev/null || code=$?
  # wait, прерванный сигналом, возвращается раньше команды.
  kill -0 "$child" 2>/dev/null || break
  sleep "$POLL"
done

if [[ -n "$signal" ]]; then
  log "команда прервана сигналом $signal, её код $code"
  result="$(sigcode "$signal")"
else
  log "команда вышла с кодом $code"
  result="$code"
fi
exit "$result"
