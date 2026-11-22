#!/usr/bin/env bash
# Сценарий make e2e «окно и компоненты AppKit» (P1.3): пункты матрицы приёмки плана appkit-foundation на
# подписанном бандле и временных профилях.
#
#   appkit.sh [--single-instance <result.json>] [<бандл>]
#
# Бандл по умолчанию — target/bundle/shellio.app, его собирает и проверяет цель make e2e. shellioctl и
# shellio-e2e-oracle сценарий собирает сам (release, --locked) и берёт пути из сообщений Cargo о собранных
# артефактах. --single-instance — result.json сценария single-instance.sh того же make e2e: по нему судится пункт
# single-instance, без флага пункт получает fail. Профили — новые каталоги /tmp/shellio-e2e.*.
#
# Каталог прогона — target/e2e/appkit/<время>-<pid>/: result.json (пункт, обязательность, статус, причина, путь
# сырого вывода относительно каталога прогона), журнал сценария log и сырой вывод каждого пункта (JSON чтений
# ui.*, PNG, front.log, stdout и stderr) в подкаталоге с его именем. Пробы предусловий пишут в preconditions/,
# уборка — в cleanup/.
#
# Предусловия, их отсутствие — ошибка исполнения «предусловие: …» до первого пункта: открытый сеанс GUI,
# разблокированный экран, запущенный Finder, бинарь бандла не запущен, запуск из процесса-потомка приложения
# терминала, известного lsappinfo; у терминала-предка разрешения Screen Recording (снимки окна screencapture -l) и
# Accessibility с Automation для System Events (клавиши и нажатия); источник ввода латинский (ABC, US или
# British); AeroSpace и другие оконные менеджеры выключены. Клавиатуру и мышь во время прогона не трогают. Пробы
# идут по окну владельца служебного профиля, поднятого до первого пункта, и пикселей продукта не смотрят. Перед
# первым пунктом снимков оракул проходит самопроверку: заведомо неверное ожидание обязано дать расхождение.
#
# Снимок — screencapture -x -o -l <windowNumber> из ui.window этого владельца, перевод в sRGB через sips, затем
# shellio-e2e-oracle compare. Снимок повторяется, пока маркер поколения в PNG не совпадёт с поколением ответа
# команды, не дольше 3 с. Клавиши и нажатия идут через System Events, передний план перед вводом сверяется
# lsappinfo front. Обычный open адресует бандл, а не PID, поэтому пункты с ним идут при ровно одном живом владельце
# прогона и передают --env SHELLIO_STATE_DIR профиля пункта.
#
# Сигналы и уборка адресуют только PID, полученные через hello сокетов прогона и сверенные с бинарём бандла
# прогона. Внешние условия опрашиваются с крайним сроком, фиксированных пауз вместо условия нет; окно замера
# простоя — само измерение.
#
# Выход — после записи result.json и уборки: 0 — все обязательные пункты pass, 1 — ошибка исполнения, 3 — ошибки
# исполнения нет, но хотя бы один обязательный пункт не pass. Необязательные пункты на код не влияют.
set -Eeuo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
cargo="${CARGO:-$HOME/.cargo/bin/cargo}"
expect_dir="$here/appkit-expect"

READY="shellio: владелец готов, pid "
RECEIPT="shellio: запуск передан владельцу (pid "
UI_QUIT="shellio: завершение: запрос UI"
OPEN_EXITED_EARLY="initial call to kevent() failed: No such process"
FINDER_ID="com.apple.finder"
LSAPPINFO=/usr/bin/lsappinfo
SRGB_PROFILE="/System/Library/ColorSync/Profiles/sRGB Profile.icc"
# Оконные менеджеры, которые переставляют окна сами (P0.2).
WINDOW_MANAGERS=(AeroSpace yabai Amethyst)
# Латинские источники ввода: keystroke набирает текст по раскладке.
LATIN_LAYOUTS=(com.apple.keylayout.ABC com.apple.keylayout.US com.apple.keylayout.British
  com.apple.keylayout.British-PC)

# Сроки в секундах и миллисекундах.
OWNER_START_S=20
SECOND_LAUNCH_S=20
EXIT_S=10
KILL_S=5
FOCUS_S=5
FRONT_LIMIT_MS=3000
FRONT_CAP_MS=10000
HOLD_MS=1000
MIRROR_MS=2000
STATE_MS=3000
MARKER_MS=3000
RELEASE_MS=5000
STARTUP_MS=10000
MENU_UI_MS=1000
MENU_OSA_S=10
IDLE_S=10
POLL=0.1
CTL_TIMEOUT_MS=2000

# Бюджеты docs/architecture.md и матрицы приёмки.
STARTUP_BUDGET_MS=500
STARTUP_RUNS=11
IDLE_CPU_PERCENT=1
IDLE_WAKEUPS=20
IDLE_SETTLE_S=30
CYCLES=10

ITEMS=(first-front window mirror close-reopen quit gui-quit-pid snapshot-light snapshot-dark contrast-cells
  system-appearance material text-field menu-tracking ax voiceover window-cycle idle startup-warm startup-cold
  tabstrip-regimes tab-underline reduce-motion single-instance)
REQUIRED=(1 1 1 1 1 0 1 1 1 0 1 1 1 1 0 1 1 1 0 1 1 1 1)
STATUS=()
REASON=()
for i in "${!ITEMS[@]}"; do
  STATUS[i]=not_run
  REASON[i]="не выполнен: прогон остановлен раньше этого пункта"
done

single_result=""
bundle_arg=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --single-instance)
      [[ $# -ge 2 ]] || {
        echo "usage: $0 [--single-instance <result.json>] [<бандл>]" >&2
        exit 2
      }
      single_result="$2"
      shift 2
      ;;
    -*)
      echo "usage: $0 [--single-instance <result.json>] [<бандл>]" >&2
      exit 2
      ;;
    *)
      [[ -z "$bundle_arg" ]] || {
        echo "usage: $0 [--single-instance <result.json>] [<бандл>]" >&2
        exit 2
      }
      bundle_arg="$1"
      shift
      ;;
  esac
done

run="$root/target/e2e/appkit/$(date +%Y%m%d-%H%M%S)-$$"
mkdir -p "$run/preconditions"
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
oracle_bin=""
terminal_pid=""
terminal_id=""
profile_a=""
owner_out_a=""
profile_g=""
owner_pid_g=""
profiles=()
known_pids=()
open_jobs=()
bg_jobs=()
failures=()
# Части window-cycle идут на разных владельцах: нарушения копятся до итога пункта.
cycle_failures=()
cycle_notes=()

# Значения, которые функции отдают через глобальные переменные: $(…) потерял бы known_pids и задания.
HELLO_PID=""
OWNER_PID=""
NEW_PROFILE=""
WHY=""
GEN=""
VALUE=""
second_job=""
second_status=""
receipt_ms=""
front_ms=""
front_abs=""
hold_lost=""
sample_front=""
sample_ms=""
front_log=""
snap_reason=""
snap_code=""

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
  # Повторный сигнал группе процессов не прерывает уборку: result.json и остановка владельцев идут до конца.
  trap '' INT TERM
  add_error "прерван сигналом $1"
  exit 1
}

now_ms() {
  /usr/bin/perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'
}

# Монотонные часы в мс: замер простоя не зависит от перевода системных часов.
mono_ms() {
  /usr/bin/perl -MTime::HiRes=clock_gettime,CLOCK_MONOTONIC -e 'printf "%d\n", clock_gettime(CLOCK_MONOTONIC) * 1000'
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

# Значение по пути в JSON-файле: ключи через точку, [N] — элемент массива, [ключ=значение] — первый элемент с
# таким полем, [#] — длина массива. Скаляр печатается как есть (true, false, null), объект и массив — JSON.
# Код 1 — пути нет, 2 — файл не JSON.
jv() {
  /usr/bin/perl -MJSON::PP -e '
    my ($file, $path) = @ARGV;
    open my $fh, "<", $file or exit 2;
    local $/;
    my $node = eval { JSON::PP->new->utf8->decode(<$fh>) };
    exit 2 if $@;
    while ($path =~ /([^.\[\]]+)|\[([^\]]*)\]/g) {
      my ($key, $sel) = ($1, $2);
      if (defined $key) {
        exit 1 unless ref $node eq "HASH" && exists $node->{$key};
        $node = $node->{$key};
      } elsif ($sel eq "#") {
        exit 1 unless ref $node eq "ARRAY";
        $node = scalar @$node;
      } elsif ($sel =~ /^\d+$/) {
        exit 1 unless ref $node eq "ARRAY" && $sel < @$node;
        $node = $node->[$sel];
      } elsif ($sel =~ /^([^=]+)=(.*)$/) {
        exit 1 unless ref $node eq "ARRAY";
        my ($field, $want) = ($1, $2);
        my ($hit) = grep { ref $_ eq "HASH" && defined $_->{$field} && $_->{$field} eq $want } @$node;
        exit 1 unless $hit;
        $node = $hit;
      } else {
        exit 1;
      }
    }
    binmode STDOUT, ":utf8";
    if (!defined $node) { print "null\n" }
    elsif (JSON::PP::is_bool($node)) { print $node ? "true\n" : "false\n" }
    elsif (ref $node) { print JSON::PP->new->canonical->encode($node), "\n" }
    else { print "$node\n" }
  ' "$1" "$2"
}

# Сравнение чисел с плавающей точкой: fcmp <a> <оператор> <b>.
fcmp() {
  awk -v a="$1" -v b="$3" -v op="$2" 'BEGIN {
    if (op == "<=") exit !(a <= b); if (op == ">=") exit !(a >= b); if (op == "<") exit !(a < b)
    if (op == ">") exit !(a > b); exit 1 }'
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
    printf '  "scenario": "appkit",\n'
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

parent_of() {
  ps -o ppid= -p "$1" 2>/dev/null | tr -d '[:space:]' || true
}

# PID процессов с бинарём бандла прогона через пробел.
run_processes() {
  ps -ax -ww -o pid=,comm= | awk -v exe="$exe" '
    { pid = $1; line = $0; sub(/^ *[0-9]+ +/, "", line); if (line == exe) { printf "%s%s", sep, pid; sep = " " } }
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
  log "SIGTERM pid $pid"
  kill -TERM "$pid" 2>/dev/null || true
  wait_gone "$pid" "$EXIT_S" && return 0
  log "pid $pid не вышел за $EXIT_S с после SIGTERM, SIGKILL"
  kill -KILL "$pid" 2>/dev/null || true
  wait_gone "$pid" "$KILL_S" || log "pid $pid не вышел и после SIGKILL"
}

# --- профиль и управляющий сокет ---

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

# Чтение ui.<what> в <prefix>.stdout; код shellioctl.
ui_read() {
  ctl "$1" "$2" --json --timeout "$CTL_TIMEOUT_MS" ui "$3"
}

