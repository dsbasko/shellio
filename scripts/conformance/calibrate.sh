#!/usr/bin/env bash
# Калибровка допусков сверки с прототипом (ADR-0037): make calibrate.
#
#   calibrate.sh
#
# Замер двух сторон на материалах conformance/calibration/ (text.json, controls.json, icons.svg): сторона прототипа
# — shellio-snap calibrate в закреплённом Chrome, два запуска из свежих процессов с побайтным сравнением; сторона
# продукта — страница галереи «Калибровка» (gallery.calibration) владельца подписанного бандла на временном профиле
# /tmp/shellio-e2e.*, в каждой ячейке controls.json чтения ui.window и ui.inspect и снимок окна
# screencapture -x -o -l с переводом в sRGB через sips. Снимок принимается, когда маркер поколения в PNG равен
# поколению квитанции команды ячейки, не дольше 3 с. Допуск ширины текста и порог маски выводит
# shellio-e2e-oracle calibrate.
#
# Каталог прогона — target/calibrate/<время>-<pid>/: log, chrome.1/ и chrome.2/, product/<ячейка>.{inspect.json,
# window.json,png}, measured.json (сырые числа обеих сторон, пишутся при любом исходе вывода), calibration.json и
# вывод оракула. conformance/calibration/calibration.json появляется только при коде 0. Запись уже есть — отказ:
# после состоявшейся калибровки повтор запрещён.
#
# Предусловия, их отсутствие — ошибка исполнения «предусловие: …» до сборки: оконные менеджеры выключены, у
# терминала-предка есть Screen Recording, бинарь бандла не запущен. Экран (масштаб 2, тон в допуске) сверяет сам
# замер оракула по фону листа. Клавиши и нажатия не посылаются.
#
# Бандл собирается и проверяется здесь же, поэтому сценарий идёт под блокировкой набора ghostty: его вызывает цель
# make calibrate. Съём идёт под блокировкой passports.lock, занятая — отказ сразу. Сигналы и уборка адресуют
# только PID из hello сокета профиля прогона, сверенный с бинарём бандла.
#
# Коды: 0 — записано, 1 — ошибка исполнения (в том числе контроли не разделены), 2 — неверный вызов.
set -Eeuo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
cargo="${CARGO:-$HOME/.cargo/bin/cargo}"
cache="${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}"
lock="$cache/passports.lock"
materials="$root/conformance/calibration"
record="$materials/calibration.json"

