#!/usr/bin/env bash
# Сценарий make e2e «единственный экземпляр» (P1.2): пункты матрицы приёмки плана single-instance-control на
# подписанном бандле и временных профилях.
#
#   single-instance.sh [<бандл>]
#
# Бандл по умолчанию — target/bundle/shellio.app, его собирает и проверяет цель make e2e. shellioctl сценарий
# собирает сам (release, --locked) и берёт путь из сообщения Cargo о собранном артефакте, как bundle.sh.
# Профили — новые каталоги /tmp/shellio-e2e.*: путь сокета в $TMPDIR близок к пределу 103 байт.
#
# Каталог прогона — target/e2e/single-instance/<время>-<pid>/: result.json (пункт, обязательность, статус,
# причина, путь сырого вывода относительно каталога прогона), журнал сценария log и сырой вывод каждого пункта
# в подкаталоге с его именем. Уборка пишет свой вывод в cleanup/.
#
# Сигналы и уборка адресуют только PID, полученные через hello сокетов прогона и сверенные с бинарём бандла
# прогона. Передний план читается через lsappinfo, разрешения Accessibility и Screen Recording не нужны. Нужны
# открытый сеанс GUI и разблокированный экран, клавиатуру и мышь во время прогона не трогают. Внешние условия
# опрашиваются с крайним сроком, фиксированных пауз вместо условия нет.
#
# Выход — после записи result.json и уборки: 0 — все обязательные пункты pass, 1 — ошибка исполнения, 3 — ошибки
# исполнения нет, но хотя бы один обязательный пункт не pass. Необязательные пункты на код не влияют.
set -Eeuo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
cargo="${CARGO:-$HOME/.cargo/bin/cargo}"

READY="shellio: владелец готов, pid "
HANDLED="shellio: запуск обработан "
RECEIPT="shellio: запуск передан владельцу (pid "
TERMINATED="shellio: завершение: SIGTERM"
# Приложение, вышедшее раньше, чем open -W подписался на его завершение, даёт open код 1 с этим текстом.
OPEN_EXITED_EARLY="initial call to kevent() failed: No such process"
FINDER_ID="com.apple.finder"
LSAPPINFO=/usr/bin/lsappinfo

# Сроки в секундах и миллисекундах. У второго запуска собственный срок 10 с, сценарий ждёт его дольше.
OWNER_START_S=20
SECOND_LAUNCH_S=20
EXIT_S=10
KILL_S=5
FINDER_S=5
FRONT_LIMIT_MS=3000
FRONT_CAP_MS=10000
POLL=0.1
CTL_TIMEOUT_MS=2000

ITEMS=(owner forward-open-n forward-direct folder-refused persist kill-recover two-profiles plain-open gui-quit)
REQUIRED=(1 1 1 1 1 1 1 0 0)
STATUS=()
REASON=()
for i in "${!ITEMS[@]}"; do
  STATUS[i]=not_run
  REASON[i]="не выполнен: прогон остановлен раньше этого пункта"
done