# Команда shellioctl с квитанцией ui.accepted; поколение — в GEN, иначе причина в WHY.
ui_cmd() {
  local state="$1" prefix="$2" code=0
  shift 2
  GEN=""
  WHY=""
  ctl "$state" "$prefix" --json --timeout "$CTL_TIMEOUT_MS" "$@" || code=$?
  if [[ $code -ne 0 ]]; then
    WHY="shellioctl $*: код $code: $(oneline "$prefix.stdout") $(oneline "$prefix.stderr")"
    return 1
  fi
  if ! GEN="$(jv "$prefix.stdout" ok.ui.accepted.generation)"; then
    WHY="shellioctl $*: нет квитанции ui.accepted: $(oneline "$prefix.stdout")"
    return 1
  fi
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

# Владелец профиля через open -n с аргументами владельца; вывод — <dir>/owner.{stdout,stderr}. PID из hello — в
# OWNER_PID, иначе WHY.
launch_owner() {
  local state="$1" dir="$2" status=0 args=()
  shift 2
  if [[ $# -gt 0 ]]; then
    args=(--args "$@")
  fi
  OWNER_PID=""
  WHY=""
  : >"$dir/owner.stdout"
  : >"$dir/owner.stderr"
  open -n --env "SHELLIO_STATE_DIR=$state" --stdout "$dir/owner.stdout" --stderr "$dir/owner.stderr" "$app" \
    ${args[@]+"${args[@]}"} >"$dir/open.log" 2>&1 || status=$?
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

# Ровно один живой владелец прогона — pid: обычный open адресует бандл, а не PID.
single_owner() {
  local left
  left="$(run_processes)"
  [[ "$left" == "$1" ]] || {
    WHY="обычный open без единственного владельца прогона: живы PID ${left:-нет}, ожидался только $1"
    return 1
  }
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

# Терминал-предок: первый предок процесса сценария по цепочке ppid, которого знает lsappinfo.
find_terminal() {
  local pid asn chain=""
  pid="$(parent_of $$)"
  while [[ "$pid" =~ ^[0-9]+$ ]] && ((pid > 1)); do
    chain="${chain:+$chain ← }$pid $(comm_of "$pid")"
    asn="$("$LSAPPINFO" find "pid=$pid" 2>/dev/null || true)"
    if [[ -n "$asn" ]]; then
      log "цепочка предков: $chain"
      terminal_pid="$pid"
      terminal_id="$("$LSAPPINFO" info -only bundleid "$asn" 2>/dev/null |
        sed -n 's/.*bundleID="\([^"]*\)".*/\1/p' | head -n 1 || true)"
      log "терминал-предок: pid $terminal_pid, ${terminal_id:-без идентификатора бандла}"
      return 0
    fi
    pid="$(parent_of "$pid")"
  done
  log "цепочка предков: $chain"
  fatal "предусловие: make e2e запущен не из приложения терминала (tmux, screen, ssh)"
}

# Передний план на Finder через open -b, подтверждение по PID за FOCUS_S; ряд — <dir>/finder.log. Отказ — ошибка
# исполнения: без подтверждённой исходной точки переход на владельца не наблюдается.
focus_finder() {
  local dir="$1" finder front deadline
  finder="$(finder_pid)"
  [[ -n "$finder" ]] || fatal "предусловие: Finder не запущен"
  open -b "$FINDER_ID" >"$dir/finder-open.log" 2>&1 ||
    fatal "предусловие: open -b $FINDER_ID не выполнен: $(oneline "$dir/finder-open.log")"
  deadline=$(($(now_ms) + FOCUS_S * 1000))
  while :; do
    front="$(front_pid)"
    printf '%s %s\n' "$(now_ms)" "${front:-нет}" >>"$dir/finder.log"
    [[ "$front" != "$finder" ]] || return 0
    (($(now_ms) < deadline)) ||
      fatal "предусловие: передний план не перешёл на Finder (pid $finder) за $FOCUS_S с, последний — pid ${front:-нет}: сеанс GUI закрыт, экран заблокирован или идёт ввод"
    sleep "$POLL"
  done
}

describe_pid() {
  local pid="$1" finder
  finder="$(finder_pid)"
  if [[ -z "$pid" ]]; then
    echo "нет"
  elif [[ "$pid" == "${2:-}" ]]; then
    echo "pid $pid (владелец)"
  elif [[ "$pid" == "$finder" ]]; then
    echo "pid $pid (Finder)"
  elif [[ "$pid" == "$terminal_pid" ]]; then
    echo "pid $pid (терминал-предок)"
  else
    echo "pid $pid ($(comm_of "$pid"))"
  fi
}

# Замер переднего плана: строка «+мс pid бинарь» в front_log от момента t0.
sample() {
  local front t0="$1"
  front="$(front_pid)"
  sample_ms="$(now_ms)"
  sample_front="$front"
  printf '+%d %s %s\n' "$((sample_ms - t0))" "${front:-нет}" "$(comm_of "$front")" >>"$front_log"
}

# Замеры от t0 до PID want на переднем плане или до FRONT_CAP_MS; смещение перехода от t0 — в front_ms, его
# момент — в front_abs, пусто, если перехода не было.
watch_front() {
  local want="$1" t0="$2"
  front_ms=""
  front_abs=""
  while :; do
    sample "$t0"
    if [[ -n "$sample_front" && "$sample_front" == "$want" ]]; then
      front_abs="$sample_ms"
      front_ms=$((sample_ms - t0))
      ((front_ms >= 0)) || front_ms=0
      printf '# владелец на переднем плане +%d\n' "$front_ms" >>"$front_log"
      return 0
    fi
    ((sample_ms - t0 < FRONT_CAP_MS)) || return 1
    sleep "$POLL"
  done
}

# Замеры HOLD_MS после перехода: каждый должен показать want, первое отклонение — в hold_lost.
hold_front() {
  local want="$1" t0="$2" off
  hold_lost=""
  while :; do
    sleep "$POLL"
    sample "$t0"
    off=$((sample_ms - front_abs))
    if [[ "$sample_front" != "$want" && -z "$hold_lost" ]]; then
      hold_lost="через $off мс после перехода — $(describe_pid "$sample_front" "$want")"
    fi
    ((off < HOLD_MS)) || break
  done
  printf '# конец удержания\n' >>"$front_log"
}

# Владелец pid на передний план через System Events по PID и подтверждение lsappinfo за FOCUS_S; иначе WHY.
front_owner() {
  local pid="$1" dir="$2" front deadline
  WHY=""
  if ! osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $pid) to true" \
    >"$dir/frontmost.log" 2>&1; then
    WHY="System Events не вывел pid $pid вперёд: $(oneline "$dir/frontmost.log")"
    return 1
  fi
  deadline=$(($(now_ms) + FOCUS_S * 1000))
  while :; do
    front="$(front_pid)"
    [[ "$front" != "$pid" ]] || return 0
    if (($(now_ms) >= deadline)); then
      WHY="передний план не перешёл на владельца pid $pid за $FOCUS_S с, последний — $(describe_pid "$front" "$pid")"
      return 1
    fi
    sleep "$POLL"
  done
}

# Перед вводом передний план — владелец; иначе WHY.
input_ready() {
  local pid="$1" front
  WHY=""
  front="$(front_pid)"
  [[ "$front" == "$pid" ]] || {
    WHY="перед вводом передний план — $(describe_pid "$front" "$pid"), а не владелец"
    return 1
  }
}

# Клавиша через System Events по коду; вывод — <dir>/keys.log.
se_key() {
  local dir="$1" code="$2"
  printf '%s key code %s\n' "$(now_ms)" "$code" >>"$dir/keys.log"
  osascript -e "tell application \"System Events\" to key code $code" >>"$dir/keys.log" 2>&1
}

se_keystroke() {
  local dir="$1" text="$2"
  printf '%s keystroke %s\n' "$(now_ms)" "$text" >>"$dir/keys.log"
  osascript -e "tell application \"System Events\" to keystroke \"$text\"" >>"$dir/keys.log" 2>&1
}

# --- второй запуск ---

start_second_open() {
  local state="$1" dir="$2"
  : >"$dir/second.stdout"
  : >"$dir/second.stderr"
  open -W -n --env "SHELLIO_STATE_DIR=$state" --stdout "$dir/second.stdout" --stderr "$dir/second.stderr" \
    "$app" >"$dir/second-open.log" 2>&1 &
  second_job=$!
  open_jobs+=("$second_job")
}

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
  local log="$1" status="$2"
  [[ $status -eq 0 ]] || { [[ $status -eq 1 ]] && grep -qF "$OPEN_EXITED_EARLY" "$log"; }
}

# --- окно и чтения ---

# Опрос ui.window до условия cond над файлом (функция cond <файл>) за ms; последний ответ — в <prefix>.stdout.
poll_window() {
  local state="$1" prefix="$2" ms="$3" cond="$4" deadline
  deadline=$(($(now_ms) + ms))
  while :; do
    if ui_read "$state" "$prefix" window && "$cond" "$prefix.stdout"; then
      return 0
    fi
    (($(now_ms) < deadline)) || return 1
    sleep "$POLL"
  done
}

poll_inspect() {
  local state="$1" prefix="$2" ms="$3" cond="$4" deadline
  deadline=$(($(now_ms) + ms))
  while :; do
    if ui_read "$state" "$prefix" inspect && "$cond" "$prefix.stdout"; then
      return 0
    fi
    (($(now_ms) < deadline)) || return 1
    sleep "$POLL"
  done
}

window_hidden() {
  [[ "$(jv "$1" ok.ui.window.visible)" == false ]]
}

window_shown() {
  [[ "$(jv "$1" ok.ui.window.visible)" == true && "$(jv "$1" ok.ui.window.key)" == true ]]
}

# Видимость, key и номер окна из ответа ui.window одной строкой.
window_line() {
  printf 'окно %s, видимо %s, key %s' "$(jv "$1" ok.ui.window.window_number || echo ?)" \
    "$(jv "$1" ok.ui.window.visible || echo ?)" "$(jv "$1" ok.ui.window.key || echo ?)"
}

# Значение поля компонента id в ответе ui.inspect.
comp() {
  jv "$1" "ok.ui.inspect.components[id=$2].$3"
}

# --- снимки и оракул ---

# Снимок окна владельца и сравнение оракулом с файлом ожиданий expect при поколении gen; expect вида
# underline:<id вкладки>:<слой> — поперечная серия линии под вкладкой. Повтор, пока маркер не совпал (код оракула
# 4), не дольше MARKER_MS. Код оракула — в snap_code, причина расхождения — в snap_reason.
# Сырые файлы — <dir>/<name>.{window.json,inspect.json,raw.png,png,report.json,oracle.stderr}. Отказ захвата,
# неверный номер окна и ошибка исполнения оракула — ошибка исполнения прогона.
snapshot() {
  local state="$1" dir="$2" name="$3" expect="$4" gen="$5" deadline number tries=0 code spec
  local oracle_args=(compare --expect "$expect")
  if [[ "$expect" == underline:* ]]; then
    spec="${expect#underline:}"
    oracle_args=(underline --tab "${spec%%:*}" --layer "${spec#*:}")
  fi
  snap_code=""
  snap_reason=""
  deadline=$(($(now_ms) + MARKER_MS))
  while :; do
    tries=$((tries + 1))
    ui_read "$state" "$dir/$name.window" window ||
      fatal "снимок $name: ui.window не прочитан: $(oneline "$dir/$name.window.stdout")"
    ui_read "$state" "$dir/$name.inspect" inspect ||
      fatal "снимок $name: ui.inspect не прочитан: $(oneline "$dir/$name.inspect.stdout")"
    number="$(jv "$dir/$name.window.stdout" ok.ui.window.window_number)" ||
      fatal "снимок $name: в ui.window нет window_number"
    rm -f "$dir/$name.raw.png" "$dir/$name.png"
    /usr/sbin/screencapture -x -o -l "$number" "$dir/$name.raw.png" >"$dir/$name.capture.log" 2>&1 ||
      fatal "снимок $name: screencapture -l $number отказал посреди прогона: $(oneline "$dir/$name.capture.log")"
    [[ -s "$dir/$name.raw.png" ]] || fatal "снимок $name: screencapture -l $number не записал PNG"
    sips --matchTo "$SRGB_PROFILE" "$dir/$name.raw.png" --out "$dir/$name.png" >"$dir/$name.sips.log" 2>&1 ||
      fatal "снимок $name: sips не перевёл PNG в sRGB: $(oneline "$dir/$name.sips.log")"
    code=0
    "$oracle_bin" "${oracle_args[@]}" --inspect "$dir/$name.inspect.stdout" \
      --window "$dir/$name.window.stdout" --png "$dir/$name.png" --generation "$gen" \
      >"$dir/$name.report.json" 2>"$dir/$name.oracle.stderr" || code=$?
    case $code in
      0 | 3)
        snap_code=$code
        snap_reason="$(oneline "$dir/$name.oracle.stderr")"
        log "снимок $name: код оракула $code за $tries попыт., $(oneline "$dir/$name.oracle.stderr")"
        return 0
        ;;
      4)
        if (($(now_ms) >= deadline)); then
          snap_code=4
          snap_reason="отрисовка поколения $gen не наблюдалась за $MARKER_MS мс: $(oneline "$dir/$name.oracle.stderr")"
          log "снимок $name: $snap_reason"
          return 0
        fi
        ;;
      *)
        fatal "снимок $name: оракул — ошибка исполнения (код $code): $(oneline "$dir/$name.oracle.stderr")"
        ;;
    esac
  done
}

# Ячейка ответа ui.window одной строкой: светлота/источник, контраст/источник.
cell_line() {
  printf '%s/%s, %s/%s' "$(jv "$1" ok.ui.window.lightness.value || echo ?)" \
    "$(jv "$1" ok.ui.window.lightness.source || echo ?)" "$(jv "$1" ok.ui.window.contrast.value || echo ?)" \
    "$(jv "$1" ok.ui.window.contrast.source || echo ?)"
}

# --- подготовка и уборка ---

# Бинарь пакета package с целью target из сообщения Cargo; путь — в BUILT.
BUILT=""
build_bin() {
  local package="$1" target="$2" messages line
  BUILT=""
  messages="$(cd "$root" && "$cargo" build --release --locked -p "$package" --message-format=json-render-diagnostics)" ||
    fatal "cargo build -p $package завершился ошибкой"
  while IFS= read -r line; do
    [[ "$line" == *'"reason":"compiler-artifact"'* && "$line" != *'"executable":null'* ]] || continue
    [[ "$(plutil -extract target.name raw -o - - <<<"$line")" == "$target" ]] || continue
    [[ -z "$BUILT" ]] || fatal "Cargo сообщил больше одного бинаря $target"
    BUILT="$(plutil -extract executable raw -o - - <<<"$line")" || fatal "в сообщении Cargo о $target нет пути"
  done <<<"$messages"
  [[ -n "$BUILT" && -x "$BUILT" ]] || fatal "Cargo не сообщил о собранном бинаре $target"
}

