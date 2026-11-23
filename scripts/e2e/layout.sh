#!/usr/bin/env bash
# Сценарий make e2e «раскладка в обычном окне» (P1.4): пункты матрицы приёмки плана layout-zones на подписанном
# бандле и временных профилях.
#
#   layout.sh [<бандл>]
#
# Бандл по умолчанию — target/bundle/shellio.app, его собирает и проверяет цель make e2e. shellioctl и
# shellio-e2e-oracle сценарий собирает сам (release, --locked). Профили — новые каталоги /tmp/shellio-e2e.*.
# Рабочая область заводится штатным CLI: workspace new, project add на временный каталог, workbench new,
# workbench open.
#
# Каталог прогона — target/e2e/layout/<время>-<pid>/: result.json (пункт, обязательность, статус, причина, путь
# сырого вывода относительно каталога прогона), журнал сценария log и сырой вывод каждого пункта (JSON query.layout
# и ui.*, AX окна, PNG, отчёты оракула) в подкаталоге с его именем. Пробы предусловий пишут в preconditions/,
# уборка — в cleanup/.
#
# Предусловия — как у appkit.sh, их отсутствие — ошибка исполнения «предусловие: …» до первого пункта: открытый
# сеанс GUI, разблокированный экран, запущенный Finder, бинарь бандла не запущен, запуск из процесса-потомка
# приложения терминала; у терминала-предка Screen Recording и Accessibility с Automation для System Events;
# латинский источник ввода; AeroSpace и другие оконные менеджеры выключены. Перед первым пунктом снимков оракул
# проходит самопроверку в режиме без маркера: заведомо неверное ожидание обязано дать расхождение.
#
# Каждый пункт сверяет чтение query.layout и независимо от него кадры по ui.inspect и AX окна (System Events):
# кадры совпадают в пределах 1 точки. Кадры query.layout — в точках содержимого с началом сверху слева, кадры AX —
# в точках экрана, из них вычитается положение окна по AX. Снимок — screencapture -x -o -l <windowNumber>, перевод
# в sRGB через sips, затем оракул в режиме без маркера: пара снимков подряд принимается, когда участки совпали, а
# поколение ui.window то же, не дольше 3 с (ADR-0021, «Уточнено в P1.4 (планирование)»). Перед первым снимком
# ui.window показывает ревизию зеркала не меньше ревизии ответа команды.
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
expect_dir="$here/layout-expect"
fixture_v1="$here/layout-fixtures/v1-open-workbench.sql"

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
EXIT_S=10
KILL_S=5
MIRROR_MS=3000
SETTLE_MS=3000
POLL=0.1
CTL_TIMEOUT_MS=2000

# Окно обычного режима и допуск кадров (план P1.4, «Матрица приёмки»).
WINDOW_W=1180
WINDOW_H=720
FRAME_TOLERANCE=1
NAVIGATOR_W=236
GAP=6
RESIZE_RIGHT=420
# Сдвиг разделителя между частями центра в resize-drag, точки.
DRAG_DX=60

ITEMS=(layout-default layout-snapshot move-cli move-menu-zone move-menu-part move-drag move-drag-body center-empty
  resize-drag split-two-fit split-button-unfocused split-single-picker split-refused stub-expand zoom-restore
  only-bottom hide-zones tooltips-ax restore-cold restore-legacy voiceover-speech system-contrast)
REQUIRED=(1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 1 0 0)
# Пункты следующих задач плана: до них прогон пишет not_run с задачей, обязательность — после их появления.
LATER=(move-menu-zone:12 move-menu-part:12 move-drag:12 move-drag-body:12 center-empty:11
  split-two-fit:11 split-button-unfocused:11 split-single-picker:11 split-refused:11 stub-expand:13 zoom-restore:11
  only-bottom:11 tooltips-ax:11)
STATUS=()
REASON=()
for i in "${!ITEMS[@]}"; do
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

run="$root/target/e2e/layout/$(date +%Y%m%d-%H%M%S)-$$"
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
snap_reason=""
snap_code=""