[[ $# -le 1 ]] || {
  echo "usage: $0 [<бандл>]" >&2
  exit 2
}

run="$root/target/e2e/single-instance/$(date +%Y%m%d-%H%M%S)-$$"
mkdir -p "$run"
for item in "${ITEMS[@]}"; do
  mkdir -p "$run/$item"
done
: >"$run/log"

error=""
err_at=""
# bash 3.2 после ошибки set -u отдаёт ловушке EXIT код 0: незаконченный прогон узнаётся по этому флагу.
completed=0
app=""
exe=""
ctl_bin=""
profile_a=""
profile_b=""
owner_out_a=""
owner_out_b=""
profiles=()
known_pids=()
open_jobs=()
direct_jobs=()
failures=()

# Значения, которые функции отдают через глобальные переменные: $(…) потерял бы known_pids и задания.
HELLO_PID=""
OWNER_PID=""
NEW_PROFILE=""
WHY=""
second_job=""
second_status=""
receipt_ms=""
front_ms=""
front_last=""

log() {
  printf '%s\n' "$*"
  printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >>"$run/log"
}

add_error() {
  error="${error:+$error; }$1"
  log "ошибка исполнения: $1"
}

# Ошибка исполнения: сценарий не может судить о пунктах. Ловушка EXIT пишет result.json и убирает.
fatal() {
  add_error "$1"
  exit 1
}

on_signal() {
  add_error "прерван сигналом $1"
  exit 1
}

now_ms() {
  /usr/bin/perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'
}

oneline() {
  [[ -f "$1" ]] || return 0
  tr '\n' ' ' <"$1" | cut -c 1-400
}

# Строка JSON: управляющие символы заменяются пробелом, обратная косая и кавычка экранируются.
json_string() {
  local text
  text="$(printf '%s' "$1" | LC_ALL=C tr '\001-\037' ' ')"
  text="${text//\\/\\\\}"
  text="${text//\"/\\\"}"
  printf '"%s"' "$text"
}

record() {
  local item="$1" status="$2" reason="$3" i
  for i in "${!ITEMS[@]}"; do
    if [[ "${ITEMS[i]}" == "$item" ]]; then
      STATUS[i]="$status"
      REASON[i]="$reason"
      log "$item: $status — $reason"
      return 0
    fi
  done
  fatal "неизвестный пункт $item"
}

# Итог пункта по списку failures: пусто — pass с текстом $2.
conclude() {
  local item="$1" ok="$2" text="" x
  for x in ${failures[@]+"${failures[@]}"}; do
    text="${text:+$text; }$x"
  done
  if [[ -n "$text" ]]; then
    record "$item" fail "$text"
  else
    record "$item" pass "$ok"
  fi
}

verdict() {
  local i
  if [[ -n "$error" ]]; then
    echo 1
    return
  fi
  for i in "${!ITEMS[@]}"; do
    if ((REQUIRED[i])) && [[ "${STATUS[i]}" != pass ]]; then
      echo 3
      return
    fi
  done
  echo 0
}

write_result() {
  local code="$1" i sep="" required
  {
    printf '{\n'
    printf '  "scenario": "single-instance",\n'
    printf '  "run_dir": %s,\n' "$(json_string "$run")"
    printf '  "bundle": %s,\n' "$(json_string "$app")"
    printf '  "exit": %d,\n' "$code"
    if [[ -n "$error" ]]; then
      printf '  "error": %s,\n' "$(json_string "$error")"
    else
      printf '  "error": null,\n'
    fi
    printf '  "items": [\n'
    for i in "${!ITEMS[@]}"; do
      required=false
      if ((REQUIRED[i])); then
        required=true
      fi
      printf '%s    {"item": %s, "required": %s, "status": %s, "reason": %s, "raw": %s}' "$sep" \
        "$(json_string "${ITEMS[i]}")" "$required" "$(json_string "${STATUS[i]}")" \
        "$(json_string "${REASON[i]}")" "$(json_string "${ITEMS[i]}")"
      sep=$',\n'
    done
    printf '\n  ]\n}\n'
  } >"$run/result.json.tmp"
  mv "$run/result.json.tmp" "$run/result.json"
}

# --- процессы ---

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
    { pid = $1; line = $0; sub(/^ *[0-9]+ +/, "", line); if (line == exe) printf "%s%s", sep, pid; sep = " " }
  '
}

remember_pid() {
  local pid
  for pid in ${known_pids[@]+"${known_pids[@]}"}; do
    [[ "$pid" != "$1" ]] || return 0
  done
  known_pids+=("$1")
}

# SIGTERM, а без выхода в срок — SIGKILL; только процессу бандла прогона.
terminate() {
  local pid="$1"
  if ! is_run_exe "$pid"; then
    log "pid $pid — не бинарь бандла прогона: сигнал не отправлен"
    return 0
  fi
  log "уборка: SIGTERM pid $pid"
  kill -TERM "$pid" 2>/dev/null || true
  wait_gone "$pid" "$EXIT_S" && return 0
  log "уборка: pid $pid не вышел за $EXIT_S с после SIGTERM, SIGKILL"
  kill -KILL "$pid" 2>/dev/null || true
  wait_gone "$pid" "$KILL_S" || log "уборка: pid $pid не вышел и после SIGKILL"
}

# --- профиль и управляющий сокет ---

# Новый профиль — в NEW_PROFILE и в списке уборки.
new_profile() {
  NEW_PROFILE="$(mktemp -d /tmp/shellio-e2e.XXXXXX)" || fatal "временный профиль не создан"
  profiles+=("$NEW_PROFILE")
}

lock_held() {
  [[ -f "$1/shellio.lock" ]] || return 1
  ! /usr/bin/lockf -s -k -t 0 "$1/shellio.lock" true 2>/dev/null
}

ctl() {
  local state="$1" prefix="$2"
  shift 2
  env -u SHELLIO_SOCKET "SHELLIO_STATE_DIR=$state" "$ctl_bin" "$@" >"$prefix.stdout" 2>"$prefix.stderr"
}

# hello по сокету профиля; PID — в HELLO_PID. Режим ready принимает только готового владельца, any — и уходящего.
hello() {
  local state="$1" prefix="$2" mode="${3:-ready}" pid owner
  HELLO_PID=""
  ctl "$state" "$prefix" --json --timeout "$CTL_TIMEOUT_MS" version || return 1
  pid="$(plutil -extract ok.hello.pid raw -o - "$prefix.stdout" 2>/dev/null)" || return 1
  owner="$(plutil -extract ok.hello.owner raw -o - "$prefix.stdout" 2>/dev/null)" || return 1
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  [[ "$mode" == any || "$owner" == ready ]] || return 1
  HELLO_PID="$pid"
  remember_pid "$pid"
}