prepare() {
  local left
  app="${bundle_arg:-$root/target/bundle/shellio.app}"
  [[ -d "$app" ]] || fatal "нет бандла $app: соберите его через make bundle"
  app="$(cd "$app" && pwd -P)"
  exe="$app/Contents/MacOS/shellio"
  [[ -x "$exe" ]] || fatal "в бандле нет исполняемого $exe"
  [[ -x "$LSAPPINFO" ]] || fatal "нет $LSAPPINFO"
  /usr/bin/perl -MJSON::PP -e 1 2>/dev/null || fatal "нет модуля JSON::PP системного perl"
  for left in "$expect_dir"/{light,dark,light-ic,dark-ic}.json; do
    [[ -f "$left" ]] || fatal "нет файла ожиданий $left"
  done
  left="$(run_processes)"
  [[ -z "$left" ]] || fatal "предусловие: бинарь бандла прогона уже запущен (PID $left), завершите его"
  [[ -n "$(finder_pid)" ]] || fatal "предусловие: Finder не запущен"
  find_terminal
  build_bin shellioctl shellioctl
  ctl_bin="$BUILT"
  build_bin shellio-e2e shellio-e2e-oracle
  oracle_bin="$BUILT"
  log "каталог прогона: $run"
  log "бандл: $app, shellioctl: $ctl_bin, оракул: $oracle_bin"
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
    for pid in ${open_jobs[@]+"${open_jobs[@]}"} ${bg_jobs[@]+"${bg_jobs[@]}"}; do
      if alive "$pid"; then
        busy="$busy $pid"
      fi
    done
    [[ -n "$busy" ]] || break
    (($(now_ms) < deadline)) || break
    sleep "$POLL"
  done
  # Задание open или osascript — сам этот процесс, а не приложение: его завершение экземпляр не трогает.
  for pid in ${open_jobs[@]+"${open_jobs[@]}"} ${bg_jobs[@]+"${bg_jobs[@]}"}; do
    if alive "$pid"; then
      kill -TERM "$pid" 2>/dev/null || true
    fi
  done
  for pid in ${open_jobs[@]+"${open_jobs[@]}"} ${bg_jobs[@]+"${bg_jobs[@]}"}; do
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
  # Без -u: ошибка set -u в ловушке EXIT у bash 3.2 тоже даёт код 0, минуя exit "$code".
  set +eu
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

# --- предусловия ---

# Пробы предусловий по окну владельца служебного профиля в режиме галереи; затем он завершается. Последняя проба —
# самопроверка оракула.
preconditions() {
  local dir="$run/preconditions" name layout ok preflight number width height scale pw ph pid gen code
  for name in "${WINDOW_MANAGERS[@]}"; do
    pid="$(pgrep -x "$name" | head -n 1 || true)"
    [[ -z "$pid" ]] ||
      fatal "предусловие: запущен оконный менеджер $name (pid $pid), он переставляет окна: выключите его на время прогона"
  done
  layout="$(defaults read com.apple.HIToolbox AppleCurrentKeyboardLayoutInputSourceID 2>/dev/null || true)"
  ok=0
  for name in "${LATIN_LAYOUTS[@]}"; do
    [[ "$layout" != "$name" ]] || ok=1
  done
  ((ok)) || fatal "предусловие: источник ввода ${layout:-не прочитан}, нужен латинский (ABC, US или British): keystroke набирает текст по раскладке"
  log "источник ввода: $layout"
  preflight="$(osascript -l JavaScript -e 'ObjC.import("CoreGraphics"); ObjC.bindFunction("CGPreflightScreenCaptureAccess", ["bool", []]); String($.CGPreflightScreenCaptureAccess())' 2>"$dir/preflight.stderr" || true)"
  [[ "$preflight" == true ]] ||
    fatal "предусловие: нет Screen Recording у терминала-предка ${terminal_id:-pid $terminal_pid}: CGPreflightScreenCaptureAccess — ${preflight:-ошибка $(oneline "$dir/preflight.stderr")}"

  new_profile
  log "== предусловия: служебный владелец галереи, профиль $NEW_PROFILE"
  launch_owner "$NEW_PROFILE" "$dir" --gallery || fatal "предусловие: служебный владелец не поднят: $WHY"
  pid="$OWNER_PID"
  # Accessibility и Automation: System Events читает имя процесса владельца по PID.
  name="$(osascript -e "tell application \"System Events\" to get name of (first process whose unix id is $pid)" \
    2>"$dir/system-events.stderr" || true)"
  [[ -n "$name" ]] ||
    fatal "предусловие: System Events не прочитал процесс pid $pid (нет Accessibility или Automation у терминала-предка): $(oneline "$dir/system-events.stderr")"
  log "System Events: процесс pid $pid — $name"
  ui_read "$NEW_PROFILE" "$dir/window" window ||
    fatal "предусловие: ui.window служебного владельца не прочитан: $(oneline "$dir/window.stdout")"
  number="$(jv "$dir/window.stdout" ok.ui.window.window_number)" || fatal "предусловие: в ui.window нет window_number"
  width="$(jv "$dir/window.stdout" ok.ui.window.frame.width)"
  height="$(jv "$dir/window.stdout" ok.ui.window.frame.height)"
  scale="$(jv "$dir/window.stdout" ok.ui.window.backing_scale_factor)"
  /usr/sbin/screencapture -x -o -l "$number" "$dir/probe.png" >"$dir/probe.log" 2>&1 ||
    fatal "предусловие: screencapture -l $number по окну служебного владельца отказал (нет Screen Recording): $(oneline "$dir/probe.log")"
  pw="$(sips -g pixelWidth "$dir/probe.png" 2>/dev/null | awk '/pixelWidth/ { print $2 }')"
  ph="$(sips -g pixelHeight "$dir/probe.png" 2>/dev/null | awk '/pixelHeight/ { print $2 }')"
  awk -v w="$width" -v h="$height" -v s="$scale" -v pw="$pw" -v ph="$ph" \
    'BEGIN { exit !(pw == w * s && ph == h * s) }' ||
    fatal "предусловие: размер снимка окна ${pw:-?}×${ph:-?} пикселей не равен кадру ${width}×${height} × $scale: нет Screen Recording или окно переставлено"
  log "Screen Recording: снимок окна $number — ${pw}×${ph}"

  # Самопроверка оракула: слот chrome заменён на text — расхождение обязано найтись.
  sed 's/"slot": "chrome"/"slot": "text"/' "$expect_dir/light.json" >"$dir/wrong-light.json"
  cmp -s "$dir/wrong-light.json" "$expect_dir/light.json" && fatal "самопроверка оракула: в light.json нет слота chrome"
  ui_cmd "$NEW_PROFILE" "$dir/cell" gallery cell light normal || fatal "самопроверка оракула: $WHY"
  gen="$GEN"
  snapshot "$NEW_PROFILE" "$dir" self-check "$dir/wrong-light.json" "$gen"
  case "$snap_code" in
    3) log "самопроверка оракула: неверное ожидание дало расхождение — $snap_reason" ;;
    0) fatal "оракул не различает цвета: ожидание со слотом text вместо chrome прошло, отчёт — preconditions/self-check.report.json" ;;
    *) fatal "самопроверка оракула не выполнена: $snap_reason" ;;
  esac
  code=0
  verified_hello "$NEW_PROFILE" "$dir/hello-stop" || fatal "предусловие: служебный владелец: $WHY"
  terminate "$HELLO_PID"
  alive "$pid" && fatal "предусловие: служебный владелец pid $pid не завершился"
  return $code
}

# --- пункты матрицы ---

# Первый передний план: open -n нового профиля без флагов от Finder.
item_first_front() {
  local item=first-front dir="$run/first-front" t0 ready_ms="" deadline status=0 pid
  failures=()
  new_profile
  profile_a="$NEW_PROFILE"
  owner_out_a="$dir/owner.stdout"
  log "== $item: профиль A $profile_a"
  focus_finder "$dir"
  front_log="$dir/front.log"
  printf '# мс от вызова open -n, pid переднего плана, бинарь\n' >"$front_log"
  : >"$dir/owner.stdout"
  : >"$dir/owner.stderr"
  t0="$(now_ms)"
  open -n --env "SHELLIO_STATE_DIR=$profile_a" --stdout "$dir/owner.stdout" --stderr "$dir/owner.stderr" "$app" \
    >"$dir/open.log" 2>&1 || status=$?
  if [[ $status -ne 0 ]]; then
    record "$item" fail "open -n завершился с кодом $status: $(oneline "$dir/open.log")"
    return 0
  fi
  deadline=$((t0 + OWNER_START_S * 1000))
  while :; do
    sample "$t0"
    if grep -qF "$READY" "$dir/owner.stdout"; then
      ready_ms="$sample_ms"
      printf '# строка готовности +%d\n' "$((ready_ms - t0))" >>"$front_log"
      break
    fi
    if (($(now_ms) >= deadline)); then
      record "$item" fail "нет строки готовности за $OWNER_START_S с: $(oneline "$dir/owner.stderr")"
      return 0
    fi
    sleep "$POLL"
  done
  if ! verified_hello "$profile_a" "$dir/hello"; then
    record "$item" fail "$WHY"
    return 0
  fi
  pid="$HELLO_PID"
  grep -qF "$READY$pid," "$dir/owner.stdout" || failures+=("строка готовности не называет pid $pid из hello")
  if watch_front "$pid" "$ready_ms"; then
    hold_front "$pid" "$ready_ms"
    ((front_ms <= FRONT_LIMIT_MS)) ||
      failures+=("передний план перешёл на владельца pid $pid через $front_ms мс после строки готовности, позже $FRONT_LIMIT_MS мс")
    [[ -z "$hold_lost" ]] || failures+=("передний план не удержался $HOLD_MS мс: $hold_lost, ряд — $item/front.log")
  else
    failures+=("передний план не перешёл на владельца pid $pid за $FRONT_CAP_MS мс после строки готовности, последний — $(describe_pid "$sample_front" "$pid"), ряд — $item/front.log")
  fi
  if ui_read "$profile_a" "$dir/window" window; then
    window_shown "$dir/window.stdout" || failures+=("ui.window: $(window_line "$dir/window.stdout"), ожидалось видимо и key")
  else
    failures+=("ui.window не прочитан: $(oneline "$dir/window.stdout")")
  fi
  conclude "$item" "Finder впереди подтверждён, передний план — владелец pid $pid через ${front_ms:-?} мс после строки готовности и держался $HOLD_MS мс, окно видимо и key"
}

item_window() {
  local item=window dir="$run/window" ws title width height srgb scale ch wx ww sx sw cw
  failures=()
  log "== $item"
  if ! verified_hello "$profile_a" "$dir/hello"; then
    record "$item" fail "$WHY"
    return 0
  fi
  if ! ctl "$profile_a" "$dir/tree" --json --timeout "$CTL_TIMEOUT_MS" tree --all; then
    record "$item" fail "shellioctl tree --all не ответил: $(oneline "$dir/tree.stdout")"
    return 0
  fi
  ws="$(jv "$dir/tree.stdout" ok.value.active_workspace)"
  title="$(jv "$dir/tree.stdout" "ok.value.workspaces[id=$ws].title")" || title=""
  if ! ui_read "$profile_a" "$dir/window" window || ! ui_read "$profile_a" "$dir/inspect" inspect; then
    record "$item" fail "ui.window или ui.inspect не прочитан: $(oneline "$dir/window.stdout") $(oneline "$dir/inspect.stdout")"
    return 0
  fi
  width="$(jv "$dir/window.stdout" ok.ui.window.frame.width)"
  height="$(jv "$dir/window.stdout" ok.ui.window.frame.height)"
  srgb="$(jv "$dir/window.stdout" ok.ui.window.color_space.srgb)"
  scale="$(jv "$dir/window.stdout" ok.ui.window.backing_scale_factor || true)"
  fcmp "$width" ">=" 1180 && fcmp "$height" ">=" 720 || failures+=("кадр окна ${width}×${height}, меньше 1180×720")
  [[ "$srgb" == true ]] || failures+=("colorSpace окна не sRGB: $(jv "$dir/window.stdout" ok.ui.window.color_space)")
  [[ -n "$scale" && "$scale" != null ]] || failures+=("backingScaleFactor не записан")
  [[ -n "$title" ]] || failures+=("в tree --all нет активного воркспейса $ws")
  [[ "$(jv "$dir/window.stdout" ok.ui.window.title)" == "$title" ]] ||
    failures+=("системный заголовок «$(jv "$dir/window.stdout" ok.ui.window.title)», ожидалось имя воркспейса «${title}»")
  ch="$(comp "$dir/inspect.stdout" chrome frame.height || echo ?)"
  fcmp "$ch" ">=" 39.5 && fcmp "$ch" "<=" 40.5 || failures+=("высота ChromeBar $ch, ожидалось 40")
  wx="$(comp "$dir/inspect.stdout" chrome.workspace frame.x || echo ?)"
  ww="$(comp "$dir/inspect.stdout" chrome.workspace frame.width || echo ?)"
  sx="$(comp "$dir/inspect.stdout" chrome.search frame.x || echo ?)"
  sw="$(comp "$dir/inspect.stdout" chrome.search frame.width || echo ?)"
  cw="$(jv "$dir/inspect.stdout" ok.ui.inspect.content.width || echo ?)"
  awk -v wx="$wx" -v ww="$ww" -v sx="$sx" -v cw="$cw" 'BEGIN { exit !(wx + ww <= sx && wx < cw / 2) }' ||
    failures+=("кнопка воркспейса x $wx ширина $ww не слева от поиска x $sx")
  awk -v sx="$sx" -v sw="$sw" -v cw="$cw" 'BEGIN { d = sx + sw / 2 - cw / 2; if (d < 0) d = -d; exit !(d <= 1) }' ||
    failures+=("центр поиска $sx+$sw/2 не совпал с центром окна $cw/2 ±1 точка")
  conclude "$item" "кадр ${width}×${height}, colorSpace sRGB, backingScaleFactor $scale, заголовок «${title}», ChromeBar высотой $ch, кнопка воркспейса слева, центр поиска в центре окна"
}

mirror_revision_reached() {
  local revision
  revision="$(jv "$1" ok.ui.window.mirror_revision)" || return 1
  [[ "$revision" =~ ^[0-9]+$ ]] && ((revision >= mirror_want))
}