# Профиль и рабочая область основной серии пунктов.
profile=""
owner_pid=""
ws=""
wb=""
project_dir=""

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
    printf '  "scenario": "layout",\n'
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
# колонка навигатора — тело её вида.
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
      print "$what: AX $role «$name» ", (@cand ? join(", ", map { $fmt->($_->{frame}) } @cand) : "нет"),
        ", query.layout ", $fmt->($want), "\n";
    };
    my $inspect_has = sub {
      my ($id, $want, $what) = @_;
      my $n = $node{$id} or do { print "$what: нет узла ui.inspect $id\n"; return };
      my $got = $flip->($n->{frame});
      print "$what: ui.inspect $id ", $fmt->($got), ", query.layout ", $fmt->($want), "\n" unless $near->($got, $want);
      print "$what: узел ui.inspect $id не видим\n" unless $n->{visible};
    };
    my %zone_name = (left => "левой зоны", right => "правой зоны", center => "центра", bottom => "нижней зоны");
    my %count;
    $count{$_->{zone}}++ for @{$f->{parts}};
    for my $p (@{$f->{parts}}) {
      my $what = "часть $p->{zone}.$p->{index}";
      my $strip = "Вкладки $zone_name{$p->{zone}}" . ($count{$p->{zone}} > 1 ? ", часть " . ($p->{index} + 1) : "");
      my $id = "layout.part.$p->{zone}.$p->{index}";
      $inspect_has->($id, $rect->($p->{frame}), "$what, плитка");
      $inspect_has->("$id.tabs", $rect->($p->{tab_strip}), "$what, строка вкладок");
      $inspect_has->("layout.panel.$p->{active}", $rect->($p->{body}), "$what, тело");
      $ax_has->("AXTabGroup", $strip, $rect->($p->{tab_strip}), "$what, строка вкладок");
      $ax_has->("AXGroup", $title{$p->{active}} // "?", $rect->($p->{body}), "$what, тело");
    }
    if (my $n = $f->{navigator}) {
      $inspect_has->("layout.panel.$n->{panel}", $rect->($n->{body}), "колонка навигатора, тело");
      $ax_has->("AXGroup", $title{$n->{panel}} // "?", $rect->($n->{body}), "колонка навигатора, тело");
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

# --- снимки и оракул ---

# Ожидания с подстановкой панелей: @FOCUS@ и @OTHER@ — PanelId части с фокусом и другой части центра.
expectations() {
  sed -e "s/@FOCUS@/$2/g" -e "s/@OTHER@/$3/g" "$expect_dir/$1.json" >"$4"
}

# Снимок окна владельца в режиме без маркера: снимки повторяются, пока пара подряд не устоялась (код оракула 0
# или 3, либо 4 с retry: false), не дольше SETTLE_MS. expect — файл ожиданий или underline:<id вкладки>:<слой>. Код
# оракула — в snap_code, причина — в snap_reason. Сырые файлы — <dir>/<name>.<N>.{window.json,…}.
snapshot() {
  local state="$1" dir="$2" name="$3" expect="$4" t0 n=0 number code spec prev="" elapsed
  local oracle_args=(compare --expect "$expect")
  if [[ "$expect" == underline:* ]]; then
    spec="${expect#underline:}"
    oracle_args=(underline --tab "${spec%%:*}" --layer "${spec#*:}")
  fi
  snap_code=""
  snap_reason=""
  t0="$(now_ms)"
  while :; do
    n=$((n + 1))
    local cur="$dir/$name.$n"
    ui_read "$state" "$cur.window" window || fatal "снимок $name: ui.window не прочитан: $(oneline "$cur.window.stdout")"
    ui_read "$state" "$cur.inspect" inspect || fatal "снимок $name: ui.inspect не прочитан: $(oneline "$cur.inspect.stdout")"
    number="$(jv "$cur.window.stdout" ok.ui.window.window_number)" || fatal "снимок $name: в ui.window нет window_number"
    /usr/sbin/screencapture -x -o -l "$number" "$cur.raw.png" >"$cur.capture.log" 2>&1 ||
      fatal "снимок $name: screencapture -l $number отказал: $(oneline "$cur.capture.log")"
    [[ -s "$cur.raw.png" ]] || fatal "снимок $name: screencapture -l $number не записал PNG"
    sips --matchTo "$SRGB_PROFILE" "$cur.raw.png" --out "$cur.png" >"$cur.sips.log" 2>&1 ||
      fatal "снимок $name: sips не перевёл PNG в sRGB: $(oneline "$cur.sips.log")"
    if [[ -n "$prev" ]]; then
      elapsed=$(($(now_ms) - t0))
      code=0
      "$oracle_bin" "${oracle_args[@]}" --inspect "$cur.inspect.stdout" --window "$cur.window.stdout" \
        --png "$cur.png" --previous-window "$prev.window.stdout" --previous-png "$prev.png" \
        --elapsed-ms "$elapsed" >"$cur.report.json" 2>"$cur.oracle.stderr" || code=$?
      case $code in
        0 | 3)
          snap_code=$code
          snap_reason="$(oneline "$cur.oracle.stderr")"
          log "снимок $name: код оракула $code за $n снимк., $snap_reason"
          return 0
          ;;
        4)
          if [[ "$(jv "$cur.report.json" retry)" != true ]]; then
            snap_code=4
            snap_reason="отрисовка не наблюдалась: $(oneline "$cur.oracle.stderr")"
            log "снимок $name: $snap_reason"
            return 0
          fi
          ;;
        *) fatal "снимок $name: оракул — ошибка исполнения (код $code): $(oneline "$cur.oracle.stderr")" ;;
      esac
    fi
    prev="$cur"
  done
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
  for left in "$expect_dir"/{light,dark}.json "$fixture_v1"; do
    [[ -f "$left" ]] || fatal "нет файла $left"
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
  # Самопроверка оракула без маркера: участок заголовка со слотом text вместо chrome обязан разойтись.
  /usr/bin/perl -MJSON::PP -e 'local $/; my $e = JSON::PP->new->utf8->decode(<STDIN>);
    $e->{regions} = [map { { %$_, slot => "text" } } grep { $_->{component} eq "chrome" } @{$e->{regions}}];
    print JSON::PP->new->utf8->canonical->encode($e)' <"$expect_dir/light.json" >"$dir/wrong-light.json"
  ui_cmd "$NEW_PROFILE" "$dir/light" ui appearance light || fatal "самопроверка оракула: $WHY"
  snapshot "$NEW_PROFILE" "$dir" self-check "$dir/wrong-light.json"
  case "$snap_code" in
    3) log "самопроверка оракула: неверное ожидание дало расхождение — $snap_reason" ;;
    0) fatal "оракул не различает цвета: ожидание со слотом text вместо chrome прошло" ;;
    *) fatal "самопроверка оракула не выполнена: $snap_reason" ;;
  esac
  verified_hello "$NEW_PROFILE" "$dir/hello-stop" || fatal "предусловие: служебный владелец: $WHY"
  terminate "$HELLO_PID"
  ! alive "$pid" || fatal "предусловие: служебный владелец pid $pid не завершился"
}

