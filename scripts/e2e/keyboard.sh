#!/usr/bin/env bash
# Сценарий make e2e «клавиатура» (P1.6): пункты матрицы приёмки плана navigation-keymap на подписанном бандле и
# временных профилях.
#
#   keyboard.sh [<бандл>]
#
# Бандл по умолчанию — target/bundle/shellio.app, его собирает и проверяет цель make e2e. shellioctl сценарий
# собирает сам (release, --locked). Профили — новые каталоги /tmp/shellio-e2e.*. Подготовка — штатный CLI:
# воркспейс, проект, рабочая область с терминалами в центре, слева, справа и снизу; каждая часть не меньше min_size, заглушка
# «Развернуть» в сцене — ошибка исполнения.
#
# Каталог прогона — target/e2e/keyboard/<время>-<pid>/: result.json (пункт, обязательность, статус, причина, путь
# сырого вывода относительно каталога прогона), журнал сценария log и сырой вывод каждого пункта (JSON query.* и
# ui.*, журнал клавиш keys.log) в подкаталоге с его именем. Пробы предусловий пишут в preconditions/, уборка — в
# cleanup/.
#
# Предусловия, их отсутствие — ошибка исполнения «предусловие: …» до первого пункта: открытый сеанс GUI,
# разблокированный экран, запущенный Finder, бинарь бандла не запущен, запуск из процесса-потомка приложения
# терминала; у терминала-предка Accessibility с Automation для System Events; латинский источник ввода; AeroSpace
# и другие оконные менеджеры выключены. Снимков сценарий не делает, Screen Recording ему не нужен.
#
# Клавиши идут владельцу на переднем плане кодами (key code) через System Events: аккорд от источника ввода не
# зависит. Свидетель клавиши — новая запись ui.receipts с источником key: «ровно один раз» судится по числу записей
# с номером больше снятого до нажатия, исход действия — по чтениям query.layout, ui.window, ui.mode и ui.history.
# Клавиша, которая ничего не должна сделать, судится после следующей клавиши с записью: без неё пустое кольцо
# значило бы только «ещё не дошла».
#
# Пункт, который меняет раскладку, идёт в своей рабочей области (fresh_bench): отказ одного пункта не портит
# сцену следующего. Команда ядра с клавиши судится ещё и одной дельтой events --since от ревизии до нажатия.
#
# Рубежи накопительные: пункт, чей срез задачи 10 ещё не выполнен, получает not_run «пункт среза N» и необязателен.
#
# Сигналы и уборка адресуют только PID, полученные через hello сокетов прогона и сверенные с бинарём бандла
# прогона. Внешние условия опрашиваются с крайним сроком, фиксированных пауз вместо условия нет.
#
# Выход — после записи result.json и уборки: 0 — все обязательные пункты pass, 1 — ошибка исполнения, 3 — ошибки
# исполнения нет, но хотя бы один обязательный пункт не pass. Необязательные пункты на код не влияют.
set -Eeuo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
cargo="${CARGO:-$HOME/.cargo/bin/cargo}"

FINDER_ID="com.apple.finder"
LSAPPINFO=/usr/bin/lsappinfo
# Оконные менеджеры, которые переставляют окна сами (P0.2).
WINDOW_MANAGERS=(AeroSpace yabai Amethyst)
# Латинские источники ввода: keystroke набирает текст по раскладке.
LATIN_LAYOUTS=(com.apple.keylayout.ABC com.apple.keylayout.US com.apple.keylayout.British
  com.apple.keylayout.British-PC)

# Сроки в секундах и миллисекундах.
OWNER_START_S=20
EXIT_S=10
KILL_S=5
SECOND_LAUNCH_S=20
MIRROR_MS=3000
POLL=0.1
CTL_TIMEOUT_MS=2000

# Окно обычного режима (план P1.4, «Матрица приёмки»).
WINDOW_W=1180
WINDOW_H=720
# Ширина левой и правой зон подготовки: центру остаётся место на две части не уже min_size терминала.
SIDE_W=240

# Пункты матрицы по порядку плана и срез задачи 10, который делает пункт обязательным; 0 — необязательный.
ITEMS=(focus-hjkl list-owns-keys move-and-split wrong-axis resize-zoom tabs zones-toggle appearance-cycle
  window-quit field-input exit-chord exit-chord-cancel history filter hot-reload hot-reload-menu key-and-menu-once
  relative-resize-cli bad-line bad-file cyrillic-physical exit-chord-hid)
STAGE=(1 3 2 3 3 2 3 1 4 4 5 5 4 4 5 5 4 4 5 5 5 0)
# Срез, до которого сценарий доведён: пункты более поздних срезов не судятся.
DONE_STAGE=2

# Коды клавиш (kVK_ANSI_*).
K_H=4
K_J=38
K_K=40
K_L=37
K_W=13
K_T=17
K_V=9
K_S=1
K_1=18
K_2=19
K_3=20

REQUIRED=()
STATUS=()
REASON=()
for i in "${!ITEMS[@]}"; do
  REQUIRED[i]=0
  if ((STAGE[i] > 0 && STAGE[i] <= DONE_STAGE)); then
    REQUIRED[i]=1
  fi
  STATUS[i]=not_run
  REASON[i]="не выполнен: прогон остановлен раньше этого пункта"
done

bundle_arg=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    -*)
      echo "usage: $0 [<бандл>]" >&2
      exit 2
      ;;
    *)
      [[ -z "$bundle_arg" ]] || {
        echo "usage: $0 [<бандл>]" >&2
        exit 2
      }
      bundle_arg="$1"
      shift
      ;;
  esac
done

run="$root/target/e2e/keyboard/$(date +%Y%m%d-%H%M%S)-$$"
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
terminal_pid=""
terminal_id=""
profiles=()
known_pids=()
open_jobs=()
bg_jobs=()
failures=()