mirror_want=0
item_mirror() {
  local item=mirror dir="$run/mirror" ws name="e2e-зеркало-$$" before rev title text
  failures=()
  log "== $item"
  if ! ctl "$profile_a" "$dir/tree" --json --timeout "$CTL_TIMEOUT_MS" tree --all; then
    record "$item" fail "shellioctl tree --all не ответил: $(oneline "$dir/tree.stdout")"
    return 0
  fi
  ws="$(jv "$dir/tree.stdout" ok.value.active_workspace)"
  if ! ctl "$profile_a" "$dir/rename" --json --timeout "$CTL_TIMEOUT_MS" workspace rename --target "$ws" "$name" --all; then
    record "$item" fail "workspace rename: $(oneline "$dir/rename.stdout") $(oneline "$dir/rename.stderr")"
    return 0
  fi
  rev="$(jv "$dir/rename.stdout" ok.revision)"
  mirror_want="$rev"
  if poll_window "$profile_a" "$dir/window" "$MIRROR_MS" mirror_revision_reached; then
    title="$(jv "$dir/window.stdout" ok.ui.window.title)"
    [[ "$title" == "$name" ]] || failures+=("системный заголовок «${title}» после ревизии $rev, ожидалось «${name}»")
    if ui_read "$profile_a" "$dir/inspect" inspect; then
      text="$(comp "$dir/inspect.stdout" chrome state.chrome.workspace_text || echo ?)"
      [[ "$text" == "$name" ]] || failures+=("кнопка воркспейса «${text}», ожидалось «${name}»")
    else
      failures+=("ui.inspect не прочитан")
    fi
  else
    failures+=("ui.window не показал ревизию $rev за $MIRROR_MS мс: ревизия зеркала $(jv "$dir/window.stdout" ok.ui.window.mirror_revision || echo ?)")
  fi
  before="$name"
  if ctl "$profile_a" "$dir/new" --json --timeout "$CTL_TIMEOUT_MS" workspace new "e2e-другой-$$" --all; then
    rev="$(jv "$dir/new.stdout" ok.revision)"
    mirror_want="$rev"
    if poll_window "$profile_a" "$dir/window-new" "$MIRROR_MS" mirror_revision_reached; then
      title="$(jv "$dir/window-new.stdout" ok.ui.window.title)"
      [[ "$title" == "$before" ]] || failures+=("workspace new неактивного сменил заголовок: «${title}»")
    else
      failures+=("ui.window не показал ревизию $rev workspace new за $MIRROR_MS мс")
    fi
  else
    failures+=("workspace new: $(oneline "$dir/new.stdout") $(oneline "$dir/new.stderr")")
  fi
  conclude "$item" "rename → заголовок и кнопка воркспейса «${name}» с ревизией ответа в пределах $MIRROR_MS мс; workspace new неактивного заголовок не сменил"
}

# window close, затем ожидание невидимого окна; нарушения — в failures с меткой.
close_window() {
  local state="$1" prefix="$2" label="$3"
  if ! ui_cmd "$state" "$prefix" window close; then
    failures+=("$label: $WHY")
    return 1
  fi
  if ! poll_window "$state" "$prefix-window" "$STATE_MS" window_hidden; then
    failures+=("$label: окно видимо через $STATE_MS мс после window close: $(window_line "$prefix-window.stdout")")
    return 1
  fi
}

same_window_shown() {
  window_shown "$1" && [[ "$(jv "$1" ok.ui.window.window_number)" == "$window_want" ]]
}

window_want=""
item_close_reopen() {
  local item=close-reopen dir="$run/close-reopen" pid status=0 t0
  failures=()
  log "== $item"
  if ! verified_hello "$profile_a" "$dir/hello"; then
    record "$item" fail "$WHY"
    return 0
  fi
  pid="$HELLO_PID"
  if ! single_owner "$pid"; then
    record "$item" fail "$WHY"
    return 0
  fi
  ui_read "$profile_a" "$dir/before" window || {
    record "$item" fail "ui.window не прочитан"
    return 0
  }
  window_want="$(jv "$dir/before.stdout" ok.ui.window.window_number)"
  if close_window "$profile_a" "$dir/close-1" "первое закрытие"; then
    alive "$pid" || failures+=("PID $pid вышел после window close")
    hello "$profile_a" "$dir/hello-closed" || failures+=("hello не ответил после window close")
    focus_finder "$dir"
    front_log="$dir/front-open.log"
    printf '# мс от вызова open, pid переднего плана, бинарь\n' >"$front_log"
    t0="$(now_ms)"
    open --env "SHELLIO_STATE_DIR=$profile_a" "$app" >"$dir/open.log" 2>&1 || status=$?
    if [[ $status -ne 0 ]]; then
      failures+=("обычный open завершился с кодом $status: $(oneline "$dir/open.log")")
    else
      if watch_front "$pid" "$t0"; then
        ((front_ms <= FRONT_LIMIT_MS)) || failures+=("обычный open: передний план — владелец через $front_ms мс, позже $FRONT_LIMIT_MS мс")
      else
        failures+=("обычный open: передний план не перешёл на владельца за $FRONT_CAP_MS мс, последний — $(describe_pid "$sample_front" "$pid")")
      fi
      poll_window "$profile_a" "$dir/reopen" "$STATE_MS" same_window_shown ||
        failures+=("обычный open: $(window_line "$dir/reopen.stdout"), ожидалось окно $window_want видимо и key")
    fi
  fi
  if close_window "$profile_a" "$dir/close-2" "второе закрытие"; then
    focus_finder "$dir"
    front_log="$dir/front-second.log"
    printf '# мс от квитанции второго запуска, pid переднего плана, бинарь\n' >"$front_log"
    start_second_open "$profile_a" "$dir"
    if await_receipt "$dir" "$pid"; then
      if watch_front "$pid" "$receipt_ms"; then
        ((front_ms <= FRONT_LIMIT_MS)) || failures+=("второй запуск: передний план — владелец через $front_ms мс после квитанции, позже $FRONT_LIMIT_MS мс")
      else
        failures+=("второй запуск: передний план не перешёл на владельца за $FRONT_CAP_MS мс после квитанции, последний — $(describe_pid "$sample_front" "$pid")")
      fi
      poll_window "$profile_a" "$dir/second" "$STATE_MS" same_window_shown ||
        failures+=("второй запуск: $(window_line "$dir/second.stdout"), ожидалось окно $window_want видимо и key")
    else
      failures+=("в stdout второго запуска нет квитанции с pid $pid: $(oneline "$dir/second.stdout") $(oneline "$dir/second.stderr")")
    fi
    if finish_second; then
      open_ok "$dir/second-open.log" "$second_status" ||
        failures+=("open -W -n второго запуска завершился с кодом $second_status: $(oneline "$dir/second-open.log")")
    else
      failures+=("второй запуск не вышел за $SECOND_LAUNCH_S с")
    fi
  fi
  alive "$pid" || failures+=("владелец pid $pid вышел")
  conclude "$item" "window close прячет окно $window_want, PID $pid жив и отвечает; обычный open и второй запуск open -n от Finder показывают то же окно видимым и key, передний план — владелец за $FRONT_LIMIT_MS мс"
}

# window-cycle (а): циклы window close и обычного open в обычном режиме.
cycle_reopen() {
  local dir="$run/window-cycle/a" pid n status
  mkdir -p "$dir"
  log "== window-cycle (а): $CYCLES циклов window close и обычного open"
  failures=()
  if ! verified_hello "$profile_a" "$dir/hello"; then
    cycle_failures+=("(а) $WHY")
    return 0
  fi
  pid="$HELLO_PID"
  if ! single_owner "$pid"; then
    cycle_failures+=("(а) $WHY")
    return 0
  fi
  ui_read "$profile_a" "$dir/before" window || {
    cycle_failures+=("(а) ui.window не прочитан")
    return 0
  }
  window_want="$(jv "$dir/before.stdout" ok.ui.window.window_number)"
  for ((n = 1; n <= CYCLES; n++)); do
    close_window "$profile_a" "$dir/close-$n" "цикл $n" || break
    status=0
    open --env "SHELLIO_STATE_DIR=$profile_a" "$app" >"$dir/open-$n.log" 2>&1 || status=$?
    if [[ $status -ne 0 ]]; then
      failures+=("цикл $n: обычный open завершился с кодом $status: $(oneline "$dir/open-$n.log")")
      break
    fi
    if ! poll_window "$profile_a" "$dir/reopen-$n" "$STATE_MS" same_window_shown; then
      failures+=("цикл $n: $(window_line "$dir/reopen-$n.stdout"), ожидалось окно $window_want видимо и key")
      break
    fi
    alive "$pid" || {
      failures+=("цикл $n: PID $pid вышел")
      break
    }
  done
  if [[ -n "${failures[*]+x}" ]]; then
    cycle_failures+=("(а) ${failures[*]}")
  else
    cycle_notes+=("(а) $CYCLES циклов: окно $window_want то же, PID $pid жив")
  fi
}

item_quit() {
  local item=quit dir="$run/quit" pid code=0
  failures=()
  log "== $item"
  if ! verified_hello "$profile_a" "$dir/hello"; then
    record "$item" fail "$WHY"
    return 0
  fi
  pid="$HELLO_PID"
  ctl "$profile_a" "$dir/quit" --json --timeout "$CTL_TIMEOUT_MS" app quit || code=$?
  [[ $code -eq 0 ]] || failures+=("shellioctl app quit: код $code: $(oneline "$dir/quit.stdout") $(oneline "$dir/quit.stderr")")
  [[ "$(jv "$dir/quit.stdout" ok.ui.accepted.command 2>/dev/null)" == app.quit ]] ||
    failures+=("нет квитанции accepted app.quit: $(oneline "$dir/quit.stdout")")
  [[ "$(jv "$dir/quit.stdout" ok.ui.accepted.pid 2>/dev/null)" == "$pid" ]] ||
    failures+=("квитанция не называет pid $pid: $(oneline "$dir/quit.stdout")")
  quit_signs "$profile_a" "$pid" "$owner_out_a"
  conclude "$item" "app quit: код 0, accepted с pid $pid; «${UI_QUIT}», сокет удалён, файл блокировки на месте, pid вышел за $EXIT_S с"
}

# Признаки выхода через applicationShouldTerminate; нарушения — в failures.
quit_signs() {
  local state="$1" pid="$2" out="$3"
  if wait_gone "$pid" "$EXIT_S"; then
    grep -qF "$UI_QUIT" "$out" || failures+=("в stdout владельца нет строки «${UI_QUIT}»: $(oneline "$out")")
    [[ ! -e "$state/shellio.sock" && ! -L "$state/shellio.sock" ]] || failures+=("сокет остался после выхода")
    [[ -f "$state/shellio.lock" ]] || failures+=("файла блокировки нет после выхода")
  else
    failures+=("pid $pid не вышел за $EXIT_S с")
  fi
}

item_gui_quit_pid() {
  local item=gui-quit-pid dir="$run/gui-quit-pid" pid answer
  failures=()
  log "== $item"
  new_profile
  if ! launch_owner "$NEW_PROFILE" "$dir"; then
    record "$item" fail "$WHY"
    return 0
  fi
  pid="$OWNER_PID"
  answer="$(osascript -l JavaScript -e "ObjC.import('AppKit'); const a = \$.NSRunningApplication.runningApplicationWithProcessIdentifier($pid); a.isNil() ? 'nil' : String(a.terminate)" \
    2>"$dir/terminate.stderr" || true)"
  printf '%s\n' "$answer" >"$dir/terminate.stdout"
  if [[ "$answer" == nil || -z "$answer" ]]; then
    record "$item" not_run "NSRunningApplication не адресовал pid $pid: ${answer:-$(oneline "$dir/terminate.stderr")}"
    return 0
  fi
  [[ "$answer" == true ]] || failures+=("NSRunningApplication.terminate ответил $answer")
  quit_signs "$NEW_PROFILE" "$pid" "$dir/owner.stdout"
  conclude "$item" "NSRunningApplication.terminate по pid $pid: «${UI_QUIT}», сокет удалён, файл блокировки на месте, pid вышел за $EXIT_S с"
}

startup_present() {
  [[ "$(jv "$1" ok.ui.stats.startup)" != null ]]
}

poll_stats() {
  local state="$1" prefix="$2" ms="$3" cond="$4" deadline
  deadline=$(($(now_ms) + ms))
  while :; do
    if ui_read "$state" "$prefix" stats && "$cond" "$prefix.stdout"; then
      return 0
    fi
    (($(now_ms) < deadline)) || return 1
    sleep "$POLL"
  done
}

item_startup_warm() {
  local item=startup-warm dir="$run/startup-warm" n sub t_open status pid ms launch max=0 list="" ext
  failures=()
  log "== $item: $STARTUP_RUNS запусков open -n, первый отбрасывается"
  printf '# запуск, launchDate → первая отрисовка окна key с первым снимком (мс), вызов open → launchDate (мс)\n' \
    >"$dir/series.txt"
  for ((n = 1; n <= STARTUP_RUNS; n++)); do
    sub="$dir/$n"
    mkdir -p "$sub"
    new_profile
    focus_finder "$sub"
    : >"$sub/owner.stdout"
    : >"$sub/owner.stderr"
    status=0
    t_open="$(now_ms)"
    open -n --env "SHELLIO_STATE_DIR=$NEW_PROFILE" --stdout "$sub/owner.stdout" --stderr "$sub/owner.stderr" \
      "$app" >"$sub/open.log" 2>&1 || status=$?
    if [[ $status -ne 0 ]]; then
      failures+=("запуск $n: open -n завершился с кодом $status")
      break
    fi
    if ! wait_owner "$NEW_PROFILE" "$sub/hello" || ! is_run_exe "$HELLO_PID"; then
      failures+=("запуск $n: владелец не ответил на hello: $(oneline "$sub/owner.stderr")")
      break
    fi
    pid="$HELLO_PID"
    if poll_stats "$NEW_PROFILE" "$sub/stats" "$STARTUP_MS" startup_present; then
      ms="$(jv "$sub/stats.stdout" ok.ui.stats.startup.key_draw_ms)"
    else
      ms=""
      ext="ui.window не прочитан"
      if ui_read "$NEW_PROFILE" "$sub/window" window; then
        ext="$(window_line "$sub/window.stdout")"
      fi
      failures+=("запуск $n: ui.stats.startup пуст за $STARTUP_MS мс: окно не стало key или не отрисовано с первым снимком, ui.window — $ext")
    fi
    launch="$(osascript -l JavaScript -e "ObjC.import('AppKit'); const a = \$.NSRunningApplication.runningApplicationWithProcessIdentifier($pid); a.isNil() || a.launchDate.isNil() ? '' : String(Math.round(a.launchDate.timeIntervalSince1970 * 1000))" 2>/dev/null || true)"
    ext="?"
    [[ ! "$launch" =~ ^[0-9]+$ ]] || ext=$((launch - t_open))
    printf '%d %s %s\n' "$n" "${ms:-нет}" "$ext" >>"$dir/series.txt"
    terminate "$pid"
    alive "$pid" && {
      failures+=("запуск $n: pid $pid не вышел")
      break
    }
    [[ -n "$ms" ]] || break
    if ((n > 1)); then
      list="${list:+$list, }$(printf '%.1f' "$ms")"
      awk -v a="$ms" -v b="$max" 'BEGIN { exit !(a > b) }' && max="$ms"
    fi
  done
  if [[ -z "${failures[*]+x}" ]] && ! fcmp "$max" "<=" "$STARTUP_BUDGET_MS"; then
    failures+=("максимум старта $(printf '%.1f' "$max") мс больше бюджета $STARTUP_BUDGET_MS мс: $list (ряд — $item/series.txt)")
  fi
  conclude "$item" "максимум 10 стартов $(printf '%.1f' "$max") мс ≤ $STARTUP_BUDGET_MS мс: $list; внешняя метка «вызов open → launchDate» — $item/series.txt"
}

