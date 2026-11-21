#!/usr/bin/env bash
# Запускает владельца из подписанного бандла через open -W -n с временным профилем и ждёт его выхода.
#
#   run.sh [<бандл>]
#
# Бандл по умолчанию — target/bundle/shellio.app, собирает и проверяет его цель make run. Каталог состояния —
# новый временный SHELLIO_STATE_DIR в /tmp: путь сокета должен быть короче 104 байт, а $TMPDIR сам занимает
# около 50. Вывод владельца пишется в target/run/<время>-<pid>/ и показывается в терминале.
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

[[ $# -le 1 ]] || {
  echo "usage: $0 [<бандл>]" >&2
  exit 2
}
app="${1:-$root/target/bundle/shellio.app}"
[[ -d "$app" ]] || die "нет бандла $app: соберите его через make bundle"
app="$(cd "$app" && pwd -P)"
exe="$app/Contents/MacOS/shellio"
[[ -x "$exe" ]] || die "в бандле нет исполняемого $exe"

logs="$root/target/run/$(date +%Y%m%d-%H%M%S)-$$"
mkdir -p "$logs"
out="$logs/stdout"
err="$logs/stderr"
: >"$out"
: >"$err"
state="$(mktemp -d /tmp/shellio-run.XXXXXX)"
lock="$state/shellio.lock"

# Профиль удаляется, пока экземпляр не запущен или когда его выход подтверждён.
keep_state=0
tails=()

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
  stop_tails
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
open -W -n --env "SHELLIO_STATE_DIR=$state" --stdout "$out" --stderr "$err" "$app" >"$logs/open.log" 2>&1 &
open_pid=$!

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