# Значения, которые функции отдают через глобальные переменные: $(…) потерял бы known_pids и задания.
HELLO_PID=""
OWNER_PID=""
NEW_PROFILE=""
WHY=""
GEN=""
REV=""
LAST=""
EVENTS=""
OUTCOME=""

# Профиль основной серии пунктов: воркспейс, рабочая область A и её терминалы по зонам.
profile=""
owner_pid=""
ws=""
project=""
bench_n=0
wb_a=""
p_center=""
p_left=""
p_right=""
p_bottom=""

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

# Обязательность пункта по ходу прогона: set_required <пункт> 0|1.
set_required() {
  local i
  for i in "${!ITEMS[@]}"; do
    if [[ "${ITEMS[i]}" == "$1" ]]; then
      REQUIRED[i]="$2"
      return 0
    fi
  done
  fatal "неизвестный пункт $1"
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
    printf '  "scenario": "keyboard",\n'
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

# --- передний план и терминал ---

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

# Значение поля компонента id в ответе ui.inspect.
comp() {
  jv "$1" "ok.ui.inspect.components[id=$2].$3"
}
# --- сборка и уборка ---

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
# --- раскладка ---

# Команда shellioctl с ответом ревизии; ревизия — в REV, иначе причина в WHY.
core_cmd() {
  local state="$1" prefix="$2" code=0
  shift 2
  REV=""
  WHY=""
  ctl "$state" "$prefix" --json --timeout "$CTL_TIMEOUT_MS" "$@" || code=$?
  if [[ $code -ne 0 ]]; then
    WHY="shellioctl $*: код $code: $(oneline "$prefix.stdout") $(oneline "$prefix.stderr")"
    return 1
  fi
  if ! REV="$(jv "$prefix.stdout" ok.revision)"; then
    WHY="shellioctl $*: нет ревизии в ответе: $(oneline "$prefix.stdout")"
    return 1
  fi
}

# ID объекта с полем title, равным title, в любом месте JSON-файла.
id_by_title() {
  /usr/bin/perl -COEA -Mutf8 -MJSON::PP -e '
    my ($file, $title) = @ARGV;
    open my $fh, "<", $file or exit 2;
    local $/;
    my $root = JSON::PP->new->utf8->decode(<$fh>);
    my @queue = ($root);
    while (@queue) {
      my $node = shift @queue;
      if (ref $node eq "HASH") {
        if (defined $node->{title} && !ref $node->{title} && $node->{title} eq $title && defined $node->{id}) {
          print "$node->{id}\n";
          exit 0;
        }
        push @queue, values %$node;
      } elsif (ref $node eq "ARRAY") {
        push @queue, @$node;
      }
    }
    exit 1;
  ' "$1" "$2"
}

# query.layout рабочей области wb профиля state в <prefix>.stdout.
layout_get() {
  ctl "$1" "$2" --json --timeout "$CTL_TIMEOUT_MS" layout get --workbench "$3" --all
}

# Опрос: ui.window показывает ревизию зеркала не меньше rev, query.layout отдаёт кадры. Ответы — <prefix>.window и
# <prefix>.layout; иначе WHY.
settle_layout() {
  local state="$1" prefix="$2" bench="$3" rev="$4" deadline seen
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$state" "$prefix.window" window && layout_get "$state" "$prefix.layout" "$bench"; then
      seen="$(jv "$prefix.window.stdout" ok.ui.window.mirror_revision || echo 0)"
      if [[ "$seen" =~ ^[0-9]+$ ]] && ((seen >= rev)) &&
        [[ "$(jv "$prefix.layout.stdout" ok.value.frames || echo null)" != null ]]; then
        return 0
      fi
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс ui.window не показал ревизию $rev или query.layout без кадров: ревизия зеркала ${seen:-?}, $(oneline "$prefix.layout.stdout" | cut -c 1-200)"
      return 1
    fi
    sleep "$POLL"
  done
}

# Окно владельца — обычное окно 1180×720 на переднем плане; иначе WHY.
window_ok() {
  local file="$1" w h
  w="$(jv "$file" ok.ui.window.frame.width || echo ?)"
  h="$(jv "$file" ok.ui.window.frame.height || echo ?)"
  WHY=""
  if [[ "$(jv "$file" ok.ui.window.mode || echo ?)" != normal ]]; then
    WHY="окно не в обычном режиме: $(jv "$file" ok.ui.window.mode || echo ?)"
  elif ! awk -v w="$w" -v h="$h" -v W="$WINDOW_W" -v H="$WINDOW_H" 'BEGIN { exit !(w == W && h == H) }'; then
    WHY="окно ${w}×${h}, а не ${WINDOW_W}×${WINDOW_H}: кадры AX и снимки несравнимы"
  fi
  [[ -z "$WHY" ]]
}

# Поле query.layout.
lv() {
  jv "$1" "ok.value.$2"
}

# Панель с фокусом: последняя запись истории фокуса.
focus_of() {
  /usr/bin/perl -MJSON::PP -e 'local $/; my $v = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{layout}{focus};
    print $v->[-1] // "", "\n"' <"$1"
}

# Раскладка без кадров каноническим JSON: равенство до и после перезапуска.
layout_canon() {
  /usr/bin/perl -MJSON::PP -e 'local $/; my $v = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value};
    print JSON::PP->new->utf8->canonical->encode({ layout => $v->{layout}, panels => $v->{panels} }), "\n"' <"$1"
}

# Зона и часть панели: «зона.часть» или пусто.
place_of() {
  /usr/bin/perl -MJSON::PP -e 'my ($panel) = @ARGV; local $/; my $l = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{layout};
    if (($l->{navigator} // "") eq $panel) { print "navigator\n"; exit }
    for my $z (qw(left right center bottom)) { my $i = 0;
      for my $p (@{$l->{zones}{$z}{parts}}) { if (grep { $_ eq $panel } @{$p->{tabs}}) { print "$z.$i\n"; exit } $i++ } }
    print "\n"' "$2" <"$1"
}

# Панели плагина по порядку частей: id через пробел.
panels_of() {
  /usr/bin/perl -MJSON::PP -e 'my ($plugin) = @ARGV; local $/; my $v = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value};
    print join(" ", map { $_->{id} } grep { $_->{plugin} eq $plugin } @{$v->{panels}}), "\n"' "$2" <"$1"
}

# Создано экземпляров класса name по ui.stats.
created_of() {
  jv "$1" "ok.ui.stats.instances[name=$2].created" 2>/dev/null || echo 0
}

# Владелец профиля в обычном режиме; PID — в OWNER_PID, иначе ошибка исполнения.
launch_normal() {
  local state="$1" dir="$2"
  mkdir -p "$dir"
  launch_owner "$state" "$dir" || fatal "владелец профиля $state не поднят: $WHY"
}

# app quit и ожидание выхода PID владельца; иначе ошибка исполнения.
quit_owner() {
  local state="$1" dir="$2" pid="$3"
  ui_cmd "$state" "$dir/quit" app quit || core_cmd "$state" "$dir/quit" app quit || true
  wait_gone "$pid" "$EXIT_S" || fatal "владелец pid $pid не вышел за $EXIT_S с после app quit"
}
# --- панели и плагины ---

# Факт панели в ui.inspect: поле state.panel узла layout.panel.<PanelId>.
pfact() {
  comp "$1" "layout.panel.$2" "state.panel.$3"
}

# Поле состояния списка в теле панели: state.virtual_list узла layout.panel.<PanelId>.<имя>.
list_fact() {
  comp "$1" "layout.panel.$2.$3" "state.virtual_list.$4"
}

# Построено тел панели по ui.stats.
built_of() {
  jv "$1" "ok.ui.stats.bodies_built[panel=$2].built" 2>/dev/null || echo "?"
}

# Опрос ui.inspect: поле path узла id равно want; ответ — <prefix>.stdout, иначе WHY.
wait_inspect() {
  local state="$1" prefix="$2" id="$3" path="$4" want="$5" deadline got=""
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$state" "$prefix" inspect; then
      got="$(comp "$prefix.stdout" "$id" "$path" 2>/dev/null || echo нет)"
      [[ "$got" != "$want" ]] || return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс ui.inspect: $path узла $id — ${got:-не прочитан}, ожидалось $want"
      return 1
    fi
    sleep "$POLL"
  done
}

# query.panel панели в <prefix>.stdout.
panel_get() {
  ctl "$1" "$2" --json --timeout "$CTL_TIMEOUT_MS" panel get "$3" --all
}

# Поле query.panel.
pv() {
  jv "$1" "ok.value.$2"
}

# Данные панели каноническим JSON: структурное равенство до и после перезапуска.
data_canon() {
  /usr/bin/perl -MJSON::PP -e 'local $/; my $v = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value};
    print JSON::PP->new->utf8->canonical->allow_nonref->encode($v->{data}), "\n"' <"$1"
}
tree_id() {
  ctl "$profile" "$1/tree-$2" --json tree --all || return 1
  id_by_title "$1/tree-$2.stdout" "$3"
}


# Процесс pid на переднем плане: меню и клавиши идут ему; иначе WHY.
front_ok() {
  local pid="$1" dir="$2" deadline
  WHY=""
  osascript -l JavaScript -e "
    const se = Application('System Events');
    se.processes.whose({unixId: $pid})[0].frontmost = true;
    'ok'" >"$dir/front.stdout" 2>"$dir/front.stderr" || {
    WHY="владелец не выведен вперёд: $(oneline "$dir/front.stderr")"
    return 1
  }
  deadline=$(($(now_ms) + MIRROR_MS))
  until [[ "$(front_pid)" == "$pid" ]]; do
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс на переднем плане не владелец, а pid $(front_pid)"
      return 1
    fi
    sleep "$POLL"
  done
}
# ui.receipts профиля state в <prefix>.stdout; с after — только записи с номером больше after.
receipts_get() {
  local state="$1" prefix="$2" after="${3:-}"
  if [[ -n "$after" ]]; then
    ctl "$state" "$prefix" --json --timeout "$CTL_TIMEOUT_MS" ui receipts --after "$after"
  else
    ctl "$state" "$prefix" --json --timeout "$CTL_TIMEOUT_MS" ui receipts
  fi
}

# Номер последней квитанции UI владельца — в LAST; иначе WHY.
receipts_last() {
  local state="$1" prefix="$2"
  WHY=""
  LAST=""
  if ! receipts_get "$state" "$prefix"; then
    WHY="ui.receipts не прочитан: $(oneline "$prefix.stdout") $(oneline "$prefix.stderr")"
    return 1
  fi
  LAST="$(jv "$prefix.stdout" ok.ui.receipts.last || true)"
  if [[ ! "$LAST" =~ ^[0-9]+$ ]]; then
    WHY="в ответе ui.receipts нет номера last: $(oneline "$prefix.stdout")"
    return 1
  fi
}

# Опрос ui.receipts до want записей с номером больше after; последнее чтение — <prefix>.stdout, иначе WHY.
wait_receipts() {
  local state="$1" prefix="$2" after="$3" want="${4:-1}" deadline seen="?"
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if receipts_get "$state" "$prefix" "$after"; then
      seen="$(jv "$prefix.stdout" 'ok.ui.receipts.receipts[#]' || echo ?)"
      [[ "$seen" =~ ^[0-9]+$ ]] && ((seen >= want)) && return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс квитанций UI с номером больше $after — $seen, ожидалось $want"
      return 1
    fi
    sleep "$POLL"
  done
}

# Поле field квитанции index чтения ui.receipts; «нет» — поля нет.
rc() {
  jv "$1" "ok.ui.receipts.receipts[$2].$3" 2>/dev/null || echo нет
}

# События после курсора rev: ответ — <prefix>.stdout, в EVENTS — «событий|panel_add|ревизии через запятую» или
# reset, если курсор вытеснен из буфера; иначе WHY.
events_after() {
  local state="$1" prefix="$2" rev="$3"
  WHY=""
  EVENTS=""
  if ! ctl "$state" "$prefix" --json --timeout "$CTL_TIMEOUT_MS" events --since "$rev" --all; then
    WHY="events --since $rev не прочитан: $(oneline "$prefix.stdout") $(oneline "$prefix.stderr")"
    return 1
  fi
  EVENTS="$(/usr/bin/perl -MJSON::PP -e 'local $/; my $ok = JSON::PP->new->utf8->decode(<STDIN>)->{ok};
    if (ref $ok ne "HASH" || ref $ok->{events} ne "ARRAY") { print "reset\n"; exit }
    my @e = @{$ok->{events}}; my $adds = 0;
    for my $e (@e) { $adds += grep { ($_->{change} // "") eq "panel_add" } @{$e->{changes}} }
    print scalar(@e), "|", $adds, "|", join(",", map { $_->{revision} } @e), "\n"' <"$prefix.stdout" 2>/dev/null || true)"
  if [[ -z "$EVENTS" ]]; then
    WHY="ответ events --since $rev не разобран: $(oneline "$prefix.stdout")"
    return 1
  fi
}


# --- клавиши ---

# Клавиша кодом процессу на переднем плане: key <журнал> <код> [control|command|option|shift]…
key() {
  local out="$1" code="$2" mods="" m
  shift 2
  for m in "$@"; do
    mods="${mods:+$mods, }$m down"
  done
  if [[ -n "$mods" ]]; then
    printf '%s key code %s using {%s}\n' "$(now_ms)" "$code" "$mods" >>"$out"
    osascript -e "tell application \"System Events\" to key code $code using {$mods}" >>"$out" 2>&1
  else
    printf '%s key code %s\n' "$(now_ms)" "$code" >>"$out"
    osascript -e "tell application \"System Events\" to key code $code" >>"$out" 2>&1
  fi
}

# Последовательность аккордов и её запись в кольце: act <каталог> <имя> <действие> <аккорд>…, аккорд —
# «код[+модификатор…]». До клавиш снимается номер последней записи (в BEFORE), после — опрос до новой записи.
# Первая новая запись обязана называть действие с источником key; её отказ — в OUTCOME («ok» без отказа). Число
# записей после BEFORE позже сверяет once. Отказ чтения или чужая запись — WHY.
BEFORE=""
act() {
  local dir="$1" name="$2" action="$3" chord code got
  shift 3
  WHY=""
  OUTCOME=""
  receipts_last "$profile" "$dir/$name.before" || return 1
  BEFORE="$LAST"
  for chord in "$@"; do
    code="${chord%%+*}"
    if [[ "$chord" == *+* ]]; then
      # shellcheck disable=SC2086
      key "$dir/keys.log" "$code" $(tr '+' ' ' <<<"${chord#*+}") || {
        WHY="клавиша $chord не отправлена: $(tail -n 1 "$dir/keys.log")"
        return 1
      }
    else
      key "$dir/keys.log" "$code" || {
        WHY="клавиша $chord не отправлена: $(tail -n 1 "$dir/keys.log")"
        return 1
      }
    fi
  done
  wait_receipts "$profile" "$dir/$name.receipts" "$BEFORE" 1 || {
    WHY="клавиши $*: $WHY"
    return 1
  }
  got="$(rc "$dir/$name.receipts.stdout" 0 action) от $(rc "$dir/$name.receipts.stdout" 0 source)"
  if [[ "$got" != "$action от key" ]]; then
    WHY="клавиши $*: запись кольца «${got}», ожидалась «$action от key»"
    return 1
  fi
  OUTCOME="$(rc "$dir/$name.receipts.stdout" 0 error)"
  [[ "$OUTCOME" != нет ]] || OUTCOME=ok
}

# После act и опроса исхода: записей кольца с номером больше BEFORE ровно одна; иначе WHY.
once() {
  local dir="$1" name="$2" seen
  WHY=""
  receipts_get "$profile" "$dir/$name.once" "$BEFORE" || {
    WHY="ui.receipts не прочитан: $(oneline "$dir/$name.once.stderr")"
    return 1
  }
  seen="$(jv "$dir/$name.once.stdout" 'ok.ui.receipts.receipts[#]' || echo ?)"
  [[ "$seen" == 1 ]] || {
    WHY="записей кольца после клавиш — $seen, ожидалась одна: $(oneline "$dir/$name.once.stdout")"
    return 1
  }
}

# Опрос query.layout рабочей области A: панель с фокусом — want; чтение — <prefix>.stdout, иначе WHY.
wait_focus() {
  local prefix="$1" want="$2" deadline got="?"
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if layout_get "$profile" "$prefix" "$wb_a"; then
      got="$(focus_of "$prefix.stdout")"
      [[ "$got" != "$want" ]] || return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс фокус query.layout — $(name_of "$got"), ожидался $(name_of "$want")"
      return 1
    fi
    sleep "$POLL"
  done
}

# Название панели подготовки для текста причины.
name_of() {
  case "$1" in
    "$p_center") echo "центр" ;;
    "$p_left") echo "левая зона" ;;
    "$p_right") echo "правая зона" ;;
    "$p_bottom") echo "нижняя зона" ;;
    *) echo "панель ${1:-?}" ;;
  esac
}

