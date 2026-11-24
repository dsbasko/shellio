#!/usr/bin/env bash
# Сценарий make e2e «хост плагинов» (P1.5): пункты матрицы приёмки плана plugin-host на подписанном бандле и
# временных профилях.
#
#   plugins.sh [<бандл>]
#
# Бандл по умолчанию — target/bundle/shellio.app, его собирает и проверяет цель make e2e. shellioctl сценарий
# собирает сам (release, --locked). Профили — новые каталоги /tmp/shellio-e2e.*. Подготовка — штатный CLI:
# воркспейсы W1 и W2, проект в каждом, рабочие области A и A2 в проекте W1 и B в W2, workbench open A.
#
# Каталог прогона — target/e2e/plugins/<время>-<pid>/: result.json (пункт, обязательность, статус, причина, путь
# сырого вывода относительно каталога прогона), журнал сценария log и сырой вывод каждого пункта (JSON query.layout,
# query.panel и ui.*, AX окна, строки state.db) в подкаталоге с его именем. Пробы предусловий пишут в
# preconditions/, уборка — в cleanup/.
#
# Предусловия — как у layout.sh, их отсутствие — ошибка исполнения «предусловие: …» до первого пункта: открытый
# сеанс GUI, разблокированный экран, запущенный Finder, бинарь бандла не запущен, запуск из процесса-потомка
# приложения терминала; у терминала-предка Screen Recording и Accessibility с Automation для System Events;
# латинский источник ввода; AeroSpace и другие оконные менеджеры выключены.
#
# Оракулы пунктов: «чтения» — query.layout и query.panel; «окно» — ui.inspect и AX окна (System Events), кадры
# совпадают в пределах 1 точки; «база» — sqlite3 по state.db остановленного владельца (version и
# hex(CAST(data AS BLOB)) строки panel_states). Пункт с собственным профилем идёт при остановленном основном
# владельце: System Events находит окно по имени процесса, и два владельца одного бандла путают AX.
#
# Рубежи накопительные (план P1.5, «Матрица приёмки»): пункт, чья задача ещё не выполнена, получает not_run
# «пункт задачи N» и необязателен.
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
fixture_missing="$here/plugins-fixtures/v3-missing-plugin.sql"

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

# Окно обычного режима и допуск кадров (план P1.4, «Матрица приёмки»).
WINDOW_W=1180
WINDOW_H=720
FRAME_TOLERANCE=1

# Пункты матрицы по порядку плана и задача, которая делает пункт обязательным.
ITEMS=(placeholders-default plugin-catalog open-cli retarget-cli drill-keys restore-list missing-plugin menu-list
  open-menu empty-workbench-open open-race race-multi singleton-split picker-singleton open-menu-native native-edit
  native-zones perapp-move perapp-scope perapp-return zones-demo stub-min-size status-stubs legacy-duplicate
  demo-snapshot restore-cold voiceover-speech)
TASK=(9 9 9 9 12 9 9 10 10 10 10 10 10 10 11 11 11 11 11 11 12 12 12 12 12 12 0)
# Задача, до которой сценарий доведён: пункты более поздних задач не судятся.
DONE_TASK=9
# Плагины реестра в порядке регистрации: id, название, default_zone, кардинальность, min_size.
CATALOG=(
  "terminal|Терминал|center|multi|200|120"
  "navigator|Навигатор|left|per_workbench|200|120"
  "demo-list|Демо: список|right|per_workbench|260|160"
)
# Фикстура missing-plugin: рабочая область и панель незарегистрированного плагина, байты её строки panel_states.
MISSING_BENCH="019a8f2e-0000-7000-8000-00000000c201"
MISSING_PANEL="019a8f2e-0000-7000-8000-00000000e203"
MISSING_ROW='1|7B2278223A317D'
MISSING_STUB="Плагин «gone-plugin» недоступен"

REQUIRED=()
STATUS=()
REASON=()
for i in "${!ITEMS[@]}"; do
  REQUIRED[i]=0
  if ((TASK[i] > 0 && TASK[i] <= DONE_TASK)); then
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

run="$root/target/e2e/plugins/$(date +%Y%m%d-%H%M%S)-$$"
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

# Профиль основной серии пунктов: воркспейсы W1 и W2, рабочие области A, A2 и B, панель демо-списка в A.
profile=""
owner_pid=""
ws1=""
ws2=""
wb_a=""
wb_a2=""
wb_b=""
demo=""

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
    printf '  "scenario": "plugins",\n'
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

# Элементы окна процесса pid по System Events с кадрами: JSON {window: [x, y, w, h], elements: [{role, description,
# frame}]}. Обход до глубины 12, внутрь групп вкладок не заходит.
ax_frames() {
  osascript -l JavaScript -e "
    const se = Application('System Events');
    const w = se.processes.whose({unixId: $1})[0].windows[0];
    function prop(f) { try { const v = f(); return v === undefined ? null : v; } catch (x) { return null; } }
    function frame(e) { const p = prop(() => e.position()); const s = prop(() => e.size());
      return p && s ? [p[0], p[1], s[0], s[1]] : null; }
    const out = [];
    function walk(e, depth) {
      const role = prop(() => e.role());
      out.push({ role: role, description: prop(() => e.description()), frame: frame(e) });
      if (depth >= 12 || role === 'AXTabGroup') return;
      let kids = [];
      try { kids = e.uiElements(); } catch (x) {}
      for (const k of kids) walk(k, depth + 1);
    }
    let kids = [];
    try { kids = w.uiElements(); } catch (x) {}
    for (const k of kids) walk(k, 1);
    JSON.stringify({ window: frame(w), elements: out });"
}