# --- пункты матрицы ---

# Открыть новую рабочую область: раскладка по умолчанию, кадры query.layout, ui.inspect и AX.
item_layout_default() {
  local item=layout-default dir="$run/layout-default" nav term focus
  failures=()
  log "== $item"
  new_profile
  profile="$NEW_PROFILE"
  launch_normal "$profile" "$dir"
  owner_pid="$OWNER_PID"
  project_dir="$(mktemp -d /tmp/shellio-e2e-project.XXXXXX)"
  profiles+=("$project_dir")
  core_cmd "$profile" "$dir/ws" workspace new "e2e-layout-$$" --all || fatal "подготовка: $WHY"
  ctl "$profile" "$dir/tree-ws" --json tree --all || fatal "подготовка: tree не прочитан"
  ws="$(id_by_title "$dir/tree-ws.stdout" "e2e-layout-$$")" || fatal "подготовка: воркспейс не найден в tree"
  core_cmd "$profile" "$dir/project" project add "$project_dir" --title "layout-$$" --workspace "$ws" ||
    fatal "подготовка: $WHY"
  ctl "$profile" "$dir/tree-project" --json tree --all || fatal "подготовка: tree не прочитан"
  local project
  project="$(id_by_title "$dir/tree-project.stdout" "layout-$$")" || fatal "подготовка: проект не найден в tree"
  core_cmd "$profile" "$dir/workbench" workbench new --target "$project" "main-$$" --workspace "$ws" ||
    fatal "подготовка: $WHY"
  ctl "$profile" "$dir/tree-workbench" --json tree --all || fatal "подготовка: tree не прочитан"
  wb="$(id_by_title "$dir/tree-workbench.stdout" "main-$$")" || fatal "подготовка: рабочая область не найдена в tree"
  core_cmd "$profile" "$dir/open" workbench open --target "$wb" --workspace "$ws" || fatal "подготовка: $WHY"
  settle_layout "$profile" "$dir/settle" "$wb" "$REV" || fatal "подготовка: $WHY"
  read_all "$profile" "$dir" default "$wb" "$owner_pid" || fatal "условие пункта: $WHY"
  nav="$(lv "$dir/default.layout.stdout" layout.navigator || echo null)"
  term="$(panels_of "$dir/default.layout.stdout" terminal)"
  focus="$(focus_of "$dir/default.layout.stdout")"
  [[ "$(panels_of "$dir/default.layout.stdout" navigator)" == "$nav" && "$nav" != null ]] ||
    failures+=("колонка навигатора не держит панель navigator: $nav")
  [[ "$(lv "$dir/default.layout.stdout" frames.navigator.frame.width || echo ?)" =~ ^${NAVIGATOR_W}(\.0)?$ ]] ||
    failures+=("ширина колонки навигатора $(lv "$dir/default.layout.stdout" frames.navigator.frame.width || echo ?), ожидалось $NAVIGATOR_W")
  [[ -n "$term" && "$term" != *" "* && "$(place_of "$dir/default.layout.stdout" "$term")" == center.0 ]] ||
    failures+=("в центре не одна панель terminal: terminal — «${term}», место $(place_of "$dir/default.layout.stdout" "$term")")
  [[ "$focus" == "$term" ]] || failures+=("фокус на ${focus:-нет}, а не на панели terminal $term")
  local parts
  parts="$(lv "$dir/default.layout.stdout" 'frames.parts[#]' || echo ?)"
  [[ "$parts" == 1 ]] || failures+=("частей в кадрах $parts, ожидалась 1 (центр)")
  [[ -z "$FRAMES" ]] || failures+=("кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  conclude "$item" "колонка навигатора $NAVIGATOR_W точек с панелью navigator, центр — одна часть с панелью terminal, фокус на ней; кадры частей, строк вкладок и тел query.layout совпали с ui.inspect и AX окна в пределах $FRAME_TOLERANCE точки"
}

# Деление центра CLI, снимки светлой и тёмной темы оракулом без маркера.
item_layout_snapshot() {
  local item=layout-snapshot dir="$run/layout-snapshot" term focus other cell gap tab
  failures=()
  log "== $item"
  term="$(panels_of "$run/layout-default/default.layout.stdout" terminal)"
  core_cmd "$profile" "$dir/split" layout split "$term" --open terminal --workspace "$ws" || {
    record "$item" fail "$WHY"
    return 0
  }
  settle_layout "$profile" "$dir/settle" "$wb" "$REV" || fatal "$item: $WHY"
  read_all "$profile" "$dir" split "$wb" "$owner_pid" || fatal "условие пункта: $WHY"
  [[ "$(lv "$dir/split.layout.stdout" 'layout.zones.center.parts[#]')" == 2 ]] ||
    failures+=("после деления в центре не две части: $(lv "$dir/split.layout.stdout" 'layout.zones.center.parts[#]' || echo ?)")
  focus="$(focus_of "$dir/split.layout.stdout")"
  other="$(lv "$dir/split.layout.stdout" 'layout.zones.center.parts[0].tabs[0]' || echo ?)"
  [[ "$other" != "$focus" ]] || other="$(lv "$dir/split.layout.stdout" 'layout.zones.center.parts[1].tabs[0]' || echo ?)"
  gap="$(/usr/bin/perl -MJSON::PP -e 'local $/; my $p = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{frames}{parts};
    my @c = sort { $a->{frame}{x} <=> $b->{frame}{x} } grep { $_->{zone} eq "center" } @$p;
    print @c == 2 ? $c[1]{frame}{x} - $c[0]{frame}{x} - $c[0]{frame}{width} : "?", "\n"' <"$dir/split.layout.stdout")"
  [[ "$gap" == "$GAP" ]] || failures+=("промежуток между плитками центра $gap точек, ожидалось $GAP")
  [[ -z "$FRAMES" ]] || failures+=("кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  for cell in light dark; do
    ui_cmd "$profile" "$dir/$cell" ui appearance "$cell" || {
      failures+=("$cell: $WHY")
      continue
    }
    expectations "$cell" "$focus" "$other" "$dir/$cell.expect.json"
    snapshot "$profile" "$dir" "$cell" "$dir/$cell.expect.json"
    [[ "$snap_code" == 0 ]] || failures+=("$cell: $snap_reason")
    tab="layout.part.$(place_of "$dir/split.layout.stdout" "$focus").tabs.tab.$focus"
    snapshot "$profile" "$dir" "$cell-underline" "underline:$tab:tone_navigation"
    [[ "$snap_code" == 0 ]] || failures+=("$cell, линия под активной вкладкой: $snap_reason")
  done
  ui_cmd "$profile" "$dir/system" ui appearance system || failures+=("возврат оформления: $WHY")
  conclude "$item" "layout split --open terminal: две плитки центра с промежутком $GAP точек по query.layout, ui.inspect и AX; оракул без маркера в светлой и тёмной теме — фон заголовка, тон фокуса у тела части с фокусом и вуаль у другой (layout-expect/*.json), линия под активной вкладкой части с фокусом в допуске толщины и цвета; линия и радиус плитки снимком не меряются (слой Surface плитки, модульный тест plate_spec)"
}

# Перенос панели CLI в правую зону: PanelId, фокус и ComponentId прежние, видов панели не создано.
item_move_cli() {
  local item=move-cli dir="$run/move-cli" term component before after place focus
  failures=()
  log "== $item"
  # Фокус следует за панелью: переносится панель с фокусом — после деления это панель новой части.
  term="$(focus_of "$run/layout-snapshot/split.layout.stdout")"
  ui_read "$profile" "$dir/before.inspect" inspect || fatal "$item: ui.inspect не прочитан"
  ui_read "$profile" "$dir/before.stats" stats || fatal "$item: ui.stats не прочитан"
  component="$(comp "$dir/before.inspect.stdout" "layout.panel.$term" state.panel.component || echo ?)"
  before="$(created_of "$dir/before.stats.stdout" ShellioPanelView)"
  core_cmd "$profile" "$dir/move" panel move "$term" --zone right --workspace "$ws" || {
    record "$item" fail "$WHY"
    return 0
  }
  settle_layout "$profile" "$dir/settle" "$wb" "$REV" || fatal "$item: $WHY"
  read_all "$profile" "$dir" moved "$wb" "$owner_pid" || fatal "условие пункта: $WHY"
  ui_read "$profile" "$dir/after.stats" stats || fatal "$item: ui.stats не прочитан"
  after="$(created_of "$dir/after.stats.stdout" ShellioPanelView)"
  place="$(place_of "$dir/moved.layout.stdout" "$term")"
  focus="$(focus_of "$dir/moved.layout.stdout")"
  [[ "$place" == right.0 ]] || failures+=("панель $term в ${place:-нет}, а не в правой зоне")
  [[ "$focus" == "$term" ]] || failures+=("фокус на ${focus:-нет}, а не на перенесённой панели")
  [[ "$(comp "$dir/moved.inspect.stdout" "layout.panel.$term" state.panel.component || echo нет)" == "$component" ]] ||
    failures+=("ComponentId вида панели $component → $(comp "$dir/moved.inspect.stdout" "layout.panel.$term" state.panel.component || echo нет)")
  [[ "$before" == "$after" ]] || failures+=("создано видов панели ShellioPanelView: $before → $after")
  [[ -z "$FRAMES" ]] || failures+=("кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  conclude "$item" "panel move --zone right: панель $term в правой зоне с прежним PanelId и фокусом, ComponentId вида прежний ($component), созданных ShellioPanelView не прибавилось ($before), кадры совпали"
}

# Кадр части zone.index из query.layout: поле field кадра (x, y, width, height).
part_field() {
  /usr/bin/perl -MJSON::PP -e 'my ($zone, $index, $field) = @ARGV; local $/;
    my ($p) = grep { $_->{zone} eq $zone && $_->{index} == $index } @{JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{frames}{parts}};
    print defined $p ? "$p->{frame}{$field}\n" : "?\n"' "$2" "$3" "$4" <"$1"
}

# Кадры query.layout каноническим JSON.
frames_canon() {
  /usr/bin/perl -MJSON::PP -e 'local $/; print JSON::PP->new->utf8->canonical->encode(JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{frames}), "\n"' <"$1"
}

# Опрос query.layout до значения want поля path; последнее чтение — <prefix>.stdout, иначе WHY.
wait_layout() {
  local state="$1" prefix="$2" bench="$3" path="$4" want="$5" deadline seen=""
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if layout_get "$state" "$prefix" "$bench"; then
      seen="$(lv "$prefix.stdout" "$path" || echo ?)"
      [[ "$seen" == "$want" ]] && return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс $path в query.layout — ${seen:-?}, а не $want"
      return 1
    fi
    sleep "$POLL"
  done
}

# Перетаскивание левой кнопкой из точки CG (x1, y1) в (x2, y2) шагами по 4 точки.
drag_cg() {
  osascript -l JavaScript -e "
    ObjC.import('CoreGraphics');
    const post = (type, x, y) => \$.CGEventPost(\$.kCGHIDEventTap,
      \$.CGEventCreateMouseEvent(null, type, \$.CGPointMake(x, y), \$.kCGMouseButtonLeft));
    post(\$.kCGEventMouseMoved, $1, $2); delay(0.05);
    post(\$.kCGEventLeftMouseDown, $1, $2); delay(0.05);
    const steps = Math.max(1, Math.ceil(Math.abs($3 - $1) / 4));
    for (let i = 1; i <= steps; i++) {
      post(\$.kCGEventLeftMouseDragged, $1 + ($3 - $1) * i / steps, $2 + ($4 - $2) * i / steps); delay(0.01);
    }
    delay(0.05);
    post(\$.kCGEventLeftMouseUp, $3, $4);
    'ok';"
}

# AXPress кнопки окна владельца pid с описанием label; иначе WHY.
press_button() {
  local pid="$1" label="$2" out="$3"
  osascript -l JavaScript -e "
    const se = Application('System Events');
    const w = se.processes.whose({unixId: $pid})[0].windows[0];
    w.buttons.whose({description: '$label'})[0].actions.byName('AXPress').perform();
    'pressed'" >"$out.stdout" 2>"$out.stderr" || {
    WHY="AXPress кнопки «$label» не выполнен: $(oneline "$out.stderr")"
    return 1
  }
}

# Пункт title меню «Вид» владельца pid; иначе WHY.
press_view_item() {
  local pid="$1" title="$2" out="$3"
  osascript -l JavaScript -e "
    const se = Application('System Events');
    const p = se.processes.whose({unixId: $pid})[0];
    p.menuBars[0].menuBarItems.byName('Вид').menus[0].menuItems.byName('$title').click();
    'clicked'" >"$out.stdout" 2>"$out.stderr" || {
    WHY="пункт «$title» меню «Вид» не выбран: $(oneline "$out.stderr")"
    return 1
  }
}

# Деление центра, перетаскивание разделителя между его частями CGEvent, скрытие и показ правой зоны.
item_resize_drag() {
  local item=resize-drag dir="$run/resize-drag" center x0 w0 x1 y gx wx wy sh share want got
  failures=()
  log "== $item"
  center="$(lv "$run/move-cli/moved.layout.stdout" 'layout.zones.center.parts[0].tabs[0]' || echo ?)"
  core_cmd "$profile" "$dir/split" layout split "$center" --open terminal --workspace "$ws" || {
    record "$item" fail "$WHY"
    return 0
  }
  settle_layout "$profile" "$dir/settle" "$wb" "$REV" || fatal "$item: $WHY"
  read_all "$profile" "$dir" before "$wb" "$owner_pid" || fatal "условие пункта: $WHY"
  x0="$(part_field "$dir/before.layout.stdout" center 0 x)"
  w0="$(part_field "$dir/before.layout.stdout" center 0 width)"
  x1="$(part_field "$dir/before.layout.stdout" center 1 x)"
  y="$(awk -v a="$(part_field "$dir/before.layout.stdout" center 0 y)" -v h="$(part_field "$dir/before.layout.stdout" center 0 height)" 'BEGIN { print a + h / 2 }')"
  share="$(lv "$dir/before.layout.stdout" 'layout.zones.center.parts[0].share' || echo ?)"
  [[ "$x1" != "?" && "$w0" != "?" ]] || fatal "условие пункта: после деления в центре нет двух частей"
  wx="$(jv "$dir/before.window.stdout" ok.ui.window.frame.x)"
  wy="$(jv "$dir/before.window.stdout" ok.ui.window.frame.y)"
  sh="$(jv "$dir/before.window.stdout" ok.ui.window.screens[0].frame.height)"
  local wh
  wh="$(jv "$dir/before.window.stdout" ok.ui.window.frame.height)"
  # Полоса захвата — промежуток между частями. Кадры query.layout — в точках содержимого с началом сверху слева.
  gx="$(awk -v a="$x0" -v w="$w0" -v b="$x1" 'BEGIN { printf "%.1f", (a + w + b) / 2 }')"
  local cx cy
  cx="$(awk -v wx="$wx" -v gx="$gx" 'BEGIN { printf "%.1f", wx + gx }')"
  cy="$(awk -v sh="$sh" -v wy="$wy" -v wh="$wh" -v y="$y" 'BEGIN { printf "%.1f", sh - (wy + wh) + y }')"
  printf 'точка CG %s %s → %s, полоса x=%s содержимого\n' "$cx" "$cy" "$DRAG_DX" "$gx" >"$dir/drag.txt"
  front_owner_ok "$dir" || {
    record "$item" fail "$WHY"
    return 0
  }
  drag_cg "$cx" "$cy" "$(awk -v x="$cx" -v d="$DRAG_DX" 'BEGIN { print x + d }')" "$cy" >"$dir/drag.log" 2>&1 ||
    fatal "перетаскивание CGEvent не выполнено: $(oneline "$dir/drag.log")"
  local deadline
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    layout_get "$profile" "$dir/after.layout" "$wb" || fatal "$item: query.layout не прочитан"
    got="$(lv "$dir/after.layout.stdout" 'layout.zones.center.parts[0].share' || echo ?)"
    [[ "$got" != "$share" ]] && break
    if (($(now_ms) >= deadline)); then
      record "$item" fail "за $MIRROR_MS мс после отпускания доля первой части центра не изменилась ($share): layout.resize не пришла"
      return 0
    fi
    sleep "$POLL"
  done
  read_all "$profile" "$dir" after "$wb" "$owner_pid" || fatal "условие пункта: $WHY"
  want="$(awk -v w="$w0" -v d="$DRAG_DX" 'BEGIN { print w + d }')"
  got="$(part_field "$dir/after.layout.stdout" center 0 width)"
  awk -v a="$got" -v b="$want" -v t="$FRAME_TOLERANCE" 'BEGIN { d = a - b; if (d < 0) d = -d; exit !(d <= t) }' ||
    failures+=("ширина первой части центра $got после отпускания, ожидалось $want ± $FRAME_TOLERANCE (доля $share → $(lv "$dir/after.layout.stdout" 'layout.zones.center.parts[0].share'))")
  [[ -z "$FRAMES" ]] || failures+=("кадры после отпускания: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  frames_canon "$dir/after.layout.stdout" >"$dir/after.frames.json"
  core_cmd "$profile" "$dir/hide" layout hide right --workbench "$wb" --workspace "$ws" || failures+=("$WHY")
  core_cmd "$profile" "$dir/show" layout show right --workbench "$wb" --workspace "$ws" || failures+=("$WHY")
  settle_layout "$profile" "$dir/settle-shown" "$wb" "${REV:-0}" || fatal "$item: $WHY"
  read_all "$profile" "$dir" shown "$wb" "$owner_pid" || fatal "условие пункта: $WHY"
  frames_canon "$dir/shown.layout.stdout" >"$dir/shown.frames.json"
  cmp -s "$dir/after.frames.json" "$dir/shown.frames.json" ||
    failures+=("кадры после скрытия и показа правой зоны другие: after.frames.json и shown.frames.json")
  [[ -z "$FRAMES" ]] || failures+=("кадры после показа: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  conclude "$item" "разделитель между частями центра сдвинут CGEvent на $DRAG_DX точек: одна layout.resize, ширина первой части $got (ожидалось $want ± $FRAME_TOLERANCE); после layout hide и show right кадры прежние и совпали с ui.inspect и AX"
}

# Владелец на переднем плане: щелчки и меню идут в его окно; иначе WHY.
front_owner_ok() {
  local dir="$1"
  WHY=""
  osascript -l JavaScript -e "
    const se = Application('System Events');
    se.processes.whose({unixId: $owner_pid})[0].frontmost = true;
    'ok'" >"$dir/front.stdout" 2>"$dir/front.stderr" || {
    WHY="владелец не выведен вперёд: $(oneline "$dir/front.stderr")"
    return 1
  }
  local deadline
  deadline=$(($(now_ms) + MIRROR_MS))
  until [[ "$(front_pid)" == "$owner_pid" ]]; do
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс на переднем плане не владелец, а pid $(front_pid)"
      return 1
    fi
    sleep "$POLL"
  done
}

# Одна кнопка или пункт меню: цель скрывается и снова показывается, размеры и кадры после показа прежние.
toggle_pair() {
  local how="$1" label="$2" field="$3" dir="$4" name="$5" before
  frames_canon "$dir/$name.base.stdout" >"$dir/$name.base.frames.json"
  before="$(lv "$dir/$name.base.stdout" layout.sizes)"
  local round want
  for round in hide show; do
    [[ $round == hide ]] && want=true || want=false
    case "$how" in
      button) press_button "$owner_pid" "$label" "$dir/$name.$round" ;;
      menu) press_view_item "$owner_pid" "$label" "$dir/$name.$round" ;;
    esac || {
      failures+=("$WHY")
      return 0
    }
    wait_layout "$profile" "$dir/$name.$round.layout" "$wb" "layout.hidden.$field" "$want" || {
      failures+=("$how «$label», $round: $WHY")
      return 0
    }
  done
  [[ "$(lv "$dir/$name.show.layout.stdout" layout.sizes)" == "$before" ]] ||
    failures+=("$how «$label»: размеры после показа $(lv "$dir/$name.show.layout.stdout" layout.sizes), до — $before")
  frames_canon "$dir/$name.show.layout.stdout" >"$dir/$name.shown.frames.json"
  cmp -s "$dir/$name.base.frames.json" "$dir/$name.shown.frames.json" ||
    failures+=("$how «$label»: кадры после показа другие ($name.base.frames.json, $name.shown.frames.json)")
}