# Владелец основного профиля на переднем плане, иначе ошибка исполнения: клавиши System Events идут переднему.
to_front() {
  front_ok "$owner_pid" "$1" || fatal "$2: $WHY"
}

# Опрос ui.window: выбор оформления — want; чтение — <prefix>.stdout, иначе WHY.
wait_appearance() {
  local prefix="$1" want="$2" deadline got="?"
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$profile" "$prefix" window; then
      got="$(jv "$prefix.stdout" ok.ui.window.appearance_choice || echo ?)"
      [[ "$got" != "$want" ]] || return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс выбор оформления ui.window — $got, ожидался $want"
      return 1
    fi
    sleep "$POLL"
  done
}

# --- подготовка ---

prepare() {
  local left
  app="${bundle_arg:-$root/target/bundle/shellio.app}"
  [[ -d "$app" ]] || fatal "нет бандла $app: соберите его через make bundle"
  app="$(cd "$app" && pwd -P)"
  exe="$app/Contents/MacOS/shellio"
  [[ -x "$exe" ]] || fatal "в бандле нет исполняемого $exe"
  [[ -x "$LSAPPINFO" ]] || fatal "нет $LSAPPINFO"
  /usr/bin/perl -MJSON::PP -e 1 2>/dev/null || fatal "нет модуля JSON::PP системного perl"
  left="$(run_processes)"
  [[ -z "$left" ]] || fatal "предусловие: бинарь бандла прогона уже запущен (PID $left), завершите его"
  [[ -n "$(finder_pid)" ]] || fatal "предусловие: Finder не запущен"
  find_terminal
  build_bin shellioctl shellioctl
  ctl_bin="$BUILT"
  log "каталог прогона: $run"
  log "бандл: $app, shellioctl: $ctl_bin"
}