# hello с PID процесса бандла прогона; иначе причина в WHY.
verified_hello() {
  WHY=""
  if ! hello "$1" "$2"; then
    WHY="владелец не ответил на hello: $(oneline "$2.stdout") $(oneline "$2.stderr")"
    return 1
  fi
  if ! is_run_exe "$HELLO_PID"; then
    WHY="PID $HELLO_PID из hello — не бинарь бандла прогона: $(comm_of "$HELLO_PID")"
    return 1
  fi
}

wait_owner() {
  local state="$1" prefix="$2" deadline
  deadline=$(($(now_ms) + OWNER_START_S * 1000))
  while ! hello "$state" "$prefix"; do
    (($(now_ms) < deadline)) || return 1
    sleep "$POLL"
  done
}

# Владелец профиля через open -n; вывод — <dir>/owner.{stdout,stderr}. PID из hello — в OWNER_PID, иначе WHY.
launch_owner() {
  local state="$1" dir="$2" status=0
  OWNER_PID=""
  WHY=""
  : >"$dir/owner.stdout"
  : >"$dir/owner.stderr"
  open -n --env "SHELLIO_STATE_DIR=$state" --stdout "$dir/owner.stdout" --stderr "$dir/owner.stderr" "$app" \
    >"$dir/open.log" 2>&1 || status=$?
  if [[ $status -ne 0 ]]; then
    WHY="open -n завершился с кодом $status: $(oneline "$dir/open.log")"
    return 1
  fi
  if ! wait_owner "$state" "$dir/hello"; then
    WHY="владелец не ответил на hello за $OWNER_START_S с: $(oneline "$dir/hello.stdout") $(oneline "$dir/owner.stderr")"
    return 1
  fi
  if ! is_run_exe "$HELLO_PID"; then
    WHY="PID $HELLO_PID из hello — не бинарь бандла прогона: $(comm_of "$HELLO_PID")"
    return 1
  fi
  OWNER_PID="$HELLO_PID"
}

handled_count() {
  local n
  n="$(grep -c "^$HANDLED" "$1" 2>/dev/null || true)"
  printf '%s\n' "${n:-0}"
}

# --- второй запуск ---