# Сверка кадров query.layout с ui.inspect и AX: строки нарушений в stdout, пусто — совпало. Кадры частей — строка
# вкладок (AXTabGroup с именем строки вкладок части) и тело (AXGroup вида панели с названием активной панели),
# колонка навигатора — тело её вида. Тело с флагом stub сверяется с заглушкой «Развернуть» своего места (узел
# <место>.stub и AXGroup «<название>: здесь тесно»): вид панели под ней скрыт.
frames_compare() {
  /usr/bin/perl -COEA -Mutf8 -MJSON::PP -e '
    my ($layout, $inspect, $ax, $tol) = @ARGV;
    my $read = sub { open my $fh, "<", $_[0] or die "$_[0]\n"; local $/; JSON::PP->new->utf8->decode(<$fh>) };
    my $l = $read->($layout)->{ok}{value};
    my $i = $read->($inspect)->{ok}{ui}{inspect};
    my $a = $read->($ax);
    my $f = $l->{frames} or do { print "query.layout без кадров\n"; exit 0 };
    my %title = map { ($_->{id} => $_->{title}) } @{$l->{panels}};
    my ($wx, $wy) = @{$a->{window} || [0, 0]};
    my $h = $i->{content}{height};
    my %node = map { ($_->{id} => $_) } @{$i->{components}};
    my $near = sub { my ($p, $q) = @_; for my $k (0 .. 3) { return 0 if abs($p->[$k] - $q->[$k]) > $tol } 1 };
    my $fmt = sub { sprintf "(%g, %g, %g×%g)", @{$_[0]} };
    my $rect = sub { my $r = shift; [$r->{x}, $r->{y}, $r->{width}, $r->{height}] };
    my $flip = sub { my $r = shift; [$r->{x}, $h - $r->{y} - $r->{height}, $r->{width}, $r->{height}] };
    my @ax = map { { role => $_->{role} // "", description => $_->{description} // "",
      frame => $_->{frame} ? [$_->{frame}[0] - $wx, $_->{frame}[1] - $wy, $_->{frame}[2], $_->{frame}[3]] : undef } }
      @{$a->{elements}};
    my $ax_has = sub {
      my ($role, $name, $want, $what) = @_;
      my @cand = grep { $_->{role} eq $role && $_->{description} eq $name && $_->{frame} } @ax;
      return if grep { $near->($_->{frame}, $want) } @cand;
      print "$what: AX $role «${name}» ", (@cand ? join(", ", map { $fmt->($_->{frame}) } @cand) : "нет"),
        ", query.layout ", $fmt->($want), "\n";
    };
    my $inspect_has = sub {
      my ($id, $want, $what) = @_;
      my $n = $node{$id} or do { print "$what: нет узла ui.inspect $id\n"; return };
      my $got = $flip->($n->{frame});
      print "$what: ui.inspect $id ", $fmt->($got), ", query.layout ", $fmt->($want), "\n" unless $near->($got, $want);
      print "$what: узел ui.inspect $id не видим\n" unless $n->{visible};
    };
    my $body_has = sub {
      my ($host, $panel, $stub, $want, $what) = @_;
      my $name = $title{$panel} // "?";
      if ($stub) {
        $inspect_has->("$host.stub", $want, "$what, заглушка");
        $ax_has->("AXGroup", "$name: здесь тесно", $want, "$what, заглушка");
      } else {
        $inspect_has->("layout.panel.$panel", $want, "$what, тело");
        $ax_has->("AXGroup", $name, $want, "$what, тело");
      }
    };
    my %zone_name = (left => "левой зоны", right => "правой зоны", center => "центра", bottom => "нижней зоны");
    if (my $z = $f->{zoomed}) {
      $inspect_has->("layout.zoom", $rect->($z->{frame}), "развёрнутая панель, плитка");
      $inspect_has->("layout.zoom.tabs", $rect->($z->{tab_strip}), "развёрнутая панель, строка вкладок");
      $ax_has->("AXTabGroup", "Вкладки развёрнутой панели", $rect->($z->{tab_strip}), "развёрнутая панель, строка вкладок");
      $body_has->("layout.zoom", $z->{active}, $z->{stub}, $rect->($z->{body}), "развёрнутая панель");
    }
    my %count;
    $count{$_->{zone}}++ for @{$f->{parts}};
    for my $p (@{$f->{parts}}) {
      my $what = "часть $p->{zone}.$p->{index}";
      my $strip = "Вкладки $zone_name{$p->{zone}}" . ($count{$p->{zone}} > 1 ? ", часть " . ($p->{index} + 1) : "");
      my $id = "layout.part.$p->{zone}.$p->{index}";
      $inspect_has->($id, $rect->($p->{frame}), "$what, плитка");
      $inspect_has->("$id.tabs", $rect->($p->{tab_strip}), "$what, строка вкладок");
      $ax_has->("AXTabGroup", $strip, $rect->($p->{tab_strip}), "$what, строка вкладок");
      $body_has->($id, $p->{active}, $p->{stub}, $rect->($p->{body}), $what);
    }
    if (my $n = $f->{navigator}) {
      $body_has->("layout.navigator", $n->{panel}, $n->{stub}, $rect->($n->{body}), "колонка навигатора");
    }
  ' "$1" "$2" "$3" "$FRAME_TOLERANCE"
}