# --- галерея ---

launch_gallery() {
  local dir="$run/gallery" deadline
  mkdir -p "$dir"
  new_profile
  profile_g="$NEW_PROFILE"
  log "== галерея: профиль $profile_g"
  launch_owner "$profile_g" "$dir" --gallery || fatal "владелец галереи не поднят: $WHY"
  owner_pid_g="$OWNER_PID"
  # hello отвечает раньше, чем владелец допишет строку готовности: строка ждётся со сроком.
  deadline=$(($(now_ms) + OWNER_START_S * 1000))
  until grep -qF "режим gallery" "$dir/owner.stdout"; do
    (($(now_ms) < deadline)) || fatal "строка готовности владельца галереи не называет режим gallery за $OWNER_START_S с: $(oneline "$dir/owner.stdout")"
    sleep "$POLL"
  done
}

# Сводка ui.stats для window-cycle: живые по классам и владельцам, поздние вызовы, прежние деревья с номером не
# меньше min и их живые объекты по классам.
stats_summary() {
  /usr/bin/perl -MJSON::PP -e '
    my ($file, $min) = @ARGV;
    open my $fh, "<", $file or exit 2; local $/;
    my $s = JSON::PP->new->utf8->decode(<$fh>)->{ok}{ui}{stats};
    binmode STDOUT, ":utf8";
    print "live $_->{kind}:$_->{name}=$_->{live}\n" for sort { "$a->{kind}$a->{name}" cmp "$b->{kind}$b->{name}" } @{$s->{instances}};
    my $late = 0; $late += $_->{count} for @{$s->{late_callbacks}};
    print "late $late\n";
    my ($trees, $alive, $system, %classes) = (0, 0, 0);
    for my $t (grep { $_->{tree} >= $min } @{$s->{gallery_trees}}) {
      $trees++; $alive += $t->{alive};
      for my $c (@{$t->{alive_classes} || []}) {
        $classes{$c->{class}} += $c->{alive};
        $system += $c->{alive} unless $c->{class} =~ /Shellio/;
      }
    }
    print "trees $trees\nalive $alive\nsystem $system\n";
    print "classes ", join(", ", map { "$_ $classes{$_}" } sort keys %classes), "\n";
  ' "$1" "$2"
}

summary_field() {
  awk -v key="$2" '$1 == key { sub(/^[^ ]+ /, ""); print; exit }' "$1"
}

# Ожидание освобождения прежних деревьев с номером не меньше min до RELEASE_MS: живых не больше limit
# (по умолчанию 0); сводка — <prefix>.summary. Каждое чтение ui.stats сливает пул run владельца, поэтому
# освобождения, отложенные до очередного события, следующее чтение уже видит.
await_release() {
  local state="$1" prefix="$2" min="$3" limit="${4:-0}" deadline
  deadline=$(($(now_ms) + RELEASE_MS))
  while :; do
    ui_read "$state" "$prefix" stats || fatal "window-cycle: ui.stats не прочитан: $(oneline "$prefix.stdout")"
    stats_summary "$prefix.stdout" "$min" >"$prefix.summary"
    (($(summary_field "$prefix.summary" alive) > limit)) || return 0
    (($(now_ms) < deadline)) || return 1
    sleep "$POLL"
  done
}

# Живые по классам и владельцам после цикла N не больше, чем после цикла 1: строки «live kind:name=count».
live_no_growth() {
  /usr/bin/awk -F= '
    NR == FNR { first[$1] = $2; next }
    { if (!($1 in first) || $2 + 0 > first[$1] + 0) { printf "%s %s → %s; ", $1, ($1 in first ? first[$1] : 0), $2; bad = 1 } }
    END { exit bad }
  ' "$1" "$2"
}

# Части (б) и (в) window-cycle: циклы команды command на владельце галереи. Режим strict — прежние деревья
# освобождены после циклов 1 и N, живые по классам и владельцам после N равны циклу 1. Режим growth —
# запасной критерий правила плана «Объекты после закрытия окна», «нет роста от 1-го к N-му циклу»: живых
# прежних деревьев, живых по классам и владельцам после N не больше, чем после 1.
cycle_gallery() {
  local part="$1" label="$2" mode="$3" dir="$run/window-cycle/$1" min n first="" last="" sys1 sys10 base=0 grew
  shift 3
  mkdir -p "$dir"
  log "== window-cycle ($label): $CYCLES циклов $*"
  failures=()
  ui_read "$profile_g" "$dir/inspect-before" inspect || fatal "window-cycle: ui.inspect не прочитан"
  min="$(jv "$dir/inspect-before.stdout" ok.ui.inspect.gallery.tree)"
  for ((n = 1; n <= CYCLES; n++)); do
    if ! ui_cmd "$profile_g" "$dir/cmd-$n" "$@"; then
      failures+=("цикл $n: $WHY")
      break
    fi
    if ((n == 1)) && [[ "$mode" == growth ]]; then
      if ! await_release "$profile_g" "$dir/stats-1" "$min"; then
        base="$(summary_field "$dir/stats-1.summary" alive)"
        cycle_notes+=("($label) после цикла 1 за $RELEASE_MS мс не освобождено $base: $(summary_field "$dir/stats-1.summary" classes)")
      fi
      first="$dir/stats-1.summary"
    elif ((n == 1 || n == CYCLES)); then
      await_release "$profile_g" "$dir/stats-$n" "$min" "$base" ||
        failures+=("цикл $n: прежние деревья не освобождены за $RELEASE_MS мс: живых $(summary_field "$dir/stats-$n.summary" alive)$([[ $base == 0 ]] || echo ", после цикла 1 — $base"), по классам — $(summary_field "$dir/stats-$n.summary" classes)")
      ((n == 1)) && first="$dir/stats-1.summary"
      ((n == CYCLES)) && last="$dir/stats-$n.summary"
    fi
  done
  if [[ -n "$first" && -n "$last" ]]; then
    grep '^live ' "$first" >"$dir/live-1.txt"
    grep '^live ' "$last" >"$dir/live-$CYCLES.txt"
    if [[ "$mode" == growth ]]; then
      grew="$(live_no_growth "$dir/live-1.txt" "$dir/live-$CYCLES.txt")" ||
        failures+=("живые по классам define_class! и владельцам после цикла $CYCLES больше, чем после цикла 1: $(cut -c 1-300 <<<"$grew")")
    else
      diff "$dir/live-1.txt" "$dir/live-$CYCLES.txt" >"$dir/live.diff" ||
        failures+=("живые по классам define_class! и владельцам после цикла $CYCLES не равны циклу 1: $(tr '\n' ' ' <"$dir/live.diff" | cut -c 1-300)")
    fi
    [[ "$(summary_field "$last" late)" == 0 ]] || failures+=("поздних вызовов $(summary_field "$last" late)")
    sys1="$(summary_field "$first" system)"
    sys10="$(summary_field "$last" system)"
    ((sys10 <= sys1)) ||
      failures+=("системных объектов прежних деревьев после цикла 1 — $sys1, после $CYCLES — $sys10: рост")
    cycle_notes+=("($label) системных живых после цикла 1 — $sys1, после $CYCLES — $sys10 ($(summary_field "$last" classes))")
  fi
  if [[ -n "${failures[*]+x}" ]]; then
    cycle_failures+=("($label) ${failures[*]}")
  else
    if [[ "$mode" == growth ]]; then
      cycle_notes+=("($label) $CYCLES циклов: роста от цикла 1 нет — прежних живых после цикла $CYCLES $(summary_field "$last" alive) при $base после цикла 1, живые по классам и владельцам не больше цикла 1, поздних вызовов нет")
    else
      cycle_notes+=("($label) $CYCLES циклов: прежние деревья освобождены, живые по классам и владельцам равны циклу 1, поздних вызовов нет")
    fi
  fi
}

item_window_cycle() {
  local text="" x
  if ! ui_cmd "$profile_g" "$run/window-cycle/focus-field" gallery focus field; then
    cycle_failures+=("(в) gallery focus field: $WHY")
  fi
  cycle_gallery c в growth gallery window recreate
  ui_cmd "$profile_g" "$run/window-cycle/focus-window" gallery focus window ||
    cycle_failures+=("(в) gallery focus window: $WHY")
  failures=(${cycle_failures[@]+"${cycle_failures[@]}"})
  for x in ${cycle_notes[@]+"${cycle_notes[@]}"}; do
    text="${text:+$text; }$x"
  done
  [[ -z "${failures[*]+x}" ]] || failures+=("наблюдения: $text")
  conclude window-cycle "$text"
}

idle_settle() {
  local dir="$1" deadline prev="" now n=0
  deadline=$(($(mono_ms) + IDLE_SETTLE_S * 1000))
  : >"$dir/settle.txt"
  while :; do
    n=$((n + 1))
    ui_read "$profile_g" "$dir/settle" stats || {
      WHY="ui.stats не прочитан во время ожидания затишья"
      return 1
    }
    now="$(jv "$dir/settle.stdout" ok.ui.stats.run_loop.wakeups)"
    printf '%s %s\n' "$(mono_ms)" "$now" >>"$dir/settle.txt"
    if [[ -n "$prev" ]] && ((now - prev <= 2)); then
      return 0
    fi
    if (($(mono_ms) >= deadline)); then
      WHY="цикл не затих за $IDLE_SETTLE_S с: пробуждений по чтениям — $(awk '{ printf "%s ", $2 }' "$dir/settle.txt")"
      return 1
    fi
    prev="$now"
    sleep 1
  done
}

item_idle() {
  local item=idle dir="$run/idle" pid c1 c2 t1 t2 draws wakeups passes cpu
  failures=()
  log "== $item: $IDLE_S с без ввода"
  pid="$owner_pid_g"
  if ! ui_cmd "$profile_g" "$dir/focus" gallery focus window; then
    record "$item" fail "$WHY"
    return 0
  fi
  # Простой начинается, когда цикл затих: свежий владелец галереи первые секунды просыпается около 20 раз в
  # секунду (источник не установлен), затем одно пробуждение за 10 с. Затишье — два чтения ui.stats подряд через
  # секунду с приростом не больше двух пробуждений (само чтение будит цикл).
  if ! idle_settle "$dir"; then
    record "$item" fail "$WHY"
    return 0
  fi
  c1="$(ps -o time= -p "$pid" | tr -d ' ')"
  t1="$(mono_ms)"
  ui_read "$profile_g" "$dir/stats-1" stats || {
    record "$item" fail "ui.stats не прочитан"
    return 0
  }
  # Окно замера — само измерение: между двумя чтениями ui.stats запросов и ввода нет.
  sleep "$IDLE_S"
  ui_read "$profile_g" "$dir/stats-2" stats || {
    record "$item" fail "ui.stats не прочитан"
    return 0
  }
  c2="$(ps -o time= -p "$pid" | tr -d ' ')"
  t2="$(mono_ms)"
  draws=$(($(jv "$dir/stats-2.stdout" ok.ui.stats.run_loop.draws) - $(jv "$dir/stats-1.stdout" ok.ui.stats.run_loop.draws)))
  wakeups=$(($(jv "$dir/stats-2.stdout" ok.ui.stats.run_loop.wakeups) - $(jv "$dir/stats-1.stdout" ok.ui.stats.run_loop.wakeups)))
  passes=$(($(jv "$dir/stats-2.stdout" ok.ui.stats.run_loop.display_passes) - $(jv "$dir/stats-1.stdout" ok.ui.stats.run_loop.display_passes)))
  cpu="$(awk -v a="$c1" -v b="$c2" -v t1="$t1" -v t2="$t2" '
    function sec(x,  n, p, i, s) { n = split(x, p, ":"); s = 0; for (i = 1; i <= n; i++) s = s * 60 + p[i]; return s }
    BEGIN { printf "%.3f", (sec(b) - sec(a)) * 100000 / (t2 - t1) }')"
  printf 'draws %d\nwakeups %d\ndisplay_passes %d\ncpu_time %s %s\nmono_ms %s %s\ncpu_percent %s\n' "$draws" \
    "$wakeups" "$passes" "$c1" "$c2" "$t1" "$t2" "$cpu" >"$dir/idle.txt"
  ((draws == 0)) || failures+=("отрисовок компонентов за $IDLE_S с — $draws, ожидалось 0")
  # Фиксация транзакции Core Animation бывает не чаще одной за итерацию цикла, а итерация — от пробуждения до
  # засыпания, поэтому порог пробуждений ограничивает и фиксации. Проходов отображения у пробуждения с событием два
  # (Exit и BeforeWaiting, передача задачи 5), они записываются наблюдением.
  ((wakeups <= IDLE_WAKEUPS)) || failures+=("пробуждений цикла $wakeups, больше $IDLE_WAKEUPS")
  fcmp "$cpu" "<=" "$IDLE_CPU_PERCENT" || failures+=("CPU $cpu%, больше $IDLE_CPU_PERCENT%")
  conclude "$item" "за $((t2 - t1)) мс: отрисовок 0, пробуждений $wakeups, проходов отображения $passes, CPU $cpu%"
}