# Пробы предусловий: оконные менеджеры, источник ввода, Automation и Accessibility по окну служебного владельца
# обычного режима; затем он завершается.
preconditions() {
  local dir="$run/preconditions" name layout ok pid
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
  new_profile
  log "== предусловия: служебный владелец, профиль $NEW_PROFILE"
  launch_owner "$NEW_PROFILE" "$dir" || fatal "предусловие: служебный владелец не поднят: $WHY"
  pid="$OWNER_PID"
  name="$(osascript -e "tell application \"System Events\" to get name of (first process whose unix id is $pid)" \
    2>"$dir/system-events.stderr" || true)"
  [[ -n "$name" ]] ||
    fatal "предусловие: System Events не прочитал процесс pid $pid (нет Automation у терминала-предка): $(oneline "$dir/system-events.stderr")"
  osascript -e "tell application \"System Events\" to count windows of (first process whose unix id is $pid)" \
    >"$dir/accessibility.stdout" 2>"$dir/accessibility.stderr" ||
    fatal "предусловие: System Events не прочитал окна pid $pid (нет Accessibility у терминала-предка): $(oneline "$dir/accessibility.stderr")"
  verified_hello "$NEW_PROFILE" "$dir/hello-stop" || fatal "предусловие: служебный владелец: $WHY"
  terminate "$HELLO_PID"
  ! alive "$pid" || fatal "предусловие: служебный владелец pid $pid не завершился"
}

