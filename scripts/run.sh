#!/usr/bin/env bash
# Запускает владельца из подписанного бандла через open -W -n с временным профилем и ждёт его выхода.
#
#   run.sh [--state <каталог>] [--scene] [<бандл>] [-- <аргументы владельца>…]
#
# Бандл по умолчанию — target/bundle/shellio.app, собирает и проверяет его цель make run. Аргументы после
# -- уходят владельцу через open --args: make run ARGS="--gallery" открывает галерею во временном профиле. Каталог состояния —
# новый временный SHELLIO_STATE_DIR в /tmp: путь сокета должен быть короче 104 байт, а $TMPDIR сам занимает
# около 50. Вывод владельца пишется в target/run/<время>-<pid>/ и показывается в терминале.
#
# --state запускает владельца на постоянном профиле: каталог создаётся при первом запуске и после выхода не
# удаляется. Занятая блокировка профиля — отказ до запуска: второй экземпляр передал бы аргументы живому
# владельцу и вышел. --scene до запуска собирает shellioctl, а после строки готовности собирает во владельце
# демо-сцену (scripts/run-scene.sh); её отказ окно не закрывает.
#
# open не передаёт код выхода приложения и не доставляет ему Ctrl-C: LaunchServices запускает приложение вне
# группы процессов терминала. Поэтому прерывание отправляет SIGTERM только владельцу, чей PID взят из его
# строки готовности и сверен с бинарём бандла. Профиль удаляется только после выхода этого экземпляра:
# open -W ждёт ровно запущенный им процесс, а свободная блокировка профиля подтверждает, что её никто не держит.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"

READY="shellio: владелец готов, pid "
# Приложение, вышедшее раньше, чем open подписался на его завершение, даёт open код 1 с этим текстом.
OPEN_EXITED_EARLY="initial call to kevent() failed: No such process"

die() {
  echo "error: $*" >&2
  exit 1
}

usage() {
  echo "usage: $0 [--state <каталог>] [--scene] [<бандл>] [-- <аргументы владельца>…]" >&2
  exit 2
}

bundle=""
state=""
scene=0
owner_args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
  --)
    shift
    owner_args=("$@")
    break
    ;;
  --state)
    [[ $# -ge 2 && -n "$2" && -z "$state" ]] || usage
    state="$2"
    shift 2
    ;;
  --scene)
    scene=1
    shift
    ;;
  -*)
    usage
    ;;
  *)
    [[ -z "$bundle" ]] || usage
    bundle="$1"
    shift
    ;;
  esac
done
app="${bundle:-$root/target/bundle/shellio.app}"
[[ -d "$app" ]] || die "нет бандла $app: соберите его через make bundle"
app="$(cd "$app" && pwd -P)"
exe="$app/Contents/MacOS/shellio"
[[ -x "$exe" ]] || die "в бандле нет исполняемого $exe"

ctl=""
if [[ $scene -eq 1 ]]; then
  ctl="$("$here/run-scene.sh" build)"
fi

persist=0
if [[ -n "$state" ]]; then
  persist=1
  mkdir -p "$state"
  state="$(cd "$state" && pwd -P)"
  [[ "$(printf %s "$state/shellio.sock" | wc -c)" -lt 104 ]] ||
    die "путь сокета $state/shellio.sock длиннее 103 байт: выберите каталог профиля короче"
  if [[ -e "$state/shellio.lock" ]] && ! /usr/bin/lockf -s -k -t 0 "$state/shellio.lock" true; then
    die "профиль $state занят: его владелец ещё работает"
  fi
  echo "профиль: $state"
else
  state="$(mktemp -d /tmp/shellio-run.XXXXXX)"
fi
lock="$state/shellio.lock"

logs="$root/target/run/$(date +%Y%m%d-%H%M%S)-$$"
mkdir -p "$logs"
out="$logs/stdout"
err="$logs/stderr"
: >"$out"
: >"$err"

# Временный профиль удаляется, пока экземпляр не запущен или когда его выход подтверждён; постоянный остаётся.
keep_state=0
tails=()
scene_pid=""