# Снимок ячейки: команды, затем сверка ячейки ui.window и оракул; нарушения — в failures с меткой.
cell_snapshot() {
  local dir="$1" name="$2" expect="$3" want_light="$4" want_light_source="$5" want_contrast="$6" got
  snapshot "$profile_g" "$dir" "$name" "$expect" "$GEN"
  got="$(cell_line "$dir/$name.window.stdout")"
  [[ "$(jv "$dir/$name.window.stdout" ok.ui.window.lightness.value)" == "$want_light" &&
    "$(jv "$dir/$name.window.stdout" ok.ui.window.lightness.source)" == "$want_light_source" &&
    "$(jv "$dir/$name.window.stdout" ok.ui.window.contrast.value)" == "$want_contrast" ]] ||
    failures+=("$name: ячейка ui.window $got, ожидалось $want_light/$want_light_source, $want_contrast")
  case "$snap_code" in
    0) ;;
    3) failures+=("$name: участки вне допуска — $snap_reason, отчёт — $(basename "$dir")/$name.report.json") ;;
    *) failures+=("$name: $snap_reason") ;;
  esac
}

item_snapshot() {
  local item="$1" lightness="$2" dir="$run/$1"
  failures=()
  log "== $item"
  if ! ui_cmd "$profile_g" "$dir/cell" gallery cell auto normal || ! ui_cmd "$profile_g" "$dir/appearance" ui appearance "$lightness"; then
    record "$item" fail "$WHY"
    return 0
  fi
  cell_snapshot "$dir" "$lightness" "$expect_dir/$lightness.json" "$lightness" ui.appearance normal
  conclude "$item" "ячейка $lightness обычная, источник светлоты ui.appearance, маркер поколения $GEN в PNG, участки $(basename "$expect_dir")/$lightness.json в допуске; PNG — $item/$lightness.png"
}

item_contrast_cells() {
  local item=contrast-cells dir="$run/contrast-cells" lightness
  failures=()
  log "== $item"
  for lightness in light dark; do
    if ! ui_cmd "$profile_g" "$dir/cell-$lightness" gallery cell "$lightness" increased; then
      failures+=("$lightness: $WHY")
      continue
    fi
    cell_snapshot "$dir" "$lightness-ic" "$expect_dir/$lightness-ic.json" "$lightness" gallery increased
  done
  ui_cmd "$profile_g" "$dir/cell-auto" gallery cell auto auto || failures+=("сброс ячейки: $WHY")
  conclude "$item" "Increase Contrast светлой и тёмной ячеек: участки ChromeBar, обеих Surface, поля и полос вкладок в допуске"
}

item_material() {
  local item=material dir="$run/material" lightness own system="" line
  failures=()
  log "== $item"
  if ! ui_cmd "$profile_g" "$dir/cell" gallery cell auto auto; then
    record "$item" fail "$WHY"
    return 0
  fi
  for lightness in light dark; do
    if ! ui_cmd "$profile_g" "$dir/appearance-$lightness" ui appearance "$lightness" ||
      ! ui_read "$profile_g" "$dir/inspect-$lightness" inspect; then
      failures+=("$lightness: ${WHY:-ui.inspect не прочитан}")
      continue
    fi
    own="$(/usr/bin/perl -MJSON::PP -e 'local $/; my $i = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{ui}{inspect};
      print join(", ", map { $_->{class} } grep { $_->{own} && $_->{visible} } @{$i->{materials}})' \
      <"$dir/inspect-$lightness.stdout")"
    line="$(/usr/bin/perl -MJSON::PP -e 'local $/; my $i = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{ui}{inspect};
      print join(", ", map { "$_->{class} (видим: " . ($_->{visible} ? "да" : "нет") . ")" } grep { !$_->{own} } @{$i->{materials}})' \
      <"$dir/inspect-$lightness.stdout")"
    [[ -z "$own" ]] || failures+=("$lightness: видимые классы материала внутри собственных видов — $own")
    system="${system:+$system; }$lightness: ${line:-нет}"
  done
  conclude "$item" "видимых классов материала в собственных видах нет в обеих темах; системный контейнер заголовка (наблюдение) — $system"
}

field_first_responder() {
  [[ "$(comp "$1" gallery.field first_responder)" == true ]]
}

field_has_value() {
  [[ "$(comp "$1" gallery.field state.text_field.value)" == "$field_want" ]]
}

# Намерение intent_want компонента intent_component с поколением больше intent_after (и значением intent_value,
# если задано) в журнале галереи.
intent_recorded() {
  /usr/bin/perl -MJSON::PP -e '
    my ($file, $component, $intent, $after, $value) = @ARGV;
    open my $fh, "<", $file or exit 2; local $/;
    my $g = JSON::PP->new->utf8->decode(<$fh>)->{ok}{ui}{inspect}{gallery} or exit 1;
    for (@{$g->{intents}}) {
      next unless $_->{component} eq $component && $_->{intent} eq $intent && $_->{generation} > $after;
      next if length $value && (!defined $_->{value} || $_->{value} ne $value);
      exit 0;
    }
    exit 1;
  ' "$1" "$intent_component" "$intent_want" "$intent_after" "$intent_value"
}

field_want=""
intent_component=""
intent_want=""
intent_after=0
intent_value=""
item_text_field() {
  local item=text-field dir="$run/text-field" pid="$owner_pid_g"
  failures=()
  log "== $item"
  if ! ui_cmd "$profile_g" "$dir/reset" gallery reset || ! front_owner "$pid" "$dir" ||
    ! ui_cmd "$profile_g" "$dir/focus" gallery focus field; then
    record "$item" fail "$WHY"
    return 0
  fi
  if ! poll_inspect "$profile_g" "$dir/focused" "$STATE_MS" field_first_responder; then
    record "$item" fail "gallery.focus field: first responder — не field editor поля: $(comp "$dir/focused.stdout" gallery.field first_responder || echo ?)"
    return 0
  fi
  if ! input_ready "$pid"; then
    record "$item" fail "$WHY"
    return 0
  fi
  se_keystroke "$dir" shellio || failures+=("keystroke не отправлен: $(oneline "$dir/keys.log")")
  field_want=shellio
  poll_inspect "$profile_g" "$dir/typed" "$STATE_MS" field_has_value ||
    failures+=("значение поля «$(comp "$dir/typed.stdout" gallery.field state.text_field.value || echo ?)», ожидалось «shellio»")
  intent_after="$(jv "$dir/typed.stdout" ok.ui.inspect.generation || echo 0)"
  intent_component=gallery.field
  intent_want=cancelled
  intent_value=""
  if input_ready "$pid"; then
    se_key "$dir" 53 || failures+=("Escape не отправлен: $(oneline "$dir/keys.log")")
    poll_inspect "$profile_g" "$dir/escaped" "$STATE_MS" intent_recorded ||
      failures+=("после Escape нет намерения cancelled поля в журнале галереи")
  else
    failures+=("$WHY")
  fi
  conclude "$item" "first responder — field editor поля, keystroke «shellio» дал значение «shellio», Escape — намерение cancelled"
}

menu_open() {
  [[ "$(comp "$1" gallery.menu state.menu.open)" == true ]]
}

menu_closed() {
  [[ "$(comp "$1" gallery.menu state.menu.open)" == false ]]
}

# Первый responder среди компонентов ответа ui.inspect: id через запятую или «нет».
responders() {
  /usr/bin/perl -MJSON::PP -e 'local $/; my $i = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{ui}{inspect};
    my @ids = map { $_->{id} } grep { $_->{first_responder} } @{$i->{components}};
    print @ids ? join(",", @ids) : "нет"' <"$1" 2>/dev/null || echo "?"
}

# AXPress кнопки «Ещё» окна владельца через System Events в фоне; задание — в bg_job.
bg_job=""
press_menu() {
  local pid="$1" out="$2"
  osascript -l JavaScript -e "
    const se = Application('System Events');
    const w = se.processes.whose({unixId: $pid})[0].windows[0];
    w.buttons.whose({description: 'Ещё'})[0].actions.byName('AXPress').perform();
    'pressed'" >"$out.stdout" 2>"$out.stderr" &
  bg_job=$!
  bg_jobs+=("$bg_job")
}

# Выход фонового osascript до срока MENU_OSA_S от его старта t0; код — в VALUE, иначе WHY.
await_bg() {
  local job="$1" t0="$2"
  VALUE=""
  while alive "$job"; do
    if (($(now_ms) - t0 >= MENU_OSA_S * 1000)); then
      kill -TERM "$job" 2>/dev/null || true
      WHY="фоновый AXPress не вышел за $MENU_OSA_S с"
      return 1
    fi
    sleep "$POLL"
  done
  if wait "$job"; then
    VALUE=0
  else
    VALUE=$?
  fi
}

item_menu_tracking() {
  local item=menu-tracking dir="$run/menu-tracking" pid="$owner_pid_g" before t0 t1 ui_ms waiting
  failures=()
  log "== $item"
  if ! ui_cmd "$profile_g" "$dir/reset" gallery reset || ! front_owner "$pid" "$dir" ||
    ! ui_read "$profile_g" "$dir/before" inspect; then
    record "$item" fail "${WHY:-ui.inspect не прочитан}"
    return 0
  fi
  before="$(responders "$dir/before.stdout")"
  intent_after="$(jv "$dir/before.stdout" ok.ui.inspect.generation || echo 0)"
  if ! input_ready "$pid"; then
    record "$item" fail "$WHY"
    return 0
  fi
  t0="$(now_ms)"
  press_menu "$pid" "$dir/press-1"
  if poll_inspect "$profile_g" "$dir/open-1" "$STATE_MS" menu_open; then
    t1="$(now_ms)"
    if ui_read "$profile_g" "$dir/during" window; then
      ui_ms=$(($(now_ms) - t1))
      ((ui_ms <= MENU_UI_MS)) || failures+=("ui.window во время отслеживания ответил за $ui_ms мс, дольше $MENU_UI_MS мс")
    else
      failures+=("ui.window во время отслеживания не ответил: $(oneline "$dir/during.stdout")")
    fi
    if input_ready "$pid"; then
      se_key "$dir" 125 && se_key "$dir" 36 || failures+=("Down и Return не отправлены: $(oneline "$dir/keys.log")")
      intent_component=gallery.menu
      intent_want=chosen
      intent_value=copy
      poll_inspect "$profile_g" "$dir/chosen" "$STATE_MS" intent_recorded ||
        failures+=("после Down и Return нет намерения chosen «copy» в журнале галереи")
      menu_closed "$dir/chosen.stdout" || failures+=("меню открыто после выбора пункта")
    else
      failures+=("$WHY")
    fi
  else
    failures+=("меню не открылось за $STATE_MS мс после AXPress: $(oneline "$dir/press-1.stderr")")
  fi
  if await_bg "$bg_job" "$t0"; then
    [[ "$VALUE" == 0 ]] || failures+=("фоновый AXPress вышел с кодом $VALUE: $(oneline "$dir/press-1.stderr")")
  else
    failures+=("$WHY")
  fi
  t0="$(now_ms)"
  press_menu "$pid" "$dir/press-2"
  if poll_inspect "$profile_g" "$dir/open-2" "$STATE_MS" menu_open; then
    waiting="нет"
    alive "$bg_job" && waiting="да"
    if input_ready "$pid"; then
      se_key "$dir" 53 || failures+=("Escape не отправлен: $(oneline "$dir/keys.log")")
      poll_inspect "$profile_g" "$dir/escaped" "$STATE_MS" menu_closed || failures+=("меню открыто после Escape")
      [[ "$(responders "$dir/escaped.stdout")" == "$before" ]] ||
        failures+=("first responder после Escape — $(responders "$dir/escaped.stdout"), до открытия — $before")
    else
      failures+=("$WHY")
    fi
  else
    failures+=("меню не открылось повторно за $STATE_MS мс: $(oneline "$dir/press-2.stderr")")
  fi
  if await_bg "$bg_job" "$t0"; then
    [[ "$VALUE" == 0 ]] || failures+=("повторный фоновый AXPress вышел с кодом $VALUE: $(oneline "$dir/press-2.stderr")")
  else
    failures+=("$WHY")
  fi
  conclude "$item" "AXPress открыл меню, ui.window во время отслеживания ответил за ${ui_ms:-?} мс; Down и Return — chosen «copy»; повторное открытие и Escape (AXPress ещё ждал: ${waiting:-?}) закрыли меню, first responder прежний ($before), фоновые osascript вышли до срока"
}

# Элементы окна процесса pid по System Events: JSON с заголовком окна и списком элементов (роль, подроль,
# заголовок, описание, AXSelected, AXExpanded, AXHelp, описание ближайшей AXTabGroup), обход до глубины 6.
ax_dump() {
  osascript -l JavaScript -e "
    const se = Application('System Events');
    const w = se.processes.whose({unixId: $1})[0].windows[0];
    function attr(e, name) { try { return e.attributes.byName(name).value(); } catch (x) { return null; } }
    function prop(f) { try { const v = f(); return v === undefined ? null : v; } catch (x) { return null; } }
    const out = [];
    function walk(e, depth, group) {
      const role = prop(() => e.role());
      const description = prop(() => e.description());
      out.push({ role: role, subrole: prop(() => e.subrole()), title: prop(() => e.name()),
        description: description, selected: attr(e, 'AXSelected'), expanded: attr(e, 'AXExpanded'),
        help: attr(e, 'AXHelp'), group: group });
      if (depth >= 6) return;
      let kids = [];
      try { kids = e.uiElements(); } catch (x) {}
      for (const k of kids) walk(k, depth + 1, role === 'AXTabGroup' ? description : group);
    }
    let kids = [];
    try { kids = w.uiElements(); } catch (x) {}
    for (const k of kids) walk(k, 1, null);
    JSON.stringify({ title: prop(() => w.name()), elements: out });"
}