if [[ $# -gt 0 ]]; then
  echo "usage: $0" >&2
  exit 2
fi

if [[ -z "${SHELLIO_PASSPORTS_LOCKED:-}" ]]; then
  mkdir -p "$cache"
  rc=0
  SHELLIO_PASSPORTS_LOCKED=1 /usr/bin/lockf -s -t 0 "$lock" "$0" || rc=$?
  if [[ $rc -eq 75 ]]; then
    echo "calibrate: блокировка $lock занята другим съёмом" >&2
    exit 1
  fi
  exit "$rc"
fi

SRGB_PROFILE="/System/Library/ColorSync/Profiles/sRGB Profile.icc"
# Оконные менеджеры, которые переставляют окна сами (P0.2).
WINDOW_MANAGERS=(AeroSpace yabai Amethyst)
# Ячейка паспорта и аргументы gallery cell.
CELLS=("light-normal light normal" "light-high light increased" "dark-normal dark normal" "dark-high dark increased")

OWNER_START_S=20
EXIT_S=10
KILL_S=5
MARKER_MS=3000
POLL=0.1
CTL_TIMEOUT_MS=2000

run=""
app=""
exe=""
ctl_bin=""
oracle_bin=""
snap_bin=""
profile=""
owner_pid=""
HELLO_PID=""
GEN=""
WHY=""

say() {
  echo "calibrate: $*" >&2
  if [[ -n "$run" ]]; then
    printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >>"$run/log"
  fi
}

fatal() {
  say "ошибка исполнения: $1"
  exit 1
}

now_ms() {
  /usr/bin/perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'
}

oneline() {
  [[ -f "$1" ]] || return 0
  tr '\n' ' ' <"$1" | cut -c 1-400
}

alive() {
  local stat
  [[ -n "$1" ]] || return 1
  stat="$(ps -o stat= -p "$1" 2>/dev/null || true)"
  stat="${stat//[[:space:]]/}"
  [[ -n "$stat" && "$stat" != Z* ]]
}

comm_of() {
  [[ -n "$1" ]] || return 0
  ps -ww -p "$1" -o comm= 2>/dev/null || true
}

is_run_exe() {
  [[ -n "$1" && "$(comm_of "$1")" == "$exe" ]]
}

wait_gone() {
  local pid="$1" deadline
  deadline=$(($(now_ms) + $2 * 1000))
  while alive "$pid"; do
    (($(now_ms) < deadline)) || return 1
    sleep "$POLL"
  done
}

# PID процессов с бинарём бандла прогона через пробел.
run_processes() {
  ps -ax -ww -o pid=,comm= | awk -v exe="$exe" '
    { pid = $1; line = $0; sub(/^ *[0-9]+ +/, "", line); if (line == exe) { printf "%s%s", sep, pid; sep = " " } }
  '
}

# SIGTERM, а без выхода в срок — SIGKILL; только процессу бандла прогона.
terminate() {
  local pid="$1"
  if ! is_run_exe "$pid"; then
    say "pid $pid — не бинарь бандла прогона: сигнал не отправлен"
    return 0
  fi
  kill -TERM "$pid" 2>/dev/null || true
  wait_gone "$pid" "$EXIT_S" && return 0
  say "pid $pid не вышел за $EXIT_S с после SIGTERM, SIGKILL"
  kill -KILL "$pid" 2>/dev/null || true
  wait_gone "$pid" "$KILL_S" || say "pid $pid не вышел и после SIGKILL"
}

lock_held() {
  [[ -f "$1/shellio.lock" ]] || return 1
  ! /usr/bin/lockf -s -k -t 0 "$1/shellio.lock" true 2>/dev/null
}

ctl() {
  local prefix="$1"
  shift
  env -u SHELLIO_SOCKET "SHELLIO_STATE_DIR=$profile" "$ctl_bin" "$@" >"$prefix.stdout" 2>"$prefix.stderr"
}

# hello по сокету профиля; PID — в HELLO_PID. Режим ready принимает только готового владельца, any — и уходящего.
hello() {
  local prefix="$1" mode="${2:-ready}" pid owner
  HELLO_PID=""
  ctl "$prefix" --json --timeout "$CTL_TIMEOUT_MS" version || return 1
  pid="$(plutil -extract ok.hello.pid raw -o - "$prefix.stdout" 2>/dev/null)" || return 1
  owner="$(plutil -extract ok.hello.owner raw -o - "$prefix.stdout" 2>/dev/null)" || return 1
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  [[ "$mode" == any || "$owner" == ready ]] || return 1
  HELLO_PID="$pid"
}

# Команда shellioctl с квитанцией ui.accepted; поколение — в GEN, иначе причина в WHY.
ui_cmd() {
  local prefix="$1" code=0
  shift
  GEN=""
  WHY=""
  ctl "$prefix" --json --timeout "$CTL_TIMEOUT_MS" "$@" || code=$?
  if [[ $code -ne 0 ]]; then
    WHY="shellioctl $*: код $code: $(oneline "$prefix.stdout") $(oneline "$prefix.stderr")"
    return 1
  fi
  if ! GEN="$(plutil -extract ok.ui.accepted.generation raw -o - "$prefix.stdout" 2>/dev/null)"; then
    WHY="shellioctl $*: нет квитанции ui.accepted: $(oneline "$prefix.stdout")"
    return 1
  fi
}

launch_gallery() {
  local dir="$run/owner" status=0 deadline
  mkdir -p "$dir"
  profile="$(mktemp -d /tmp/shellio-e2e.XXXXXX)" || fatal "временный профиль не создан"
  say "владелец галереи: профиль $profile"
  : >"$dir/owner.stdout"
  : >"$dir/owner.stderr"
  open -n --env "SHELLIO_STATE_DIR=$profile" --stdout "$dir/owner.stdout" --stderr "$dir/owner.stderr" "$app" \
    --args --gallery >"$dir/open.log" 2>&1 || status=$?
  [[ $status -eq 0 ]] || fatal "open -n завершился с кодом $status: $(oneline "$dir/open.log")"
  deadline=$(($(now_ms) + OWNER_START_S * 1000))
  while ! hello "$dir/hello"; do
    (($(now_ms) < deadline)) ||
      fatal "владелец не ответил на hello за $OWNER_START_S с: $(oneline "$dir/hello.stdout") $(oneline "$dir/owner.stderr")"
    sleep "$POLL"
  done
  is_run_exe "$HELLO_PID" || fatal "PID $HELLO_PID из hello — не бинарь бандла прогона: $(comm_of "$HELLO_PID")"
  owner_pid="$HELLO_PID"
  # hello отвечает раньше, чем владелец допишет строку готовности: строка ждётся со сроком.
  until grep -qF "режим gallery" "$dir/owner.stdout"; do
    (($(now_ms) < deadline)) ||
      fatal "строка готовности владельца не называет режим gallery за $OWNER_START_S с: $(oneline "$dir/owner.stdout")"
    sleep "$POLL"
  done
}

# Чтения и снимок ячейки при поколении gen: повтор, пока маркер поколения в PNG и поколение ui.window не равны gen.
capture_cell() {
  local cell="$1" gen="$2" dir="$run/product" deadline number seen shown tries=0 code
  deadline=$(($(now_ms) + MARKER_MS))
  while :; do
    tries=$((tries + 1))
    ctl "$dir/$cell.window" --json --timeout "$CTL_TIMEOUT_MS" ui window ||
      fatal "$cell: ui.window не прочитан: $(oneline "$dir/$cell.window.stdout") $(oneline "$dir/$cell.window.stderr")"
    ctl "$dir/$cell.inspect" --json --timeout "$CTL_TIMEOUT_MS" ui inspect ||
      fatal "$cell: ui.inspect не прочитан: $(oneline "$dir/$cell.inspect.stderr")"
    number="$(plutil -extract ok.ui.window.window_number raw -o - "$dir/$cell.window.stdout" 2>/dev/null)" ||
      fatal "$cell: в ui.window нет window_number"
    shown="$(plutil -extract ok.ui.window.generation raw -o - "$dir/$cell.window.stdout" 2>/dev/null)" ||
      fatal "$cell: в ui.window нет generation"
    rm -f "$dir/$cell.raw.png" "$dir/$cell.png"
    /usr/sbin/screencapture -x -o -l "$number" "$dir/$cell.raw.png" >"$dir/$cell.capture.log" 2>&1 ||
      fatal "$cell: screencapture -l $number отказал: $(oneline "$dir/$cell.capture.log")"
    [[ -s "$dir/$cell.raw.png" ]] || fatal "$cell: screencapture -l $number не записал PNG"
    sips --matchTo "$SRGB_PROFILE" "$dir/$cell.raw.png" --out "$dir/$cell.png" >"$dir/$cell.sips.log" 2>&1 ||
      fatal "$cell: sips не перевёл PNG в sRGB: $(oneline "$dir/$cell.sips.log")"
    code=0
    seen="$("$oracle_bin" marker --inspect "$dir/$cell.inspect.stdout" --window "$dir/$cell.window.stdout" \
      --png "$dir/$cell.png" 2>"$dir/$cell.marker.stderr")" || code=$?
    case $code in
      0)
        if [[ "$seen" == "$gen" && "$shown" == "$gen" ]]; then
          cp "$dir/$cell.window.stdout" "$dir/$cell.window.json"
          cp "$dir/$cell.inspect.stdout" "$dir/$cell.inspect.json"
          say "$cell: поколение $gen в маркере и в ui.window за $tries попыт."
          return 0
        fi
        ;;
      4) ;;
      *) fatal "$cell: оракул marker — код $code: $(oneline "$dir/$cell.marker.stderr")" ;;
    esac
    (($(now_ms) < deadline)) ||
      fatal "$cell: отрисовка поколения $gen не наблюдалась за $MARKER_MS мс: маркер ${seen:-не читается}, ui.window $shown"
  done
}