# Три кнопки зон ChromeBar и пункты меню «Вид» в двух конфигурациях: правая занята; правая пуста, плитка занята.
item_hide_zones() {
  local item=hide-zones dir="$run/hide-zones" bottom right label
  failures=()
  log "== $item"
  # Нижняя зона получает панель второй части центра, правая уже занята переносом move-cli.
  bottom="$(lv "$run/resize-drag/shown.layout.stdout" 'layout.zones.center.parts[1].tabs[0]' || echo ?)"
  right="$(lv "$run/resize-drag/shown.layout.stdout" 'layout.zones.right.parts[0].tabs[0]' || echo ?)"
  core_cmd "$profile" "$dir/bottom" panel move "$bottom" --zone bottom --workspace "$ws" || fatal "условие пункта: $WHY"
  settle_layout "$profile" "$dir/settle" "$wb" "$REV" || fatal "$item: $WHY"
  read_all "$profile" "$dir" one "$wb" "$owner_pid" || fatal "условие пункта: $WHY"
  [[ -z "$FRAMES" ]] || failures+=("кадры первой конфигурации: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  front_owner_ok "$dir" || {
    record "$item" fail "$WHY"
    return 0
  }
  for label in "Навигация:navigator:nav" "Нижняя зона:bottom:bottom" "Правая зона:right:right"; do
    IFS=: read -r text field name <<<"$label"
    cp "$dir/one.layout.stdout" "$dir/b-$name.base.stdout"
    toggle_pair button "$text" "$field" "$dir" "b-$name"
    cp "$dir/one.layout.stdout" "$dir/m-$name.base.stdout"
    toggle_pair menu "$text" "$field" "$dir" "m-$name"
  done
  # Вторая конфигурация: правая пуста, плитка левой зоны занята — третья кнопка «Левая зона».
  core_cmd "$profile" "$dir/left" panel move "$right" --zone left --workspace "$ws" || fatal "условие пункта: $WHY"
  settle_layout "$profile" "$dir/settle-two" "$wb" "$REV" || fatal "$item: $WHY"
  read_all "$profile" "$dir" two "$wb" "$owner_pid" || fatal "условие пункта: $WHY"
  [[ -z "$FRAMES" ]] || failures+=("кадры второй конфигурации: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  [[ "$(comp "$dir/two.inspect.stdout" chrome.zone.right label 2>/dev/null || echo ?)" == "Левая зона" ||
    -n "$(osascript -l JavaScript -e "Application('System Events').processes.whose({unixId: $owner_pid})[0].windows[0].buttons.whose({description: 'Левая зона'}).length" 2>/dev/null | grep -v '^0$')" ]] ||
    failures+=("третья кнопка не называется «Левая зона» во второй конфигурации")
  cp "$dir/two.layout.stdout" "$dir/b-plate.base.stdout"
  toggle_pair button "Левая зона" plate "$dir" b-plate
  cp "$dir/two.layout.stdout" "$dir/m-plate.base.stdout"
  toggle_pair menu "Левая зона" plate "$dir" m-plate
  # Правая зона возвращается: restore-cold меняет её размер.
  core_cmd "$profile" "$dir/back" panel move "$right" --zone right --workspace "$ws" || fatal "$item: $WHY"
  settle_layout "$profile" "$dir/settle-back" "$wb" "$REV" || fatal "$item: $WHY"
  conclude "$item" "кнопки «Навигация», «Нижняя зона», «Правая зона» и те же пункты меню «Вид» скрывают и показывают колонку навигатора, нижнюю и правую зоны; при пустой правой и занятой плитке третья кнопка и пункт «Левая зона» скрывают и показывают плитку; размеры и кадры после показа прежние"
}