# Сверка AX: компоненты ui.inspect с ролью против элементов System Events и ожиданий фикстуры. Строки нарушений
# в stdout, пусто — совпало.
ax_compare() {
  /usr/bin/perl -COEA -Mutf8 -MJSON::PP -e '
    my ($inspect, $ax, $workspace, $choice) = @ARGV;
    my $read = sub { open my $fh, "<", $_[0] or die; local $/; JSON::PP->new->utf8->decode(<$fh>) };
    my $i = $read->($inspect)->{ok}{ui}{inspect};
    my $a = $read->($ax);
    my %fixture = (
      "chrome.workspace" => ["AXButton", "Воркспейс: $workspace"],
      "chrome.search" => ["AXGroup", "Поиск и команды"],
      "chrome.zone.left" => ["AXButton", "Навигация"],
      "chrome.zone.bottom" => ["AXButton", "Нижняя зона"],
      "chrome.zone.right" => ["AXButton", "Правая зона"],
      "chrome.appearance" => ["AXButton", "Оформление: $choice"],
      "gallery.field" => ["AXTextField", "Фильтр галереи"],
      "gallery.menu" => ["AXButton", "Ещё"],
      "gallery.tabs.side" => ["AXTabGroup", "Вкладки боковой зоны"],
      "gallery.tabs.side.tab.term" => ["AXRadioButton", "zsh", "Вкладки боковой зоны"],
      "gallery.tabs.side.tab.build" => ["AXRadioButton", "Сборка", "Вкладки боковой зоны"],
      "gallery.tabs.side.split" => ["AXButton", "Разделить: одна над другой", "Вкладки боковой зоны"],
      "gallery.tabs.side.zoom" => ["AXButton", "Развернуть панель", "Вкладки боковой зоны"],
      "gallery.tabs.side.menu" => ["AXButton", "Действия панели", "Вкладки боковой зоны"],
      "gallery.tabs.center" => ["AXTabGroup", "Вкладки центра"],
      "gallery.tabs.center.split" => ["AXButton", "Разделить: части рядом", "Вкладки центра"],
      "gallery.tabs.half" => ["AXTabGroup", "Вкладки половины центра"],
      "gallery.tabs.wide" => ["AXTabGroup", "Вкладки широкого центра"],
      "gallery.tabs.wide.tab.term" => ["AXRadioButton", "zsh", "Вкладки широкого центра"],
      "gallery.tabs.wide.tab.build" => ["AXRadioButton", "Сборка", "Вкладки широкого центра"],
    );
    my %seen;
    my $norm = sub { my $v = shift; return "нет" unless defined $v; JSON::PP::is_bool($v) ? ($v ? "true" : "false") : "$v" };
    for my $c (@{$i->{components}}) {
      my $want = $fixture{$c->{id}} or next;
      $seen{$c->{id}} = 1;
      my ($role, $label, $group) = @$want;
      my $got_role = $c->{role} // "нет";
      my $got_label = $c->{label} // "нет";
      print "$c->{id}: ui.inspect роль $got_role, имя «${got_label}», фикстура — $role «${label}»\n"
        if $got_role ne $role || $got_label ne $label;
      my ($el) = grep { ($_->{role} // "") eq $got_role && ($_->{description} // "") eq $got_label
        && (!defined $group || ($_->{group} // "") eq $group) } @{$a->{elements}};
      my $where = defined $group ? " в «${group}»" : "";
      if (!$el) { print "$c->{id}: System Events не нашёл элемент $got_role «${got_label}»$where\n"; next }
      if (($c->{help} // "") ne "" || ($el->{help} // "") ne "") {
        my ($mine, $theirs) = ($c->{help} // "нет", $el->{help} // "нет");
        print "$c->{id}: accessibilityHelp в ui.inspect «${mine}», у System Events «${theirs}»\n" if $mine ne $theirs;
      }
      for my $f (qw(selected expanded)) {
        my $mine = $norm->($c->{$f});
        my $theirs = $norm->($el->{$f});
        $theirs = "нет" if $mine eq "нет" && $theirs eq "false";
        print "$c->{id}: $f в ui.inspect $mine, у System Events $theirs\n" if $mine ne $theirs;
      }
    }
    print "$_: нет в ui.inspect\n" for grep { !$seen{$_} } sort keys %fixture;
  ' "$1" "$2" "$3" "$4"
}

appearance_focused() {
  [[ "$(comp "$1" chrome.appearance first_responder)" == true ]]
}

lightness_dark() {
  [[ "$(jv "$1" ok.ui.window.lightness.value)" == dark && "$(jv "$1" ok.ui.window.appearance_choice)" == dark ]]
}

item_ax() {
  local item=ax dir="$run/ax" pid="$owner_pid_g" workspace mismatch ax_title
  failures=()
  log "== $item"
  if ! ui_cmd "$profile_g" "$dir/reset" gallery reset || ! ui_cmd "$profile_g" "$dir/appearance" ui appearance light ||
    ! front_owner "$pid" "$dir"; then
    record "$item" fail "$WHY"
    return 0
  fi
  if ! ui_read "$profile_g" "$dir/window" window || ! ui_read "$profile_g" "$dir/inspect" inspect; then
    record "$item" fail "ui.window или ui.inspect не прочитан"
    return 0
  fi
  if ! ax_dump "$pid" >"$dir/system-events.json" 2>"$dir/system-events.stderr"; then
    record "$item" fail "System Events не обошёл окно pid $pid: $(oneline "$dir/system-events.stderr")"
    return 0
  fi
  workspace="$(comp "$dir/inspect.stdout" chrome state.chrome.workspace_text || echo ?)"
  mismatch="$(ax_compare "$dir/inspect.stdout" "$dir/system-events.json" "$workspace" светлое 2>&1)" ||
    mismatch="сверка AX не выполнена: $mismatch"
  printf '%s\n' "$mismatch" >"$dir/compare.txt"
  [[ -z "$mismatch" ]] || failures+=("$(tr '\n' ';' <"$dir/compare.txt" | sed 's/;$//')")
  ax_title="$(jv "$dir/system-events.json" title || echo ?)"
  [[ "$ax_title" == "$(jv "$dir/window.stdout" ok.ui.window.title)" ]] ||
    failures+=("системный заголовок у System Events «${ax_title}», в ui.window «$(jv "$dir/window.stdout" ok.ui.window.title)»")
  # Клавиатура ChromeBar: Tab от окна — кнопка оформления, Space — следующий выбор (светлое → тёмное).
  if input_ready "$pid"; then
    se_key "$dir" 48 || failures+=("Tab не отправлен: $(oneline "$dir/keys.log")")
    if poll_inspect "$profile_g" "$dir/tab" "$STATE_MS" appearance_focused; then
      if input_ready "$pid"; then
        se_key "$dir" 49 || failures+=("Space не отправлен: $(oneline "$dir/keys.log")")
        poll_window "$profile_g" "$dir/space" "$STATE_MS" lightness_dark ||
          failures+=("Space на кнопке оформления: ячейка $(cell_line "$dir/space.stdout"), выбор $(jv "$dir/space.stdout" ok.ui.window.appearance_choice || echo ?), ожидалось тёмное")
      else
        failures+=("$WHY")
      fi
    else
      failures+=("Tab: first responder — $(responders "$dir/tab.stdout"), ожидалась кнопка оформления")
    fi
  else
    failures+=("$WHY")
  fi
  conclude "$item" "System Events от окна pid $pid дошёл до частей ChromeBar, поля, кнопки меню и полос вкладок (группы, вкладки с выбором, кнопки с accessibilityHelp): роль, имя, выбор и раскрытие совпали с фикстурой и ui.inspect, заголовок окна «${ax_title}»; Tab — кнопка оформления, Space — тёмное"
}

# --- полосы вкладок (задача 11) ---

# Значение поля полосы strip_id в ответе ui.inspect.
strip() {
  comp "$1" "$strip_id" "state.tab_strip.$2"
}

# Выбранная вкладка полосы strip_id — strip_want и видима целиком.
strip_selected() {
  [[ "$(strip "$1" selected)" == "$strip_want" &&
    "$(jv "$1" "ok.ui.inspect.components[id=$strip_id].state.tab_strip.tabs[id=$strip_want].fully_visible")" == true ]]
}

strip_focused() {
  [[ "$(comp "$1" "$strip_id" first_responder)" == true ]]
}

strip_menu_open() {
  [[ "$(comp "$1" "$strip_id.menu" state.menu.open)" == true ]]
}

strip_menu_closed() {
  [[ "$(comp "$1" "$strip_id.menu" state.menu.open)" == false ]]
}

# Последняя вкладка полосы strip_id в ответе ui.inspect.
strip_last() {
  /usr/bin/perl -MJSON::PP -e 'local $/; my ($id) = @ARGV; my $i = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{ui}{inspect};
    my ($c) = grep { $_->{id} eq $id } @{$i->{components}} or exit 1;
    print $c->{state}{tab_strip}{tabs}[-1]{id}' "$strip_id" <"$1"
}

# Геометрия и признаки всех полос по ответу ui.inspect: нарушения построчно, пусто — всё сходится.
strip_geometry() {
  /usr/bin/perl -COEA -Mutf8 -MJSON::PP -e '
    local $/; my $i = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{ui}{inspect};
    my %c = map { $_->{id} => $_ } @{$i->{components}};
    my %regime = ("side" => "scroll", "center" => "scroll", "half" => "in_menu", "wide" => "visible");
    my $inside = sub { my ($a, $b) = @_; $a->{x} >= $b->{x} - 0.5 && $a->{y} >= $b->{y} - 0.5
      && $a->{x} + $a->{width} <= $b->{x} + $b->{width} + 0.5 && $a->{y} + $a->{height} <= $b->{y} + $b->{height} + 0.5 };
    my $cross = sub { my ($a, $b) = @_; $a->{x} < $b->{x} + $b->{width} - 0.5 && $b->{x} < $a->{x} + $a->{width} - 0.5
      && $a->{y} < $b->{y} + $b->{height} - 0.5 && $b->{y} < $a->{y} + $a->{height} - 0.5 };
    for my $name (qw(side center half wide)) {
      my $id = "gallery.tabs.$name";
      my $s = $c{$id} or do { print "$id: нет в ui.inspect\n"; next };
      my $t = $s->{state}{tab_strip};
      print "$id: режим $t->{regime}, фикстура — $regime{$name}\n" if $t->{regime} ne $regime{$name};
      my @tabs = grep { ($_->{parent} // "") eq $id && $_->{class} eq "Tab" && $_->{visible} && $_->{frame}{width} > 0 }
        @{$i->{components}};
      print "$id: нет видимых вкладок\n" unless @tabs;
      my @buttons = map { "$id.$_->{id}" } @{$t->{buttons}};
      print "$id: кнопок " . scalar(@buttons) . ", ожидалось 3\n" unless @buttons == 3;
      for my $bid (@buttons) {
        my $b = $c{$bid} or do { print "$bid: нет в ui.inspect\n"; next };
        print "$bid: кадр кнопки вне полосы\n" unless $inside->($b->{frame}, $s->{frame});
        for my $tab (@tabs) { print "$bid: кадр пересекает вкладку $tab->{id}\n" if $cross->($b->{frame}, $tab->{frame}) }
        print "$bid: AX-имя пусто\n" unless length($b->{label} // "");
        print "$bid: accessibilityHelp «" . ($b->{help} // "нет") . "», ожидалось «Клавиша не назначена»\n"
          if ($b->{help} // "") ne "Клавиша не назначена";
      }
      if ($name eq "half") {
        print "$id: нагрузка видна в полосе в режиме «в меню»\n" if $t->{payload_visible};
        my ($head) = grep { !$_->{separator} } @{$c{"$id.menu"}{state}{menu}{items} // []};
        print "$id.menu: заголовок меню «" . ($head->{title} // "нет") . "», нагрузка — «$t->{payload}»\n"
          if !$head || $head->{title} ne $t->{payload};
      }
      print "$id: нагрузка скрыта в режиме «видна»\n" if $name eq "wide" && !$t->{payload_visible};
    }
    my ($side, $center) = map { $c{"gallery.tabs.$_"}{state}{tab_strip} } qw(side center);
    if ($side && $center) {
      my @si = grep { defined $_->{icon} } @{$side->{tabs}};
      my @ci = grep { !defined $_->{icon} } @{$center->{tabs}};
      print "боковая полоса: значок типа у " . scalar(@si) . " вкладок\n" if @si;
      print "полоса центра: без значка типа " . scalar(@ci) . " вкладок\n" if @ci;
      my ($ss) = map { $_->{symbol} } grep { $_->{id} eq "split" } @{$side->{buttons}};
      my ($cs) = map { $_->{symbol} } grep { $_->{id} eq "split" } @{$center->{buttons}};
      print "глиф «Разделить» боковой и центральной полосы одинаков: " . ($ss // "нет") . "\n"
        if ($ss // "") eq ($cs // "");
    }' <"$1" 2>&1
}

# AXPress кнопки «Действия панели» в группе вкладок с описанием group окна pid через System Events в фоне;
# задание — в bg_job.
press_strip_menu() {
  local pid="$1" group="$2" out="$3"
  osascript -l JavaScript -e "
    const se = Application('System Events');
    const w = se.processes.whose({unixId: $pid})[0].windows[0];
    function prop(f) { try { return f(); } catch (x) { return null; } }
    function find(e, depth, test) {
      let kids = [];
      try { kids = e.uiElements(); } catch (x) {}
      for (const k of kids) {
        if (test(k)) return k;
        if (depth < 6) { const hit = find(k, depth + 1, test); if (hit) return hit; }
      }
      return null;
    }
    const g = find(w, 1, k => prop(() => k.role()) === 'AXTabGroup' && prop(() => k.description()) === '$group');
    if (!g) throw new Error('нет группы $group');
    const b = find(g, 1, k => prop(() => k.role()) === 'AXButton' && prop(() => k.description()) === 'Действия панели');
    if (!b) throw new Error('нет кнопки «Действия панели» в $group');
    b.actions.byName('AXPress').perform();
    'pressed'" >"$out.stdout" 2>"$out.stderr" &
  bg_job=$!
  bg_jobs+=("$bg_job")
}

strip_id=""
strip_want=""
item_tabstrip_regimes() {
  local item=tabstrip-regimes dir="$run/tabstrip-regimes" pid="$owner_pid_g" bad last t0 next
  failures=()
  log "== $item"
  if ! ui_cmd "$profile_g" "$dir/reset" gallery reset || ! ui_read "$profile_g" "$dir/fixture" inspect; then
    record "$item" fail "${WHY:-ui.inspect не прочитан}"
    return 0
  fi
  bad="$(strip_geometry "$dir/fixture.stdout")"
  [[ -z "$bad" ]] || failures+=("$(printf '%s' "$bad" | tr '\n' ';' | sed 's/;$//; s/;/; /g')")
  strip_id=gallery.tabs.side
  last="$(strip_last "$dir/fixture.stdout")" || last=""
  if [[ -z "$last" ]]; then
    failures+=("у боковой полосы нет вкладок в ui.inspect")
  elif ui_cmd "$profile_g" "$dir/select-last" gallery tabs select side 7; then
    strip_want="$last"
    poll_inspect "$profile_g" "$dir/last" "$STATE_MS" strip_selected ||
      failures+=("прокрутка: выбор $(strip "$dir/last.stdout" selected || echo ?), последняя вкладка $last не выбрана или не видна целиком")
    bad="$(strip_geometry "$dir/last.stdout")"
    [[ -z "$bad" ]] || failures+=("после выбора последней: $(printf '%s' "$bad" | tr '\n' ';' | sed 's/;$//; s/;/; /g')")
  else
    failures+=("$WHY")
  fi
  # Меню «…» полосы в режиме «в меню»: заголовок — полная нагрузка.
  strip_id=gallery.tabs.half
  if ! ui_cmd "$profile_g" "$dir/reset-menu" gallery reset || ! front_owner "$pid" "$dir" || ! input_ready "$pid"; then
    failures+=("меню «…»: $WHY")
  else
    t0="$(now_ms)"
    press_strip_menu "$pid" "Вкладки половины центра" "$dir/press"
    if poll_inspect "$profile_g" "$dir/menu-open" "$STATE_MS" strip_menu_open; then
      bad="$(strip_geometry "$dir/menu-open.stdout" | grep 'menu: заголовок' || true)"
      [[ -z "$bad" ]] || failures+=("открытое меню: $bad")
      if input_ready "$pid"; then
        se_key "$dir" 53 || failures+=("Escape не отправлен: $(oneline "$dir/keys.log")")
        poll_inspect "$profile_g" "$dir/menu-closed" "$STATE_MS" strip_menu_closed || failures+=("меню «…» открыто после Escape")
      else
        failures+=("$WHY")
      fi
    else
      failures+=("меню «…» полосы half не открылось за $STATE_MS мс после AXPress: $(oneline "$dir/press.stderr")")
    fi
    if await_bg "$bg_job" "$t0"; then
      [[ "$VALUE" == 0 ]] || failures+=("фоновый AXPress меню «…» вышел с кодом $VALUE: $(oneline "$dir/press.stderr")")
    else
      failures+=("$WHY")
    fi
  fi
  # Стрелки при фокусе в боковой полосе.
  strip_id=gallery.tabs.side
  if ! ui_cmd "$profile_g" "$dir/reset-keys" gallery reset || ! front_owner "$pid" "$dir" ||
    ! ui_cmd "$profile_g" "$dir/focus" gallery focus tabs; then
    failures+=("стрелки: $WHY")
  elif ! poll_inspect "$profile_g" "$dir/focused" "$STATE_MS" strip_focused; then
    failures+=("gallery.focus tabs: боковая полоса не first responder ($(responders "$dir/focused.stdout"))")
  else
    next="$(jv "$dir/focused.stdout" "ok.ui.inspect.components[id=$strip_id].state.tab_strip.tabs[1].id" || echo ?)"
    strip_want="$next"
    if input_ready "$pid" && se_key "$dir" 124; then
      poll_inspect "$profile_g" "$dir/right" "$STATE_MS" strip_selected ||
        failures+=("Right: выбор $(strip "$dir/right.stdout" selected || echo ?), ожидалась $next")
    else
      failures+=("Right: ${WHY:-не отправлена: $(oneline "$dir/keys.log")}")
    fi
    strip_want="$(jv "$dir/focused.stdout" "ok.ui.inspect.components[id=$strip_id].state.tab_strip.tabs[0].id" || echo ?)"
    if input_ready "$pid" && se_key "$dir" 123; then
      poll_inspect "$profile_g" "$dir/left" "$STATE_MS" strip_selected ||
        failures+=("Left: выбор $(strip "$dir/left.stdout" selected || echo ?), ожидалась $strip_want")
    else
      failures+=("Left: ${WHY:-не отправлена: $(oneline "$dir/keys.log")}")
    fi
  fi
  ui_cmd "$profile_g" "$dir/reset-end" gallery reset || failures+=("сброс: $WHY")
  conclude "$item" "режимы side и center — scroll, half — in_menu, wide — visible; кнопки внутри полос и не пересекают вкладки во всех режимах, AX-имя действия и «Клавиша не назначена»; открытое меню «…» полосы half несёт полную нагрузку в заголовке; последняя вкладка $last выбрана и видима; боковая и центральная полосы различаются значком типа и глифом «Разделить»; Right и Left меняют выбор"
}

item_tab_underline() {
  local item=tab-underline dir="$run/tab-underline" lightness contrast name tab notes=""
  failures=()
  log "== $item"
  if ! ui_cmd "$profile_g" "$dir/reset" gallery reset || ! ui_read "$profile_g" "$dir/fixture" inspect; then
    record "$item" fail "${WHY:-ui.inspect не прочитан}"
    return 0
  fi
  tab="$(comp "$dir/fixture.stdout" gallery.tabs.side state.tab_strip.selected)" || tab=""
  if [[ -z "$tab" ]]; then
    record "$item" fail "у боковой полосы нет выбранной вкладки в ui.inspect"
    return 0
  fi
  for lightness in light dark; do
    for contrast in normal increased; do
      name="$lightness-$contrast"
      if ! ui_cmd "$profile_g" "$dir/cell-$name" gallery cell "$lightness" "$contrast"; then
        failures+=("$name: $WHY")
        continue
      fi
      snapshot "$profile_g" "$dir" "$name" "underline:gallery.tabs.side.tab.$tab:tone_navigation" "$GEN"
      case "$snap_code" in
        0) notes="${notes:+$notes, }$name $(jv "$dir/$name.report.json" thickness_points || echo ?) из $(jv "$dir/$name.report.json" expected_points || echo ?) точ." ;;
        3) failures+=("$name: линия вне допуска — $snap_reason, отчёт — $(basename "$dir")/$name.report.json") ;;
        *) failures+=("$name: $snap_reason") ;;
      esac
    done
  done
  ui_cmd "$profile_g" "$dir/cell-auto" gallery cell auto auto || failures+=("сброс ячейки: $WHY")
  conclude "$item" "линия под выбранной вкладкой $tab боковой полосы в подложке с фокусом: оба края найдены, толщина — --focus-h ячейки ±0,5 точки, цвет середины в допуске ($notes)"
}

# Запись журнала анимаций ui.stats за поколение anim_gen у компонента anim_component: «путь добавлено».
anim_entry() {
  /usr/bin/perl -MJSON::PP -e 'local $/; my ($gen, $id) = @ARGV;
    my $s = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{ui}{stats};
    my @e = grep { $_->{generation} == $gen && $_->{component} eq $id } @{$s->{animations}} or exit 1;
    my $added = 0; $added += $_->{added} for @e;
    my %paths = map { $_->{path} => 1 } @e;
    print join("+", sort keys %paths), " $added"' "$anim_gen" "$anim_component" <"$1"
}

anim_present() {
  anim_entry "$1" >/dev/null
}

anim_gen=0
anim_component=""
item_reduce_motion() {
  local item=reduce-motion dir="$run/reduce-motion" motion entry last keys reduced="" control=""
  failures=()
  log "== $item"
  anim_component=gallery.tabs.side
  strip_id=gallery.tabs.side
  for motion in reduced normal; do
    if ! ui_cmd "$profile_g" "$dir/reset-$motion" gallery reset || ! ui_cmd "$profile_g" "$dir/motion-$motion" gallery motion "$motion" ||
      ! ui_read "$profile_g" "$dir/before-$motion" inspect; then
      failures+=("$motion: ${WHY:-ui.inspect не прочитан}")
      continue
    fi
    last="$(strip_last "$dir/before-$motion.stdout")" || last=""
    if [[ -z "$last" ]] || ! ui_cmd "$profile_g" "$dir/select-$motion" gallery tabs select side 7; then
      failures+=("$motion: ${WHY:-у боковой полосы нет вкладок}")
      continue
    fi
    anim_gen="$GEN"
    if ! poll_stats "$profile_g" "$dir/stats-$motion" "$STATE_MS" anim_present; then
      failures+=("$motion: в журнале анимаций ui.stats нет записи боковой полосы за поколение $anim_gen")
      continue
    fi
    entry="$(anim_entry "$dir/stats-$motion.stdout")"
    strip_want="$last"
    poll_inspect "$profile_g" "$dir/after-$motion" "$STATE_MS" strip_selected ||
      failures+=("$motion: выбранная вкладка $last не видна целиком после выбора")
    keys="$(comp "$dir/after-$motion.stdout" gallery.tabs.side animation_keys || echo ?)"
    if [[ "$motion" == reduced ]]; then
      reduced="путь ${entry% *}, добавлено ${entry#* }, ключей анимаций у полосы $keys"
      [[ "${entry% *}" == instant && "${entry#* }" == 0 ]] ||
        failures+=("reduced: журнал за поколение $anim_gen — путь ${entry% *}, добавлено анимаций ${entry#* }, ожидался путь без анимации")
      [[ "$(strip "$dir/after-$motion.stdout" offset)" == "$(strip "$dir/after-$motion.stdout" max_offset)" ]] ||
        failures+=("reduced: смещение $(strip "$dir/after-$motion.stdout" offset || echo ?), конечное $(strip "$dir/after-$motion.stdout" max_offset || echo ?)")
    else
      control="путь ${entry% *}, добавлено ${entry#* }"
      [[ "${entry% *}" == animated ]] && fcmp "${entry#* }" ">=" 1 ||
        failures+=("контроль normal: журнал за поколение $anim_gen — путь ${entry% *}, добавлено ${entry#* }, ожидался анимированный путь")
    fi
  done
  ui_cmd "$profile_g" "$dir/reset-end" gallery reset || failures+=("сброс: $WHY")
  conclude "$item" "motion reduced, выбор последней вкладки боковой полосы: $reduced, конечное смещение достигнуто, вкладка видима; контроль motion normal: $control"
}

item_single_instance() {
  local item=single-instance bad
  log "== $item"
  if [[ -z "$single_result" ]]; then
    record "$item" fail "single-instance.sh в этом прогоне не запускался: нет --single-instance <result.json>"
    return 0
  fi
  if [[ ! -f "$single_result" ]]; then
    record "$item" fail "нет result.json сценария single-instance.sh: $single_result"
    return 0
  fi
  cp "$single_result" "$run/$item/result.json"
  bad="$(/usr/bin/perl -COEA -Mutf8 -MJSON::PP -e 'local $/; my $r = JSON::PP->new->utf8->decode(<STDIN>);
    print "ошибка исполнения: $r->{error}; " if defined $r->{error};
    print join("; ", map { "$_->{item}: $_->{status}" } grep { $_->{required} && $_->{status} ne "pass" } @{$r->{items}})' \
    <"$single_result" 2>&1)" || bad="result.json не разобран: $bad"
  if [[ -n "$bad" ]]; then
    record "$item" fail "$bad (каталог — $(dirname "$single_result"))"
  else
    record "$item" pass "все обязательные пункты single-instance.sh pass, включая оба направления two-owners-front (каталог — $(dirname "$single_result"))"
  fi
}

prepare
item_single_instance
preconditions
item_first_front
item_window
item_mirror
item_close_reopen
cycle_reopen
item_quit
item_gui_quit_pid
item_startup_warm
launch_gallery
item_idle
cycle_gallery b б strict gallery rebuild
item_window_cycle
item_snapshot snapshot-light light
item_snapshot snapshot-dark dark
item_contrast_cells
item_material
item_text_field
item_menu_tracking
item_ax
item_tabstrip_regimes
item_tab_underline
item_reduce_motion
record system-appearance not_run "переключение системного оформления и Increase Contrast меняет глобальные настройки пользователя; путь уведомления покрыт модульным тестом"
record voiceover not_run "речь VoiceOver автоматика не слышит; чтение AX — пункт ax"
record startup-cold not_run "нет подтверждённого протокола холодных кэшей: сброс требует sudo purge или перезагрузки"
completed=1