cleanup() {
  local status=$? deadline
  trap '' INT TERM
  set +eu
  # bash 3.2 при обрыве по set -u выходит с кодом 0: исход без отметки конца — ошибка исполнения.
  if [[ $completed -ne 1 && $status -eq 0 ]]; then
    say "сценарий оборван до конца"
    status=1
  fi
  if [[ -n "$profile" && -d "$profile" ]]; then
    if lock_held "$profile"; then
      deadline=$(($(now_ms) + OWNER_START_S * 1000))
      until hello "$run/owner/hello-stop" any; do
        lock_held "$profile" || break
        (($(now_ms) < deadline)) || break
        sleep "$POLL"
      done
      [[ -z "$HELLO_PID" ]] || terminate "$HELLO_PID"
    fi
    if lock_held "$profile"; then
      say "экземпляр прогона держит блокировку профиля $profile, профиль оставлен"
      status=1
    else
      rm -rf "$profile"
    fi
  fi
  exit "$status"
}

[[ ! -e "$record" ]] ||
  fatal "$record уже есть: после состоявшейся калибровки повтор запрещён, допуски закреплены"
for name in "${WINDOW_MANAGERS[@]}"; do
  pid="$(pgrep -x "$name" | head -n 1 || true)"
  [[ -z "$pid" ]] ||
    fatal "предусловие: запущен оконный менеджер $name (pid $pid), он переставляет окна: выключите его на время замера"