# Через open -W -n в фоне; вывод — <dir>/second.{stdout,stderr}, вывод open — <dir>/open.log.
start_second_open() {
  local state="$1" dir="$2" args=()
  shift 2
  if [[ $# -gt 0 ]]; then
    args=(--args "$@")
  fi
  : >"$dir/second.stdout"
  : >"$dir/second.stderr"
  open -W -n --env "SHELLIO_STATE_DIR=$state" --stdout "$dir/second.stdout" --stderr "$dir/second.stderr" \
    "$app" ${args[@]+"${args[@]}"} >"$dir/open.log" 2>&1 &
  second_job=$!
  open_jobs+=("$second_job")
}

# Прямой запуск бинаря бандла в фоне из каталога cwd с SHELLIO_STATE_DIR профиля.
start_second_direct() {
  local state="$1" dir="$2" cwd="$3"
  shift 3
  (cd "$cwd" && exec env "SHELLIO_STATE_DIR=$state" "$exe" ${@+"$@"}) >"$dir/second.stdout" 2>"$dir/second.stderr" &
  second_job=$!
  direct_jobs+=("$second_job")
}

# Квитанция с PID владельца в stdout второго запуска; момент, когда она замечена, — в receipt_ms.
await_receipt() {
  local dir="$1" pid="$2" deadline
  receipt_ms=""
  deadline=$(($(now_ms) + SECOND_LAUNCH_S * 1000))
  while :; do
    if grep -qF "$RECEIPT$pid)" "$dir/second.stdout"; then
      receipt_ms="$(now_ms)"
      return 0
    fi
    alive "$second_job" || grep -qF "$RECEIPT$pid)" "$dir/second.stdout" || return 1
    (($(now_ms) < deadline)) || return 1
    sleep "$POLL"
  done
}

# Выход второго запуска в срок; его код (у open — код open) — в second_status.
finish_second() {
  second_status=""
  wait_gone "$second_job" "$SECOND_LAUNCH_S" || return 1
  if wait "$second_job"; then
    second_status=0
  else
    second_status=$?
  fi
}

open_ok() {
  local dir="$1" status="$2"
  [[ $status -eq 0 ]] || { [[ $status -eq 1 ]] && grep -qF "$OPEN_EXITED_EARLY" "$dir/open.log"; }
}

# --- передний план ---

app_pid() {
  "$LSAPPINFO" info -only pid "$1" 2>/dev/null | grep -oE '"?pid"? ?= ?[0-9]+' | grep -oE '[0-9]+$' | head -n 1
}

front_pid() {
  local asn
  asn="$("$LSAPPINFO" front 2>/dev/null)" || return 0
  [[ -n "$asn" ]] || return 0
  app_pid "$asn" || true
}

finder_pid() {
  "$LSAPPINFO" info -only pid -app "$FINDER_ID" 2>/dev/null | grep -oE '"?pid"? ?= ?[0-9]+' |
    grep -oE '[0-9]+$' | head -n 1 || true
}

# Передний план на Finder, подтверждение по PID; ряд опроса — <dir>/finder.log. Отказ — ошибка исполнения:
# без подтверждённого Finder переход на владельца не наблюдается.
focus_finder() {
  local dir="$1" finder front deadline
  finder="$(finder_pid)"
  [[ -n "$finder" ]] || fatal "предусловие: Finder не запущен"
  open -b "$FINDER_ID" >"$dir/finder-open.log" 2>&1 ||
    fatal "предусловие: open -b $FINDER_ID не выполнен: $(oneline "$dir/finder-open.log")"
  deadline=$(($(now_ms) + FINDER_S * 1000))
  while :; do
    front="$(front_pid)"
    printf '%s %s\n' "$(now_ms)" "${front:-нет}" >>"$dir/finder.log"
    [[ "$front" != "$finder" ]] || return 0
    (($(now_ms) < deadline)) ||
      fatal "предусловие: передний план не перешёл на Finder (pid $finder) за $FINDER_S с, последний — pid ${front:-нет}: сеанс GUI закрыт, экран заблокирован или идёт ввод"
    sleep "$POLL"
  done
}

# Опрос переднего плана с момента t0 до PID want или до FRONT_CAP_MS; ряд «+мс pid бинарь» — <dir>/front.log.
# Смещение первого совпадения от t0 — в front_ms, пусто, если совпадения не было.
watch_front() {
  local dir="$1" want="$2" t0="$3" front off
  front_ms=""
  while :; do
    front="$(front_pid)"
    off=$(($(now_ms) - t0))
    ((off >= 0)) || off=0
    printf '+%d %s %s\n' "$off" "${front:-нет}" "$(comm_of "$front")" >>"$dir/front.log"
    if [[ -n "$front" && "$front" == "$want" ]]; then
      front_ms="$off"
      return 0
    fi
    ((off < FRONT_CAP_MS)) || return 1
    sleep "$POLL"
  done
}

# Ряд переднего плана в течение ms без условия; последний PID — в front_last.
observe_front() {
  local dir="$1" t0="$2" ms="$3" front off
  front_last=""
  while :; do
    front="$(front_pid)"
    off=$(($(now_ms) - t0))
    printf '+%d %s %s\n' "$off" "${front:-нет}" "$(comm_of "$front")" >>"$dir/front.log"
    front_last="$front"
    ((off < ms)) || return 0
    sleep "$POLL"
  done
}

describe_pid() {
  local pid="$1" finder
  finder="$(finder_pid)"
  if [[ -z "$pid" ]]; then
    echo "нет"
  elif [[ "$pid" == "${2:-}" ]]; then
    echo "pid $pid (владелец профиля A)"
  elif [[ "$pid" == "${3:-}" ]]; then
    echo "pid $pid (владелец профиля B)"
  elif [[ "$pid" == "$finder" ]]; then
    echo "pid $pid (Finder)"
  else
    echo "pid $pid ($(comm_of "$pid"))"
  fi
}

# --- подготовка и уборка ---

build_ctl() {
  local messages line
  messages="$(cd "$root" && "$cargo" build --release --locked -p shellioctl --message-format=json-render-diagnostics)" ||
    fatal "cargo build -p shellioctl завершился ошибкой"
  while IFS= read -r line; do
    [[ "$line" == *'"reason":"compiler-artifact"'* && "$line" != *'"executable":null'* ]] || continue
    [[ "$(plutil -extract target.name raw -o - - <<<"$line")" == shellioctl ]] || continue
    [[ -z "$ctl_bin" ]] || fatal "Cargo сообщил больше одного бинаря shellioctl"
    ctl_bin="$(plutil -extract executable raw -o - - <<<"$line")" || fatal "в сообщении Cargo о shellioctl нет пути"
  done <<<"$messages"
  [[ -n "$ctl_bin" && -x "$ctl_bin" ]] || fatal "Cargo не сообщил о собранном бинаре shellioctl"
}

prepare() {
  local left
  app="${1:-$root/target/bundle/shellio.app}"
  [[ -d "$app" ]] || fatal "нет бандла $app: соберите его через make bundle"
  app="$(cd "$app" && pwd -P)"
  exe="$app/Contents/MacOS/shellio"
  [[ -x "$exe" ]] || fatal "в бандле нет исполняемого $exe"
  [[ -x "$LSAPPINFO" ]] || fatal "нет $LSAPPINFO"
  left="$(run_processes)"
  [[ -z "$left" ]] || fatal "предусловие: бинарь бандла прогона уже запущен (PID $left), завершите его"
  [[ -n "$(finder_pid)" ]] || fatal "предусловие: Finder не запущен"
  build_ctl
  log "каталог прогона: $run"
  log "бандл: $app, shellioctl: $ctl_bin"
}

stop_profile() {
  local state="$1" prefix deadline
  [[ -d "$state" ]] || return 0
  lock_held "$state" || return 0
  prefix="$run/cleanup/hello-$(basename "$state")"
  deadline=$(($(now_ms) + OWNER_START_S * 1000))
  while ! hello "$state" "$prefix" any; do
    lock_held "$state" || return 0
    if (($(now_ms) >= deadline)); then
      log "уборка: блокировку профиля $state держат, а hello нет — PID не сверить"
      return 0
    fi
    sleep "$POLL"
  done
  terminate "$HELLO_PID"
}

wait_jobs() {
  local deadline pid busy
  deadline=$(($(now_ms) + SECOND_LAUNCH_S * 1000))
  while :; do
    busy=""
    for pid in ${open_jobs[@]+"${open_jobs[@]}"} ${direct_jobs[@]+"${direct_jobs[@]}"}; do
      if alive "$pid"; then
        busy="$busy $pid"
      fi
    done
    [[ -n "$busy" ]] || break
    (($(now_ms) < deadline)) || break
    sleep "$POLL"
  done
  # Задание open — сам open, а не приложение: его завершение экземпляр не трогает.
  for pid in ${open_jobs[@]+"${open_jobs[@]}"}; do
    if alive "$pid"; then
      kill -TERM "$pid" 2>/dev/null || true
    fi
  done
  for pid in ${open_jobs[@]+"${open_jobs[@]}"} ${direct_jobs[@]+"${direct_jobs[@]}"}; do
    if ! alive "$pid"; then
      wait "$pid" 2>/dev/null || true
    fi
  done
}

cleanup() {
  local round state pid left
  mkdir -p "$run/cleanup"
  for round in 1 2; do
    for state in ${profiles[@]+"${profiles[@]}"}; do
      stop_profile "$state"
    done
    for pid in ${known_pids[@]+"${known_pids[@]}"}; do
      if alive "$pid"; then
        terminate "$pid"
      fi
    done
    log "уборка: круг $round, жду фоновые запуски"
    wait_jobs
  done
  for state in ${profiles[@]+"${profiles[@]}"}; do
    [[ -d "$state" ]] || continue
    if lock_held "$state"; then
      add_error "экземпляр прогона держит блокировку профиля $state, профиль оставлен"
    else
      rm -rf "$state"
    fi
  done
  if [[ -n "$exe" ]]; then
    left="$(run_processes)"
    [[ -z "$left" ]] || add_error "остались процессы бандла прогона: PID $left"
  fi
}

finish() {
  local status=$? code
  trap '' INT TERM
  trap - ERR
  set +e
  if [[ $completed -ne 1 && -z "$error" ]]; then
    add_error "сценарий остановился до конца: код $status${err_at:+, последний отказ команды в строке $err_at}"
  fi
  write_result "$(verdict)"
  cleanup
  code="$(verdict)"
  write_result "$code"
  log "итог: код $code, result.json — $run/result.json"
  exit "$code"
}

trap 'err_at=$LINENO' ERR
trap finish EXIT
trap 'on_signal SIGINT' INT
trap 'on_signal SIGTERM' TERM

# --- пункты матрицы ---

item_owner() {
  local item=owner dir="$run/owner" code=0
  failures=()
  new_profile
  profile_a="$NEW_PROFILE"
  log "== $item: профиль A $profile_a"
  if ! launch_owner "$profile_a" "$dir"; then
    record "$item" fail "$WHY"
    return 0
  fi
  owner_out_a="$dir/owner.stdout"
  grep -qF "$READY$OWNER_PID," "$owner_out_a" ||
    failures+=("в журнале владельца нет строки готовности с pid $OWNER_PID")
  ctl "$profile_a" "$dir/status" --json --timeout "$CTL_TIMEOUT_MS" status || code=$?
  [[ $code -eq 0 ]] || failures+=("shellioctl status: код $code: $(oneline "$dir/status.stdout")")
  code=0
  ctl "$profile_a" "$dir/tree" --json --timeout "$CTL_TIMEOUT_MS" tree --all || code=$?
  [[ $code -eq 0 ]] || failures+=("shellioctl tree --all: код $code: $(oneline "$dir/tree.stdout")")
  conclude "$item" "pid $OWNER_PID из hello — процесс бандла прогона, status и tree --all ответили"
}

# Второй запуск способом how (open или direct) при Finder на переднем плане.
item_forward() {
  local item="$1" how="$2" dir="$run/$1" pid before after
  failures=()
  front_ms=""
  log "== $item"
  if ! verified_hello "$profile_a" "$dir/hello-before"; then
    record "$item" fail "$WHY"
    return 0
  fi
  pid="$HELLO_PID"
  focus_finder "$dir"
  before="$(handled_count "$owner_out_a")"
  case "$how" in
    open) start_second_open "$profile_a" "$dir" ;;
    direct) start_second_direct "$profile_a" "$dir" "$dir" ;;
  esac
  if await_receipt "$dir" "$pid"; then
    watch_front "$dir" "$pid" "$receipt_ms" || true
  else
    failures+=("в stdout второго запуска нет квитанции с pid $pid за $SECOND_LAUNCH_S с: $(oneline "$dir/second.stdout") $(oneline "$dir/second.stderr")")
  fi
  if finish_second; then
    case "$how" in
      open)
        open_ok "$dir" "$second_status" ||
          failures+=("open -W -n завершился с кодом $second_status: $(oneline "$dir/open.log")")
        ;;
      direct)
        [[ $second_status -eq 0 ]] || failures+=("второй запуск вышел с кодом $second_status")
        ;;
    esac
  else
    failures+=("второй запуск не вышел за $SECOND_LAUNCH_S с")
  fi
  after="$(handled_count "$owner_out_a")"
  [[ $after -eq $((before + 1)) ]] ||
    failures+=("строк обработки в журнале владельца было $before, стало $after, ожидалась одна новая")
  if hello "$profile_a" "$dir/hello-after"; then
    [[ "$HELLO_PID" == "$pid" ]] || failures+=("PID владельца сменился: $pid → $HELLO_PID")
  else
    failures+=("владелец не ответил на hello после второго запуска")
  fi
  if [[ -n "$receipt_ms" ]]; then
    if [[ -z "$front_ms" ]]; then
      failures+=("передний план не перешёл на PID владельца $pid за $FRONT_CAP_MS мс после квитанции, ряд — $item/front.log")
    elif ((front_ms > FRONT_LIMIT_MS)); then
      failures+=("передний план перешёл на PID владельца $pid через $front_ms мс после квитанции, позже $FRONT_LIMIT_MS мс")
    fi
  fi
  conclude "$item" "квитанция, строка обработки, PID владельца $pid прежний, передний план — PID владельца через $front_ms мс после квитанции"
}