# Перенос, изменение размера CLI и перетаскиванием, скрытие и фокус → app quit → новый запуск на том же профиле.
item_restore_cold() {
  local item=restore-cold dir="$run/restore-cold" other focus before after
  failures=()
  log "== $item"
  core_cmd "$profile" "$dir/resize" layout resize --zone right --workbench "$wb" "$RESIZE_RIGHT" --workspace "$ws" || {
    record "$item" fail "$WHY"
    return 0
  }
  other="$(lv "$run/move-cli/moved.layout.stdout" 'layout.zones.center.parts[0].tabs[0]' || echo ?)"
  core_cmd "$profile" "$dir/focus" panel focus "$other" --workspace "$ws" || {
    record "$item" fail "$WHY"
    return 0
  }
  core_cmd "$profile" "$dir/hide" layout hide bottom --workbench "$wb" --workspace "$ws" || {
    record "$item" fail "$WHY"
    return 0
  }
  settle_layout "$profile" "$dir/settle" "$wb" "$REV" || fatal "$item: $WHY"
  read_all "$profile" "$dir" before "$wb" "$owner_pid" || fatal "условие пункта: $WHY"
  [[ -z "$FRAMES" ]] || failures+=("до перезапуска кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  [[ "$(lv "$dir/before.layout.stdout" layout.hidden.bottom)" == true ]] ||
    failures+=("layout hide bottom: нижняя зона не скрыта")
  [[ "$(lv "$dir/before.layout.stdout" layout.sizes.right)" == "$RESIZE_RIGHT" ]] ||
    failures+=("layout resize: размер правой зоны $(lv "$dir/before.layout.stdout" layout.sizes.right || echo ?)")
  focus="$(focus_of "$dir/before.layout.stdout")"
  [[ "$focus" == "$other" ]] || failures+=("panel focus: фокус на ${focus:-нет}, а не на $other")
  layout_canon "$dir/before.layout.stdout" >"$dir/before.canon.json"
  quit_owner "$profile" "$dir" "$owner_pid"
  launch_normal "$profile" "$dir/relaunch"
  owner_pid="$OWNER_PID"
  settle_layout "$profile" "$dir/settle-after" "$wb" 0 || fatal "$item: после перезапуска: $WHY"
  read_all "$profile" "$dir" after "$wb" "$owner_pid" || fatal "условие пункта: после перезапуска: $WHY"
  layout_canon "$dir/after.layout.stdout" >"$dir/after.canon.json"
  cmp -s "$dir/before.canon.json" "$dir/after.canon.json" ||
    failures+=("раскладка после перезапуска не равна прежней побайтно: before.canon.json и after.canon.json")
  [[ "$(lv "$dir/after.layout.stdout" layout.hidden.bottom)" == true ]] ||
    failures+=("после перезапуска нижняя зона не скрыта")
  [[ "$(focus_of "$dir/after.layout.stdout")" == "$focus" ]] ||
    failures+=("фокус после перезапуска на $(focus_of "$dir/after.layout.stdout"), а не на $focus")
  [[ -z "$FRAMES" ]] || failures+=("после перезапуска кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  before="$(/usr/bin/perl -MJSON::PP -e 'local $/; print JSON::PP->new->utf8->canonical->encode(JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{frames})' <"$dir/before.layout.stdout")"
  after="$(/usr/bin/perl -MJSON::PP -e 'local $/; print JSON::PP->new->utf8->canonical->encode(JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{frames})' <"$dir/after.layout.stdout")"
  [[ "$before" == "$after" ]] || failures+=("кадры query.layout после перезапуска другие")
  conclude "$item" "перенос в правую зону, деление и перетаскивание разделителя центра, layout resize --zone right $RESIZE_RIGHT, скрытие нижней зоны и panel focus → app quit → запуск на том же профиле: раскладка и панели query.layout побайтно равны прежним, фокус на $focus, кадры query.layout равны прежним и совпали с ui.inspect и AX"
}

# Профиль схемы v1 с открытой рабочей областью: раскладка по умолчанию сразу, те же PanelId после перезапуска.
item_restore_legacy() {
  local item=restore-legacy dir="$run/restore-legacy" legacy bench first second
  failures=()
  log "== $item"
  new_profile
  legacy="$NEW_PROFILE"
  bench="019a8f2e-0000-7000-8000-00000000c101"
  chmod 700 "$legacy"
  /usr/bin/sqlite3 "$legacy/state.db" <"$fixture_v1" >"$dir/sqlite.log" 2>&1 ||
    fatal "фикстура v1 не загружена: $(oneline "$dir/sqlite.log")"
  launch_normal "$legacy" "$dir/first"
  local pid="$OWNER_PID"
  settle_layout "$legacy" "$dir/settle-first" "$bench" 0 || {
    record "$item" fail "сразу после запуска: $WHY"
    return 0
  }
  read_all "$legacy" "$dir" first "$bench" "$pid" || fatal "условие пункта: $WHY"
  first="$(panels_of "$dir/first.layout.stdout" navigator) $(panels_of "$dir/first.layout.stdout" terminal)"
  [[ "$(lv "$dir/first.layout.stdout" layout.navigator || echo null)" != null ]] ||
    failures+=("после запуска нет колонки навигатора")
  [[ "$(place_of "$dir/first.layout.stdout" "$(panels_of "$dir/first.layout.stdout" terminal)")" == center.0 ]] ||
    failures+=("после запуска в центре нет панели terminal")
  [[ -z "$FRAMES" ]] || failures+=("кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  quit_owner "$legacy" "$dir/first" "$pid"
  launch_normal "$legacy" "$dir/second"
  pid="$OWNER_PID"
  settle_layout "$legacy" "$dir/settle-second" "$bench" 0 || fatal "$item: после перезапуска: $WHY"
  layout_get "$legacy" "$dir/second.layout" "$bench" || fatal "$item: query.layout не прочитан"
  second="$(panels_of "$dir/second.layout.stdout" navigator) $(panels_of "$dir/second.layout.stdout" terminal)"
  [[ "$first" == "$second" ]] || failures+=("PanelId после второго запуска «${second}», после первого «${first}»")
  layout_canon "$dir/first.layout.stdout" >"$dir/first.canon.json"
  layout_canon "$dir/second.layout.stdout" >"$dir/second.canon.json"
  cmp -s "$dir/first.canon.json" "$dir/second.canon.json" || failures+=("раскладка после второго запуска другая")
  quit_owner "$legacy" "$dir/second" "$pid"
  conclude "$item" "state.db схемы v1 с открытой рабочей областью: сразу после запуска раскладка по умолчанию (колонка навигатора, terminal в центре), кадры совпали; после второго запуска те же PanelId ($first) и та же раскладка"
}

# Пункты следующих задач плана.
later_items() {
  local entry
  for entry in "${LATER[@]}"; do
    set_required "${entry%%:*}" 0
    record "${entry%%:*}" not_run "пункт задачи ${entry##*:} плана P1.4, в этом прогоне ещё не исполняется"
  done
}

prepare
preconditions
item_layout_default
item_layout_snapshot
item_move_cli
item_resize_drag
item_hide_zones
item_restore_cold
# System Events разрешает элементы окна по имени процесса: при двух владельцах бандла AX читает чужое окно, поэтому
# restore-legacy идёт при единственном живом владельце.
quit_owner "$profile" "$run/restore-cold" "$owner_pid"
item_restore_legacy
later_items
record voiceover-speech not_run "речь VoiceOver автоматика не наблюдает, роли и названия проверяет tooltips-ax"
record system-contrast not_run "процесс не переключает Increase Contrast системы, слоты плиток в ячейке IC проверяют модульные тесты темы"
completed=1
