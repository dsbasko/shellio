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
# Срок, за который наблюдение за каталогом конфигурации применяет правку keymap.toml.
KEYMAP_MS=5000
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
STAGE=(1 2 2 3 5 2 4 1 6 8 11 11 8 7 9 9 6 5 10 10 10 0)
# Срез, до которого сценарий доведён: пункты более поздних срезов не судятся.
DONE_STAGE=10

# Коды клавиш (kVK_ANSI_*).
K_H=4
K_J=38
K_K=40
K_L=37
K_W=13
K_T=17
K_V=9
K_S=1
K_X=7
K_Z=6
K_0=29
K_O=31
K_EQUAL=24
K_MINUS=27
K_COMMA=43
K_PERIOD=47
K_SPACE=49
K_1=18
K_2=19
K_3=20
K_Q=12
K_B=11
K_SLASH=44
K_ENTER=36
K_ESC=53
K_TAB=48
K_A=0
K_C=8
K_I=34
K_M=46

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
  # Источник ввода, сменённый пунктом cyrillic-physical, возвращается и на неудачном пути.
  if [[ -n "$SOURCE_BEFORE" ]]; then
    tis select "$SOURCE_BEFORE" >/dev/null 2>&1 || add_error "источник ввода $SOURCE_BEFORE не возвращён"
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