item_folder() {
  local item=folder-refused dir="$run/folder-refused" pid before after cwd folder last
  failures=()
  log "== $item"
  if ! verified_hello "$profile_a" "$dir/hello-before"; then
    record "$item" fail "$WHY"
    return 0
  fi
  pid="$HELLO_PID"
  mkdir -p "$dir/folder"
  cwd="$(cd "$dir" && pwd -P)"
  folder="$cwd/folder"
  before="$(handled_count "$owner_out_a")"
  # Путь относительный: второй запуск делает его абсолютным от своего рабочего каталога.
  start_second_direct "$profile_a" "$dir" "$cwd" folder
  if finish_second; then
    [[ $second_status -eq 0 ]] || failures+=("второй запуск вышел с кодом $second_status")
  else
    failures+=("второй запуск не вышел за $SECOND_LAUNCH_S с")
  fi
  grep -qF "$RECEIPT$pid)" "$dir/second.stdout" ||
    failures+=("в stdout второго запуска нет квитанции с pid $pid: $(oneline "$dir/second.stdout")")
  grep -qF "shellio: $folder: folder_open_unsupported — папка не открыта" "$dir/second.stdout" ||
    failures+=("в квитанции нет отказа folder_open_unsupported с текстом «папка не открыта» для $folder")
  after="$(handled_count "$owner_out_a")"
  if [[ $after -eq $((before + 1)) ]]; then
    last="$(grep "^$HANDLED" "$owner_out_a" | tail -n 1)"
    [[ "$last" == *"\"arg\":\"$folder\""* && "$last" == *folder_open_unsupported* ]] ||
      failures+=("строка обработки владельца не называет $folder с folder_open_unsupported: $last")
  else
    failures+=("строк обработки в журнале владельца было $before, стало $after, ожидалась одна новая")
  fi
  if hello "$profile_a" "$dir/hello-after"; then
    [[ "$HELLO_PID" == "$pid" ]] || failures+=("PID владельца сменился: $pid → $HELLO_PID")
  else
    failures+=("владелец не ответил на hello после второго запуска")
  fi
  conclude "$item" "квитанция с folder_open_unsupported для $folder, код 0, текст «папка не открыта», владелец обработал путь"
}