# Панель плагина в зоне рабочей области A штатным CLI; PanelId — в PANEL, иначе ошибка исполнения.
PANEL=""
open_panel() {
  local prefix="$1" plugin="$2" zone="$3"
  core_cmd "$profile" "$prefix" panel open "$plugin" --workbench "$wb_a" --zone "$zone" --all ||
    fatal "подготовка: $WHY"
  PANEL="$(jv "$prefix.stdout" ok.value.panel)" || fatal "подготовка: в ответе panel open нет панели"
}

# Заглушки «Развернуть» в кадрах query.layout: «зона.часть» через пробел, пусто — каждая часть не меньше min_size.
stubs_of() {
  /usr/bin/perl -MJSON::PP -e 'local $/; my $f = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{frames}; my @s;
    push @s, "navigator" if $f->{navigator} && $f->{navigator}{stub};
    for my $p (@{$f->{parts} // []}) { push @s, "$p->{geometry}{zone}.$p->{index}" if $p->{stub} }
    print join(" ", @s), "\n"' <"$1"
}

# Сцена без заглушек; иначе WHY: фокус и клавиши на заглушке пункта не доказывают.
no_stubs() {
  local stubs
  WHY=""
  stubs="$(stubs_of "$1")" || {
    WHY="кадры query.layout не разобраны: $(oneline "$1" | cut -c 1-200)"
    return 1
  }
  [[ -z "$stubs" ]] || {
    WHY="в сцене заглушки «Развернуть»: $stubs"
    return 1
  }
}

# Основной профиль штатным CLI: владелец, воркспейс и проект.
setup_main() {
  local dir="$run/preconditions/setup" dir1
  mkdir -p "$dir"
  log "== подготовка основного профиля"
  new_profile
  profile="$NEW_PROFILE"
  launch_normal "$profile" "$dir"
  owner_pid="$OWNER_PID"
  dir1="$(mktemp -d /tmp/shellio-e2e-project.XXXXXX)"
  profiles+=("$dir1")
  core_cmd "$profile" "$dir/ws" workspace new "e2e-keyboard-w-$$" --all || fatal "подготовка: $WHY"
  ws="$(tree_id "$dir" ws "e2e-keyboard-w-$$")" || fatal "подготовка: воркспейс не найден в tree"
  core_cmd "$profile" "$dir/project" project add "$dir1" --title "keyboard-p-$$" --workspace "$ws" ||
    fatal "подготовка: $WHY"
  project="$(tree_id "$dir" project "keyboard-p-$$")" || fatal "подготовка: проект не найден в tree"
  ui_read "$profile" "$dir/window" window || fatal "подготовка: ui.window не прочитан"
  window_ok "$dir/window.stdout" || fatal "подготовка: $WHY"
}

# Новая открытая рабочая область пункта: терминалы в центре, слева, справа и снизу, боковые зоны по SIDE_W, фокус
# в центре, ни одной заглушки. Область — в wb_a, панели — в p_center, p_left, p_right, p_bottom.
fresh_bench() {
  local item="$1" dir="$2/bench" name
  mkdir -p "$dir"
  bench_n=$((bench_n + 1))
  name="b$bench_n-$$"
  core_cmd "$profile" "$dir/new" workbench new --target "$project" "$name" --workspace "$ws" || fatal "$item: $WHY"
  wb_a="$(tree_id "$dir" bench "$name")" || fatal "$item: рабочая область $name не найдена в tree"
  core_cmd "$profile" "$dir/open" workbench open --target "$wb_a" --workspace "$ws" || fatal "$item: $WHY"
  settle_layout "$profile" "$dir/settle" "$wb_a" "$REV" || fatal "$item: $WHY"
  p_center="$(panels_of "$dir/settle.layout.stdout" terminal)"
  [[ -n "$p_center" && "$p_center" != *" "* && "$(place_of "$dir/settle.layout.stdout" "$p_center")" == center.0 ]] ||
    fatal "$item: в новой рабочей области нет одного терминала в центре: «${p_center}»"
  open_panel "$dir/open-left" terminal left
  p_left="$PANEL"
  open_panel "$dir/open-right" terminal right
  p_right="$PANEL"
  open_panel "$dir/open-bottom" terminal bottom
  p_bottom="$PANEL"
  core_cmd "$profile" "$dir/size-left" layout resize --zone left --workbench "$wb_a" "$SIDE_W" --workspace "$ws" ||
    fatal "$item: $WHY"
  core_cmd "$profile" "$dir/size-right" layout resize --zone right --workbench "$wb_a" "$SIDE_W" --workspace "$ws" ||
    fatal "$item: $WHY"
  core_cmd "$profile" "$dir/focus" panel focus "$p_center" --all || fatal "$item: $WHY"
  settle_layout "$profile" "$dir/settled" "$wb_a" "$REV" || fatal "$item: $WHY"
  no_stubs "$dir/settled.layout.stdout" || fatal "$item: подготовка: $WHY"
  log "$item: область $wb_a, центр $p_center, слева $p_left, справа $p_right, снизу $p_bottom"
}

# --- пункты матрицы ---

# Переход фокуса клавишами: step <пункт> <каталог> <имя> <действие> <цель> <аккорд>… — одна запись кольца без
# отказа и фокус query.layout на цели.
step() {
  local item="$1" dir="$2" name="$3" action="$4" want="$5"
  shift 5
  if ! act "$dir" "$name" "$action" "$@"; then
    failures+=("$name: $WHY")
    return 0
  fi
  [[ "$OUTCOME" == ok ]] || failures+=("$name: действие $action отказало: $OUTCOME")
  wait_focus "$dir/$name.layout" "$want" || failures+=("$name: $WHY")
  once "$dir" "$name" || failures+=("$name: $WHY")
}

# Ctrl-w h/j/k/l и голые h/j/k/l у терминалов (нейтральная панель без списка) между четырьмя зонами.
item_focus_hjkl() {
  local item=focus-hjkl dir="$run/focus-hjkl" pre w
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  to_front "$dir" "$item"
  wait_focus "$dir/start.layout" "$p_center" || fatal "$item: $WHY"
  for pre in ctrl-w bare; do
    w=()
    [[ "$pre" == bare ]] || w=("$K_W+control")
    step "$item" "$dir" "$pre-l" panel.focus.right "$p_right" ${w[@]+"${w[@]}"} "$K_L"
    step "$item" "$dir" "$pre-h" panel.focus.left "$p_center" ${w[@]+"${w[@]}"} "$K_H"
    step "$item" "$dir" "$pre-h-left" panel.focus.left "$p_left" ${w[@]+"${w[@]}"} "$K_H"
    step "$item" "$dir" "$pre-l-center" panel.focus.right "$p_center" ${w[@]+"${w[@]}"} "$K_L"
    step "$item" "$dir" "$pre-j" panel.focus.down "$p_bottom" ${w[@]+"${w[@]}"} "$K_J"
    step "$item" "$dir" "$pre-k" panel.focus.up "$p_center" ${w[@]+"${w[@]}"} "$K_K"
  done
  if layout_get "$profile" "$dir/end" "$wb_a"; then
    no_stubs "$dir/end.stdout" || failures+=("$WHY")
  fi
  conclude "$item" "терминалы в центре, слева, справа и снизу без заглушек: Ctrl-w l, h, h, l, j, k и те же голые клавиши ведут фокус query.layout центр → правая → центр → левая → центр → нижняя → центр; на каждый переход одна запись ui.receipts panel.focus.* с источником key без отказа"
}

# Активная вкладка части зоны по query.layout: PanelId или пусто.
active_of() {
  /usr/bin/perl -MJSON::PP -e 'my ($zone, $part) = @ARGV; local $/;
    my $p = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{layout}{zones}{$zone}{parts}[$part];
    print $p ? ($p->{tabs}[$p->{active}] // "") : "", "\n"' "$2" "$3" <"$1"
}

# Число частей зоны по query.layout.
parts_of() {
  jv "$1" "ok.value.layout.zones.$2.parts[#]" 2>/dev/null || echo "?"
}

# Ревизия ядра до клавиши — в REV0; чтение — <каталог>/<имя>.mark.stdout, иначе WHY.
REV0=""
mark() {
  WHY=""
  REV0=""
  layout_get "$profile" "$1/$2.mark" "$wb_a" && REV0="$(jv "$1/$2.mark.stdout" ok.revision)" && return 0
  WHY="query.layout до клавиши не прочитан: $(oneline "$1/$2.mark.stdout" | cut -c 1-200)"
  return 1
}

# После клавиши и её исхода: событий ядра после REV0 ровно want; иначе WHY.
events_are() {
  local dir="$1" name="$2" want="$3"
  events_after "$profile" "$dir/$name.events" "$REV0" || return 1
  [[ "${EVENTS%%|*}" == "$want" ]] || {
    WHY="событий ядра после ревизии $REV0 — ${EVENTS%%|*} (ревизии ${EVENTS##*|}), ожидалось $want"
    return 1
  }
}

# Опрос query.layout рабочей области пункта: perl-условие над ok.value ($v) истинно; чтение — <prefix>.stdout,
# иначе WHY с текстом ожидания.
wait_layout() {
  local prefix="$1" what="$2" cond="$3" deadline
  shift 3
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if layout_get "$profile" "$prefix" "$wb_a" &&
      /usr/bin/perl -MJSON::PP -e 'my $cond = shift; local $/; my $v = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value};
        my $l = $v->{layout}; exit(eval($cond) ? 0 : 1)' "$cond" "$@" <"$prefix.stdout" 2>/dev/null; then
      return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс query.layout не показал: $what"
      return 1
    fi
    sleep "$POLL"
  done
}

# Клавиши команды ядра: kstep <каталог> <имя> <действие> <ожидание текстом> <perl-условие> <аккорд>… — одна запись
# кольца без отказа, условие над query.layout и одна дельта events --since от ревизии до нажатия.
kstep() {
  local dir="$1" name="$2" action="$3" what="$4" cond="$5"
  shift 5
  if ! mark "$dir" "$name"; then
    failures+=("$name: $WHY")
    return 0
  fi
  if ! act "$dir" "$name" "$action" "$@"; then
    failures+=("$name: $WHY")
    return 0
  fi
  [[ "$OUTCOME" == ok ]] || failures+=("$name: действие $action отказало: $OUTCOME")
  wait_layout "$dir/$name.layout" "$what" "$cond" || failures+=("$name: $WHY")
  events_are "$dir" "$name" 1 || failures+=("$name: $WHY")
  once "$dir" "$name" || failures+=("$name: $WHY")
}

# Perl-условие: активная вкладка части 0 центра — панель.
active_is() {
  printf '($l->{zones}{center}{parts}[0]{tabs}[$l->{zones}{center}{parts}[0]{active}] // "") eq "%s"' "$1"
}

# Perl-условие: панель лежит в зоне.
in_zone() {
  printf 'grep { grep { $_ eq "%s" } @{$_->{tabs}} } @{$l->{zones}{%s}{parts}}' "$1" "$2"
}

# H, L и 1…3 у трёх вкладок центра.
item_tabs() {
  local item=tabs dir="$run/tabs" t2 t3
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  open_panel "$dir/open-t2" terminal center
  t2="$PANEL"
  open_panel "$dir/open-t3" terminal center
  t3="$PANEL"
  settle_layout "$profile" "$dir/start" "$wb_a" "$REV" || fatal "$item: $WHY"
  [[ "$(lv "$dir/start.layout.stdout" 'layout.zones.center.parts[0].tabs')" == "[\"$p_center\",\"$t2\",\"$t3\"]" &&
    "$(active_of "$dir/start.layout.stdout" center 0)" == "$t3" && "$(focus_of "$dir/start.layout.stdout")" == "$t3" ]] ||
    fatal "$item: подготовка: в части 0 центра не три вкладки с активной и фокусом на третьей: $(lv "$dir/start.layout.stdout" layout.zones.center || echo ?)"
  no_stubs "$dir/start.layout.stdout" || fatal "$item: подготовка: $WHY"
  to_front "$dir" "$item"
  kstep "$dir" key-1 tab.select "активная вкладка центра — первая" "$(active_is "$p_center")" "$K_1"
  kstep "$dir" key-L tab.next "активная вкладка центра — вторая" "$(active_is "$t2")" "$K_L+shift"
  kstep "$dir" key-L-again tab.next "активная вкладка центра — третья" "$(active_is "$t3")" "$K_L+shift"
  kstep "$dir" key-H tab.prev "активная вкладка центра — вторая" "$(active_is "$t2")" "$K_H+shift"
  kstep "$dir" key-3 tab.select "активная вкладка центра — третья" "$(active_is "$t3")" "$K_3"
  kstep "$dir" key-2 tab.select "активная вкладка центра — вторая" "$(active_is "$t2")" "$K_2"
  conclude "$item" "три вкладки терминала в центре: 1, L, L, H, 3, 2 ведут активную вкладку query.layout первая → вторая → третья → вторая → третья → вторая; на каждую клавишу одна запись ui.receipts tab.* с источником key без отказа и одна дельта events --since"
}

# Perl-условие: панель — единственная вкладка части зоны.
alone_in() {
  printf 'join(",", @{($l->{zones}{%s}{parts}[%s] // {})->{tabs} // []}) eq "%s"' "$2" "$3" "$1"
}

# Ctrl-w L, H, K, J, K переносят вторую панель центра в зону по направлению (H — левая, L — правая, K — центр,
# J — нижняя); Ctrl-w v делит центр, Ctrl-w s — правую зону. Делится часть с двумя вкладками: активная уходит в
# новую часть. Выбор «Открыть в новой части» у части с одной вкладкой судят plugins.sh и сверка.
item_move_and_split() {
  local item=move-and-split dir="$run/move-and-split" t2 t3
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  open_panel "$dir/open-t2" terminal center
  t2="$PANEL"
  settle_layout "$profile" "$dir/start" "$wb_a" "$REV" || fatal "$item: $WHY"
  [[ "$(place_of "$dir/start.layout.stdout" "$t2")" == center.0 && "$(focus_of "$dir/start.layout.stdout")" == "$t2" ]] ||
    fatal "$item: подготовка: вторая панель не в центре с фокусом: $(place_of "$dir/start.layout.stdout" "$t2")"
  to_front "$dir" "$item"
  kstep "$dir" move-L panel.move.right "панель в правой зоне" "$(in_zone "$t2" right)" "$K_W+control" "$K_L+shift"
  kstep "$dir" move-H panel.move.left "панель в левой зоне" "$(in_zone "$t2" left)" "$K_W+control" "$K_H+shift"
  kstep "$dir" move-K panel.move.up "панель в центре" "$(in_zone "$t2" center)" "$K_W+control" "$K_K+shift"
  kstep "$dir" move-J panel.move.down "панель в нижней зоне" "$(in_zone "$t2" bottom)" "$K_W+control" "$K_J+shift"
  kstep "$dir" move-K-back panel.move.up "панель в центре" "$(in_zone "$t2" center)" "$K_W+control" "$K_K+shift"
  if layout_get "$profile" "$dir/moved" "$wb_a"; then
    [[ "$(focus_of "$dir/moved.stdout")" == "$t2" ]] ||
      failures+=("после переносов фокус не на перенесённой панели: $(name_of "$(focus_of "$dir/moved.stdout")")")
    [[ "$(place_of "$dir/moved.stdout" "$t2")" == center.0 ]] ||
      failures+=("после переносов панель не в части 0 центра: $(place_of "$dir/moved.stdout" "$t2")")
    no_stubs "$dir/moved.stdout" || failures+=("после переносов: $WHY")
  else
    failures+=("query.layout после переносов не прочитан")
  fi
  kstep "$dir" split-v layout.split "в центре две части рядом, перенесённая панель одна во второй" \
    "@{\$l->{zones}{center}{parts}} == 2 && $(alone_in "$t2" center 1)" "$K_W+control" "$K_V"
  open_panel "$dir/open-t3" terminal right
  t3="$PANEL"
  wait_focus "$dir/open-t3.layout" "$t3" || fatal "$item: $WHY"
  [[ "$(place_of "$dir/open-t3.layout.stdout" "$t3")" == right.0 ]] ||
    fatal "$item: подготовка: вторая панель правой зоны не в её части 0: $(place_of "$dir/open-t3.layout.stdout" "$t3")"
  kstep "$dir" split-s layout.split "в правой зоне две части одна над другой, вторая панель одна во второй" \
    "@{\$l->{zones}{right}{parts}} == 2 && $(alone_in "$t3" right 1)" "$K_W+control" "$K_S"
  if layout_get "$profile" "$dir/split" "$wb_a"; then
    no_stubs "$dir/split.stdout" || failures+=("после делений: $WHY")
  else
    failures+=("query.layout после делений не прочитан")
  fi
  conclude "$item" "вторая панель центра: Ctrl-w L, H, K, J, K переносят её центр → правая → левая → центр → нижняя → центр, фокус остаётся на ней; Ctrl-w v уводит её во вторую часть центра, Ctrl-w s — вторую панель правой зоны во вторую часть правой, заглушек нет; на каждую последовательность одна запись ui.receipts с источником key без отказа и одна дельта events --since"
}

# Cmd-Alt-t трижды: системное → светлое → тёмное → системное по ui.window.
item_appearance_cycle() {
  local item=appearance-cycle dir="$run/appearance-cycle" want n=0
  log "== $item"
  failures=()
  ui_cmd "$profile" "$dir/system" ui appearance system || fatal "$item: $WHY"
  wait_appearance "$dir/start.window" system || fatal "$item: $WHY"
  to_front "$dir" "$item"
  for want in light dark system; do
    n=$((n + 1))
    if act "$dir" "cycle-$n" ui.appearance.cycle "$K_T+command+option"; then
      [[ "$OUTCOME" == ok ]] || failures+=("нажатие $n: действие отказало: $OUTCOME")
      wait_appearance "$dir/cycle-$n.window" "$want" || failures+=("нажатие $n: $WHY")
      once "$dir" "cycle-$n" || failures+=("нажатие $n: $WHY")
    else
      failures+=("нажатие $n: $WHY")
    fi
  done
  conclude "$item" "Cmd-Alt-t трижды: выбор оформления ui.window системное → светлое → тёмное → системное, на каждое нажатие одна запись ui.receipts ui.appearance.cycle с источником key"
}

prepare
preconditions
setup_main
item_focus_hjkl
item_move_and_split
item_tabs
item_appearance_cycle
if [[ -n "$owner_pid" ]]; then
  mkdir -p "$run/cleanup"
  quit_owner "$profile" "$run/cleanup" "$owner_pid"
  owner_pid=""
fi
for i in "${!ITEMS[@]}"; do
  if ((STAGE[i] > DONE_STAGE)); then
    record "${ITEMS[i]}" not_run "пункт среза ${STAGE[i]} задачи 10 плана navigation-keymap"
  fi
done
record exit-chord-hid not_run "наблюдает только человек: Ctrl-\\ на настоящей клавиатуре через системные сочетания и источники ввода (ADR-0010, «Уточнено в P0.3»)"
completed=1