# Чтения пункта: ui.window, query.layout, ui.inspect и AX в <dir>/<name>.*; сверка кадров — в FRAMES (пусто —
# совпало). Отказ чтения — в WHY.
read_all() {
  local state="$1" dir="$2" name="$3" bench="$4" pid="$5"
  WHY=""
  FRAMES=""
  ui_read "$state" "$dir/$name.window" window || { WHY="ui.window не прочитан: $(oneline "$dir/$name.window.stdout")"; return 1; }
  window_ok "$dir/$name.window.stdout" || return 1
  layout_get "$state" "$dir/$name.layout" "$bench" || { WHY="query.layout не прочитан: $(oneline "$dir/$name.layout.stderr")"; return 1; }
  ui_read "$state" "$dir/$name.inspect" inspect || { WHY="ui.inspect не прочитан: $(oneline "$dir/$name.inspect.stdout")"; return 1; }
  # Сразу после запуска System Events отдаёт элементы окна без свойств: AX опрашивается до совпадения кадров или
  # до срока, итог — последнее чтение.
  local deadline
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    ax_frames "$pid" >"$dir/$name.ax.json" 2>"$dir/$name.ax.stderr" ||
      fatal "AX окна pid $pid не прочитан: $(oneline "$dir/$name.ax.stderr")"
    FRAMES="$(frames_compare "$dir/$name.layout.stdout" "$dir/$name.inspect.stdout" "$dir/$name.ax.json" 2>&1)" ||
      fatal "сверка кадров $name не выполнена: $FRAMES"
    [[ -n "$FRAMES" ]] || return 0
    (($(now_ms) < deadline)) || return 0
    sleep "$POLL"
  done
}
FRAMES=""

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

# Строка panel_states панели в state.db остановленного владельца: «version|HEX байтов data».
row_of() {
  /usr/bin/sqlite3 -readonly "$1/state.db" \
    "SELECT version || '|' || hex(CAST(data AS BLOB)) FROM panel_states WHERE panel = X'${2//-/}';" 2>&1
}