item_persist() {
  local item=persist dir="$run/persist" pid title="e2e-persist-$$" code=0
  failures=()
  log "== $item"
  if ! verified_hello "$profile_a" "$dir/hello-before"; then
    record "$item" fail "$WHY"
    return 0
  fi
  ctl "$profile_a" "$dir/workspace-new" --json --timeout "$CTL_TIMEOUT_MS" workspace new "$title" --all || code=$?
  if [[ $code -ne 0 ]]; then
    record "$item" fail "shellioctl workspace new: код $code: $(oneline "$dir/workspace-new.stdout")"
    return 0
  fi
  # PID сверяется по hello непосредственно перед сигналом.
  if ! verified_hello "$profile_a" "$dir/hello-signal"; then
    record "$item" fail "$WHY"
    return 0
  fi
  pid="$HELLO_PID"
  log "$item: SIGTERM pid $pid"
  if ! kill -TERM "$pid"; then
    record "$item" fail "SIGTERM pid $pid не доставлен"
    return 0
  fi
  if ! wait_gone "$pid" "$EXIT_S"; then
    record "$item" fail "владелец pid $pid не вышел за $EXIT_S с после SIGTERM"
    return 0
  fi
  [[ ! -e "$profile_a/shellio.sock" && ! -L "$profile_a/shellio.sock" ]] ||
    failures+=("сокет остался после выхода владельца")
  if [[ -f "$profile_a/shellio.lock" ]]; then
    ! lock_held "$profile_a" || failures+=("блокировку профиля держат после выхода владельца")
  else
    failures+=("файла блокировки нет после выхода владельца")
  fi
  grep -qF "$TERMINATED" "$owner_out_a" || failures+=("в журнале владельца нет строки «${TERMINATED}»")
  if ! launch_owner "$profile_a" "$dir"; then
    failures+=("новый владелец: $WHY")
    conclude "$item" ""
    return 0
  fi
  owner_out_a="$dir/owner.stdout"
  [[ "$OWNER_PID" != "$pid" ]] || failures+=("новый владелец с прежним PID $pid")
  code=0
  ctl "$profile_a" "$dir/tree" --json --timeout "$CTL_TIMEOUT_MS" tree --all || code=$?
  if [[ $code -ne 0 ]]; then
    failures+=("shellioctl tree --all: код $code: $(oneline "$dir/tree.stdout")")
  elif ! grep -qF "\"$title\"" "$dir/tree.stdout"; then
    failures+=("tree --all нового владельца не показывает воркспейс «${title}»")
  fi
  conclude "$item" "воркспейс «${title}» записан, после SIGTERM pid $pid сокет удалён, файл блокировки на месте и свободен, новый владелец pid $OWNER_PID показывает воркспейс"
}