# Немая клавиша: что владелец отдаёт, когда клавиши не дали записи кольца. Чтения без клавиш (ui.mode, ui.window,
# передний план, первый ответчик, выбор), затем аккорд global с записью (Cmd-Alt-t, оформление возвращается), затем
# один Ctrl-w и узлы строки статуса: набранный префикс виден — маршрутизатор аккорд в Navigation получил; запись
# global есть, а префикса нет — клавиша пришла, но контекст панели не выбран; нет ни того, ни другого — событие
# владельцу не дошло. Порядок важен: аккорд global после набранного префикса был бы поглощён. Файлы —
# <prefix>.mute.*, сводка — в MUTE.
MUTE=""
mute_facts() {
  local prefix="$1" mode win keywin choice front resp picker last global="нет" status
  MUTE=""
  ctl "$profile" "$prefix.mute.mode" --json --timeout "$CTL_TIMEOUT_MS" ui mode get || true
  mode="$(tr -d '\n' <"$prefix.mute.mode.stdout" | cut -c1-300)"
  ui_read "$profile" "$prefix.mute.window" window || true
  win="$(jv "$prefix.mute.window.stdout" ok.ui.window.visible 2>/dev/null || echo "?")"
  keywin="$(jv "$prefix.mute.window.stdout" ok.ui.window.key 2>/dev/null || echo "?")"
  choice="$(jv "$prefix.mute.window.stdout" ok.ui.window.appearance_choice 2>/dev/null || true)"
  front="$(front_pid)"
  ui_read "$profile" "$prefix.mute.inspect" inspect || true
  resp="$(responder_of "$prefix.mute.inspect.stdout" 2>/dev/null || echo "?")"
  picker="$(comp "$prefix.mute.inspect.stdout" layout.picker ck 2>/dev/null || echo нет)"
  if receipts_last "$profile" "$prefix.mute.global.before"; then
    last="$LAST"
    key "$prefix.mute.keys.log" "$K_T" command option || true
    if wait_receipts "$profile" "$prefix.mute.global" "$last" 1; then
      global="$(rc "$prefix.mute.global.stdout" 0 action) от $(rc "$prefix.mute.global.stdout" 0 source)"
      [[ -z "$choice" ]] || ctl "$profile" "$prefix.mute.appearance" --json --timeout "$CTL_TIMEOUT_MS" ui appearance "$choice" || true
    fi
  fi
  key "$prefix.mute.keys.log" "$K_W" control || true
  sleep "$POLL"
  ui_read "$profile" "$prefix.mute.prefix" inspect || true
  status="$(/usr/bin/perl -MJSON::PP -e 'local $/; my $v = eval { JSON::PP->new->utf8->decode(<STDIN>) } or exit;
    print join(" ", map { "$_->{id}=«" . ($_->{label} // "") . "»" } grep { $_->{id} =~ /^status\./ && $_->{visible} && defined $_->{label} }
      @{ $v->{ok}{ui}{inspect}{components} // [] })' <"$prefix.mute.prefix.stdout" 2>/dev/null | /usr/bin/perl -CS -pe '1' | cut -c1-400 || true)"
  key "$prefix.mute.keys.log" "$K_ESC" || true
  MUTE="немая клавиша: ui.mode ${mode:-не прочитан}; окно visible $win, key $keywin; передний план pid ${front:-?} (владелец $owner_pid); первый ответчик «${resp}»; выбор ck ${picker}; Cmd-Alt-t — запись «${global}»; строка статуса после Ctrl-w: ${status:-не прочитана}"
}

# Последовательность аккордов и её запись в кольце: act <каталог> <имя> <действие> <аккорд>…, аккорд —
# «код[+модификатор…]». До клавиш снимается номер последней записи (в BEFORE), после — опрос до новой записи.
# Первая новая запись обязана называть действие с источником key; её отказ — в OUTCOME («ok» без отказа). Число
# записей после BEFORE позже сверяет once. Отказ чтения или чужая запись — WHY.
BEFORE=""
act() {
  local dir="$1" name="$2" action="$3" chord code got why
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
    why="$WHY"
    mute_facts "$dir/$name"
    WHY="клавиши $*: $why; $MUTE"
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

# Владелец основного профиля на переднем плане и его окно показано, иначе ошибка исполнения: клавиши System Events
# идут переднему, а без окна ни одна клавиша пункта записи не даст (окно мог закрыть внешний ввод).
to_front() {
  front_ok "$owner_pid" "$1" || fatal "$2: $WHY"
  ui_read "$profile" "$1/front-window" window ||
    fatal "$2: ui.window не прочитан: $(oneline "$1/front-window.stdout") $(oneline "$1/front-window.stderr")"
  [[ "$(jv "$1/front-window.stdout" ok.ui.window.visible || echo "?")" == true ]] ||
    fatal "$2: окно владельца закрыто до пункта: клавиши пункта ему не дойдут"
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

# Опрос ui.inspect: сообщение строки статуса показано (shown) или убрано (hidden); текст показанного — в
# STATUS_TEXT, чтение — <prefix>.stdout, иначе WHY.
STATUS_TEXT=""
wait_status() {
  local prefix="$1" want="$2" deadline node="ok.ui.inspect.components[id=status.message]" seen label
  WHY=""
  STATUS_TEXT=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$profile" "$prefix" inspect; then
      seen="$(jv "$prefix.stdout" "$node.visible" 2>/dev/null || echo false)"
      label="$(jv "$prefix.stdout" "$node.label" 2>/dev/null || echo null)"
      [[ "$seen" == true && -n "$label" && "$label" != null ]] || seen=false
      if [[ "$want" == shown && "$seen" == true ]]; then
        STATUS_TEXT="$label"
        return 0
      fi
      [[ "$want" != hidden || "$seen" == true ]] || return 0
    fi
    if (($(now_ms) >= deadline)); then
      if [[ "$want" == shown ]]; then
        WHY="за $MIRROR_MS мс строка статуса не показала сообщение"
      else
        WHY="за $MIRROR_MS мс сообщение строки статуса не убрано: «${label:-?}»"
      fi
      return 1
    fi
    sleep "$POLL"
  done
}

# keymap.toml основного профиля: text — новый файл, пусто — файла нет. Правку применяет наблюдение за каталогом
# конфигурации: опрос query.keymap до строки с последовательностью seq (present) или без неё (absent); чтение —
# <prefix>.stdout, иначе WHY.
keymap_file() {
  local prefix="$1" text="$2" seq="$3" want="$4" deadline has
  WHY=""
  mkdir -p "$profile/config"
  if [[ -n "$text" ]]; then
    printf '%s\n' "$text" >"$profile/config/keymap.toml.new"
    cp "$profile/config/keymap.toml.new" "$prefix.keymap.toml"
    mv "$profile/config/keymap.toml.new" "$profile/config/keymap.toml"
  else
    rm -f "$profile/config/keymap.toml"
  fi
  deadline=$(($(now_ms) + KEYMAP_MS))
  while :; do
    if ctl "$profile" "$prefix" --json --timeout "$CTL_TIMEOUT_MS" keymap list; then
      has=absent
      ! grep -qF "\"$seq\"" "$prefix.stdout" || has=present
      [[ "$has" != "$want" ]] || return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $KEYMAP_MS мс query.keymap после правки keymap.toml: строка «${seq}» — ${has:-не прочитана}, ожидалось $want"
      return 1
    fi
    sleep "$POLL"
  done
}

# Клавиши с отказом ядра: refused <каталог> <имя> <действие> <текст> <аккорд>… — до клавиш сообщения строки статуса
# нет, после — одна запись кольца действия с отказом invalid_argument, сообщение text, ни одной дельты ядра, раскладка
# прежняя.
refused() {
  local dir="$1" name="$2" action="$3" text="$4" canon
  shift 4
  wait_status "$dir/$name.before-status" hidden || failures+=("$name: до клавиши: $WHY")
  if ! mark "$dir" "$name"; then
    failures+=("$name: $WHY")
    return 0
  fi
  canon="$(layout_canon "$dir/$name.mark.stdout")"
  if ! act "$dir" "$name" "$action" "$@"; then
    failures+=("$name: $WHY")
    return 0
  fi
  [[ "$OUTCOME" == invalid_argument ]] ||
    failures+=("$name: исход записи кольца — $OUTCOME, ожидался отказ invalid_argument")
  if wait_status "$dir/$name.status" shown; then
    [[ "$STATUS_TEXT" == "$text" ]] || failures+=("$name: сообщение строки статуса «${STATUS_TEXT}», ожидалось «${text}»")
  else
    failures+=("$name: $WHY")
  fi
  events_are "$dir" "$name" 0 || failures+=("$name: $WHY")
  if layout_get "$profile" "$dir/$name.after" "$wb_a"; then
    [[ "$(layout_canon "$dir/$name.after.stdout")" == "$canon" ]] || failures+=("$name: раскладка после отказа изменилась")
  else
    failures+=("$name: query.layout после отказа не прочитан")
  fi
  once "$dir" "$name" || failures+=("$name: $WHY")
}

# Ctrl-w v у панели правой зоны: отказ не той оси с сообщением text.
refused_split() {
  refused "$1" "$2" layout.split "$3" "$K_W+control" "$K_V"
}

# Ctrl-w v в правой зоне (ось stack): отказ split_wrong_axis с сообщением, которое называет действующую клавишу оси
# зоны; следующая клавиша сообщение убирает. Затем keymap.toml переносит деление оси stack на Ctrl-w x: сообщение
# называет её, и она делит часть. У части две вкладки: названная клавиша делит её без выбора новой части.
item_wrong_axis() {
  local item=wrong-axis dir="$run/wrong-axis" t2 said="Правая зона делится одна над другой" code=0
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  open_panel "$dir/open-t2" terminal right
  t2="$PANEL"
  wait_focus "$dir/open-t2.layout" "$t2" || fatal "$item: $WHY"
  [[ "$(place_of "$dir/open-t2.layout.stdout" "$t2")" == right.0 ]] ||
    fatal "$item: подготовка: вторая панель правой зоны не в её части 0: $(place_of "$dir/open-t2.layout.stdout" "$t2")"
  no_stubs "$dir/open-t2.layout.stdout" || fatal "$item: подготовка: $WHY"
  # Причину отказа называет ядро: та же команда штатным CLI.
  ctl "$profile" "$dir/cli-split" --json --timeout "$CTL_TIMEOUT_MS" layout split "$t2" --axis row --all || code=$?
  [[ $code -ne 0 ]] && grep -qF split_wrong_axis "$dir/cli-split.stdout" "$dir/cli-split.stderr" ||
    failures+=("layout split --axis row панели правой зоны: код $code без причины split_wrong_axis: $(oneline "$dir/cli-split.stdout")")
  to_front "$dir" "$item"
  refused_split "$dir" default "$said: Ctrl-w s"
  step "$item" "$dir" next-key panel.focus.left "$p_center" "$K_H"
  wait_status "$dir/next-key.status" hidden || failures+=("next-key: $WHY")
  if keymap_file "$dir/remap" $'[navigation]\n"Ctrl-w x" = { action = "layout.split", args = { axis = "stack" } }\n"Ctrl-w s" = false' "Ctrl-w x" present; then
    step "$item" "$dir" back panel.focus.right "$t2" "$K_L"
    refused_split "$dir" remapped "$said: Ctrl-w x"
    kstep "$dir" split-x layout.split "в правой зоне две части одна над другой, вторая панель одна во второй" \
      "@{\$l->{zones}{right}{parts}} == 2 && $(alone_in "$t2" right 1)" "$K_W+control" "$K_X"
    wait_status "$dir/split-x.status" hidden || failures+=("split-x: $WHY")
    if layout_get "$profile" "$dir/split" "$wb_a"; then
      no_stubs "$dir/split.stdout" || failures+=("после деления: $WHY")
    else
      failures+=("query.layout после деления не прочитан")
    fi
  else
    failures+=("keymap.toml: $WHY")
  fi
  # Следующие пункты идут на таблице по умолчанию.
  keymap_file "$dir/restore" "" "Ctrl-w x" absent || fatal "$item: keymap.toml не убран: $WHY"
  conclude "$item" "две вкладки в правой зоне: Ctrl-w v — одна запись ui.receipts layout.split с отказом, ни одной дельты events --since, раскладка прежняя, сообщение строки статуса «$said: Ctrl-w s» (причина split_wrong_axis — по той же команде CLI), следующая клавиша h сообщение убирает; после правки keymap.toml (Ctrl-w x — ось stack, Ctrl-w s снята) сообщение называет Ctrl-w x, и Ctrl-w x делит правую зону одной дельтой"
}

# Опрос query.panel: data.key панели равен want (или, с «!» впереди, отличается от него); значение — в KEY, чтение —
# <prefix>.stdout, иначе WHY.
KEY=""
wait_key() {
  local prefix="$1" panel="$2" want="$3" deadline
  WHY=""
  KEY="?"
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if panel_get "$profile" "$prefix" "$panel"; then
      KEY="$(pv "$prefix.stdout" data.key 2>/dev/null || echo "?")"
      if [[ "$want" == "!"* ]]; then
        [[ "$KEY" == "?" || "$KEY" == "${want#!}" ]] || return 0
      else
        [[ "$KEY" != "$want" ]] || return 0
      fi
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс data.key query.panel — $KEY, ожидалось $want"
      return 1
    fi
    sleep "$POLL"
  done
}

# Клавиша списка: lstep <каталог> <имя> <действие> <панель> <ожидаемый data.key> <аккорд>… — одна запись кольца
# list.* с источником key без отказа, выбор query.panel по ожиданию, фокус query.layout остался в панели.
lstep() {
  local dir="$1" name="$2" action="$3" panel="$4" want="$5"
  shift 5
  if ! act "$dir" "$name" "$action" "$@"; then
    failures+=("$name: $WHY")
    return 0
  fi
  [[ "$OUTCOME" == ok ]] || failures+=("$name: действие $action отказало: $OUTCOME")
  wait_key "$dir/$name.panel" "$panel" "$want" || failures+=("$name: $WHY")
  if layout_get "$profile" "$dir/$name.layout" "$wb_a"; then
    [[ "$(focus_of "$dir/$name.layout.stdout")" == "$panel" ]] ||
      failures+=("$name: фокус ушёл из списка: $(name_of "$(focus_of "$dir/$name.layout.stdout")")")
  else
    failures+=("$name: query.layout не прочитан")
  fi
  once "$dir" "$name" || failures+=("$name: $WHY")
}

# «Демо: список» в центре между левой и нижней зонами: j на последней строке и h остаются в списке. Клавиша без
# исхода судится по следующей: k после них уводит выбор с последней строки на предпоследнюю.
item_list_owns_keys() {
  local item=list-owns-keys dir="$run/list-owns-keys" demo first last prev
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  open_panel "$dir/open-demo" demo-list center
  demo="$PANEL"
  wait_focus "$dir/open-demo.layout" "$demo" || fatal "$item: $WHY"
  [[ "$(place_of "$dir/open-demo.layout.stdout" "$demo")" == center.0 ]] ||
    fatal "$item: подготовка: список не в части 0 центра: $(place_of "$dir/open-demo.layout.stdout" "$demo")"
  settle_layout "$profile" "$dir/start" "$wb_a" "$REV" || fatal "$item: $WHY"
  no_stubs "$dir/start.layout.stdout" || fatal "$item: подготовка: $WHY"
  wait_key "$dir/first" "$demo" "!?" || fatal "$item: подготовка: $WHY"
  first="$KEY"
  ui_cmd "$profile" "$dir/bottom" ui list bottom || fatal "$item: подготовка: $WHY"
  wait_key "$dir/last" "$demo" "!$first" || fatal "$item: подготовка: ui list bottom не сменил выбор: $WHY"
  last="$KEY"
  to_front "$dir" "$item"
  if act "$dir" key-k list.up "$K_K"; then
    wait_key "$dir/key-k.panel" "$demo" "!$last" || failures+=("key-k: $WHY")
    prev="$KEY"
    once "$dir" key-k || failures+=("key-k: $WHY")
    lstep "$dir" key-j list.down "$demo" "$last" "$K_J"
    lstep "$dir" key-j-last list.down "$demo" "$last" "$K_J"
    lstep "$dir" key-h list.collapse "$demo" "$last" "$K_H"
    lstep "$dir" key-k-after list.up "$demo" "$prev" "$K_K"
  else
    failures+=("key-k: $WHY")
  fi
  conclude "$item" "«Демо: список» в центре, слева и снизу — терминалы: k и j ведут выбор query.panel $last → ${prev:-?} → $last, j на последней строке и h оставляют выбор $last (следующий k уводит на ${prev:-?}), фокус query.layout остаётся в списке; на каждую клавишу одна запись ui.receipts list.* с источником key без отказа"
}

# Отметка кнопки зоны с названием label: значение AX-переключателя кнопки окна владельца — в MARK («нет» — кнопки
# с таким названием нет); вывод — <prefix>.stdout, иначе WHY.
MARK=""
zone_mark() {
  local prefix="$1" label="$2"
  WHY=""
  MARK=""
  osascript -l JavaScript -e "
    const p = Application('System Events').processes.whose({unixId: $owner_pid})[0];
    const b = p.windows[0].buttons.whose({description: '$label'});
    b.length ? String(b[0].attributes.byName('AXValue').value()) : 'нет'" >"$prefix.stdout" 2>"$prefix.stderr" || {
    WHY="AX кнопки «${label}» не прочитан: $(oneline "$prefix.stderr")"
    return 1
  }
  MARK="$(tr -d '[:space:]' <"$prefix.stdout")"
}

# Опрос отметки кнопки зоны до значения want: окно может отстать от query.layout; иначе WHY.
wait_mark() {
  local prefix="$1" label="$2" want="$3" deadline
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if zone_mark "$prefix" "$label" && [[ "$MARK" == "$want" ]]; then
      return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="${WHY:-за $MIRROR_MS мс отметка AX кнопки «${label}» — ${MARK:-?}, ожидалась $want}"
      return 1
    fi
    sleep "$POLL"
  done
}

# Клавиши видимости зоны дважды: zstep <каталог> <имя> <действие> <поле hidden> <название кнопки> <аккорд>… — первое
# нажатие скрывает, второе показывает; на каждое одна запись кольца, одна дельта ядра, layout.hidden по ожиданию,
# название третьей кнопки в ui.inspect и отметка AX кнопки (true — зона показана, false — скрыта).
zstep() {
  local dir="$1" name="$2" action="$3" field="$4" label="$5" round want mark
  shift 5
  for round in hide show; do
    want='$l->{hidden}{'"$field"'}'
    mark=false
    if [[ "$round" == show ]]; then
      want="!$want"
      mark=true
    fi
    kstep "$dir" "$name-$round" "$action" "layout.hidden.$field после $round" "$want" "$@"
    if ui_read "$profile" "$dir/$name-$round.inspect" inspect; then
      [[ "$(comp "$dir/$name-$round.inspect.stdout" "$ZONE_NODE" label 2>/dev/null || echo ?)" == "$ZONE_LABEL" ]] ||
        failures+=("$name-$round: третья кнопка в ui.inspect — «$(comp "$dir/$name-$round.inspect.stdout" "$ZONE_NODE" label 2>/dev/null || echo ?)», ожидалась «${ZONE_LABEL}»")
    else
      failures+=("$name-$round: ui.inspect не прочитан")
    fi
    wait_mark "$dir/$name-$round.mark-ax" "$label" "$mark" || failures+=("$name-$round: $WHY")
  done
}

# Cmd-0, Cmd-j, Cmd-Alt-0 и Space z h, j, l в двух раскладках: правая зона занята — третья кнопка «Правая зона»
# и цель side — правая зона; правая пуста, плитка левой занята — «Левая зона» и плитка. Клавиши идут при фокусе в
# теле панели центра.
ZONE_NODE=chrome.zone.right
ZONE_LABEL=""
item_zones_toggle() {
  local item=zones-toggle dir="$run/zones-toggle"
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  to_front "$dir" "$item"
  ZONE_LABEL="Правая зона"
  zstep "$dir" cmd-0 layout.toggle.navigator navigator "Навигация" "$K_0+command"
  zstep "$dir" cmd-j layout.toggle.bottom bottom "Нижняя зона" "$K_J+command"
  zstep "$dir" cmd-alt-0 layout.toggle.side right "Правая зона" "$K_0+command+option"
  zstep "$dir" space-z-h layout.toggle.navigator navigator "Навигация" "$K_SPACE" "$K_Z" "$K_H"
  zstep "$dir" space-z-j layout.toggle.bottom bottom "Нижняя зона" "$K_SPACE" "$K_Z" "$K_J"
  zstep "$dir" space-z-l layout.toggle.side right "Правая зона" "$K_SPACE" "$K_Z" "$K_L"
  # Вторая раскладка: правая зона пуста, плитка левой занята.
  core_cmd "$profile" "$dir/close-right" panel close "$p_right" --all || fatal "$item: $WHY"
  core_cmd "$profile" "$dir/focus-center" panel focus "$p_center" --all || fatal "$item: $WHY"
  settle_layout "$profile" "$dir/two" "$wb_a" "$REV" || fatal "$item: $WHY"
  [[ "$(parts_of "$dir/two.layout.stdout" right)" == 0 && "$(parts_of "$dir/two.layout.stdout" left)" == 1 ]] ||
    fatal "$item: подготовка второй раскладки: частей справа $(parts_of "$dir/two.layout.stdout" right), слева $(parts_of "$dir/two.layout.stdout" left)"
  ZONE_LABEL="Левая зона"
  zstep "$dir" two-cmd-alt-0 layout.toggle.side plate "Левая зона" "$K_0+command+option"
  zstep "$dir" two-space-z-l layout.toggle.side plate "Левая зона" "$K_SPACE" "$K_Z" "$K_L"
  conclude "$item" "правая зона занята: Cmd-0, Cmd-j, Cmd-Alt-0 и Space z h, j, l скрывают и показывают колонку навигатора, нижнюю и правую зоны, третья кнопка — «Правая зона»; правая пуста, плитка левой занята: Cmd-Alt-0 и Space z l скрывают и показывают плитку, третья кнопка — «Левая зона»; на каждое нажатие одна запись ui.receipts layout.toggle.* с источником key, одна дельта events --since, layout.hidden и отметка AX кнопки по ожиданию"
}

# Perl-условие: размер зоны layout.sizes.<поле> равен значению.
size_is() {
  printf '$l->{sizes}{%s} == %s' "$1" "$2"
}

# Высота кадра части зоны по query.layout; «?» — части нет.
part_height() {
  /usr/bin/perl -MJSON::PP -e 'my ($zone, $index) = @ARGV; local $/;
    my ($p) = grep { $_->{zone} eq $zone && $_->{index} == $index } @{JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{frames}{parts}};
    print $p ? $p->{frame}{height} : "?", "\n"' "$2" "$3" <"$1"
}

# Ctrl-w + - > < меняют размер на 40 точек: высота центра — нижнюю зону, ширина центра — правую, высота части
# правой зоны — её долю; Ctrl-w = выравнивает доли, Ctrl-w z разворачивает и возвращает, Ctrl-w o оставляет зону и
# возвращает. Шаг, после которого правая зона стала бы уже min_size, и высота колонки навигатора — отказ с
# сообщением и без дельты; ширину колонки навигатора меняют Ctrl-w > и <.
item_resize_zoom() {
  local item=resize-zoom dir="$run/resize-zoom" b r n nav t2 h canon code=0 min max
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  b="$(lv "$dir/bench/settled.layout.stdout" layout.sizes.bottom)"
  r="$(lv "$dir/bench/settled.layout.stdout" layout.sizes.right)"
  n="$(lv "$dir/bench/settled.layout.stdout" layout.sizes.navigator)"
  nav="$(lv "$dir/bench/settled.layout.stdout" layout.navigator)"
  [[ "$b" =~ ^[0-9]+$ && "$r" =~ ^[0-9]+$ && "$n" =~ ^[0-9]+$ && -n "$nav" && "$nav" != null ]] ||
    fatal "$item: подготовка: размеры зон или панель колонки навигатора не прочитаны: $b, $r, $n, $nav"
  # Границы отказа называет ядро: тот же шаг штатным CLI. Шире центр — уже правая зона.
  ctl "$profile" "$dir/cli-wider" --json --timeout "$CTL_TIMEOUT_MS" layout resize --by 40 --width --panel "$p_center" --all || code=$?
  min="$(jv "$dir/cli-wider.stdout" core_error.details.min 2>/dev/null || echo "?")"
  max="$(jv "$dir/cli-wider.stdout" core_error.details.max 2>/dev/null || echo "?")"
  [[ $code -ne 0 && "$(jv "$dir/cli-wider.stdout" core_error.details.reason 2>/dev/null || echo "?")" == resize_out_of_range &&
    "$min" =~ ^[0-9]+$ && "$max" =~ ^[0-9]+$ ]] && ((r - 40 < min && r >= min)) ||
    fatal "$item: подготовка: layout resize --by 40 --width панели центра при правой зоне $r не отказал по min_size: код $code, $(oneline "$dir/cli-wider.stdout")"
  to_front "$dir" "$item"
  kstep "$dir" grow layout.grow "нижняя зона $((b - 40))" "$(size_is bottom $((b - 40)))" "$K_W+control" "$K_EQUAL+shift"
  kstep "$dir" shrink layout.shrink "нижняя зона $b" "$(size_is bottom "$b")" "$K_W+control" "$K_MINUS"
  refused "$dir" min-size layout.wider "Размер зоны — от $min до $max точек" "$K_W+control" "$K_PERIOD+shift"
  kstep "$dir" narrower layout.narrower "правая зона $((r + 40))" "$(size_is right $((r + 40)))" "$K_W+control" "$K_COMMA+shift"
  wait_status "$dir/narrower.status" hidden || failures+=("narrower: $WHY")
  kstep "$dir" wider layout.wider "правая зона $r" "$(size_is right "$r")" "$K_W+control" "$K_PERIOD+shift"
  if layout_get "$profile" "$dir/sized" "$wb_a"; then
    no_stubs "$dir/sized.stdout" || failures+=("после шагов размера: $WHY")
    canon="$(layout_canon "$dir/sized.stdout")"
  else
    failures+=("query.layout после шагов размера не прочитан")
    canon=""
  fi
  kstep "$dir" zoom layout.zoom "панель центра развёрнута" \
    "(\$l->{zoomed} // '') eq '$p_center' && defined \$v->{frames}{zoomed}" "$K_W+control" "$K_Z"
  kstep "$dir" unzoom layout.zoom "раскладка возвращена" \
    "!defined \$l->{zoomed} && !defined \$v->{frames}{zoomed}" "$K_W+control" "$K_Z"
  kstep "$dir" only layout.only "видна только зона центра" \
    "(\$l->{only} // '') eq 'center' && !defined \$v->{frames}{navigator} && !grep { \$_->{zone} ne 'center' } @{\$v->{frames}{parts}}" \
    "$K_W+control" "$K_O"
  kstep "$dir" only-back layout.only "видны все зоны" \
    "!defined \$l->{only} && defined \$v->{frames}{navigator} && @{\$v->{frames}{parts}} == 4" "$K_W+control" "$K_O"
  if layout_get "$profile" "$dir/back" "$wb_a"; then
    [[ -z "$canon" || "$(layout_canon "$dir/back.stdout")" == "$canon" ]] ||
      failures+=("раскладка после разворота, одной зоны и возврата не равна прежней")
  else
    failures+=("query.layout после возврата не прочитан")
  fi
  # Две части правой зоны: шаг высоты меняет долю части, Ctrl-w = выравнивает доли.
  core_cmd "$profile" "$dir/split-right" layout split "$p_right" --open terminal --all || fatal "$item: $WHY"
  settle_layout "$profile" "$dir/two" "$wb_a" "$REV" || fatal "$item: $WHY"
  t2="$(lv "$dir/two.layout.stdout" 'layout.zones.right.parts[1].tabs[0]' 2>/dev/null || true)"
  [[ -n "$t2" && "$(parts_of "$dir/two.layout.stdout" right)" == 2 ]] ||
    fatal "$item: подготовка: в правой зоне не две части: $(lv "$dir/two.layout.stdout" layout.zones.right || echo "?")"
  core_cmd "$profile" "$dir/focus-t2" panel focus "$t2" --all || fatal "$item: $WHY"
  wait_focus "$dir/focus-t2.layout" "$t2" || fatal "$item: $WHY"
  no_stubs "$dir/focus-t2.layout.stdout" || fatal "$item: подготовка: $WHY"
  h="$(part_height "$dir/focus-t2.layout.stdout" right 1)"
  [[ "$h" =~ ^[0-9.]+$ ]] || fatal "$item: подготовка: высота второй части правой зоны не прочитана: $h"
  kstep "$dir" part-grow layout.grow "вторая часть правой зоны выше на 40 точек" \
    "my (\$p) = grep { \$_->{zone} eq 'right' && \$_->{index} == 1 } @{\$v->{frames}{parts}}; \$p && abs(\$p->{frame}{height} - ($h + 40)) <= 1 && \$l->{zones}{right}{parts}[1]{share} > \$l->{zones}{right}{parts}[0]{share}" \
    "$K_W+control" "$K_EQUAL+shift"
  kstep "$dir" equal layout.equal "доли частей правой зоны равны" \
    '$l->{zones}{right}{parts}[0]{share} == $l->{zones}{right}{parts}[1]{share}' "$K_W+control" "$K_EQUAL"
  # Колонка навигатора: ширина меняется, высоты у неё нет.
  core_cmd "$profile" "$dir/focus-nav" panel focus "$nav" --all || fatal "$item: $WHY"
  wait_focus "$dir/focus-nav.layout" "$nav" || fatal "$item: $WHY"
  kstep "$dir" nav-wider layout.wider "колонка навигатора $((n + 40))" "$(size_is navigator $((n + 40)))" "$K_W+control" "$K_PERIOD+shift"
  refused "$dir" nav-grow layout.grow "Панель стоит в колонке навигатора: частей у колонки нет" "$K_W+control" "$K_EQUAL+shift"
  kstep "$dir" nav-narrower layout.narrower "колонка навигатора $n" "$(size_is navigator "$n")" "$K_W+control" "$K_COMMA+shift"
  refused "$dir" nav-shrink layout.shrink "Панель стоит в колонке навигатора: частей у колонки нет" "$K_W+control" "$K_MINUS"
  # Следующий пункт начинает без сообщения в строке статуса.
  if act "$dir" leave panel.focus.right "$K_W+control" "$K_L"; then
    wait_status "$dir/leave.status" hidden || failures+=("leave: $WHY")
  else
    failures+=("leave: $WHY")
  fi
  conclude "$item" "Ctrl-w + и - меняют нижнюю зону $b → $((b - 40)) → $b, Ctrl-w < и > — правую $r → $((r + 40)) → $r, Ctrl-w > при правой зоне $r — отказ с сообщением «Размер зоны — от $min до $max точек» (причина resize_out_of_range — по той же команде CLI) без дельты; Ctrl-w z разворачивает панель центра и возвращает, Ctrl-w o оставляет зону центра и возвращает, раскладка после них прежняя; у двух частей правой зоны Ctrl-w + поднимает часть на 40 точек, Ctrl-w = выравнивает доли; в колонке навигатора Ctrl-w > и < меняют ширину $n → $((n + 40)) → $n, Ctrl-w + и - — отказ с сообщением без дельты; на каждую последовательность одна запись ui.receipts с источником key"
}

# Команда штатного CLI с одной дельтой ядра: cstep <каталог> <имя> <ожидание текстом> <perl-условие> <вызов>…
cstep() {
  local dir="$1" name="$2" what="$3" cond="$4"
  shift 4
  if ! mark "$dir" "$name"; then
    failures+=("$name: $WHY")
    return 0
  fi
  if ! ctl "$profile" "$dir/$name" --json --timeout "$CTL_TIMEOUT_MS" "$@"; then
    failures+=("$name: shellioctl $*: $(oneline "$dir/$name.stdout") $(oneline "$dir/$name.stderr")")
    return 0
  fi
  wait_layout "$dir/$name.layout" "$what" "$cond" || failures+=("$name: $WHY")
  events_are "$dir" "$name" 1 || failures+=("$name: $WHY")
}

# layout toggle side дважды: tstep <каталог> <имя> <поле hidden> <название третьей кнопки> — первое скрывает цель,
# второе показывает; одна дельта ядра, название кнопки в ui.inspect и отметка AX кнопки по ожиданию.
tstep() {
  local dir="$1" name="$2" field="$3" label="$4" round want mark got
  for round in hide show; do
    want='$l->{hidden}{'"$field"'}'
    mark=false
    if [[ "$round" == show ]]; then
      want="!$want"
      mark=true
    fi
    cstep "$dir" "$name-$round" "layout.hidden.$field после $round" "$want" layout toggle side
    if ui_read "$profile" "$dir/$name-$round.inspect" inspect; then
      got="$(comp "$dir/$name-$round.inspect.stdout" "$ZONE_NODE" label 2>/dev/null || echo "?")"
      [[ "$got" == "$label" ]] || failures+=("$name-$round: третья кнопка в ui.inspect — «${got}», ожидалась «${label}»")
    else
      failures+=("$name-$round: ui.inspect не прочитан")
    fi
    wait_mark "$dir/$name-$round.mark-ax" "$label" "$mark" || failures+=("$name-$round: $WHY")
  done
}

# shellioctl layout resize --by и layout toggle side в двух раскладках: правая зона занята — цель side правая зона,
# правая пуста и плитка левой занята — плитка, как у третьей кнопки заголовка.
item_relative_resize_cli() {
  local item=relative-resize-cli dir="$run/relative-resize-cli" b r
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  b="$(lv "$dir/bench/settled.layout.stdout" layout.sizes.bottom)"
  r="$(lv "$dir/bench/settled.layout.stdout" layout.sizes.right)"
  [[ "$b" =~ ^[0-9]+$ && "$r" =~ ^[0-9]+$ ]] || fatal "$item: подготовка: размеры зон не прочитаны: $b, $r"
  core_cmd "$profile" "$dir/focus-right" panel focus "$p_right" --all || fatal "$item: $WHY"
  wait_focus "$dir/focus-right.layout" "$p_right" || fatal "$item: $WHY"
  cstep "$dir" by-focus "правая зона $((r + 40))" "$(size_is right $((r + 40)))" \
    layout resize --by 40 --width --workbench "$wb_a" --all
  cstep "$dir" by-panel "правая зона $r" "$(size_is right "$r")" layout resize --by -40 --width --panel "$p_right" --all
  cstep "$dir" by-height "нижняя зона $((b + 40))" "$(size_is bottom $((b + 40)))" \
    layout resize --by 40 --height --panel "$p_bottom" --all
  if layout_get "$profile" "$dir/sized" "$wb_a"; then
    no_stubs "$dir/sized.stdout" || failures+=("после шагов размера: $WHY")
  else
    failures+=("query.layout после шагов размера не прочитан")
  fi
  tstep "$dir" side-right right "Правая зона"
  core_cmd "$profile" "$dir/close-right" panel close "$p_right" --all || fatal "$item: $WHY"
  core_cmd "$profile" "$dir/focus-center" panel focus "$p_center" --all || fatal "$item: $WHY"
  settle_layout "$profile" "$dir/two" "$wb_a" "$REV" || fatal "$item: $WHY"
  [[ "$(parts_of "$dir/two.layout.stdout" right)" == 0 && "$(parts_of "$dir/two.layout.stdout" left)" == 1 ]] ||
    fatal "$item: подготовка второй раскладки: частей справа $(parts_of "$dir/two.layout.stdout" right), слева $(parts_of "$dir/two.layout.stdout" left)"
  tstep "$dir" side-plate plate "Левая зона"
  conclude "$item" "layout resize --by 40 --width у панели с фокусом и --by -40 --width --panel ведут правую зону $r → $((r + 40)) → $r, --by 40 --height --panel нижней зоны — $b → $((b + 40)), каждая команда — одна дельта events --since; layout toggle side при занятой правой зоне скрывает и показывает правую зону (третья кнопка «Правая зона»), при пустой правой и занятой плитке — плитку («Левая зона»), отметка AX кнопки по ожиданию"
}

# Пункт меню окна владельца по System Events: режим find только находит пункт, press — нажимает его один раз.
AX_MENU_JS='function run(argv) {
  const [mode, pid, bar, title] = argv;
  const p = Application("System Events").processes.whose({unixId: Number(pid)})[0];
  const item = p.menuBars[0].menuBarItems.byName(bar).menus[0].menuItems.byName(title);
  if (item.name() !== title) throw new Error("нет пункта «" + title + "»");
  if (mode === "press") item.click();
  return mode + " " + title;
}'

# Нажатие пункта title меню bar владельца: сразу после запуска System Events отдаёт меню без пунктов, поэтому пункт
# сначала опрашивается чтением, затем нажимается один раз; вывод — <prefix>.find.* и <prefix>.press.*, иначе WHY.
menu_press() {
  local prefix="$1" bar="$2" title="$3" deadline
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  until osascript -l JavaScript -e "$AX_MENU_JS" find "$owner_pid" "$bar" "$title" >"$prefix.find.stdout" 2>"$prefix.find.stderr"; do
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс System Events не отдал пункт «${title}» меню «${bar}»: $(oneline "$prefix.find.stderr")"
      return 1
    fi
    sleep "$POLL"
  done
  osascript -l JavaScript -e "$AX_MENU_JS" press "$owner_pid" "$bar" "$title" >"$prefix.press.stdout" 2>"$prefix.press.stderr" || {
    WHY="пункт «${title}» меню «${bar}» не нажат: $(oneline "$prefix.press.stderr")"
    return 1
  }
}

# Пункт меню команды ядра: mstep <каталог> <имя> <действие> <ожидание текстом> <perl-условие> <меню> <пункт> — одна
# запись кольца действия с источником menu без отказа, условие над query.layout и одна дельта events --since.
mstep() {
  local dir="$1" name="$2" action="$3" what="$4" cond="$5" bar="$6" title="$7" got
  if ! mark "$dir" "$name"; then
    failures+=("$name: $WHY")
    return 0
  fi
  if ! receipts_last "$profile" "$dir/$name.before"; then
    failures+=("$name: $WHY")
    return 0
  fi
  BEFORE="$LAST"
  printf '%s AXPress «%s → %s»\n' "$(now_ms)" "$bar" "$title" >>"$dir/keys.log"
  if ! menu_press "$dir/$name.menu" "$bar" "$title"; then
    failures+=("$name: $WHY")
    return 0
  fi
  if ! wait_receipts "$profile" "$dir/$name.receipts" "$BEFORE" 1; then
    failures+=("$name: пункт «${title}»: $WHY")
    return 0
  fi
  got="$(rc "$dir/$name.receipts.stdout" 0 action) от $(rc "$dir/$name.receipts.stdout" 0 source)"
  [[ "$got" == "$action от menu" ]] || failures+=("$name: запись кольца «${got}», ожидалась «$action от menu»")
  got="$(rc "$dir/$name.receipts.stdout" 0 error)"
  [[ "$got" == нет ]] || failures+=("$name: действие $action отказало: $got")
  wait_layout "$dir/$name.layout" "$what" "$cond" || failures+=("$name: $WHY")
  events_are "$dir" "$name" 1 || failures+=("$name: $WHY")
  once "$dir" "$name" || failures+=("$name: $WHY")
}

# Cmd-0 клавишей и пункт «Вид → Навигация» нажатием AX: каждый путь скрывает и показывает колонку навигатора, на
# каждое нажатие одна запись кольца своего источника, одна дельта ядра и отметка AX кнопки «Навигация».
item_key_and_menu_once() {
  local item=key-and-menu-once dir="$run/key-and-menu-once" hidden='$l->{hidden}{navigator}'
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  to_front "$dir" "$item"
  kstep "$dir" key-hide layout.toggle.navigator "колонка навигатора скрыта" "$hidden" "$K_0+command"
  wait_mark "$dir/key-hide.mark-ax" "Навигация" false || failures+=("key-hide: $WHY")
  mstep "$dir" menu-show layout.toggle.navigator "колонка навигатора показана" "!$hidden" "Вид" "Навигация"
  wait_mark "$dir/menu-show.mark-ax" "Навигация" true || failures+=("menu-show: $WHY")
  mstep "$dir" menu-hide layout.toggle.navigator "колонка навигатора скрыта" "$hidden" "Вид" "Навигация"
  wait_mark "$dir/menu-hide.mark-ax" "Навигация" false || failures+=("menu-hide: $WHY")
  kstep "$dir" key-show layout.toggle.navigator "колонка навигатора показана" "!$hidden" "$K_0+command"
  wait_mark "$dir/key-show.mark-ax" "Навигация" true || failures+=("key-show: $WHY")
  conclude "$item" "Cmd-0 и пункт «Вид → Навигация» нажатием AX по очереди скрывают и показывают колонку навигатора: клавиша — одна запись ui.receipts layout.toggle.navigator с источником key, пункт меню — одна запись с источником menu, на каждое нажатие одна дельта events --since, layout.hidden.navigator и отметка AX кнопки «Навигация» по ожиданию"
}

# Cmd-w, затем Cmd-q во втором владельце на своём профиле. Владелец запущен прямо бинарём бандла: его PID равен
# PID задания оболочки, и wait отдаёт код выхода (open его не передаёт). Клавиша идёт только владельцу на переднем
# плане: после закрытия окна передний план сверяется заново, без него Cmd-q не отправляется.
item_window_quit() {
  local item=window-quit dir="$run/window-quit" state job code deadline seen="?"
  log "== $item"
  failures=()
  new_profile
  state="$NEW_PROFILE"
  (exec env "SHELLIO_STATE_DIR=$state" "$exe") >"$dir/owner.stdout" 2>"$dir/owner.stderr" &
  job=$!
  wait_owner "$state" "$dir/hello" ||
    fatal "$item: второй владелец не ответил на hello за $OWNER_START_S с: $(oneline "$dir/owner.stderr")"
  is_run_exe "$HELLO_PID" || fatal "$item: PID $HELLO_PID из hello — не бинарь бандла прогона: $(comm_of "$HELLO_PID")"
  [[ "$HELLO_PID" == "$job" ]] ||
    fatal "$item: PID владельца $HELLO_PID не равен PID задания $job: код выхода не прочитать"
  # act и once читают кольцо профиля пункта.
  local profile="$state" owner_pid="$job"
  to_front "$dir" "$item"
  if act "$dir" cmd-w window.close "$K_W+command"; then
    [[ "$OUTCOME" == ok ]] || failures+=("Cmd-w: действие window.close отказало: $OUTCOME")
    deadline=$(($(now_ms) + MIRROR_MS))
    while :; do
      if ui_read "$state" "$dir/closed.window" window; then
        seen="$(jv "$dir/closed.window.stdout" ok.ui.window.visible || echo "?")"
        [[ "$seen" != false ]] || break
      fi
      if (($(now_ms) >= deadline)); then
        failures+=("Cmd-w: за $MIRROR_MS мс ui.window.visible — $seen, ожидалось false")
        break
      fi
      sleep "$POLL"
    done
    once "$dir" cmd-w || failures+=("Cmd-w: $WHY")
    alive "$job" || failures+=("Cmd-w: процесс владельца вышел вместе с окном")
  else
    failures+=("Cmd-w: $WHY")
  fi
  if ! alive "$job"; then
    :
  elif front_ok "$job" "$dir"; then
    key "$dir/keys.log" "$K_Q" command || failures+=("Cmd-q: клавиша не отправлена: $(tail -n 1 "$dir/keys.log")")
  else
    failures+=("Cmd-q не отправлен: владелец без окна не на переднем плане: $WHY")
  fi
  if wait_gone "$job" "$EXIT_S"; then
    wait "$job" && code=0 || code=$?
    [[ $code -eq 0 ]] || failures+=("процесс владельца вышел с кодом $code, ожидался 0")
    ! lock_held "$state" || failures+=("после выхода владельца блокировку профиля держат")
  else
    failures+=("владелец pid $job не вышел за $EXIT_S с после Cmd-q")
  fi
  conclude "$item" "второй владелец прямым запуском бинаря бандла на своём профиле: Cmd-w — одна запись ui.receipts window.close с источником key, ui.window.visible — false, процесс жив; Cmd-q владельцу на переднем плане — процесс вышел с кодом 0 и отпустил блокировку профиля"
}

# Cmd-Alt-t трижды: системное → светлое → тёмное → системное по ui.window.
# Опрос ui.mode: режим панели с фокусом — want (navigation или input) и подпись плашки status.mode в ui.inspect ему
# соответствует; чтения — <prefix>.stdout и <prefix>.inspect.stdout, иначе WHY.
wait_mode() {
  local prefix="$1" want="$2" deadline got="?" plate="?" label="НАВИГАЦИЯ"
  WHY=""
  [[ "$want" != input ]] || label="ВВОД"
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ctl "$profile" "$prefix" --json --timeout "$CTL_TIMEOUT_MS" ui mode get; then
      got="$(jv "$prefix.stdout" ok.ui.mode.mode 2>/dev/null || echo "?")"
      if [[ "$got" == "$want" ]] && ui_read "$profile" "$prefix.inspect" inspect; then
        plate="$(comp "$prefix.inspect.stdout" status.mode label 2>/dev/null || echo "?")"
        [[ "$plate" != "$label" ]] || return 0
      fi
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс режим ui.mode — $got, плашка — «${plate}», ожидались $want и «${label}»"
      return 1
    fi
    sleep "$POLL"
  done
}

# Опрос строки фильтра панели в ui.inspect: open — строка видна и её поле держит ввод, closed — узел строки есть,
# строка скрыта и ввода в поле нет, fresh — строка скрыта или её узла ещё нет (строка не открывалась); текст поля — в
# ROW_TEXT; чтение — <prefix>.stdout, иначе WHY.
ROW_TEXT=""
wait_row() {
  local prefix="$1" panel="$2" want="$3" deadline seen="?" editing="?" id="layout.panel.$2.filter"
  WHY=""
  ROW_TEXT=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$profile" "$prefix" inspect; then
      seen="$(comp "$prefix.stdout" "$id" visible 2>/dev/null || echo "?")"
      editing="$(comp "$prefix.stdout" "$id.input" state.text_field.editing 2>/dev/null || echo "?")"
      ROW_TEXT="$(comp "$prefix.stdout" "$id.input" state.text_field.value 2>/dev/null || true)"
      case "$want" in
        open) [[ "$seen" != true || "$editing" != true ]] || return 0 ;;
        closed) [[ "$seen" != false || "$editing" == true ]] || return 0 ;;
        *) [[ "$seen" == true || "$editing" == true ]] || return 0 ;;
      esac
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс строка фильтра: видна — $seen, ввод — $editing, ожидалось «${want}»"
      return 1
    fi
    sleep "$POLL"
  done
}

# Строки списка панели в ui.inspect, чьё название содержит text без учёта регистра: ключи через пробел. Это
# независимое ожидание сужения: его считает сценарий по полному списку, а не плагин.
rows_with() {
  /usr/bin/perl -MJSON::PP -Mutf8 -e '
    my ($file, $id, $text) = @ARGV;
    utf8::decode($text);
    open my $fh, "<", $file or exit 2;
    local $/;
    my $v = eval { JSON::PP->new->utf8->decode(<$fh>) };
    exit 2 if $@;
    my ($list) = grep { $_->{id} eq $id } @{ $v->{ok}{ui}{inspect}{components} };
    exit 1 unless $list;
    print join(" ", map { $_->{id} } grep { index(lc $_->{text}, lc $text) >= 0 } @{ $list->{state}{virtual_list}{visible} }), "\n";
  ' "$1" "layout.panel.$2.list" "$3"
}

# Опрос фильтра: data.filter query.panel равен text и показанные строки списка в ui.inspect — want (ключи через
# пробел); чтения — <prefix>.panel.stdout и <prefix>.stdout, иначе WHY.
wait_filter() {
  local prefix="$1" panel="$2" text="$3" want="$4" deadline got="?" rows="?"
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if panel_get "$profile" "$prefix.panel" "$panel"; then
      got="$(pv "$prefix.panel.stdout" data.filter 2>/dev/null || echo "?")"
      if [[ "$got" == "$text" ]] && ui_read "$profile" "$prefix" inspect; then
        rows="$(rows_with "$prefix.stdout" "$panel" "" 2>/dev/null || echo "?")"
        [[ "$rows" != "$want" ]] || return 0
      fi
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс фильтр query.panel — «${got}», строки списка — «${rows}», ожидались «${text}» и «${want}»"
      return 1
    fi
    sleep "$POLL"
  done
}

# Клавиша / у списка: одна запись кольца panel.filter с источником key без отказа, строка фильтра открыта с текстом
# text и держит ввод, режим панели — input.
fopen() {
  local dir="$1" name="$2" panel="$3" text="$4"
  if ! act "$dir" "$name" panel.filter "$K_SLASH"; then
    failures+=("$name: $WHY")
    return 0
  fi
  [[ "$OUTCOME" == ok ]] || failures+=("$name: действие panel.filter отказало: $OUTCOME")
  if wait_row "$dir/$name.row" "$panel" open; then
    [[ "$ROW_TEXT" == "$text" ]] || failures+=("$name: текст строки фильтра «${ROW_TEXT}», ожидался «${text}»")
  else
    failures+=("$name: $WHY")
  fi
  wait_mode "$dir/$name.mode" input || failures+=("$name: $WHY")
  once "$dir" "$name" || failures+=("$name: $WHY")
}

# Клавиша поля без записи кольца (буква, Enter, Esc): fkey <каталог> <имя> <код>; отказ отправки — в failures.
fkey() {
  key "$1/keys.log" "$3" || failures+=("$2: клавиша $3 не отправлена: $(tail -n 1 "$1/keys.log")")
}

# Фильтр «Демо: список»: /, буква, Enter, второй /, Esc клавишами; затем непустое сужение — текст кириллицей командой
# CLI в открытую строку и Esc клавишей в ней. Буква под латинским источником названий списка (кириллица) не находит:
# ход клавишами судит текст фильтра и пустой список, а строки между нулём и всеми — ход с текстом CLI. Ожидаемые
# строки считает сценарий по полному списку. Клавиши поля записей кольца не дают: закрытие судят строка, режим и
# следующая клавиша с записью — второй / и j.
item_filter() {
  local item=filter dir="$run/filter" demo all typed want first text="ета" limit
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  open_panel "$dir/open-demo" demo-list center
  demo="$PANEL"
  wait_focus "$dir/open-demo.layout" "$demo" || fatal "$item: $WHY"
  settle_layout "$profile" "$dir/start" "$wb_a" "$REV" || fatal "$item: $WHY"
  no_stubs "$dir/start.layout.stdout" || fatal "$item: подготовка: $WHY"
  wait_key "$dir/first" "$demo" "!?" || fatal "$item: подготовка: $WHY"
  first="$KEY"
  wait_row "$dir/start.row" "$demo" fresh || fatal "$item: подготовка: $WHY"
  all="$(rows_with "$dir/start.row.stdout" "$demo" "")" || fatal "$item: подготовка: список не прочитан в ui.inspect"
  [[ "$(wc -w <<<"$all" | tr -d ' ')" -ge 4 ]] || fatal "$item: подготовка: в списке меньше четырёх строк: «${all}»"
  wait_filter "$dir/start.filter" "$demo" "" "$all" || fatal "$item: подготовка: $WHY"
  wait_mode "$dir/start.mode" navigation || fatal "$item: подготовка: $WHY"
  to_front "$dir" "$item"

  fopen "$dir" slash "$demo" ""
  fkey "$dir" letter "$K_B"
  typed=""
  if wait_row "$dir/letter.row" "$demo" open; then
    limit=$(($(now_ms) + MIRROR_MS))
    while [[ -z "$ROW_TEXT" ]] && (($(now_ms) < limit)); do
      sleep "$POLL"
      wait_row "$dir/letter.row" "$demo" open || break
    done
    typed="$ROW_TEXT"
  fi
  if [[ -n "$typed" ]]; then
    want="$(rows_with "$dir/start.row.stdout" "$demo" "$typed")"
    wait_filter "$dir/letter.filter" "$demo" "$typed" "$want" || failures+=("буква: $WHY")
    wait_mode "$dir/letter.mode" input || failures+=("буква: $WHY")
    fkey "$dir" enter "$K_ENTER"
    wait_row "$dir/enter.row" "$demo" closed || failures+=("Enter: $WHY")
    wait_mode "$dir/enter.mode" navigation || failures+=("Enter: $WHY")
    wait_filter "$dir/enter.filter" "$demo" "$typed" "$want" || failures+=("Enter: фильтр не остался: $WHY")
    fopen "$dir" slash-again "$demo" "$typed"
    fkey "$dir" esc "$K_ESC"
    wait_row "$dir/esc.row" "$demo" closed || failures+=("Esc: $WHY")
    wait_filter "$dir/esc.filter" "$demo" "" "$all" || failures+=("Esc: фильтр не снят: $WHY")
    wait_mode "$dir/esc.mode" navigation || failures+=("Esc: $WHY")
  else
    failures+=("буква: строка фильтра текста не получила")
  fi

  want="$(rows_with "$dir/start.row.stdout" "$demo" "$text")"
  if [[ -z "$want" || "$want" == "$all" ]]; then
    failures+=("текст «${text}» список не сужает: строки «${want}» из «${all}»")
  elif ui_cmd "$profile" "$dir/cli-filter" ui filter "$text"; then
    if wait_row "$dir/cli.row" "$demo" open; then
      [[ "$ROW_TEXT" == "$text" ]] || failures+=("ui filter: текст строки фильтра «${ROW_TEXT}», ожидался «${text}»")
    else
      failures+=("ui filter: $WHY")
    fi
    wait_filter "$dir/cli.filter" "$demo" "$text" "$want" || failures+=("ui filter: $WHY")
    wait_mode "$dir/cli.mode" input || failures+=("ui filter: $WHY")
    to_front "$dir" "$item"
    fkey "$dir" esc-open "$K_ESC"
    wait_row "$dir/esc-open.row" "$demo" closed || failures+=("Esc в открытой строке: $WHY")
    wait_filter "$dir/esc-open.filter" "$demo" "" "$all" || failures+=("Esc в открытой строке: фильтр не снят: $WHY")
    wait_mode "$dir/esc-open.mode" navigation || failures+=("Esc в открытой строке: $WHY")
  else
    failures+=("ui filter: $WHY")
  fi

  if act "$dir" key-j list.down "$K_J"; then
    [[ "$OUTCOME" == ok ]] || failures+=("j после фильтра: действие list.down отказало: $OUTCOME")
    wait_key "$dir/key-j.panel" "$demo" "!$first" || failures+=("j после фильтра: $WHY")
    once "$dir" key-j || failures+=("j после фильтра: $WHY")
  else
    failures+=("j после фильтра: $WHY")
  fi
  conclude "$item" "«Демо: список»: / открывает строку фильтра (одна запись ui.receipts panel.filter с источником key, режим input, плашка «ВВОД»), буква «${typed:-?}» ставит data.filter query.panel и оставляет строки, чьё название её содержит; Enter закрывает строку, фильтр остаётся, режим — navigation; второй / открывает строку с тем же текстом одной записью кольца, Esc снимает фильтр и возвращает все строки; текст «${text}» командой CLI оставляет строки «${want}» из «${all}», Esc в открытой строке снимает фильтр; j после этого — одна запись list.down"
}

# Чтение ui.history: «точек до конца от курсора|область и панель точки под курсором|панели последних k точек».
hist_fact() {
  /usr/bin/perl -MJSON::PP -e '
    my ($file, $k) = @ARGV;
    open my $fh, "<", $file or exit 2;
    local $/;
    my $h = eval { JSON::PP->new->utf8->decode(<$fh>)->{ok}{ui}{history} };
    exit 2 if $@ || ref $h ne "HASH" || ref $h->{points} ne "ARRAY" || !defined $h->{cursor};
    my @p = @{ $h->{points} };
    my $at = $p[$h->{cursor}] or exit 1;
    my $from = @p > $k ? @p - $k : 0;
    print $#p - $h->{cursor}, "|$at->{workbench} $at->{panel}|", join(" ", map { $_->{panel} } @p[$from .. $#p]), "\n";
  ' "$1" "$2"
}

# Опрос места после перехода: открыта рабочая область bench (route tree), фокус её query.layout — panel, точка
# ui.history под курсором — эта область и панель, от курсора до конца dist точек, панели последних точек — tail.
# Чтения — <prefix>.tree.stdout, <prefix>.layout.stdout и <prefix>.history.stdout, иначе WHY.
hwait() {
  local prefix="$1" bench="$2" panel="$3" dist="$4" tail="$5" deadline k route="?" focus="?" fact="?"
  WHY=""
  k="$(wc -w <<<"$tail" | tr -d ' ')"
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ctl "$profile" "$prefix.tree" --json --timeout "$CTL_TIMEOUT_MS" tree --all &&
      layout_get "$profile" "$prefix.layout" "$bench" &&
      ctl "$profile" "$prefix.history" --json --timeout "$CTL_TIMEOUT_MS" ui history get; then
      route="$(jv "$prefix.tree.stdout" ok.value.route.workbench 2>/dev/null || echo "?")"
      focus="$(focus_of "$prefix.layout.stdout" 2>/dev/null || echo "?")"
      fact="$(hist_fact "$prefix.history.stdout" "$k" 2>/dev/null || echo "?")"
      [[ "$route" != "$bench" || "$focus" != "$panel" || "$fact" != "$dist|$bench $panel|$tail" ]] || return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс открытая область — ${route}, фокус — ${focus}, история — «${fact}»; ожидались область ${bench}, фокус ${panel}, история «$dist|$bench $panel|${tail}»"
      return 1
    fi
    sleep "$POLL"
  done
}

# Шаг истории клавишей: hstep <каталог> <имя> <действие> <аккорд> <исход> <область> <панель> <до конца> <хвост> —
# одна запись кольца с источником key и исходом want (ok или not_applicable) и место по hwait.
hstep() {
  local dir="$1" name="$2" action="$3" chord="$4" want="$5"
  shift 5
  if ! act "$dir" "$name" "$action" "$chord"; then
    failures+=("$name: $WHY")
    return 0
  fi
  [[ "$OUTCOME" == "$want" ]] || failures+=("$name: исход действия $action — ${OUTCOME}, ожидался ${want}")
  hwait "$dir/$name" "$@" || failures+=("$name: $WHY")
  once "$dir" "$name" || failures+=("$name: $WHY")
}

# История переходов на терминалах (поле ввода при возврате берёт ввод себе и клавиши Navigation дальше не идут):
# A → B → A клавишами, Ctrl-o и Ctrl-i, край вперёд, ветка C после возврата, закрытая цель, другая рабочая область.
# История — на воркспейс и глубиной 100 точек, прежние пункты её уже наполнили: судятся хвост точек и расстояние
# курсора от конца, а не номера. Шаг в другую область — одна запись кольца и две команды ядра, одной дельтой он
# не судится.
item_history() {
  local item=history dir="$run/history" x a b c y d name back="$K_O+control" fwd="$K_I+control"
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  x="$wb_a"
  a="$p_center"
  b="$p_right"
  c="$p_bottom"
  to_front "$dir" "$item"
  step "$item" "$dir" jump-b panel.focus.right "$b" "$K_L"
  step "$item" "$dir" jump-a panel.focus.left "$a" "$K_H"
  hwait "$dir/start" "$x" "$a" 0 "$a $b $a" || fatal "$item: подготовка: $WHY"
  log "$item: точек истории воркспейса $(jv "$dir/start.history.stdout" 'ok.ui.history.points[#]' || echo "?")"

  hstep "$dir" back-b history.back "$back" ok "$x" "$b" 1 "$a $b $a"
  hstep "$dir" forward-a history.forward "$fwd" ok "$x" "$a" 0 "$a $b $a"
  hstep "$dir" forward-edge history.forward "$fwd" not_applicable "$x" "$a" 0 "$a $b $a"
  hstep "$dir" back-b-again history.back "$back" ok "$x" "$b" 1 "$a $b $a"
  if core_cmd "$profile" "$dir/branch" panel focus "$c" --all; then
    hwait "$dir/branch-c" "$x" "$c" 0 "$a $b $c" || failures+=("ветка C: $WHY")
  else
    failures+=("ветка C: $WHY")
  fi
  to_front "$dir" "$item"
  hstep "$dir" forward-cut history.forward "$fwd" not_applicable "$x" "$c" 0 "$a $b $c"
  if core_cmd "$profile" "$dir/close-b" panel close "$b" --all; then
    hwait "$dir/closed-b" "$x" "$c" 0 "$a $b $c" || failures+=("закрытие B: $WHY")
  else
    failures+=("закрытие B: $WHY")
  fi
  to_front "$dir" "$item"
  hstep "$dir" back-over-closed history.back "$back" ok "$x" "$a" 2 "$a $b $c"
  hstep "$dir" forward-over-closed history.forward "$fwd" ok "$x" "$c" 0 "$a $b $c"

  mkdir -p "$dir/other"
  bench_n=$((bench_n + 1))
  name="b$bench_n-$$"
  core_cmd "$profile" "$dir/other/new" workbench new --target "$project" "$name" --workspace "$ws" || fatal "$item: $WHY"
  y="$(tree_id "$dir/other" bench "$name")" || fatal "$item: рабочая область $name не найдена в tree"
  core_cmd "$profile" "$dir/other/open" workbench open --target "$y" --workspace "$ws" || fatal "$item: $WHY"
  settle_layout "$profile" "$dir/other/settle" "$y" "$REV" || fatal "$item: $WHY"
  d="$(panels_of "$dir/other/settle.layout.stdout" terminal)"
  [[ -n "$d" && "$d" != *" "* ]] || fatal "$item: в новой рабочей области нет одного терминала: «${d}»"
  no_stubs "$dir/other/settle.layout.stdout" || fatal "$item: подготовка: $WHY"
  hwait "$dir/other-d" "$y" "$d" 0 "$a $b $c $d" || failures+=("другая область: $WHY")
  to_front "$dir" "$item"
  hstep "$dir" back-other history.back "$back" ok "$x" "$c" 1 "$a $b $c $d"
  hstep "$dir" forward-other history.forward "$fwd" ok "$y" "$d" 0 "$a $b $c $d"
  conclude "$item" "терминалы без заглушек: l и h дают точки A → B → A; Ctrl-o возвращает к B, Ctrl-i — к A, Ctrl-i на конце — запись not_applicable без перехода; после Ctrl-o переход к C отсекает ветку вперёд (хвост A, B, C, Ctrl-i — not_applicable); закрытую B шаги назад и вперёд пропускают; Ctrl-o из другой рабочей области открывает прежнюю с фокусом на C, Ctrl-i возвращает; на каждую клавишу одна запись ui.receipts history.back или history.forward с источником key, место судят route tree, фокус query.layout и ui.history"
}

# Узлы ui.inspect с первым ответчиком окна: id через запятую, пусто — первый ответчик не узел (окно).
responder_of() {
  /usr/bin/perl -MJSON::PP -e '
    open my $fh, "<", $ARGV[0] or exit 2;
    local $/;
    my $v = eval { JSON::PP->new->utf8->decode(<$fh>) };
    exit 2 if $@ || ref $v->{ok}{ui}{inspect}{components} ne "ARRAY";
    print join(",", map { $_->{id} } grep { $_->{first_responder} } @{ $v->{ok}{ui}{inspect}{components} }), "\n";
  ' "$1"
}

# Наибольшее число нажатий Tab до поля: цепочка ключевых видов окна идёт от кнопок окна через полосы вкладок.
TAB_MAX=40

# «Демо: поле»: панель открывается с вводом в поле, Esc возвращает Navigation. Tab маршрутизатор в Navigation не
# глотает: он идёт по цепочке ключевых видов окна и доходит до поля, поле с вводом переводит панель в Input. Затем
# текст, Esc и клавиши с записью кольца: i (mode.input) и h (panel.focus.left). Клавиши поля, Tab и Esc записей не
# дают — после всего хода в кольце ровно эти две.
item_field_input() {
  local item=field-input dir="$run/field-input" demo field start prev now deadline n chain="" reached=0 seen
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  open_panel "$dir/open-demo" demo-native center
  demo="$PANEL"
  field="layout.panel.$demo.field"
  wait_focus "$dir/open-demo.layout" "$demo" || fatal "$item: $WHY"
  settle_layout "$profile" "$dir/start" "$wb_a" "$REV" || fatal "$item: $WHY"
  no_stubs "$dir/start.layout.stdout" || fatal "$item: подготовка: $WHY"
  wait_mode "$dir/start.mode" input || fatal "$item: подготовка: $WHY"
  receipts_last "$profile" "$dir/start.receipts" || fatal "$item: подготовка: $WHY"
  start="$LAST"
  to_front "$dir" "$item"

  fkey "$dir" esc-open "$K_ESC"
  wait_mode "$dir/esc-open.mode" navigation || failures+=("Esc после открытия: $WHY")
  wait_inspect "$profile" "$dir/esc-open.field" "$field" state.text_field.editing false ||
    failures+=("Esc после открытия: $WHY")
  prev="$(responder_of "$dir/esc-open.field.stdout" 2>/dev/null || echo "?")"
  [[ -z "$prev" ]] || failures+=("Esc после открытия: первый ответчик — «${prev}», ожидалось окно")

  if ((${#failures[@]} == 0)); then
    for ((n = 1; n <= TAB_MAX; n++)); do
      fkey "$dir" "tab-$n" "$K_TAB"
      deadline=$(($(now_ms) + MIRROR_MS))
      now="$prev"
      while [[ "$now" == "$prev" ]] && (($(now_ms) < deadline)); do
        ui_read "$profile" "$dir/tab-$n" inspect && now="$(responder_of "$dir/tab-$n.stdout" 2>/dev/null || echo "$prev")"
        [[ "$now" != "$prev" ]] || sleep "$POLL"
      done
      if [[ "$now" == "$prev" ]]; then
        failures+=("Tab $n: первый ответчик за $MIRROR_MS мс не сменился: «${prev}»")
        break
      fi
      seen=" $chain "
      chain="${chain:+$chain }$now"
      prev="$now"
      if [[ "$now" == "$field" ]]; then
        reached=$n
        break
      fi
      if [[ "$seen" == *" $now "* ]]; then
        failures+=("Tab $n: цепочка ключевых видов замкнулась на «${now}» без поля: ${chain}")
        break
      fi
    done
    printf '%s\n' "$chain" | tr ' ' '\n' >"$dir/tab-chain.txt"
    ((reached > 0 || ${#failures[@]} > 0)) || failures+=("Tab: за $TAB_MAX нажатий поле ввод не получило: ${chain}")
  fi

  if ((reached > 0)); then
    wait_mode "$dir/tab.mode" input || failures+=("Tab в поле: $WHY")
    wait_inspect "$profile" "$dir/tab.field" "$field" state.text_field.editing true || failures+=("Tab в поле: $WHY")
  fi
  if ((reached > 0 && ${#failures[@]} == 0)); then
    fkey "$dir" letter-a "$K_A"
    fkey "$dir" letter-b "$K_B"
    fkey "$dir" letter-c "$K_C"
    wait_inspect "$profile" "$dir/typed.field" "$field" state.text_field.value abc || failures+=("ввод: $WHY")
    wait_mode "$dir/typed.mode" input || failures+=("ввод: $WHY")
    fkey "$dir" esc "$K_ESC"
    wait_mode "$dir/esc.mode" navigation || failures+=("Esc: $WHY")
    wait_inspect "$profile" "$dir/esc.field" "$field" state.text_field.editing false || failures+=("Esc: $WHY")
    wait_focus "$dir/esc.layout" "$demo" || failures+=("Esc: $WHY")
    if act "$dir" key-i mode.input "$K_I"; then
      [[ "$OUTCOME" == ok ]] || failures+=("i: действие mode.input отказало: $OUTCOME")
      wait_mode "$dir/key-i.mode" input || failures+=("i: $WHY")
      once "$dir" key-i || failures+=("i: $WHY")
    else
      failures+=("i: $WHY")
    fi
    fkey "$dir" esc-i "$K_ESC"
    wait_mode "$dir/esc-i.mode" navigation || failures+=("Esc после i: $WHY")
    step "$item" "$dir" key-h panel.focus.left "$p_left" "$K_H"
    if receipts_get "$profile" "$dir/all.receipts" "$start"; then
      seen="$(jv "$dir/all.receipts.stdout" 'ok.ui.receipts.receipts[#]' || echo "?") $(rc "$dir/all.receipts.stdout" 0 action) $(rc "$dir/all.receipts.stdout" 1 action)"
      [[ "$seen" == "2 mode.input panel.focus.left" ]] ||
        failures+=("записи кольца за весь ход — «${seen}», ожидались две: mode.input и panel.focus.left")
    else
      failures+=("ui.receipts не прочитан: $(oneline "$dir/all.receipts.stderr")")
    fi
  fi
  conclude "$item" "«Демо: поле» открыта с вводом в поле; Esc — режим navigation и плашка «НАВИГАЦИЯ», первый ответчик — окно; Tab ${reached} раз по цепочке ключевых видов окна доводит ввод до поля — режим input, плашка «ВВОД»; клавиши a, b, c — текст поля «abc»; Esc — navigation, «НАВИГАЦИЯ», фокус query.layout в панели; i — одна запись ui.receipts mode.input с источником key и режим input, Esc и h — одна запись panel.focus.left; Tab, буквы и Esc записей кольца не дают"
}

# --- keymap.toml и источник ввода ---

# keymap.toml основного профиля без ожидания: text — новый файл (копия — <prefix>.keymap.toml), пусто — файла нет.
keymap_put() {
  local prefix="$1" text="$2"
  mkdir -p "$profile/config"
  if [[ -n "$text" ]]; then
    printf '%s\n' "$text" >"$profile/config/keymap.toml.new"
    cp "$profile/config/keymap.toml.new" "$prefix.keymap.toml"
    mv "$profile/config/keymap.toml.new" "$profile/config/keymap.toml"
  else
    rm -f "$profile/config/keymap.toml"
  fi
}

# Опрос query.keymap: perl-условие над ok.keymap ($k; rows(<контекст>, <последовательность>) — действия строк)
# истинно; чтение — <prefix>.stdout, иначе WHY с текстом ожидания.
wait_keymap() {
  local prefix="$1" what="$2" cond="$3" deadline
  WHY=""
  deadline=$(($(now_ms) + KEYMAP_MS))
  while :; do
    if ctl "$profile" "$prefix" --json --timeout "$CTL_TIMEOUT_MS" keymap list &&
      /usr/bin/perl -MJSON::PP -e 'my $cond = shift; local $/; my $k = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{keymap};
        sub rows { my ($c, $s) = @_; map { $_->{action} } grep { $_->{context} eq $c && $_->{sequence} eq $s } @{$k->{bindings}} }
        exit(eval($cond) ? 0 : 1)' "$cond" <"$prefix.stdout" 2>/dev/null; then
      return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $KEYMAP_MS мс query.keymap после правки keymap.toml не показал: $what"
      return 1
    fi
    sleep "$POLL"
  done
}

# Версия и строки query.keymap каноническим JSON: равенство до и после нечитаемого файла.
keymap_canon() {
  /usr/bin/perl -MJSON::PP -e 'local $/; my $k = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{keymap};
    print JSON::PP->new->utf8->canonical->encode({ version => $k->{version}, bindings => $k->{bindings} }), "\n"' <"$1"
}

# Файла keymap.toml нет, таблица — по умолчанию, сообщения строки статуса нет; иначе ошибка исполнения: следующие
# пункты идут на таблице по умолчанию.
keymap_clean() {
  local item="$1" dir="$2"
  keymap_put "$dir/restore" ""
  wait_keymap "$dir/restore" "таблица без строк из файла и без диагностики" \
    '!@{$k->{diagnostics}} && !grep { $_->{source} ne "default" } @{$k->{bindings}}' ||
    fatal "$item: keymap.toml не убран: $WHY"
  wait_status "$dir/restore.status" hidden || fatal "$item: после уборки keymap.toml: $WHY"
}

# Подсказка кнопки «Развернуть панель» части 0 центра: поле help узла ui.inspect и AXHelp кнопки окна с этим
# описанием равны want. Таблицу keymap главный поток получает позже query.keymap: клавиши после правки идут после
# этого опроса. Чтения — <prefix>.stdout и <prefix>.ax.stdout, иначе WHY.
ZOOM_LABEL="Развернуть панель"
wait_help() {
  local prefix="$1" want="$2" deadline got="?" ax="?"
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$profile" "$prefix" inspect; then
      got="$(comp "$prefix.stdout" layout.part.center.0.tabs.zoom help 2>/dev/null || echo "?")"
      if [[ "$got" == "$want" ]]; then
        ax="$(osascript -l JavaScript -e "
          const p = Application('System Events').processes.whose({unixId: $owner_pid})[0];
          const out = [];
          function walk(e, d) {
            if (d > 7) return;
            let kids = [];
            try { kids = e.uiElements(); } catch (x) { return; }
            for (const k of kids) {
              let ds = null;
              try { ds = k.description(); } catch (x) {}
              if (ds === '$ZOOM_LABEL') { try { out.push(String(k.attributes.byName('AXHelp').value())); } catch (x) { out.push('нет'); } }
              walk(k, d + 1);
            }
          }
          walk(p.windows[0], 0);
          out.join('|')" 2>"$prefix.ax.stderr" | tee "$prefix.ax.stdout" || true)"
        # У каждой части окна своя кнопка: все называют одну клавишу.
        if [[ -n "$ax" ]] && ! tr '|' '\n' <<<"$ax" | grep -qvxF "$want"; then
          return 0
        fi
      fi
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс подсказка кнопки «${ZOOM_LABEL}»: help ui.inspect — «${got}», AXHelp — «${ax}», ожидалось «${want}»"
      return 1
    fi
    sleep "$POLL"
  done
}

# Эквивалент пункта title меню bar по AX: «символ|модификаторы» (AXMenuItemCmdChar и AXMenuItemCmdModifiers: 0 — ⌘,
# 2 — ⌥⌘) равен want; чтение — <prefix>.stdout, иначе WHY.
AX_MENU_KEY_JS='function run(argv) {
  const [pid, bar, title] = argv;
  const p = Application("System Events").processes.whose({unixId: Number(pid)})[0];
  const item = p.menuBars[0].menuBarItems.byName(bar).menus[0].menuItems.byName(title);
  if (item.name() !== title) throw new Error("нет пункта «" + title + "»");
  const a = n => { try { return String(item.attributes.byName(n).value()); } catch (x) { return "нет"; } };
  return a("AXMenuItemCmdChar") + "|" + a("AXMenuItemCmdModifiers");
}'
wait_menu_key() {
  local prefix="$1" bar="$2" title="$3" want="$4" deadline got="?"
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if osascript -l JavaScript -e "$AX_MENU_KEY_JS" "$owner_pid" "$bar" "$title" >"$prefix.stdout" 2>"$prefix.stderr"; then
      got="$(tr -d '\n' <"$prefix.stdout")"
      [[ "$got" != "$want" ]] || return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс эквивалент пункта «${title}» меню «${bar}» по AX — «${got}», ожидался «${want}» $(oneline "$prefix.stderr")"
      return 1
    fi
    sleep "$POLL"
  done
}

# Perl-условие: панель центра развёрнута.
zoomed_center() {
  printf '($l->{zoomed} // "") eq "%s" && defined $v->{frames}{zoomed}' "$p_center"
}

# keymap.toml переносит layout.zoom с Ctrl-w z на Ctrl-w m: подсказка кнопки «Развернуть панель» называет новую
# клавишу, Ctrl-w z раскладку не меняет, Ctrl-w m разворачивает панель одной дельтой. Снятая клавиша и новая идут
# одним ходом: запись кольца и дельта после обеих — одни.
item_hot_reload() {
  local item=hot-reload dir="$run/hot-reload"
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  to_front "$dir" "$item"
  wait_help "$dir/default.help" "Ctrl-w z" || fatal "$item: подготовка: $WHY"
  keymap_put "$dir/edit" $'[navigation]\n"Ctrl-w m" = "layout.zoom"\n"Ctrl-w z" = false'
  if wait_keymap "$dir/edit" "строка Ctrl-w m — layout.zoom из файла, строки Ctrl-w z нет" \
    'join(",", rows("navigation", "Ctrl-w m")) eq "layout.zoom" && !rows("navigation", "Ctrl-w z") && !@{$k->{diagnostics}}'; then
    if wait_help "$dir/edit.help" "Ctrl-w m"; then
      kstep "$dir" z-then-m layout.zoom "панель центра развёрнута" "$(zoomed_center)" \
        "$K_W+control" "$K_Z" "$K_W+control" "$K_M"
      kstep "$dir" m-again layout.zoom "разворот снят" '!defined $l->{zoomed} && !defined $v->{frames}{zoomed}' \
        "$K_W+control" "$K_M"
    else
      failures+=("$WHY")
    fi
  else
    failures+=("keymap.toml: $WHY")
  fi
  keymap_clean "$item" "$dir"
  wait_help "$dir/restore.help" "Ctrl-w z" || failures+=("после уборки keymap.toml: $WHY")
  # Клавиши Navigation после уборки файла действуют: следующий пункт начинает с живого маршрутизатора.
  step "$item" "$dir" after-l panel.focus.right "$p_right" "$K_L"
  step "$item" "$dir" after-h panel.focus.left "$p_center" "$K_H"
  conclude "$item" "правка keymap.toml («Ctrl-w m» — layout.zoom, «Ctrl-w z» снята) применена наблюдением за каталогом: query.keymap отдаёт строку из файла, help кнопки «${ZOOM_LABEL}» в ui.inspect и AXHelp кнопок окна называют Ctrl-w m; ход Ctrl-w z, Ctrl-w m — одна запись ui.receipts layout.zoom с источником key и одна дельта events --since, панель центра развёрнута; второй Ctrl-w m разворот снимает; без файла подсказка снова Ctrl-w z, клавиши l и h ведут фокус центр → правая → центр, на каждую одна запись ui.receipts"
}

# keymap.toml переносит layout.toggle.bottom с Cmd-j на Cmd-Alt-b: пункт «Вид → Нижняя зона» по AX называет ⌥⌘B,
# Cmd-j видимость не меняет, Cmd-Alt-b скрывает нижнюю зону одним переходом.
item_hot_reload_menu() {
  local item=hot-reload-menu dir="$run/hot-reload-menu" hidden='$l->{hidden}{bottom}'
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  to_front "$dir" "$item"
  wait_menu_key "$dir/default.menu" "Вид" "Нижняя зона" "J|0" || fatal "$item: подготовка: $WHY"
  keymap_put "$dir/edit" $'[global]\n"Cmd-Alt-b" = "layout.toggle.bottom"\n"Cmd-j" = false'
  if wait_keymap "$dir/edit" "строка Cmd-Alt-b — layout.toggle.bottom из файла, строки Cmd-j нет" \
    'join(",", rows("global", "Cmd-Alt-b")) eq "layout.toggle.bottom" && !rows("global", "Cmd-j") && !@{$k->{diagnostics}}'; then
    if wait_menu_key "$dir/edit.menu" "Вид" "Нижняя зона" "B|2"; then
      # Правка контекста global клавиши Navigation не трогает: l и h после правки, после аккордов global и после
      # уборки файла.
      step "$item" "$dir" edit-l panel.focus.right "$p_right" "$K_L"
      step "$item" "$dir" edit-h panel.focus.left "$p_center" "$K_H"
      kstep "$dir" j-then-b layout.toggle.bottom "нижняя зона скрыта" "$hidden" "$K_J+command" "$K_B+command+option"
      wait_mark "$dir/j-then-b.mark-ax" "Нижняя зона" false || failures+=("j-then-b: $WHY")
      kstep "$dir" b-again layout.toggle.bottom "нижняя зона показана" "!$hidden" "$K_B+command+option"
      wait_mark "$dir/b-again.mark-ax" "Нижняя зона" true || failures+=("b-again: $WHY")
      step "$item" "$dir" keys-l panel.focus.right "$p_right" "$K_L"
      step "$item" "$dir" keys-h panel.focus.left "$p_center" "$K_H"
    else
      failures+=("$WHY")
    fi
  else
    failures+=("keymap.toml: $WHY")
  fi
  keymap_clean "$item" "$dir"
  wait_menu_key "$dir/restore.menu" "Вид" "Нижняя зона" "J|0" || failures+=("после уборки keymap.toml: $WHY")
  step "$item" "$dir" after-l panel.focus.right "$p_right" "$K_L"
  step "$item" "$dir" after-h panel.focus.left "$p_center" "$K_H"
  conclude "$item" "правка keymap.toml («Cmd-Alt-b» — layout.toggle.bottom, «Cmd-j» снята) применена наблюдением за каталогом: пункт «Вид → Нижняя зона» по AX называет ⌥⌘B (до правки ⌘J); ход Cmd-j, Cmd-Alt-b — одна запись ui.receipts layout.toggle.bottom с источником key, одна дельта events --since и один переход layout.hidden.bottom, отметка AX кнопки «Нижняя зона» снята; второй Cmd-Alt-b зону показывает; без файла пункт снова ⌘J; клавиши l и h после правки, после аккордов global и после уборки файла ведут фокус центр → правая → центр, на каждую одна запись ui.receipts"
}

# Диагностика query.keymap номер 0: поле; «нет» — диагностики нет.
diag_of() {
  jv "$1" "ok.keymap.diagnostics[0].$2" 2>/dev/null || echo нет
}

# Файл с верной и неверной строкой: верная действует, неверная — в диагностике query.keymap с номером строки, её
# сообщение показывает строка статуса.
item_bad_line() {
  local item=bad-line dir="$run/bad-line"
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  to_front "$dir" "$item"
  wait_status "$dir/start.status" hidden || fatal "$item: подготовка: $WHY"
  keymap_put "$dir/edit" $'[navigation]\n"Ctrl-w m" = "layout.zoom"\n"Ctrl-w y" = "no.such.action"'
  if wait_keymap "$dir/edit" "строка Ctrl-w m из файла, строки Ctrl-w y нет, одна диагностика строки 3" \
    'join(",", rows("navigation", "Ctrl-w m")) eq "layout.zoom" && !rows("navigation", "Ctrl-w y") && @{$k->{diagnostics}} == 1 && ($k->{diagnostics}[0]{line} // 0) == 3'; then
    [[ "$(diag_of "$dir/edit.stdout" message)" == *no.such.action* ]] ||
      failures+=("диагностика не называет неверное действие: «$(diag_of "$dir/edit.stdout" message)»")
    # Сообщение диагностики показывает главный поток: с ним он получил и таблицу файла, клавиша идёт после него.
    if wait_status "$dir/edit.status" shown; then
      [[ "$STATUS_TEXT" == "$(diag_of "$dir/edit.stdout" message)" ]] ||
        failures+=("сообщение строки статуса «${STATUS_TEXT}», ожидалось сообщение диагностики «$(diag_of "$dir/edit.stdout" message)»")
      kstep "$dir" key-m layout.zoom "панель центра развёрнута" "$(zoomed_center)" "$K_W+control" "$K_M"
    else
      failures+=("$WHY")
    fi
  else
    failures+=("keymap.toml: $WHY")
  fi
  keymap_clean "$item" "$dir"
  conclude "$item" "keymap.toml с верной строкой («Ctrl-w m» — layout.zoom) и неверной («Ctrl-w y» — неизвестное действие): query.keymap отдаёт строку Ctrl-w m из файла и одну диагностику строки 3, строки Ctrl-w y нет, строка статуса показывает сообщение диагностики; Ctrl-w m — одна запись ui.receipts layout.zoom с источником key и одна дельта events --since; без файла диагностики и сообщения нет"
}

# Нечитаемый TOML после исправной правки: версия и строки query.keymap прежние, диагностика новая, прежняя таблица
# действует.
item_bad_file() {
  local item=bad-file dir="$run/bad-file" before
  log "== $item"
  failures=()
  fresh_bench "$item" "$dir"
  to_front "$dir" "$item"
  keymap_put "$dir/good" $'[navigation]\n"Ctrl-w m" = "layout.zoom"'
  wait_keymap "$dir/good" "строка Ctrl-w m из файла без диагностики" \
    'join(",", rows("navigation", "Ctrl-w m")) eq "layout.zoom" && !@{$k->{diagnostics}}' ||
    fatal "$item: подготовка: $WHY"
  before="$(keymap_canon "$dir/good.stdout")"
  wait_status "$dir/good.status" hidden || fatal "$item: подготовка: $WHY"
  keymap_put "$dir/bad" $'[navigation\n"Ctrl-w m" = '
  if wait_keymap "$dir/bad" "диагностика unreadable" '@{$k->{diagnostics}} == 1 && $k->{diagnostics}[0]{kind} eq "unreadable"'; then
    [[ "$(keymap_canon "$dir/bad.stdout")" == "$before" ]] ||
      failures+=("версия или строки query.keymap после нечитаемого файла изменились: версия $(jv "$dir/good.stdout" ok.keymap.version || echo "?") → $(jv "$dir/bad.stdout" ok.keymap.version || echo "?")")
    if wait_status "$dir/bad.status" shown; then
      [[ "$STATUS_TEXT" == "$(diag_of "$dir/bad.stdout" message)" ]] ||
        failures+=("сообщение строки статуса «${STATUS_TEXT}», ожидалось сообщение диагностики «$(diag_of "$dir/bad.stdout" message)»")
    else
      failures+=("$WHY")
    fi
    kstep "$dir" key-m layout.zoom "панель центра развёрнута" "$(zoomed_center)" "$K_W+control" "$K_M"
  else
    failures+=("keymap.toml: $WHY")
  fi
  keymap_clean "$item" "$dir"
  conclude "$item" "после исправной правки keymap.toml («Ctrl-w m» — layout.zoom) нечитаемый TOML: версия и строки query.keymap прежние, диагностика — одна новая вида unreadable, строка статуса показывает её сообщение; прежняя таблица действует: Ctrl-w m — одна запись ui.receipts layout.zoom с источником key и одна дельта events --since"
}

# Мост к источникам ввода (TIS), как у scripts/e2e/with-latin-input.sh: current, list, select <ID>.
INPUT_JXA='function run(argv) {
  ObjC.import("Carbon");
  ObjC.bindFunction("TISCopyCurrentKeyboardInputSource", ["id", []]);
  ObjC.bindFunction("TISCreateInputSourceList", ["id", ["id", "bool"]]);
  ObjC.bindFunction("TISSelectInputSource", ["int", ["id"]]);
  ObjC.bindFunction("TISGetInputSourceProperty", ["id", ["id", "void *"]]);
  const prop = (source, key) => $.TISGetInputSourceProperty(source, key).js;
  function sources() {
    const list = $.TISCreateInputSourceList($(), false);
    const out = [];
    for (let i = 0; i < list.count; i++) {
      const source = list.objectAtIndex(i);
      if (prop(source, $.kTISPropertyInputSourceCategory) === "TISCategoryKeyboardInputSource" &&
        prop(source, $.kTISPropertyInputSourceIsSelectCapable)) out.push(source);
    }
    return out;
  }
  if (argv[0] === "current") return prop($.TISCopyCurrentKeyboardInputSource(), $.kTISPropertyInputSourceID);
  if (argv[0] === "list") return sources().map(s => prop(s, $.kTISPropertyInputSourceID)).join("\n");
  const hit = sources().filter(s => prop(s, $.kTISPropertyInputSourceID) === argv[1])[0];
  if (!hit) throw new Error("источник " + argv[1] + " не включён");
  const status = $.TISSelectInputSource(hit);
  if (status !== 0) throw new Error("TISSelectInputSource: код " + status);
  return argv[1];
}'

tis() {
  osascript -l JavaScript -e "$INPUT_JXA" "$@"
}

# Источник ввода, который сценарий обязан вернуть: ID до переключения; пусто — сценарий источник не менял.
SOURCE_BEFORE=""

# Выбор источника ввода и опрос моста до него; вывод — <prefix>.select.*, иначе WHY.
source_select() {
  local prefix="$1" want="$2" deadline got="?"
  WHY=""
  tis select "$want" >"$prefix.select.stdout" 2>"$prefix.select.stderr" || {
    WHY="источник ввода $want не выбран: $(oneline "$prefix.select.stderr")"
    return 1
  }
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    got="$(tis current 2>>"$prefix.select.stderr" || true)"
    [[ "$got" != "$want" ]] || return 0
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс источник ввода — ${got:-не прочитан}, а не $want"
      return 1
    fi
    sleep "$POLL"
  done
}

# Источник «Русская»: Ctrl-w l, h, j ведут фокус как с латинской, / открывает строку фильтра «Демо: список», а
# клавиша B в ней даёт кириллицу — владелец в самом деле получает ввод русского источника. Без установленного
# русского источника пункт необязателен и получает not_run.
item_cyrillic_physical() {
  local item=cyrillic-physical dir="$run/cyrillic-physical" ru demo
  log "== $item"
  failures=()
  ru="$(tis list 2>"$dir/sources.stderr" | tee "$dir/sources.stdout" | grep -E '^com\.apple\.keylayout\.Russian' | head -n 1 || true)"
  if [[ -z "$ru" ]]; then
    [[ -s "$dir/sources.stdout" ]] || fatal "$item: список источников ввода не прочитан: $(oneline "$dir/sources.stderr")"
    set_required "$item" 0
    record "$item" not_run "нет русского источника ввода"
    return 0
  fi
  fresh_bench "$item" "$dir"
  to_front "$dir" "$item"
  SOURCE_BEFORE="$(tis current 2>"$dir/current.stderr")" || fatal "$item: источник ввода не прочитан: $(oneline "$dir/current.stderr")"
  [[ -n "$SOURCE_BEFORE" ]] || fatal "$item: источник ввода не прочитан: мост вернул пустой ID"
  log "$item: источник ввода $SOURCE_BEFORE → $ru"
  if source_select "$dir/russian" "$ru"; then
    to_front "$dir" "$item"
    step "$item" "$dir" ctrl-w-l panel.focus.right "$p_right" "$K_W+control" "$K_L"
    step "$item" "$dir" bare-h panel.focus.left "$p_center" "$K_H"
    step "$item" "$dir" bare-j panel.focus.down "$p_bottom" "$K_J"
    # Свидетель источника: тот же физический B в поле даёт кириллицу.
    open_panel "$dir/open-demo" demo-list center
    demo="$PANEL"
    wait_focus "$dir/open-demo.layout" "$demo" || failures+=("«Демо: список»: $WHY")
    wait_mode "$dir/open-demo.mode" navigation || failures+=("«Демо: список»: $WHY")
    fopen "$dir" slash "$demo" ""
    fkey "$dir" letter "$K_B"
    wait_inspect "$profile" "$dir/letter.row" "layout.panel.$demo.filter.input" state.text_field.value "и" ||
      failures+=("клавиша B в строке фильтра под источником $ru: $WHY")
    fkey "$dir" esc "$K_ESC"
    wait_row "$dir/esc.row" "$demo" closed || failures+=("Esc: $WHY")
    wait_mode "$dir/esc.mode" navigation || failures+=("Esc: $WHY")
    [[ "$(tis current 2>/dev/null || true)" == "$ru" ]] || failures+=("к концу хода источник ввода — не $ru")
  else
    failures+=("$WHY")
  fi
  # Следующие пункты и подготовка набирают латиницу.
  source_select "$dir/restore" "$SOURCE_BEFORE" || fatal "$item: источник ввода не возвращён: $WHY"
  SOURCE_BEFORE=""
  conclude "$item" "источник ввода $ru: Ctrl-w l, h и j ведут фокус query.layout центр → правая → центр → нижняя, / открывает строку фильтра «Демо: список», на каждую клавишу одна запись ui.receipts с источником key; клавиша B в строке фильтра даёт «и» — владелец получает ввод русского источника; прежний источник возвращён"
}

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
item_list_owns_keys
item_move_and_split
item_wrong_axis
item_resize_zoom
item_tabs
item_zones_toggle
item_appearance_cycle
item_relative_resize_cli
item_key_and_menu_once
item_filter
item_history
item_field_input
item_hot_reload
item_hot_reload_menu
# Пункты среза 10 вызываются, когда срез доказан приёмочным проходом (DONE_STAGE не меньше 10).
if ((DONE_STAGE >= 10)); then
  item_bad_line
  item_bad_file
  item_cyrillic_physical
fi
if [[ -n "$owner_pid" ]]; then
  mkdir -p "$run/cleanup"
  quit_owner "$profile" "$run/cleanup" "$owner_pid"
  owner_pid=""
fi
# Второй владелец идёт после выхода основного: на переднем плане один процесс бандла.
item_window_quit
for i in "${!ITEMS[@]}"; do
  if ((STAGE[i] > DONE_STAGE)); then
    record "${ITEMS[i]}" not_run "пункт среза ${STAGE[i]} задачи 10 плана navigation-keymap"
  fi
done
record exit-chord-hid not_run "наблюдает только человек: Ctrl-\\ на настоящей клавиатуре через системные сочетания и источники ввода (ADR-0010, «Уточнено в P0.3»)"
completed=1