stop_tails() {
  local pid
  for pid in ${tails[@]+"${tails[@]}"}; do
    kill "$pid" 2>/dev/null || true
    # wait забирает задачу без строки оболочки «Terminated».
    wait "$pid" 2>/dev/null || true
  done
  tails=()
}

cleanup() {
  if [[ -n "$scene_pid" ]]; then
    kill "$scene_pid" 2>/dev/null || true
    wait "$scene_pid" 2>/dev/null || true
  fi
  stop_tails
  [[ $persist -eq 0 ]] || return 0
  if [[ $keep_state -eq 0 ]]; then
    rm -rf "$state"
  else
    echo "профиль оставлен: $state" >&2
  fi
}
trap cleanup EXIT

owner_pid() {
  sed -n "s/^${READY}\([0-9][0-9]*\), .*/\1/p" "$out" | head -n 1
}

owner_socket() {
  sed -n "s/^${READY}[0-9][0-9]*, сокет \(.*\), режим .*/\1/p" "$out" | head -n 1
}

# Ждёт строку готовности, пока жив open, и собирает сцену; вывод — в терминал и в scene.log.
run_scene() {
  local sock=""
  while [[ -z "$sock" ]]; do
    sock="$(owner_socket)"
    [[ -n "$sock" ]] && break
    kill -0 "$open_pid" 2>/dev/null || return 0
    sleep 0.1
  done
  "$here/run-scene.sh" apply "$ctl" "$sock" >"$logs/scene.log" 2>&1 ||
    echo "сцена не собрана, окно остаётся открытым" >>"$logs/scene.log"
  cat "$logs/scene.log" >&2
}

# Ctrl-C и SIGTERM сценария передаются владельцу; сценарий продолжает ждать его выхода.
stop_owner() {
  local pid
  pid="$(owner_pid)"
  if [[ -z "$pid" ]]; then
    echo "владелец ещё не сообщил PID: сигнал не отправлен, повторите после строки готовности" >&2
    return
  fi
  if [[ "$(ps -ww -p "$pid" -o comm= 2>/dev/null || true)" != "$exe" ]]; then
    echo "pid $pid — не бинарь бандла $app: сигнал не отправлен" >&2
    return
  fi
  echo "завершаю владельца: SIGTERM pid $pid" >&2
  kill -TERM "$pid" 2>/dev/null || true
}

# Фоновые задачи неинтерактивной оболочки не получают SIGINT терминала: и open, и показ вывода
# переживают Ctrl-C, пока ловушка передаёт его владельцу.
trap stop_owner INT TERM
tail -n +1 -f "$out" &
tails+=("$!")
tail -n +1 -f "$err" >&2 &
tails+=("$!")

keep_state=1
open_args=(-W -n --env "SHELLIO_STATE_DIR=$state" --stdout "$out" --stderr "$err" "$app")
if [[ ${#owner_args[@]} -gt 0 ]]; then
  open_args+=(--args "${owner_args[@]}")
fi
open "${open_args[@]}" >"$logs/open.log" 2>&1 &
open_pid=$!
if [[ $scene -eq 1 ]]; then
  run_scene &
  scene_pid=$!
fi

status=0
while :; do
  wait "$open_pid" && status=0 || status=$?
  # wait, прерванный ловушкой, возвращает код больше 128, пока open ещё ждёт приложение.
  kill -0 "$open_pid" 2>/dev/null || break
done
if [[ $status -gt 128 ]]; then
  wait "$open_pid" && status=0 || status=$?
fi
trap - INT TERM
stop_tails

if [[ -e "$lock" ]] && ! /usr/bin/lockf -s -k -t 0 "$lock" true; then
  die "блокировку профиля держит процесс: экземпляр не вышел"
fi
keep_state=0

if [[ $status -ne 0 ]] && ! { [[ $status -eq 1 ]] && grep -qF "$OPEN_EXITED_EARLY" "$logs/open.log"; }; then
  die "open завершился с кодом $status: $(tr '\n' ' ' <"$logs/open.log")"
fi
grep -q "^$READY" "$out" || die "владелец не сообщил о готовности: $(tr '\n' ' ' <"$err")"
echo "владелец вышел, журнал: $logs"