item_kill() {
  local item=kill-recover dir="$run/kill-recover" pid lock_ino sock_ino lock_now sock_now="нет" stale
  failures=()
  log "== $item"
  if ! verified_hello "$profile_a" "$dir/hello-before"; then
    record "$item" fail "$WHY"
    return 0
  fi
  pid="$HELLO_PID"
  if [[ ! -S "$profile_a/shellio.sock" ]]; then
    record "$item" fail "у живого владельца pid $pid нет сокета"
    return 0
  fi
  lock_ino="$(stat -f %i "$profile_a/shellio.lock")"
  sock_ino="$(stat -f %i "$profile_a/shellio.sock")"
  # PID только что сверен по hello.
  log "$item: SIGKILL pid $pid"
  if ! kill -KILL "$pid"; then
    record "$item" fail "SIGKILL pid $pid не доставлен"
    return 0
  fi
  if ! wait_gone "$pid" "$KILL_S"; then
    record "$item" fail "владелец pid $pid не вышел за $KILL_S с после SIGKILL"
    return 0
  fi
  stale="нет"
  if [[ -S "$profile_a/shellio.sock" ]]; then
    stale="да"
  fi
  if ! launch_owner "$profile_a" "$dir"; then
    failures+=("новый владелец: $WHY")
    conclude "$item" ""
    return 0
  fi
  owner_out_a="$dir/owner.stdout"
  [[ "$OWNER_PID" != "$pid" ]] || failures+=("новый владелец с прежним PID $pid")
  if [[ -S "$profile_a/shellio.sock" ]]; then
    sock_now="$(stat -f %i "$profile_a/shellio.sock")"
    [[ "$sock_now" != "$sock_ino" ]] || failures+=("сокет не заменён: inode $sock_ino прежний")
  else
    failures+=("у нового владельца нет сокета")
  fi
  lock_now="$(stat -f %i "$profile_a/shellio.lock")"
  [[ "$lock_now" == "$lock_ino" ]] || failures+=("inode блокировки сменился: $lock_ino → $lock_now")
  conclude "$item" "после SIGKILL pid $pid (сокет остался: $stale) новый владелец pid $OWNER_PID, сокет заменён (inode $sock_ino → $sock_now), inode блокировки $lock_ino прежний"
}