# В чтении AX окна есть элемент с описанием text.
ax_has_text() {
  /usr/bin/perl -COEA -Mutf8 -MJSON::PP -e '
    my ($file, $text) = @ARGV;
    open my $fh, "<", $file or exit 2;
    local $/;
    my $a = eval { JSON::PP->new->utf8->decode(<$fh>) } or exit 2;
    exit((grep { ($_->{description} // "") eq $text } @{$a->{elements}}) ? 0 : 1);
  ' "$1" "$2"
}

# Опрос AX окна pid: элемент с описанием text; чтение — в file.
wait_ax_text() {
  local pid="$1" file="$2" text="$3" deadline
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    ax_frames "$pid" >"$file" 2>"$file.stderr" || fatal "AX окна pid $pid не прочитан: $(oneline "$file.stderr")"
    ! ax_has_text "$file" "$text" || return 0
    (($(now_ms) < deadline)) || return 1
    sleep "$POLL"
  done
}

# ID объекта tree с названием title; чтение — <dir>/tree-<name>.stdout.
tree_id() {
  ctl "$profile" "$1/tree-$2" --json tree --all || return 1
  id_by_title "$1/tree-$2.stdout" "$3"
}

# --- подготовка, уборка, предусловия ---

prepare() {
  local left
  app="${bundle_arg:-$root/target/bundle/shellio.app}"
  [[ -d "$app" ]] || fatal "нет бандла $app: соберите его через make bundle"
  app="$(cd "$app" && pwd -P)"
  exe="$app/Contents/MacOS/shellio"
  [[ -x "$exe" ]] || fatal "в бандле нет исполняемого $exe"
  [[ -x "$LSAPPINFO" ]] || fatal "нет $LSAPPINFO"
  [[ -x /usr/bin/sqlite3 ]] || fatal "нет /usr/bin/sqlite3"
  /usr/bin/perl -MJSON::PP -e 1 2>/dev/null || fatal "нет модуля JSON::PP системного perl"
  [[ -f "$fixture_missing" ]] || fatal "нет файла $fixture_missing"
  left="$(run_processes)"
  [[ -z "$left" ]] || fatal "предусловие: бинарь бандла прогона уже запущен (PID $left), завершите его"
  [[ -n "$(finder_pid)" ]] || fatal "предусловие: Finder не запущен"
  find_terminal
  build_bin shellioctl shellioctl
  ctl_bin="$BUILT"
  log "каталог прогона: $run"
  log "бандл: $app, shellioctl: $ctl_bin"
}

# Пробы предусловий: оконные менеджеры, источник ввода, Screen Recording, Automation и Accessibility по окну
# служебного владельца обычного режима; затем он завершается.
preconditions() {
  local dir="$run/preconditions" name layout ok preflight pid number
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
  ui_read "$NEW_PROFILE" "$dir/window" window || fatal "предусловие: ui.window служебного владельца не прочитан"
  number="$(jv "$dir/window.stdout" ok.ui.window.window_number)" || fatal "предусловие: в ui.window нет window_number"
  /usr/sbin/screencapture -x -o -l "$number" "$dir/probe.png" >"$dir/probe.log" 2>&1 ||
    fatal "предусловие: screencapture -l $number отказал (нет Screen Recording): $(oneline "$dir/probe.log")"
  verified_hello "$NEW_PROFILE" "$dir/hello-stop" || fatal "предусловие: служебный владелец: $WHY"
  terminate "$HELLO_PID"
  ! alive "$pid" || fatal "предусловие: служебный владелец pid $pid не завершился"
}

# Основной профиль штатным CLI: воркспейсы W1 и W2, проект в каждом, рабочие области A и A2 в проекте W1 и B в
# W2, workbench open A.
setup_main() {
  local dir="$run/preconditions/setup" project1 project2 dir1 dir2
  mkdir -p "$dir"
  log "== подготовка основного профиля"
  new_profile
  profile="$NEW_PROFILE"
  launch_normal "$profile" "$dir"
  owner_pid="$OWNER_PID"
  dir1="$(mktemp -d /tmp/shellio-e2e-project.XXXXXX)"
  dir2="$(mktemp -d /tmp/shellio-e2e-project.XXXXXX)"
  profiles+=("$dir1" "$dir2")
  core_cmd "$profile" "$dir/ws1" workspace new "e2e-plugins-w1-$$" --all || fatal "подготовка: $WHY"
  ws1="$(tree_id "$dir" ws1 "e2e-plugins-w1-$$")" || fatal "подготовка: воркспейс W1 не найден в tree"
  core_cmd "$profile" "$dir/ws2" workspace new "e2e-plugins-w2-$$" --all || fatal "подготовка: $WHY"
  ws2="$(tree_id "$dir" ws2 "e2e-plugins-w2-$$")" || fatal "подготовка: воркспейс W2 не найден в tree"
  core_cmd "$profile" "$dir/project1" project add "$dir1" --title "plugins-p1-$$" --workspace "$ws1" ||
    fatal "подготовка: $WHY"
  project1="$(tree_id "$dir" project1 "plugins-p1-$$")" || fatal "подготовка: проект W1 не найден в tree"
  core_cmd "$profile" "$dir/project2" project add "$dir2" --title "plugins-p2-$$" --workspace "$ws2" ||
    fatal "подготовка: $WHY"
  project2="$(tree_id "$dir" project2 "plugins-p2-$$")" || fatal "подготовка: проект W2 не найден в tree"
  core_cmd "$profile" "$dir/wb-a" workbench new --target "$project1" "a-$$" --workspace "$ws1" ||
    fatal "подготовка: $WHY"
  wb_a="$(tree_id "$dir" wb-a "a-$$")" || fatal "подготовка: рабочая область A не найдена в tree"
  core_cmd "$profile" "$dir/wb-a2" workbench new --target "$project1" "a2-$$" --workspace "$ws1" ||
    fatal "подготовка: $WHY"
  wb_a2="$(tree_id "$dir" wb-a2 "a2-$$")" || fatal "подготовка: рабочая область A2 не найдена в tree"
  core_cmd "$profile" "$dir/wb-b" workbench new --target "$project2" "b-$$" --workspace "$ws2" ||
    fatal "подготовка: $WHY"
  wb_b="$(tree_id "$dir" wb-b "b-$$")" || fatal "подготовка: рабочая область B не найдена в tree"
  core_cmd "$profile" "$dir/open" workbench open --target "$wb_a" --workspace "$ws1" || fatal "подготовка: $WHY"
  settle_layout "$profile" "$dir/settle" "$wb_a" "$REV" || fatal "подготовка: $WHY"
  log "W1 $ws1, W2 $ws2, A $wb_a, A2 $wb_a2, B $wb_b"
}

# --- пункты матрицы ---

# Новая рабочая область A: навигатор в колонке и терминал в центре с нейтральной карточкой заполнителя.
item_placeholders_default() {
  local item=placeholders-default dir="$run/placeholders-default" nav term panel plugin size
  failures=()
  log "== $item"
  read_all "$profile" "$dir" default "$wb_a" "$owner_pid" || fatal "условие пункта: $WHY"
  nav="$(lv "$dir/default.layout.stdout" layout.navigator || echo null)"
  term="$(panels_of "$dir/default.layout.stdout" terminal)"
  [[ "$nav" != null && "$(panels_of "$dir/default.layout.stdout" navigator)" == "$nav" ]] ||
    failures+=("колонка навигатора не держит панель navigator: $nav")
  [[ -n "$term" && "$term" != *" "* && "$(place_of "$dir/default.layout.stdout" "$term")" == center.0 ]] ||
    failures+=("в центре не одна панель terminal: «${term}», место $(place_of "$dir/default.layout.stdout" "$term")")
  for plugin in navigator terminal; do
    panel="$nav"
    [[ "$plugin" == navigator ]] || panel="$term"
    [[ -n "$panel" && "$panel" != null && "$panel" != *" "* ]] || continue
    [[ "$(pfact "$dir/default.inspect.stdout" "$panel" plugin || echo нет)" == "$plugin" ]] ||
      failures+=("ui.inspect: plugin панели $plugin — $(pfact "$dir/default.inspect.stdout" "$panel" plugin || echo нет)")
    [[ "$(pfact "$dir/default.inspect.stdout" "$panel" status || echo нет)" == ready ]] ||
      failures+=("ui.inspect: статус панели $plugin — $(pfact "$dir/default.inspect.stdout" "$panel" status || echo нет)")
    [[ "$(comp "$dir/default.inspect.stdout" "layout.panel.$panel.card" class || echo нет)" == NeutralCard ]] ||
      failures+=("тело панели $plugin — не нейтральная карточка: $(comp "$dir/default.inspect.stdout" "layout.panel.$panel.card" class || echo нет)")
    if panel_get "$profile" "$dir/panel-$plugin" "$panel"; then
      size="$(pv "$dir/panel-$plugin.stdout" min_size.width || echo ?)×$(pv "$dir/panel-$plugin.stdout" min_size.height || echo ?)"
      [[ "$size" == 200×120 ]] || failures+=("query.panel: min_size панели $plugin $size, ожидалось 200×120")
      [[ "$(pv "$dir/panel-$plugin.stdout" status || echo ?)" == ready ]] ||
        failures+=("query.panel: статус панели $plugin — $(pv "$dir/panel-$plugin.stdout" status || echo ?)")
    else
      failures+=("query.panel панели $plugin не прочитан: $(oneline "$dir/panel-$plugin.stdout") $(oneline "$dir/panel-$plugin.stderr")")
    fi
  done
  [[ -z "$FRAMES" ]] || failures+=("кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  conclude "$item" "новая рабочая область: навигатор в колонке и терминал в центре, у обоих нейтральная карточка, plugin и status ready в ui.inspect, min_size 200×120 в query.panel; кадры query.layout совпали с ui.inspect и AX окна в пределах $FRAME_TOLERANCE точки"
}

# shellioctl plugin list: реестр в порядке регистрации с полями манифестов.
item_plugin_catalog() {
  local item=plugin-catalog dir="$run/plugin-catalog" file count i entry id title zone card w h got zones z names=""
  failures=()
  log "== $item"
  file="$dir/plugins.stdout"
  ctl "$profile" "$dir/plugins" --json --timeout "$CTL_TIMEOUT_MS" plugin list || {
    record "$item" fail "shellioctl plugin list отказал: $(oneline "$file") $(oneline "$dir/plugins.stderr")"
    return 0
  }
  count="$(jv "$file" 'ok.value.plugins[#]' || echo ?)"
  [[ "$count" == "${#CATALOG[@]}" ]] || failures+=("плагинов в реестре $count, ожидалось ${#CATALOG[@]}")
  for i in "${!CATALOG[@]}"; do
    entry="${CATALOG[i]}"
    IFS='|' read -r id title zone card w h <<<"$entry"
    names="${names:+$names, }$id"
    got="$(jv "$file" "ok.value.plugins[$i].id" || echo нет)"
    if [[ "$got" != "$id" ]]; then
      failures+=("плагин $i — $got, ожидался $id")
      continue
    fi
    [[ "$(jv "$file" "ok.value.plugins[$i].title" || echo ?)" == "$title" ]] ||
      failures+=("$id: название «$(jv "$file" "ok.value.plugins[$i].title" || echo ?)», ожидалось «${title}»")
    [[ "$(jv "$file" "ok.value.plugins[$i].default_zone" || echo ?)" == "$zone" ]] ||
      failures+=("$id: default_zone $(jv "$file" "ok.value.plugins[$i].default_zone" || echo ?), ожидалась $zone")
    [[ "$(jv "$file" "ok.value.plugins[$i].cardinality" || echo ?)" == "$card" ]] ||
      failures+=("$id: кардинальность $(jv "$file" "ok.value.plugins[$i].cardinality" || echo ?), ожидалась $card")
    got="$(jv "$file" "ok.value.plugins[$i].min_size.width" || echo ?)×$(jv "$file" "ok.value.plugins[$i].min_size.height" || echo ?)"
    [[ "$got" == "${w}×${h}" ]] || failures+=("$id: min_size $got, ожидалось ${w}×${h}")
    zones="$(jv "$file" "ok.value.plugins[$i].placements" || echo ?)"
    [[ "$(jv "$file" "ok.value.plugins[$i].placements[#]" || echo ?)" == 4 ]] ||
      failures+=("$id: зон $(jv "$file" "ok.value.plugins[$i].placements[#]" || echo ?), ожидалось 4")
    for z in left right center bottom; do
      [[ "$zones" == *"\"$z\""* ]] || failures+=("$id: в placements нет зоны $z: $zones")
    done
  done
  conclude "$item" "plugin list: $names в порядке регистрации, у каждого четыре зоны, default_zone, кардинальность и min_size манифеста"
}

# panel open demo-list: новая панель в правой зоне с фокусом, вариант drill, восемь строк, выбрана a.
item_open_cli() {
  local item=open-cli dir="$run/open-cli" outcome focus
  failures=()
  log "== $item"
  core_cmd "$profile" "$dir/open" panel open demo-list --workspace "$ws1" || {
    record "$item" fail "$WHY"
    return 0
  }
  demo="$(jv "$dir/open.stdout" ok.value.panel || true)"
  outcome="$(jv "$dir/open.stdout" ok.value.outcome || echo ?)"
  [[ -n "$demo" ]] || {
    record "$item" fail "в ответе panel open нет панели: $(oneline "$dir/open.stdout")"
    return 0
  }
  [[ "$outcome" == created ]] || failures+=("исход $outcome, ожидался created")
  settle_layout "$profile" "$dir/settle" "$wb_a" "$REV" || fatal "$item: $WHY"
  wait_inspect "$profile" "$dir/wait" "layout.panel.$demo.list" state.virtual_list.count 8 ||
    failures+=("список демо в окне: $WHY")
  read_all "$profile" "$dir" opened "$wb_a" "$owner_pid" || fatal "условие пункта: $WHY"
  [[ "$(place_of "$dir/opened.layout.stdout" "$demo")" == right.0 ]] ||
    failures+=("панель в «$(place_of "$dir/opened.layout.stdout" "$demo")», а не в правой зоне")
  focus="$(focus_of "$dir/opened.layout.stdout")"
  [[ "$focus" == "$demo" ]] || failures+=("фокус на ${focus:-нет}, а не на панели демо-списка $demo")
  [[ "$(pfact "$dir/opened.inspect.stdout" "$demo" variant || echo нет)" == drill ]] ||
    failures+=("вариант $(pfact "$dir/opened.inspect.stdout" "$demo" variant || echo нет), ожидался drill")
  [[ "$(pfact "$dir/opened.inspect.stdout" "$demo" status || echo нет)" == ready ]] ||
    failures+=("статус $(pfact "$dir/opened.inspect.stdout" "$demo" status || echo нет), ожидался ready")
  [[ "$(list_fact "$dir/opened.inspect.stdout" "$demo" list count || echo нет)" == 8 ]] ||
    failures+=("строк в списке $(list_fact "$dir/opened.inspect.stdout" "$demo" list count || echo нет), ожидалось 8")
  [[ "$(list_fact "$dir/opened.inspect.stdout" "$demo" list selected || echo нет)" == a ]] ||
    failures+=("в окне выбрана $(list_fact "$dir/opened.inspect.stdout" "$demo" list selected || echo нет), ожидалась a")
  [[ -z "$FRAMES" ]] || failures+=("кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  if panel_get "$profile" "$dir/panel" "$demo"; then
    [[ "$(pv "$dir/panel.stdout" panel.id || echo ?)" == "$demo" ]] ||
      failures+=("query.panel: panel.id $(pv "$dir/panel.stdout" panel.id || echo ?), в ответе команды $demo")
    [[ "$(pv "$dir/panel.stdout" data.key || echo ?)" == a ]] ||
      failures+=("query.panel: data.key $(pv "$dir/panel.stdout" data.key || echo ?), ожидался a")
    [[ "$(pv "$dir/panel.stdout" data.history || echo ?)" == "[]" ]] ||
      failures+=("query.panel: data.history $(pv "$dir/panel.stdout" data.history || echo ?), ожидалась пустая")
  else
    failures+=("query.panel не прочитан: $(oneline "$dir/panel.stdout") $(oneline "$dir/panel.stderr")")
  fi
  conclude "$item" "panel open demo-list — created, панель $demo в правой зоне с фокусом; окно: вариант drill, восемь строк, выбрана a, кадры совпали с ui.inspect и AX; query.panel: data.key a, история пуста, panel.id из ответа"
}

# Повтор с аргументами и demo-list.back: тот же PanelId, контейнер и тело.
item_retarget_cli() {
  local item=retarget-cli dir="$run/retarget-cli" component got
  failures=()
  log "== $item"
  [[ -n "$demo" ]] || {
    record "$item" fail "панели демо-списка нет: open-cli её не создал"
    return 0
  }
  ui_read "$profile" "$dir/before.inspect" inspect || fatal "$item: ui.inspect не прочитан"
  component="$(pfact "$dir/before.inspect.stdout" "$demo" component || echo нет)"
  core_cmd "$profile" "$dir/open" panel open demo-list --args '{"key":"c"}' --workspace "$ws1" || {
    record "$item" fail "$WHY"
    return 0
  }
  [[ "$(jv "$dir/open.stdout" ok.value.panel || echo ?)" == "$demo" ]] ||
    failures+=("повтор вернул панель $(jv "$dir/open.stdout" ok.value.panel || echo ?), а не $demo")
  [[ "$(jv "$dir/open.stdout" ok.value.outcome || echo ?)" == focused ]] ||
    failures+=("исход повтора $(jv "$dir/open.stdout" ok.value.outcome || echo ?), ожидался focused")
  wait_inspect "$profile" "$dir/wait-c" "layout.panel.$demo.list" state.virtual_list.selected c ||
    failures+=("после повтора с key c: $WHY")
  if panel_get "$profile" "$dir/panel-c" "$demo"; then
    [[ "$(pv "$dir/panel-c.stdout" data.key || echo ?)" == c ]] ||
      failures+=("query.panel после повтора: data.key $(pv "$dir/panel-c.stdout" data.key || echo ?), ожидался c")
    [[ "$(pv "$dir/panel-c.stdout" data.history || echo ?)" == '["a"]' ]] ||
      failures+=("query.panel после повтора: data.history $(pv "$dir/panel-c.stdout" data.history || echo ?), ожидалась [\"a\"]")
  else
    failures+=("query.panel после повтора не прочитан: $(oneline "$dir/panel-c.stderr")")
  fi
  core_cmd "$profile" "$dir/back" panel command "$demo" demo-list.back --workspace "$ws1" || {
    record "$item" fail "$WHY"
    return 0
  }
  wait_inspect "$profile" "$dir/wait-a" "layout.panel.$demo.list" state.virtual_list.selected a ||
    failures+=("после demo-list.back: $WHY")
  if panel_get "$profile" "$dir/panel-a" "$demo"; then
    [[ "$(pv "$dir/panel-a.stdout" data.key || echo ?)" == a ]] ||
      failures+=("query.panel после back: data.key $(pv "$dir/panel-a.stdout" data.key || echo ?), ожидался a")
    [[ "$(pv "$dir/panel-a.stdout" data.history || echo ?)" == "[]" ]] ||
      failures+=("query.panel после back: data.history $(pv "$dir/panel-a.stdout" data.history || echo ?), ожидалась пустая")
  else
    failures+=("query.panel после back не прочитан: $(oneline "$dir/panel-a.stderr")")
  fi
  ui_read "$profile" "$dir/after.inspect" inspect || fatal "$item: ui.inspect не прочитан"
  ui_read "$profile" "$dir/after.stats" stats || fatal "$item: ui.stats не прочитан"
  got="$(pfact "$dir/after.inspect.stdout" "$demo" component || echo нет)"
  [[ "$component" != нет && "$got" == "$component" ]] ||
    failures+=("ComponentId контейнера $got, до повтора $component")
  [[ "$(built_of "$dir/after.stats.stdout" "$demo")" == 1 ]] ||
    failures+=("bodies_built демо-списка $(built_of "$dir/after.stats.stdout" "$demo"), ожидалось 1")
  conclude "$item" "panel open demo-list --args {\"key\":\"c\"} — тот же PanelId $demo, focused, выбрана c, история [\"a\"]; demo-list.back — выбрана a, история пуста; ComponentId контейнера $component прежний, bodies_built 1"
}

# Демо-список с ключом e переживает app quit и новый запуск: PanelId, данные, байты строки и выбор в окне.
item_restore_list() {
  local item=restore-list dir="$run/restore-list" before after row1 row2
  failures=()
  log "== $item"
  [[ -n "$demo" ]] || {
    record "$item" fail "панели демо-списка нет: open-cli её не создал"
    return 0
  }
  core_cmd "$profile" "$dir/open" panel open demo-list --args '{"key":"e"}' --workspace "$ws1" || {
    record "$item" fail "$WHY"
    return 0
  }
  wait_inspect "$profile" "$dir/wait-before" "layout.panel.$demo.list" state.virtual_list.selected e ||
    fatal "условие пункта: $WHY"
  panel_get "$profile" "$dir/before" "$demo" || fatal "$item: query.panel не прочитан"
  before="$(data_canon "$dir/before.stdout")"
  quit_owner "$profile" "$dir" "$owner_pid"
  owner_pid=""
  row1="$(row_of "$profile" "$demo")"
  printf '%s\n' "$row1" >"$dir/row-before.txt"
  launch_normal "$profile" "$dir/relaunch"
  owner_pid="$OWNER_PID"
  settle_layout "$profile" "$dir/settle" "$wb_a" 0 || fatal "$item: после перезапуска: $WHY"
  wait_inspect "$profile" "$dir/wait-after" "layout.panel.$demo.list" state.virtual_list.selected e ||
    failures+=("в окне после перезапуска: $WHY")
  read_all "$profile" "$dir" after "$wb_a" "$owner_pid" || fatal "условие пункта: после перезапуска: $WHY"
  [[ "$(panels_of "$dir/after.layout.stdout" demo-list)" == "$demo" ]] ||
    failures+=("панели demo-list после перезапуска «$(panels_of "$dir/after.layout.stdout" demo-list)», до него $demo")
  [[ -z "$FRAMES" ]] || failures+=("после перезапуска кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  if panel_get "$profile" "$dir/after" "$demo"; then
    after="$(data_canon "$dir/after.stdout")"
    [[ "$after" == "$before" ]] || failures+=("данные после перезапуска $after, до него $before")
    [[ "$(pv "$dir/after.stdout" status || echo ?)" == ready ]] ||
      failures+=("статус после перезапуска $(pv "$dir/after.stdout" status || echo ?)")
  else
    failures+=("query.panel после перезапуска не прочитан: $(oneline "$dir/after.stdout") $(oneline "$dir/after.stderr")")
  fi
  quit_owner "$profile" "$dir/relaunch" "$owner_pid"
  owner_pid=""
  row2="$(row_of "$profile" "$demo")"
  printf '%s\n' "$row2" >"$dir/row-after.txt"
  [[ "$row1" =~ ^[0-9]+\|[0-9A-F]+$ ]] || failures+=("строка panel_states до перезапуска не прочитана: $row1")
  [[ "$row1" == "$row2" ]] || failures+=("байты строки panel_states после перезапуска другие: «${row2}», до него «${row1}»")
  conclude "$item" "демо-список с ключом e → app quit → запуск на том же профиле: тот же PanelId $demo, данные $before структурно равны, строка panel_states $row1 побайтно прежняя, в окне выбрана e, кадры совпали с ui.inspect и AX"
}

# Профиль схемы v3 с панелью незарегистрированного плагина: заглушка с причиной, отказ команд, байты строки целы.
item_missing_plugin() {
  local item=missing-plugin dir="$run/missing-plugin" state pid place row code
  failures=()
  log "== $item"
  new_profile
  state="$NEW_PROFILE"
  chmod 700 "$state"
  /usr/bin/sqlite3 "$state/state.db" <"$fixture_missing" >"$dir/sqlite.log" 2>&1 ||
    fatal "фикстура v3-missing-plugin не загружена: $(oneline "$dir/sqlite.log")"
  launch_normal "$state" "$dir/first"
  pid="$OWNER_PID"
  if ! settle_layout "$state" "$dir/settle-first" "$MISSING_BENCH" 0; then
    record "$item" fail "сразу после запуска: $WHY"
    quit_owner "$state" "$dir/first" "$pid"
    return 0
  fi
  layout_get "$state" "$dir/first.layout" "$MISSING_BENCH" || fatal "$item: query.layout не прочитан"
  place="$(place_of "$dir/first.layout.stdout" "$MISSING_PANEL")"
  [[ "$place" == right.0 ]] || failures+=("панель gone-plugin в «${place}», в фикстуре — правая зона")
  wait_inspect "$state" "$dir/first.inspect" "layout.part.$place.stub" label "$MISSING_STUB" ||
    failures+=("заглушка в окне: $WHY")
  ! comp "$dir/first.inspect.stdout" "layout.part.$place.stub.expand" id >/dev/null 2>&1 ||
    failures+=("у заглушки статуса есть кнопка «Развернуть»")
  [[ "$(pfact "$dir/first.inspect.stdout" "$MISSING_PANEL" status || echo нет)" == plugin_missing ]] ||
    failures+=("ui.inspect: статус $(pfact "$dir/first.inspect.stdout" "$MISSING_PANEL" status || echo нет), ожидался plugin_missing")
  wait_ax_text "$pid" "$dir/first.ax.json" "$MISSING_STUB" ||
    failures+=("в AX окна нет элемента «${MISSING_STUB}»")
  if panel_get "$state" "$dir/first.panel" "$MISSING_PANEL"; then
    [[ "$(pv "$dir/first.panel.stdout" status || echo ?)" == plugin_missing ]] ||
      failures+=("query.panel: статус $(pv "$dir/first.panel.stdout" status || echo ?), ожидался plugin_missing")
  else
    failures+=("query.panel не прочитан: $(oneline "$dir/first.panel.stdout") $(oneline "$dir/first.panel.stderr")")
  fi
  code=0
  ctl "$state" "$dir/command" --json --timeout "$CTL_TIMEOUT_MS" panel command "$MISSING_PANEL" gone-plugin.any --all ||
    code=$?
  [[ $code -ne 0 && "$(jv "$dir/command.stdout" core_error.details.reason || echo нет)" == panel_unavailable ]] ||
    failures+=("panel command: код $code, причина $(jv "$dir/command.stdout" core_error.details.reason || echo нет), ожидался отказ panel_unavailable")
  core_cmd "$state" "$dir/move" panel move "$MISSING_PANEL" --zone bottom --all || failures+=("panel move: $WHY")
  quit_owner "$state" "$dir/first" "$pid"
  row="$(row_of "$state" "$MISSING_PANEL")"
  printf '%s\n' "$row" >"$dir/row-moved.txt"
  [[ "$row" == "$MISSING_ROW" ]] || failures+=("строка panel_states после panel move «${row}», в фикстуре «${MISSING_ROW}»")
  launch_normal "$state" "$dir/second"
  pid="$OWNER_PID"
  if settle_layout "$state" "$dir/settle-second" "$MISSING_BENCH" 0; then
    layout_get "$state" "$dir/second.layout" "$MISSING_BENCH" || fatal "$item: query.layout не прочитан"
    place="$(place_of "$dir/second.layout.stdout" "$MISSING_PANEL")"
    [[ "$place" == bottom.0 ]] || failures+=("после перезапуска панель в «${place}», а не в нижней зоне")
    wait_inspect "$state" "$dir/second.inspect" "layout.part.$place.stub" label "$MISSING_STUB" ||
      failures+=("заглушка после перезапуска: $WHY")
    if panel_get "$state" "$dir/second.panel" "$MISSING_PANEL"; then
      [[ "$(pv "$dir/second.panel.stdout" status || echo ?)" == plugin_missing ]] ||
        failures+=("query.panel после перезапуска: статус $(pv "$dir/second.panel.stdout" status || echo ?)")
    else
      failures+=("query.panel после перезапуска не прочитан: $(oneline "$dir/second.panel.stderr")")
    fi
  else
    failures+=("после перезапуска: $WHY")
  fi
  quit_owner "$state" "$dir/second" "$pid"
  row="$(row_of "$state" "$MISSING_PANEL")"
  printf '%s\n' "$row" >"$dir/row-restarted.txt"
  [[ "$row" == "$MISSING_ROW" ]] || failures+=("строка panel_states после перезапуска «${row}», в фикстуре «${MISSING_ROW}»")
  conclude "$item" "state.db схемы v3 с панелью gone-plugin: заглушка «${MISSING_STUB}» без кнопки в ui.inspect и AX, статус plugin_missing, panel command — panel_unavailable; после panel move в нижнюю зону и перезапуска строка panel_states $MISSING_ROW побайтно прежняя"
}

prepare
preconditions
setup_main
item_placeholders_default
item_plugin_catalog
item_open_cli
item_retarget_cli
# restore-list оставляет основного владельца остановленным: missing-plugin идёт на собственном профиле, а System
# Events находит окно по имени процесса.
item_restore_list
item_missing_plugin
for i in "${!ITEMS[@]}"; do
  if ((TASK[i] > DONE_TASK)); then
    record "${ITEMS[i]}" not_run "пункт задачи ${TASK[i]}"
  fi
done
record voiceover-speech not_run "речь VoiceOver автоматика не наблюдает, роли и названия проверяют open-cli и menu-list"
completed=1