done
preflight="$(osascript -l JavaScript -e 'ObjC.import("CoreGraphics"); ObjC.bindFunction("CGPreflightScreenCaptureAccess", ["bool", []]); String($.CGPreflightScreenCaptureAccess())' 2>/dev/null || true)"
[[ "$preflight" == true ]] ||
  fatal "предусловие: нет Screen Recording у терминала-предка: CGPreflightScreenCaptureAccess — ${preflight:-ошибка}"
[[ -n "${SHELLIO_GHOSTTY_LOCKED:-}" ]] ||
  fatal "нет блокировки набора ghostty: бандл собирается под ней, запускайте make calibrate"

run="$root/target/calibrate/$(date +%Y%m%d-%H%M%S)-$$"
mkdir -p "$run/product"
: >"$run/log"
say "каталог прогона: $run"

completed=0
trap cleanup EXIT
trap 'say "прерван сигналом"; exit 1' INT TERM

cd "$root"
CARGO="$cargo" "$root/scripts/bundle.sh" >"$run/bundle.log" 2>&1 || fatal "бандл не собран: $run/bundle.log"
"$root/scripts/check-bundle.sh" >"$run/check-bundle.log" 2>&1 || fatal "бандл не прошёл проверку: $run/check-bundle.log"
app="$(cd "$root/target/bundle/shellio.app" && pwd -P)"
exe="$app/Contents/MacOS/shellio"
[[ -x "$exe" ]] || fatal "в бандле нет исполняемого $exe"
left="$(run_processes)"
[[ -z "$left" ]] || fatal "предусловие: бинарь бандла прогона уже запущен (PID $left), завершите его"

"$cargo" build --release --locked -q -p shellioctl -p shellio-e2e >"$run/build.log" 2>&1 ||
  fatal "shellioctl и оракул не собраны: $run/build.log"
"$cargo" build --locked -q -p shellio-conformance --bin shellio-snap >>"$run/build.log" 2>&1 ||
  fatal "shellio-snap не собран: $run/build.log"
target="${CARGO_TARGET_DIR:-$root/target}"
ctl_bin="$target/release/shellioctl"
oracle_bin="$target/release/shellio-e2e-oracle"
snap_bin="$target/debug/shellio-snap"
for bin in "$ctl_bin" "$oracle_bin" "$snap_bin"; do
  [[ -x "$bin" ]] || fatal "нет собранного бинаря $bin"
done

# Сторона прототипа: два запуска из свежих процессов обязаны совпасть побайтно.
"$root/scripts/conformance/tools.sh" chrome >&2
chrome="$("$root/scripts/conformance/tools.sh" chrome-path)"
for shot in 1 2; do
  "$snap_bin" calibrate --chrome "$chrome" --root "$root" --out "$run/chrome.$shot" 2>"$run/chrome.$shot.log" ||
    fatal "замер Chrome, запуск $shot не удался: $(oneline "$run/chrome.$shot.log")"
done
diff -r "$run/chrome.1" "$run/chrome.2" >"$run/chrome.diff" 2>&1 ||
  fatal "два замера Chrome разошлись: $run/chrome.diff"
say "сторона прототипа: два замера совпали"

# Сторона продукта: страница «Калибровка» в каждой ячейке.
launch_gallery
ui_cmd "$run/owner/calibration" gallery calibration || fatal "$WHY"
for entry in "${CELLS[@]}"; do
  read -r cell lightness contrast <<<"$entry"
  ui_cmd "$run/product/$cell.cell" gallery cell "$lightness" "$contrast" || fatal "$cell: $WHY"
  capture_cell "$cell" "$GEN"
done
hello "$run/owner/hello-stop" && is_run_exe "$HELLO_PID" || fatal "владелец не ответил на hello перед остановкой"
terminate "$HELLO_PID"
alive "$owner_pid" && fatal "владелец pid $owner_pid не завершился"

code=0
"$oracle_bin" calibrate --chrome "$run/chrome.1" --product "$run/product" --materials "$materials" \
  --raw "$run/measured.json" --out "$run/calibration.json" 2>"$run/oracle.stderr" || code=$?
cat "$run/oracle.stderr" >&2
[[ $code -eq 0 ]] || fatal "калибровка не состоялась (код оракула $code), сырые числа — $run/measured.json"
cp "$run/calibration.json" "$materials/.calibration.json.part"
mv "$materials/.calibration.json.part" "$record"
say "калибровка записана: $record"
completed=1