item_two() {
  local item=two-profiles dir="$run/two-profiles" pid_a
  failures=()
  log "== $item"
  if ! verified_hello "$profile_a" "$dir/hello-a"; then
    record "$item" fail "профиль A: $WHY"
    return 0
  fi
  pid_a="$HELLO_PID"
  new_profile
  profile_b="$NEW_PROFILE"
  log "$item: профиль B $profile_b"
  if ! launch_owner "$profile_b" "$dir"; then
    record "$item" fail "профиль B: $WHY"
    return 0
  fi
  owner_out_b="$dir/owner.stdout"
  [[ "$OWNER_PID" != "$pid_a" ]] || failures+=("hello двух профилей дал один PID $pid_a")
  if verified_hello "$profile_a" "$dir/hello-a-after"; then
    [[ "$HELLO_PID" == "$pid_a" ]] || failures+=("PID владельца A сменился: $pid_a → $HELLO_PID")
  else
    failures+=("профиль A после запуска B: $WHY")
  fi
  conclude "$item" "владелец A pid $pid_a и владелец B pid $OWNER_PID работают рядом"
}

item_plain_open() {
  local item=plain-open dir="$run/plain-open" pid_a pid_b="" before_a after_a before_b=0 after_b=0 status=0 t0
  failures=()
  log "== $item"
  if ! verified_hello "$profile_a" "$dir/hello-a"; then
    record "$item" fail "профиль A: $WHY"
    return 0
  fi
  pid_a="$HELLO_PID"
  if [[ -n "$profile_b" ]] && verified_hello "$profile_b" "$dir/hello-b"; then
    pid_b="$HELLO_PID"
  fi
  before_a="$(handled_count "$owner_out_a")"
  [[ -z "$owner_out_b" ]] || before_b="$(handled_count "$owner_out_b")"
  printf 'до open: %s\n' "$(describe_pid "$(front_pid)" "$pid_a" "$pid_b")" >"$dir/front-before.log"
  : >"$dir/plain.stdout"
  : >"$dir/plain.stderr"
  # Профиль задаётся и здесь: если LaunchServices всё же породит процесс, тот не тронет настоящий профиль.
  open --env "SHELLIO_STATE_DIR=$profile_a" --stdout "$dir/plain.stdout" --stderr "$dir/plain.stderr" "$app" \
    >"$dir/open.log" 2>&1 || status=$?
  t0="$(now_ms)"
  if [[ $status -ne 0 ]]; then
    record "$item" fail "open завершился с кодом $status: $(oneline "$dir/open.log")"
    return 0
  fi
  observe_front "$dir" "$t0" "$FRONT_LIMIT_MS"
  if hello "$profile_a" "$dir/hello-a-after"; then
    [[ "$HELLO_PID" == "$pid_a" ]] || failures+=("PID владельца A сменился: $pid_a → $HELLO_PID")
  else
    failures+=("владелец A не ответил на hello после open")
  fi
  if [[ -n "$pid_b" ]]; then
    if hello "$profile_b" "$dir/hello-b-after"; then
      [[ "$HELLO_PID" == "$pid_b" ]] || failures+=("PID владельца B сменился: $pid_b → $HELLO_PID")
    else
      failures+=("владелец B не ответил на hello после open")
    fi
  fi
  after_a="$(handled_count "$owner_out_a")"
  [[ -z "$owner_out_b" ]] || after_b="$(handled_count "$owner_out_b")"
  conclude "$item" "PID владельцев прежние (A $pid_a${pid_b:+, B $pid_b}); наблюдение: передний план через $FRONT_LIMIT_MS мс — $(describe_pid "$front_last" "$pid_a" "$pid_b"), новых строк обработки у A $((after_a - before_a)), у B $((after_b - before_b)), вывод нового процесса: $(oneline "$dir/plain.stdout")"
}

item_gui_quit() {
  record gui-quit not_run "Apple Events адресуют идентификатор бандла, а не PID, и задели бы чужой экземпляр"
}

prepare "$@"
item_owner
item_forward forward-open-n open
item_forward forward-direct direct
item_folder
item_persist
item_kill
item_two
item_plain_open
item_gui_quit
completed=1
