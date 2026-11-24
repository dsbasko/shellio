#!/usr/bin/env bash
# Сценарий make e2e «хост плагинов» (P1.5): пункты матрицы приёмки плана plugin-host на подписанном бандле и
# временных профилях.
#
#   plugins.sh [<бандл>]
#
# Бандл по умолчанию — target/bundle/shellio.app, его собирает и проверяет цель make e2e. shellioctl и оракул
# shellio-e2e-oracle сценарий собирает сам (release, --locked). Профили — новые каталоги /tmp/shellio-e2e.*.
# Подготовка — штатный CLI: воркспейсы W1 и W2, проект в каждом, рабочие области A и A2 в проекте W1 и B в W2,
# workbench open A.
#
# Каталог прогона — target/e2e/plugins/<время>-<pid>/: result.json (пункт, обязательность, статус, причина, путь
# сырого вывода относительно каталога прогона), журнал сценария log и сырой вывод каждого пункта (JSON query.layout,
# query.panel и ui.*, AX окна, строки state.db, снимки PNG и отчёты оракула) в подкаталоге с его именем. Пробы
# предусловий пишут в preconditions/, уборка — в cleanup/.
#
# Предусловия — как у layout.sh, их отсутствие — ошибка исполнения «предусловие: …» до первого пункта: открытый
# сеанс GUI, разблокированный экран, запущенный Finder, бинарь бандла не запущен, запуск из процесса-потомка
# приложения терминала; у терминала-предка Screen Recording и Accessibility с Automation для System Events;
# латинский источник ввода; AeroSpace и другие оконные менеджеры выключены.
#
# Оракулы пунктов: «чтения» — query.layout и query.panel; «окно» — ui.inspect и AX окна (System Events), кадры
# совпадают в пределах 1 точки; «база» — sqlite3 по state.db остановленного владельца (version и
# hex(CAST(data AS BLOB)) строки panel_states); «снимок» — оракул без маркера по ожиданиям plugins-expect/*.json
# с подстановкой PanelId демо-списка. Пункт с собственным профилем идёт при остановленном основном владельце:
# System Events находит окно по имени процесса, и два владельца одного бандла путают AX.
#
# Рубежи накопительные (план P1.5, «Матрица приёмки»): пункт, чья задача ещё не выполнена, получает not_run
# «пункт задачи N» и необязателен.
#
# Свидетель меню: пункт, который выбирает команду в меню «Вид → Открыть панель», судится по новой квитанции
# ui.receipts с номером больше снятого до действия; пункт просмотра или отмены требует прежний номер.
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
fixture_status="$here/plugins-fixtures/v3-status.sql"
fixture_duplicate="$here/plugins-fixtures/v2-duplicate-demo-native.sql"
expect_dir="$here/plugins-expect"

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
SECOND_LAUNCH_S=20
MIRROR_MS=3000
# Срок фонового osascript пункта меню и shellioctl гонки.
MENU_OSA_S=20
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
DONE_TASK=12
# Плагины реестра в порядке регистрации: id, название, default_zone, кардинальность, min_size.
CATALOG=(
  "terminal|Терминал|center|multi|200|120"
  "navigator|Навигатор|left|per_workbench|200|120"
  "demo-list|Демо: список|right|per_workbench|260|160"
  "demo-native|Демо: поле|center|per_app|220|140"
)
# Фикстура missing-plugin: рабочая область и панель незарегистрированного плагина, байты её строки panel_states.
MISSING_BENCH="019a8f2e-0000-7000-8000-00000000c201"
MISSING_PANEL="019a8f2e-0000-7000-8000-00000000e203"
MISSING_ROW='1|7B2278223A317D'
MISSING_STUB="Плагин «gone-plugin» недоступен"
# Сообщение рабочей области без панелей с подсказкой меню и пометка занятого singleton в выборе новой части.
NO_PANELS_TEXT="Нет открытых панелей. Откройте панель: Вид → Открыть панель"
PICKER_TAKEN="(уже открыт)"
# «Демо: поле»: название пункта меню, имя поля в AX и подпись отказа на 257 символов.
NATIVE_TITLE="Демо: поле"
FIELD_AX="Значение"
TOO_LONG="Не сохранено: значение длиннее 256 символов: 257"
# «Демо: список»: min_size манифеста, подпись заглушки «Развернуть» и доли ширины списка в вариантах split и bar.
DEMO_MIN_W=260
DEMO_MIN_H=160
DEMO_TIGHT="Демо: список: здесь тесно"
SPLIT_SHARE=0.4
BAR_SHARE=0.6
# Фикстура status-stubs: рабочая область, демо-список с данными новее версии плагина и демо-поле с нечитаемыми.
STATUS_BENCH="019a8f2e-0000-7000-8000-00000000c301"
STATUS_LIST="019a8f2e-0000-7000-8000-00000000e303"
STATUS_FIELD="019a8f2e-0000-7000-8000-00000000e304"
STATUS_NEWER="Сохранено более новой версией"
STATUS_CORRUPT="Состояние не прочитано"
# Фикстура legacy-duplicate: воркспейс, рабочие области A и B, старшая панель демо-поля в A и младшая в B.
DUP_WS="019a8f2e-0000-7000-8000-00000000a401"
DUP_A="019a8f2e-0000-7000-8000-00000000c401"
DUP_B="019a8f2e-0000-7000-8000-00000000c402"
DUP_OLD="019a8f2e-0000-7000-8000-00000000e403"
DUP_NEW="019a8f2e-0000-7000-8000-00000000e406"
DUP_STUB="Уже открыт: Демо: поле"
# Значение демо-поля пункта restore-cold.
COLD_VALUE="cold-значение"
# Коды клавиш (kVK_*).
KEY_ESC=53
KEY_RETURN=36
KEY_DOWN=125

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
LAST=""
VALUE=""
EVENTS=""
# open-menu этого прогона прошёл: без квитанции UI в гонке это fail, а не «меню не нажато» (план P1.5, «Квитанции
# гонки»).
OPEN_MENU_PASS=0

# Профиль основной серии пунктов: воркспейсы W1 и W2, рабочие области A, A2 и B, панель демо-списка в A.
profile=""
owner_pid=""
ws1=""
ws2=""
wb_a=""
wb_a2=""
wb_b=""
demo=""
# Панель «Демо: поле»: одна на приложение, её создаёт open-menu-native и закрывает perapp-return.
native=""

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

# --- меню «Открыть панель», квитанции UI и выбор новой части (задача 10) ---

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

# Подменю «Вид → Открыть панель» владельца по System Events. Режим list — JSON пунктов [{title, enabled}]; press —
# AXPress пункта title; armed — ссылка на пункт title готовится заранее, в файл ready пишется отметка, AXPress
# идёт после одного байта из stdin: так нажатие стартует от того же сигнала, что и команда CLI гонки.
AX_OPEN_MENU_JS='function run(argv) {
  const [mode, pid, title, ready] = argv;
  const se = Application("System Events");
  const p = se.processes.whose({unixId: Number(pid)})[0];
  const menu = p.menuBars[0].menuBarItems.byName("Вид").menus[0].menuItems.byName("Открыть панель").menus[0];
  if (mode === "list") {
    return JSON.stringify(menu.menuItems().map((i) => ({ title: i.name(), enabled: i.enabled() })));
  }
  const item = menu.menuItems.byName(title);
  if (item.name() !== title) throw new Error("нет пункта «" + title + "»");
  if (mode === "armed") {
    ObjC.import("Foundation");
    $("ready\n").writeToFileAtomicallyEncodingError($(ready), true, $.NSUTF8StringEncoding, null);
    $.NSFileHandle.fileHandleWithStandardInput.readDataOfLength(1);
  }
  item.click();
  return "pressed " + title;
}'

# Пункты подменю «Открыть панель» владельца pid строкой «название:доступен|…» по порядку; чтение — <out>.json.
# Сразу после запуска System Events отдаёт меню без пунктов: опрос до want пунктов или срока, иначе WHY.
MENU=""
menu_items() {
  local pid="$1" out="$2" want="$3" deadline
  WHY=""
  MENU=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if osascript -l JavaScript -e "$AX_OPEN_MENU_JS" list "$pid" >"$out.json" 2>"$out.stderr"; then
      MENU="$(/usr/bin/perl -COEA -Mutf8 -MJSON::PP -e 'local $/; open my $fh, "<", $ARGV[0] or exit 2;
        my $items = eval { JSON::PP->new->utf8->decode(<$fh>) } or exit 2;
        print join("|", map { ($_->{title} // "?") . ":" . ($_->{enabled} ? "true" : "false") } @$items), "\n"' \
        "$out.json" 2>/dev/null || true)"
      if [[ -n "$MENU" ]]; then
        local seen=1 rest="$MENU"
        while [[ "$rest" == *"|"* ]]; do
          rest="${rest#*|}"
          seen=$((seen + 1))
        done
        ((seen < want)) || return 0
      fi
    fi
    if (($(now_ms) >= deadline)); then
      WHY="подменю «Вид → Открыть панель» по AX: «${MENU}», ожидалось пунктов $want: $(oneline "$out.stderr")"
      return 1
    fi
    sleep "$POLL"
  done
}

# AXPress пункта title подменю «Вид → Открыть панель» владельца pid; иначе WHY.
press_open_item() {
  local pid="$1" title="$2" out="$3"
  WHY=""
  printf '%s AXPress «%s»\n' "$(now_ms)" "$title" >>"$out.log"
  osascript -l JavaScript -e "$AX_OPEN_MENU_JS" press "$pid" "$title" >"$out.stdout" 2>"$out.stderr" || {
    WHY="пункт «${title}» меню «Вид → Открыть панель» не нажат: $(oneline "$out.stderr")"
    return 1
  }
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

# Число вкладок с названием title во всех видимых строках вкладок окна по ui.inspect.
window_tabs() {
  /usr/bin/perl -COEA -Mutf8 -MJSON::PP -e 'my ($file, $title) = @ARGV; open my $fh, "<", $file or exit 2; local $/;
    my $c = JSON::PP->new->utf8->decode(<$fh>)->{ok}{ui}{inspect}{components}; my $n = 0;
    for my $node (@$c) {
      next unless $node->{visible} && ref $node->{state} eq "HASH" && ref $node->{state}{tab_strip} eq "HASH";
      $n += grep { ($_->{title} // "") eq $title } @{$node->{state}{tab_strip}{tabs}};
    }
    print "$n\n"' "$1" "$2"
}

# Опрос ui.inspect до want вкладок с названием title; последнее чтение — <prefix>.stdout, иначе WHY.
wait_tabs() {
  local state="$1" prefix="$2" title="$3" want="$4" deadline seen="?"
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$state" "$prefix" inspect; then
      seen="$(window_tabs "$prefix.stdout" "$title" || echo ?)"
      [[ "$seen" == "$want" ]] && return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс вкладок «${title}» в окне — $seen, ожидалось $want"
      return 1
    fi
    sleep "$POLL"
  done
}

# Вкладки части zone.index раскладки query.layout: id через пробел.
tabs_of() {
  /usr/bin/perl -MJSON::PP -e 'my ($zone, $index) = @ARGV; local $/;
    my $p = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{layout}{zones}{$zone}{parts}[$index];
    print $p ? join(" ", @{$p->{tabs}}) : "?", "\n"' "$2" "$3" <"$1"
}

# Число частей зоны раскладки query.layout.
parts_count() {
  lv "$1" "layout.zones.$2.parts[#]" || echo ?
}

# Панели раскладки query.layout вне колонки навигатора: id через пробел.
tile_panels() {
  /usr/bin/perl -MJSON::PP -e 'local $/; my $l = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{layout};
    print join(" ", map { @{$_->{tabs}} } map { @{$l->{zones}{$_}{parts}} } qw(left right center bottom)), "\n"' <"$1"
}

# Строка вкладок окна по описанию: обход дерева AX до глубины 12, внутрь групп вкладок не заходит. AXPress кнопки
# строки, описание которой начинается с prefix.
AX_STRIP_JS='function run(argv) {
  const [pid, strip, prefix] = argv;
  const se = Application("System Events");
  const w = se.processes.whose({unixId: Number(pid)})[0].windows[0];
  const prop = (f) => { try { const v = f(); return v === undefined ? null : v; } catch (x) { return null; } };
  let found = null;
  function walk(e, depth) {
    if (found) return;
    const role = prop(() => e.role());
    if (role === "AXTabGroup") {
      if (prop(() => e.description()) === strip) found = e;
      return;
    }
    if (depth >= 12) return;
    let kids = [];
    try { kids = e.uiElements(); } catch (x) {}
    for (const k of kids) walk(k, depth + 1);
  }
  walk(w, 0);
  if (!found) throw new Error("нет строки вкладок «" + strip + "»");
  const buttons = found.buttons().map((b) => ({ element: b, description: prop(() => b.description()) || "" }));
  const hit = buttons.find((b) => b.description.startsWith(prefix));
  if (!hit) throw new Error("нет кнопки «" + prefix + "» в строке «" + strip + "»");
  hit.element.actions.byName("AXPress").perform();
  return "pressed " + hit.description;
}'

# Описание строки вкладок части index зоны zone из count частей, как его задаёт адаптер.
strip_label() {
  local zone="$1" index="$2" count="$3" name
  case "$zone" in
    left) name="левой зоны" ;;
    right) name="правой зоны" ;;
    center) name="центра" ;;
    bottom) name="нижней зоны" ;;
  esac
  if ((count > 1)); then
    printf 'Вкладки %s, часть %s\n' "$name" $((index + 1))
  else
    printf 'Вкладки %s\n' "$name"
  fi
}

# AXPress кнопки prefix строки вкладок strip окна владельца pid; иначе WHY.
strip_press() {
  local pid="$1" strip="$2" prefix="$3" out="$4"
  WHY=""
  osascript -l JavaScript -e "$AX_STRIP_JS" "$pid" "$strip" "$prefix" >"$out.stdout" 2>"$out.stderr" || {
    WHY="AXPress кнопки «${prefix}» строки «${strip}» не выполнен: $(oneline "$out.stderr")"
    return 1
  }
}

# Клавиша key code через System Events; журнал — <out>.
se_key() {
  printf '%s key code %s\n' "$(now_ms)" "$1" >>"$2"
  osascript -e "tell application \"System Events\" to key code $1" >>"$2" 2>&1
}

# Опрос ui.inspect до состояния выбора «Открыть в новой части»: layout.picker open — want; последнее чтение —
# <prefix>.stdout, иначе WHY.
wait_picker() {
  local state="$1" prefix="$2" want="$3" deadline seen=""
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$state" "$prefix" inspect; then
      seen="$(comp "$prefix.stdout" layout.picker state.menu.open || echo ?)"
      [[ "$seen" == "$want" ]] && return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс layout.picker open — ${seen:-?}, а не $want"
      return 1
    fi
    sleep "$POLL"
  done
}

# Открытый выбор закрывается Escape: путь отказа пункта не оставляет меню следующему.
close_picker() {
  local state="$1" dir="$2"
  ui_read "$state" "$dir/picker-left" inspect || return 0
  [[ "$(comp "$dir/picker-left.stdout" layout.picker state.menu.open || echo false)" == true ]] || return 0
  se_key "$KEY_ESC" "$dir/keys.log" || true
  wait_picker "$state" "$dir/picker-closed" false || true
}

# Открытый выбор новой части по AX: меню процесса и его окна, JSON [[{title, enabled}]]; пустой список — System
# Events меню не отдал.
AX_PICKER_JS='function run(argv) {
  const se = Application("System Events");
  const p = se.processes.whose({unixId: Number(argv[0])})[0];
  const prop = (f) => { try { const v = f(); return v === undefined ? null : v; } catch (x) { return null; } };
  let roots = [];
  try { roots = roots.concat(p.menus()); } catch (x) {}
  try { roots = roots.concat(p.windows[0].menus()); } catch (x) {}
  const out = [];
  for (const m of roots) {
    let items = [];
    try { items = m.menuItems(); } catch (x) {}
    out.push(items.map((i) => ({ title: prop(() => i.name()), enabled: prop(() => i.enabled()) })));
  }
  return JSON.stringify(out);
}'

# Выход фонового задания до срока MENU_OSA_S от его старта t0; код — в VALUE, иначе WHY.
await_bg() {
  local job="$1" t0="$2"
  VALUE=""
  while alive "$job"; do
    if (($(now_ms) - t0 >= MENU_OSA_S * 1000)); then
      kill -TERM "$job" 2>/dev/null || true
      WHY="фоновое задание $job не вышло за $MENU_OSA_S с"
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

# Гонка меню и CLI от одного сигнала (план P1.5, «Свидетель меню»): osascript с готовой ссылкой на пункт title и
# shellioctl panel open plugin ждут по байту из одного FIFO, сигнал — одна запись двух байтов. Конец ввода для
# этого не годится: FIFO macOS снимает его признак после первого читателя, второй остаётся ждать. Вывод —
# <dir>/ax.* и <dir>/cli.*, коды — в AX_CODE и CLI_CODE; иначе WHY. Перекрытие в обработке доказывает тест с
# барьером, а не этот сценарий.
AX_CODE=""
CLI_CODE=""
race_open() {
  local state="$1" dir="$2" pid="$3" title="$4" plugin="$5" bench="$6" ax cli t0 deadline
  WHY=""
  AX_CODE=""
  CLI_CODE=""
  rm -f "$dir/go" "$dir/ax.ready" "$dir/cli.ready"
  mkfifo "$dir/go" || {
    WHY="FIFO $dir/go не создан"
    return 1
  }
  # Пишущий конец держит сценарий: участники читают без признака конца ввода, пока он открыт.
  exec 9<>"$dir/go"
  osascript -l JavaScript -e "$AX_OPEN_MENU_JS" armed "$pid" "$title" "$dir/ax.ready" \
    <"$dir/go" 9>&- >"$dir/ax.stdout" 2>"$dir/ax.stderr" &
  ax=$!
  bg_jobs+=("$ax")
  (
    trap - ERR EXIT
    set +e
    exec 9>&- 3<"$dir/go"
    : >"$dir/cli.ready"
    read -r -n 1 _ <&3
    exec env -u SHELLIO_SOCKET "SHELLIO_STATE_DIR=$state" "$ctl_bin" --json --timeout "$CTL_TIMEOUT_MS" \
      panel open "$plugin" --all --workbench "$bench" 3<&- >"$dir/cli.stdout" 2>"$dir/cli.stderr"
  ) &
  cli=$!
  bg_jobs+=("$cli")
  t0="$(now_ms)"
  deadline=$((t0 + MENU_OSA_S * 1000))
  until [[ -e "$dir/ax.ready" && -e "$dir/cli.ready" ]]; do
    if (($(now_ms) >= deadline)) || ! alive "$ax" || ! alive "$cli"; then
      # Сначала участники завершаются, потом закрывается FIFO: конец ввода не должен запустить команду.
      kill -TERM "$ax" "$cli" 2>/dev/null || true
      await_bg "$ax" "$t0" || true
      await_bg "$cli" "$t0" || true
      exec 9>&-
      WHY="участники гонки не готовы: пункт «${title}» — $(oneline "$dir/ax.stderr"), CLI — $(oneline "$dir/cli.stderr")"
      return 1
    fi
    sleep "$POLL"
  done
  printf '%s сигнал: пункт «%s» и panel open %s\n' "$(now_ms)" "$title" "$plugin" >"$dir/go.log"
  printf 'gg' >&9
  if ! await_bg "$ax" "$t0"; then
    exec 9>&-
    return 1
  fi
  AX_CODE="$VALUE"
  if ! await_bg "$cli" "$t0"; then
    exec 9>&-
    return 1
  fi
  CLI_CODE="$VALUE"
  exec 9>&-
}

# Основной владелец запускается снова на своём профиле: restore-list и пункты с собственным профилем оставляют
# его остановленным. Окно показывает рабочую область маршрута профиля.
resume_main() {
  local dir="$1"
  [[ -z "$owner_pid" ]] || return 0
  launch_normal "$profile" "$dir"
  owner_pid="$OWNER_PID"
}

# Окно основного владельца переходит к рабочей области bench воркспейса ws и выходит вперёд; иначе ошибка
# исполнения.
show_bench() {
  local dir="$1" bench="$2" ws="$3"
  core_cmd "$profile" "$dir/show-bench" workbench open --target "$bench" --workspace "$ws" ||
    fatal "условие пункта: $WHY"
  settle_layout "$profile" "$dir/show-settle" "$bench" "$REV" || fatal "условие пункта: $WHY"
  front_ok "$owner_pid" "$dir" || fatal "условие пункта: $WHY"
}

# --- «Демо: поле»: ввод, подпись и фокус (задача 11) ---

# Текст через System Events владельцу на переднем плане; журнал — <out>. Только латиница: keystroke набирает по
# раскладке.
se_keystroke() {
  printf '%s keystroke %s (%s символов)\n' "$(now_ms)" "${1:0:40}" "${#1}" >>"$2"
  osascript -e "tell application \"System Events\" to keystroke \"$1\"" >>"$2" 2>&1
}

# Поле с вводом очищается командами редактора Control-A (в начало строки) и Control-K (удалить до конца): меню
# «Правка» с «Выбрать все» у приложения нет.
field_clear() {
  printf '%s control-a control-k\n' "$(now_ms)" >>"$1"
  osascript -e 'tell application "System Events" to keystroke "a" using control down' \
    -e 'tell application "System Events" to keystroke "k" using control down' >>"$1" 2>&1
}

# Опрос ui.inspect: текст поля тела панели равен want; ответ — <prefix>.stdout, иначе WHY.
wait_field() {
  local state="$1" prefix="$2" panel="$3" want="$4" deadline got="нет"
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$state" "$prefix" inspect; then
      got="$(comp "$prefix.stdout" "layout.panel.$panel.field" state.text_field.value 2>/dev/null || echo нет)"
      [[ "$got" != "$want" ]] || return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс текст поля — «${got:0:40}» (${#got} символов), ожидался «${want:0:40}» (${#want} символов)"
      return 1
    fi
    sleep "$POLL"
  done
}

# Опрос ui.inspect: подпись тела панели равна want; ответ — <prefix>.stdout, иначе WHY.
wait_caption() {
  wait_inspect "$1" "$2" "layout.panel.$3.caption.message" label "$4"
}

# Поля ввода и подписи окна процесса pid по System Events: JSON [{role, description, value}]; последним — элемент
# с вводом приложения (AXFocusedUIElement) с ролью focus.
AX_TEXTS_JS='function run(argv) {
  const se = Application("System Events");
  const p = se.processes.whose({unixId: Number(argv[0])})[0];
  function prop(f) { try { const v = f(); return v === undefined ? null : v; } catch (x) { return null; } }
  const out = [];
  function walk(e, depth) {
    const role = prop(() => e.role());
    if (role === "AXTextField" || role === "AXStaticText") {
      out.push({ role: role, description: prop(() => e.description()), value: prop(() => e.value()) });
    }
    if (depth >= 12 || role === "AXTabGroup") return;
    let kids = [];
    try { kids = e.uiElements(); } catch (x) {}
    for (const k of kids) walk(k, depth + 1);
  }
  let kids = [];
  try { kids = p.windows[0].uiElements(); } catch (x) {}
  for (const k of kids) walk(k, 1);
  const f = prop(() => p.attributes.byName("AXFocusedUIElement").value());
  out.push({ role: "focus", description: f ? prop(() => f.role()) + "|" + prop(() => f.description()) : null,
    value: null });
  return JSON.stringify(out);
}'

# Значение элемента роли role с описанием description в чтении AX_TEXTS_JS; код 1 — элемента нет.
ax_text() {
  /usr/bin/perl -COEA -Mutf8 -MJSON::PP -e '
    my ($file, $role, $description) = @ARGV;
    open my $fh, "<", $file or exit 2;
    local $/;
    my $a = eval { JSON::PP->new->utf8->decode(<$fh>) } or exit 2;
    my ($hit) = grep { ($_->{role} // "") eq $role && ($_->{description} // "") eq $description } @$a;
    exit 1 unless $hit;
    print $hit->{value} // "", "\n";
  ' "$1" "$2" "$3"
}

# Опрос AX окна pid: поле «Значение» держит текст want и, если подпись задана, в окне есть подпись caption; чтение
# — в file, иначе WHY.
wait_ax_field() {
  local pid="$1" file="$2" want="$3" caption="${4:-}" deadline got="нет" seen=1
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    osascript -l JavaScript -e "$AX_TEXTS_JS" "$pid" >"$file" 2>"$file.stderr" ||
      fatal "AX окна pid $pid не прочитан: $(oneline "$file.stderr")"
    got="$(ax_text "$file" AXTextField "$FIELD_AX" 2>/dev/null || echo нет)"
    seen=1
    [[ -z "$caption" ]] || ax_text "$file" AXStaticText "$caption" >/dev/null 2>&1 || seen=0
    [[ "$got" != "$want" || $seen -ne 1 ]] || return 0
    if (($(now_ms) >= deadline)); then
      WHY="AX: поле «${FIELD_AX}» — «${got:0:40}» (${#got} символов), ожидалось «${want:0:40}» (${#want} символов)"
      ((seen)) || WHY="$WHY; подписи «${caption}» в окне нет"
      return 1
    fi
    sleep "$POLL"
  done
}

# Фокус панели демо-поля рабочей области bench, ввод у её поля, владелец впереди; иначе WHY.
native_focus() {
  local dir="$1" bench="$2"
  core_cmd "$profile" "$dir/focus" panel focus "$native" --all || return 1
  settle_layout "$profile" "$dir/focus-settle" "$bench" "$REV" || return 1
  wait_inspect "$profile" "$dir/focus-field" "layout.panel.$native.field" first_responder true || return 1
  front_ok "$owner_pid" "$dir"
}

# Ревизия ядра по query.layout рабочей области bench — в NOW_REV; чтение — <prefix>.stdout, иначе ошибка
# исполнения.
NOW_REV=""
revision_of() {
  layout_get "$profile" "$1" "$2" || fatal "query.layout не прочитан: $(oneline "$1.stderr")"
  NOW_REV="$(jv "$1.stdout" ok.revision)" || fatal "в query.layout нет ревизии: $(oneline "$1.stdout")"
}

# --- снимки и оракул (задача 12) ---

# Ожидания с подстановкой панели: @DEMO@ — PanelId демо-списка.
expectations() {
  sed -e "s/@DEMO@/$2/g" "$expect_dir/$1.json" >"$3"
}

# Снимок окна владельца в режиме без маркера: снимки повторяются, пока пара подряд не устоялась (код оракула 0
# или 3, либо 4 с retry: false). expect — файл ожиданий или underline:<id вкладки>:<слой>. Код оракула — в
# snap_code, причина — в snap_reason. Сырые файлы — <dir>/<name>.<N>.{window.json,…}.
snap_code=""
snap_reason=""
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

# --- клавиши демо, зоны, заглушки и холодный запуск (задача 12) ---

# Опрос query.panel: поле path равно want; ответ — <prefix>.stdout, иначе WHY.
wait_panel() {
  local state="$1" prefix="$2" panel="$3" path="$4" want="$5" deadline got=""
  WHY=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if panel_get "$state" "$prefix" "$panel"; then
      got="$(pv "$prefix.stdout" "$path" 2>/dev/null || echo нет)"
      [[ "$got" != "$want" ]] || return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс query.panel: $path — ${got:-не прочитан}, ожидалось $want"
      return 1
    fi
    sleep "$POLL"
  done
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

# Демо-список основного профиля командами: история пуста, детали закрыты, выбран key; чтения — <dir>/reset-*,
# иначе WHY.
demo_reset() {
  local dir="$1" key="$2" n=0
  WHY=""
  while :; do
    panel_get "$profile" "$dir/reset-$n" "$demo" || {
      WHY="query.panel демо-списка не прочитан: $(oneline "$dir/reset-$n.stdout") $(oneline "$dir/reset-$n.stderr")"
      return 1
    }
    [[ "$(pv "$dir/reset-$n.stdout" data.history || echo ?)" != "[]" ]] || break
    if ((n >= 33)); then
      WHY="история демо-списка не опустела за 33 команды demo-list.back"
      return 1
    fi
    core_cmd "$profile" "$dir/reset-back-$n" panel command "$demo" demo-list.back --workspace "$ws1" || return 1
    n=$((n + 1))
  done
  core_cmd "$profile" "$dir/reset-details" panel command "$demo" demo-list.details --args '{"open":false}' \
    --workspace "$ws1" || return 1
  core_cmd "$profile" "$dir/reset-select" panel command "$demo" demo-list.select --args "{\"key\":\"$key\"}" \
    --workspace "$ws1" || return 1
}

# Вариант демо-списка по таблице плагина для зоны и класса размера query.layout.
demo_variant() {
  case "$1.$2" in
    *.short_bar) echo bar ;;
    *.narrow) echo drill ;;
    left.regular | right.regular) echo drill ;;
    left.wide | right.wide | center.regular | center.wide) echo split ;;
    bottom.regular | bottom.wide) echo bar ;;
    *) echo "?" ;;
  esac
}

# Часть query.layout с активной панелью panel: «зона номер класс ширина высота stub» её тела; пусто — такой части
# нет.
part_with() {
  /usr/bin/perl -MJSON::PP -e 'my ($panel) = @ARGV; local $/;
    my ($p) = grep { ($_->{active} // "") eq $panel } @{JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{frames}{parts}};
    print $p ? join(" ", $p->{zone}, $p->{index}, $p->{geometry}{size}, $p->{body}{width}, $p->{body}{height},
      $p->{stub} ? "true" : "false") : "", "\n"' "$2" <"$1"
}

# Скрытого списка тела в ui.inspect нет: узел id отсутствует или не видим.
pane_hidden() {
  [[ "$(comp "$1" "$2" visible 2>/dev/null || echo false)" == false ]]
}

# Регионы представления демо-списка в окне по ui.inspect: строки нарушений в stdout, пусто — совпало. drill — один
# список во всё тело (детали закрыты, их узла в дереве нет), split и bar — список слева на долю ширины тела и
# детали на остальной, как их считает модель плагина.
demo_regions() {
  /usr/bin/perl -COEA -Mutf8 -MJSON::PP -e '
    my ($file, $panel, $variant, $split, $bar, $tol) = @ARGV;
    open my $fh, "<", $file or die "$file\n";
    local $/;
    my %node = map { ($_->{id} => $_) } @{JSON::PP->new->utf8->decode(<$fh>)->{ok}{ui}{inspect}{components}};
    my ($body, $list, $details) = map { $node{"layout.panel.$panel.$_"} } qw(body list details);
    for (["body", $body], ["list", $list], ["details", $details]) {
      next if $_->[1] || ($variant eq "drill" && $_->[0] eq "details");
      print "нет узла layout.panel.$panel.$_->[0]\n";
      exit 0;
    }
    my $near = sub { abs($_[0] - $_[1]) <= $tol };
    my $fmt = sub { my $f = $_[0]{frame}; sprintf "(%g, %g, %g×%g)", @$f{qw(x y width height)} };
    my $same = sub {
      my ($n, $x, $w, $what) = @_;
      my $f = $n->{frame};
      my $b = $body->{frame};
      print "$what не видим\n" unless $n->{visible};
      print "$what ", $fmt->($n), ", по модели x $x, ширина $w, высота тела: тело ", $fmt->($body), "\n"
        unless $near->($f->{x}, $x) && $near->($f->{width}, $w) && $near->($f->{y}, $b->{y})
        && $near->($f->{height}, $b->{height});
    };
    my $b = $body->{frame};
    if ($variant eq "drill") {
      $same->($list, $b->{x}, $b->{width}, "список");
      print "детали видимы при закрытых деталях\n" if $details && $details->{visible};
    } else {
      my $w = int($b->{width} * ($variant eq "split" ? $split : $bar));
      $same->($list, $b->{x}, $w, "список");
      $same->($details, $b->{x} + $w, $b->{width} - $w, "детали");
    }
  ' "$1" "$2" "$3" "$SPLIT_SHARE" "$BAR_SHARE" "$FRAME_TOLERANCE"
}

# Кадр части zone.index из query.layout: поле field (x, y, width, height) прямоугольника rect части — frame (по
# умолчанию), body или tab_strip.
part_field() {
  /usr/bin/perl -MJSON::PP -e 'my ($zone, $index, $field, $rect) = @ARGV; local $/;
    my ($p) = grep { $_->{zone} eq $zone && $_->{index} == $index } @{JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{frames}{parts}};
    print defined $p ? $p->{$rect}{$field} . "\n" : "?\n"' "$2" "$3" "$4" "${5:-frame}" <"$1"
}

# Активная вкладка части zone.index раскладки query.layout.
active_of() {
  /usr/bin/perl -MJSON::PP -e 'my ($zone, $index) = @ARGV; local $/;
    my $p = JSON::PP->new->utf8->decode(<STDIN>)->{ok}{value}{layout}{zones}{$zone}{parts}[$index];
    print $p ? $p->{tabs}[$p->{active}] // "?" : "?", "\n"' "$2" "$3" <"$1"
}

# AXPress кнопки «Развернуть» заглушки, центр которой лежит в прямоугольнике тела (x, y, w, h) в точках содержимого
# окна pid с началом сверху слева: заглушки разных частей различаются только местом. Обход дерева AX до глубины 12,
# внутрь групп вкладок не заходит.
AX_STUB_JS='function run(argv) {
  const [pid, x, y, w, h] = argv.map(Number);
  const se = Application("System Events");
  const win = se.processes.whose({unixId: pid})[0].windows[0];
  const prop = (f) => { try { const v = f(); return v === undefined ? null : v; } catch (e) { return null; } };
  const [wx, wy] = win.position();
  let found = null;
  function walk(e, depth) {
    if (found) return;
    const role = prop(() => e.role());
    if (role === "AXTabGroup") return;
    if (role === "AXButton" && prop(() => e.description()) === "Развернуть") {
      const p = prop(() => e.position());
      const s = prop(() => e.size());
      if (p && s) {
        const cx = p[0] - wx + s[0] / 2;
        const cy = p[1] - wy + s[1] / 2;
        if (cx >= x && cx <= x + w && cy >= y && cy <= y + h) { found = e; return; }
      }
    }
    if (depth >= 12) return;
    let kids = [];
    try { kids = e.uiElements(); } catch (e) {}
    for (const k of kids) walk(k, depth + 1);
  }
  walk(win, 0);
  if (!found) throw new Error("нет кнопки «Развернуть» в теле (" + [x, y, w, h].join(", ") + ")");
  found.actions.byName("AXPress").perform();
  return "pressed";
}'

# AXPress кнопки «Развернуть» заглушки в теле части zone.index по query.layout layout; иначе WHY.
stub_press() {
  local pid="$1" layout="$2" zone="$3" index="$4" out="$5" x y w h
  WHY=""
  x="$(part_field "$layout" "$zone" "$index" x body)"
  y="$(part_field "$layout" "$zone" "$index" y body)"
  w="$(part_field "$layout" "$zone" "$index" width body)"
  h="$(part_field "$layout" "$zone" "$index" height body)"
  [[ "$x" != "?" ]] || {
    WHY="нет части $zone.$index в query.layout"
    return 1
  }
  osascript -l JavaScript -e "$AX_STUB_JS" "$pid" "$x" "$y" "$w" "$h" >"$out.stdout" 2>"$out.stderr" || {
    WHY="AXPress кнопки «Развернуть» части $zone.$index не выполнен: $(oneline "$out.stderr")"
    return 1
  }
}

# Строка workbench_layouts рабочей области в state.db остановленного владельца: HEX байтов layout.
layout_row() {
  /usr/bin/sqlite3 -readonly "$1/state.db" \
    "SELECT hex(CAST(layout AS BLOB)) FROM workbench_layouts WHERE workbench = X'${2//-/}';" 2>&1
}

# Заглушка статуса панели panel профиля state: подпись label заглушки её места без кнопки «Развернуть» по
# ui.inspect и AX окна pid, статус status в ui.inspect и query.panel, ни одного построенного тела. Нарушения — в
# failures с пометкой what; чтения — <prefix>.*.
status_stub() {
  local state="$1" prefix="$2" pid="$3" bench="$4" panel="$5" status="$6" label="$7" what="$8" place got
  layout_get "$state" "$prefix.layout" "$bench" || fatal "$what: query.layout не прочитан: $(oneline "$prefix.layout.stderr")"
  place="$(place_of "$prefix.layout.stdout" "$panel")"
  [[ -n "$place" && "$place" != navigator ]] || {
    failures+=("$what: панели $panel нет в плитках раскладки: «${place}»")
    return 0
  }
  wait_inspect "$state" "$prefix.inspect" "layout.part.$place.stub" label "$label" ||
    failures+=("$what: заглушка в окне: $WHY")
  [[ "$(comp "$prefix.inspect.stdout" "layout.part.$place.stub" visible || echo нет)" == true ]] ||
    failures+=("$what: заглушка части $place не видима")
  ! comp "$prefix.inspect.stdout" "layout.part.$place.stub.expand" id >/dev/null 2>&1 ||
    failures+=("$what: у заглушки статуса есть кнопка «Развернуть»")
  got="$(pfact "$prefix.inspect.stdout" "$panel" status || echo нет)"
  [[ "$got" == "$status" ]] || failures+=("$what: ui.inspect: статус $got, ожидался $status")
  wait_ax_text "$pid" "$prefix.ax.json" "$label" || failures+=("$what: в AX окна нет элемента «${label}»")
  if panel_get "$state" "$prefix.panel" "$panel"; then
    got="$(pv "$prefix.panel.stdout" status || echo ?)"
    [[ "$got" == "$status" ]] || failures+=("$what: query.panel: статус $got, ожидался $status")
  else
    failures+=("$what: query.panel не прочитан: $(oneline "$prefix.panel.stdout") $(oneline "$prefix.panel.stderr")")
  fi
  ui_read "$state" "$prefix.stats" stats || fatal "$what: ui.stats не прочитан"
  got="$(built_of "$prefix.stats.stdout" "$panel")"
  [[ "$got" == 0 ]] || failures+=("$what: bodies_built панели $got, ожидалось 0")
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
  for left in "$fixture_missing" "$fixture_status" "$fixture_duplicate" "$expect_dir"/{light,dark}.json; do
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
# служебного владельца обычного режима, самопроверка оракула на его снимке; затем он завершается.
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

# Подменю «Вид → Открыть панель» по AX: пункты совпадают с plugin list по названиям и порядку, terminal первым;
# в WorkspaceHome нового профиля пункты недоступны. Просмотр меню квитанций UI не добавляет. Проверка WorkspaceHome
# идёт при остановленном основном владельце, затем он запускается снова для остальных пунктов.
item_menu_list() {
  local item=menu-list dir="$run/menu-list" state pid code count i title want="" off="" last0 file
  failures=()
  log "== $item"
  if [[ -n "$owner_pid" ]]; then
    quit_owner "$profile" "$dir" "$owner_pid"
    owner_pid=""
  fi
  new_profile
  state="$NEW_PROFILE"
  launch_normal "$state" "$dir/home"
  pid="$OWNER_PID"
  code=0
  ctl "$state" "$dir/home.layout" --json --timeout "$CTL_TIMEOUT_MS" layout get --all || code=$?
  [[ $code -ne 0 && "$(jv "$dir/home.layout.stdout" core_error.details.reason || echo нет)" == no_open_workbench ]] ||
    fatal "условие пункта: новый профиль не в WorkspaceHome: $(oneline "$dir/home.layout.stdout")"
  file="$dir/home.plugins.stdout"
  ctl "$state" "$dir/home.plugins" --json --timeout "$CTL_TIMEOUT_MS" plugin list ||
    fatal "$item: plugin list не прочитан: $(oneline "$file") $(oneline "$dir/home.plugins.stderr")"
  count="$(jv "$file" 'ok.value.plugins[#]' || echo 0)"
  [[ "$count" =~ ^[0-9]+$ ]] && ((count > 0)) || fatal "$item: в plugin list нет плагинов: $(oneline "$file")"
  for ((i = 0; i < count; i++)); do
    title="$(jv "$file" "ok.value.plugins[$i].title" || echo ?)"
    want="${want:+$want|}$title:true"
    off="${off:+$off|}$title:false"
  done
  front_ok "$pid" "$dir/home" || fatal "условие пункта: $WHY"
  receipts_last "$state" "$dir/home.receipts-before" || fatal "$item: $WHY"
  last0="$LAST"
  if menu_items "$pid" "$dir/home.menu" "$count"; then
    [[ "$MENU" == "$off" ]] ||
      failures+=("WorkspaceHome: пункты меню «${MENU}», ожидались недоступные «${off}»")
  else
    failures+=("WorkspaceHome: $WHY")
  fi
  receipts_last "$state" "$dir/home.receipts-after" || fatal "$item: $WHY"
  [[ "$LAST" == "$last0" ]] || failures+=("WorkspaceHome: просмотр меню добавил квитанцию UI: $last0 → $LAST")
  quit_owner "$state" "$dir/home" "$pid"

  resume_main "$dir/main"
  settle_layout "$profile" "$dir/main-settle" "$wb_a" 0 || fatal "$item: основной владелец: $WHY"
  front_ok "$owner_pid" "$dir" || fatal "условие пункта: $WHY"
  file="$dir/plugins.stdout"
  ctl "$profile" "$dir/plugins" --json --timeout "$CTL_TIMEOUT_MS" plugin list ||
    fatal "$item: plugin list не прочитан: $(oneline "$file") $(oneline "$dir/plugins.stderr")"
  [[ "$(jv "$file" 'ok.value.plugins[0].id' || echo ?)" == terminal ]] ||
    failures+=("первый плагин реестра — $(jv "$file" 'ok.value.plugins[0].id' || echo ?), а не terminal")
  receipts_last "$profile" "$dir/receipts-before" || fatal "$item: $WHY"
  last0="$LAST"
  if menu_items "$owner_pid" "$dir/menu" "$count"; then
    [[ "$MENU" == "$want" ]] ||
      failures+=("открытая рабочая область: пункты меню «${MENU}», по plugin list ожидались доступные «${want}»")
  else
    failures+=("открытая рабочая область: $WHY")
  fi
  receipts_last "$profile" "$dir/receipts-after" || fatal "$item: $WHY"
  [[ "$LAST" == "$last0" ]] || failures+=("просмотр меню добавил квитанцию UI: $last0 → $LAST")
  conclude "$item" "AX меню «Вид → Открыть панель»: пункты «${want}» совпали с plugin list по названиям и порядку, terminal первым, при открытой рабочей области доступны; в WorkspaceHome нового профиля те же пункты недоступны; номер последней квитанции UI прежний ($last0)"
}

# AXPress «Вид → Открыть панель → Демо: список» в A2: одна новая квитанция created с панелью из чтений, панель в
# правой зоне окна.
item_open_menu() {
  local item=open-menu dir="$run/open-menu" last0 panel rev
  failures=()
  log "== $item"
  show_bench "$dir" "$wb_a2" "$ws1"
  layout_get "$profile" "$dir/before" "$wb_a2" || fatal "$item: query.layout не прочитан"
  [[ -z "$(panels_of "$dir/before.stdout" demo-list)" ]] ||
    fatal "условие пункта: в A2 уже есть демо-список $(panels_of "$dir/before.stdout" demo-list)"
  receipts_last "$profile" "$dir/receipts-before" || fatal "$item: $WHY"
  last0="$LAST"
  press_open_item "$owner_pid" "Демо: список" "$dir/press" || {
    record "$item" fail "$WHY"
    return 0
  }
  wait_receipts "$profile" "$dir/receipts" "$last0" 1 || {
    record "$item" fail "меню закрылось без квитанции: $WHY"
    return 0
  }
  panel="$(rc "$dir/receipts.stdout" 0 panel)"
  rev="$(rc "$dir/receipts.stdout" 0 revision)"
  [[ "$(jv "$dir/receipts.stdout" 'ok.ui.receipts.receipts[#]' || echo ?)" == 1 ]] ||
    failures+=("новых квитанций UI $(jv "$dir/receipts.stdout" 'ok.ui.receipts.receipts[#]' || echo ?), ожидалась одна")
  [[ "$(jv "$dir/receipts.stdout" ok.ui.receipts.last || echo ?)" == $((last0 + 1)) ]] ||
    failures+=("номер последней квитанции $(jv "$dir/receipts.stdout" ok.ui.receipts.last || echo ?), до нажатия $last0")
  [[ "$(rc "$dir/receipts.stdout" 0 plugin)" == demo-list ]] ||
    failures+=("квитанция плагина $(rc "$dir/receipts.stdout" 0 plugin), а не demo-list")
  [[ "$(rc "$dir/receipts.stdout" 0 outcome)" == created ]] ||
    failures+=("исход квитанции $(rc "$dir/receipts.stdout" 0 outcome) (отказ $(rc "$dir/receipts.stdout" 0 error)), ожидался created")
  [[ "$(rc "$dir/receipts.stdout" 0 workbench)" == "$wb_a2" ]] ||
    failures+=("рабочая область квитанции $(rc "$dir/receipts.stdout" 0 workbench), а не A2 $wb_a2")
  if [[ "$rev" =~ ^[0-9]+$ ]]; then
    settle_layout "$profile" "$dir/settle" "$wb_a2" "$rev" || fatal "$item: $WHY"
    wait_inspect "$profile" "$dir/wait" "layout.panel.$panel" state.panel.plugin demo-list ||
      failures+=("панель в окне: $WHY")
    read_all "$profile" "$dir" opened "$wb_a2" "$owner_pid" || fatal "условие пункта: $WHY"
    [[ "$(panels_of "$dir/opened.layout.stdout" demo-list)" == "$panel" ]] ||
      failures+=("демо-список в чтениях «$(panels_of "$dir/opened.layout.stdout" demo-list)», в квитанции $panel")
    [[ "$(place_of "$dir/opened.layout.stdout" "$panel")" == right.* ]] ||
      failures+=("панель в «$(place_of "$dir/opened.layout.stdout" "$panel")», а не в правой зоне")
    [[ -z "$FRAMES" ]] || failures+=("кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  else
    failures+=("в квитанции нет ревизии: $(oneline "$dir/receipts.stdout")")
  fi
  [[ ${#failures[@]} -ne 0 ]] || OPEN_MENU_PASS=1
  conclude "$item" "AXPress «Вид → Открыть панель → Демо: список» в A2: одна новая квитанция ui.receipts $((last0 + 1)) — panel.open demo-list created, панель $panel равна панели query.layout и стоит в правой зоне окна, кадры совпали с ui.inspect и AX"
}

# A2 без панелей: сообщение «Нет открытых панелей» с подсказкой меню; меню «Терминал» открывает терминал в центре.
item_empty_workbench_open() {
  local item=empty-workbench-open dir="$run/empty-workbench-open" nav panel last0 rev n=0
  failures=()
  log "== $item"
  show_bench "$dir" "$wb_a2" "$ws1"
  layout_get "$profile" "$dir/before" "$wb_a2" || fatal "$item: query.layout не прочитан"
  nav="$(lv "$dir/before.stdout" layout.navigator || echo null)"
  for panel in $(tile_panels "$dir/before.stdout"); do
    n=$((n + 1))
    core_cmd "$profile" "$dir/close-$n" panel close "$panel" --workspace "$ws1" || fatal "условие пункта: $WHY"
  done
  if [[ "$nav" != null ]]; then
    core_cmd "$profile" "$dir/close-navigator" panel close "$nav" --workspace "$ws1" || fatal "условие пункта: $WHY"
  fi
  ((n > 0)) || [[ "$nav" != null ]] || fatal "условие пункта: в A2 нечего закрывать"
  settle_layout "$profile" "$dir/settle-empty" "$wb_a2" "$REV" || fatal "$item: $WHY"
  read_all "$profile" "$dir" empty "$wb_a2" "$owner_pid" || fatal "условие пункта: $WHY"
  [[ "$(lv "$dir/empty.layout.stdout" 'panels[#]' || echo ?)" == 0 ]] ||
    fatal "условие пункта: в A2 остались панели: $(lv "$dir/empty.layout.stdout" panels | cut -c 1-200)"
  [[ "$(lv "$dir/empty.layout.stdout" frames.no_panels || echo null)" != null ]] ||
    failures+=("в query.layout нет места сообщения frames.no_panels")
  wait_inspect "$profile" "$dir/empty.note" layout.note label "$NO_PANELS_TEXT" ||
    failures+=("сообщение в окне: $WHY")
  [[ "$(comp "$dir/empty.note.stdout" layout.note visible || echo false)" == true ]] ||
    failures+=("узел layout.note не видим")
  wait_ax_text "$owner_pid" "$dir/empty.note.ax.json" "$NO_PANELS_TEXT" ||
    failures+=("в AX окна нет элемента «${NO_PANELS_TEXT}»")
  front_ok "$owner_pid" "$dir" || fatal "условие пункта: $WHY"
  receipts_last "$profile" "$dir/receipts-before" || fatal "$item: $WHY"
  last0="$LAST"
  press_open_item "$owner_pid" "Терминал" "$dir/press" || {
    record "$item" fail "$WHY"
    return 0
  }
  wait_receipts "$profile" "$dir/receipts" "$last0" 1 || {
    record "$item" fail "меню закрылось без квитанции: $WHY"
    return 0
  }
  panel="$(rc "$dir/receipts.stdout" 0 panel)"
  rev="$(rc "$dir/receipts.stdout" 0 revision)"
  [[ "$(rc "$dir/receipts.stdout" 0 plugin)" == terminal ]] ||
    failures+=("квитанция плагина $(rc "$dir/receipts.stdout" 0 plugin), а не terminal")
  [[ "$(rc "$dir/receipts.stdout" 0 outcome)" == created ]] ||
    failures+=("исход квитанции $(rc "$dir/receipts.stdout" 0 outcome) (отказ $(rc "$dir/receipts.stdout" 0 error)), ожидался created")
  if [[ "$rev" =~ ^[0-9]+$ ]]; then
    settle_layout "$profile" "$dir/settle" "$wb_a2" "$rev" || fatal "$item: $WHY"
    wait_inspect "$profile" "$dir/wait" "layout.panel.$panel" state.panel.plugin terminal ||
      failures+=("терминал в окне: $WHY")
    read_all "$profile" "$dir" opened "$wb_a2" "$owner_pid" || fatal "условие пункта: $WHY"
    [[ "$(place_of "$dir/opened.layout.stdout" "$panel")" == center.0 ]] ||
      failures+=("терминал в «$(place_of "$dir/opened.layout.stdout" "$panel")», а не в центре")
    [[ "$(panels_of "$dir/opened.layout.stdout" terminal)" == "$panel" ]] ||
      failures+=("панели terminal в чтениях «$(panels_of "$dir/opened.layout.stdout" terminal)», в квитанции $panel")
    [[ "$(comp "$dir/opened.inspect.stdout" layout.note visible || echo false)" != true ]] ||
      failures+=("сообщение «Нет открытых панелей» осталось видимым")
    [[ -z "$FRAMES" ]] || failures+=("кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  else
    failures+=("в квитанции нет ревизии: $(oneline "$dir/receipts.stdout")")
  fi
  conclude "$item" "A2 без панелей (закрыто $n вне колонки и навигатор): сообщение «${NO_PANELS_TEXT}» в query.layout, ui.inspect и AX; меню «Терминал» — квитанция created, терминал $panel в центре, кадры совпали с ui.inspect и AX"
}

# Общая часть open-race и race-multi: окно показывает B, курсор и номер квитанции снимаются до действия, меню и CLI
# стартуют от одного сигнала. Ответ CLI — <dir>/cli.stdout, квитанция UI — <dir>/receipts.stdout, курсор — в
# RACE_REV. Код 1 — пункт уже записан.
RACE_REV=""
race_run() {
  local item="$1" dir="$2" title="$3" plugin="$4" last0
  RACE_REV=""
  receipts_last "$profile" "$dir/receipts-before" || fatal "$item: $WHY"
  last0="$LAST"
  layout_get "$profile" "$dir/cursor" "$wb_b" || fatal "$item: query.layout не прочитан"
  RACE_REV="$(jv "$dir/cursor.stdout" ok.revision)" || fatal "$item: в query.layout нет ревизии"
  race_open "$profile" "$dir" "$owner_pid" "$title" "$plugin" "$wb_b" || fatal "$item: $WHY"
  if ! wait_receipts "$profile" "$dir/receipts" "$last0" 1; then
    # План P1.5, «Квитанции гонки»: без квитанции UI судит одиночный open-menu того же прогона.
    if ((OPEN_MENU_PASS)); then
      record "$item" fail "квитанции UI нет при open-menu pass: $WHY; osascript — код $AX_CODE, $(oneline "$dir/ax.stderr")"
      return 1
    fi
    fatal "$item: меню не нажато: $WHY; osascript — код $AX_CODE, $(oneline "$dir/ax.stderr")"
  fi
  [[ "$(jv "$dir/receipts.stdout" 'ok.ui.receipts.receipts[#]' || echo ?)" == 1 ]] ||
    failures+=("новых квитанций UI $(jv "$dir/receipts.stdout" 'ok.ui.receipts.receipts[#]' || echo ?), ожидалась одна")
  [[ "$CLI_CODE" == 0 ]] ||
    failures+=("shellioctl panel open $plugin: код $CLI_CODE: $(oneline "$dir/cli.stdout") $(oneline "$dir/cli.stderr")")
  [[ "$(rc "$dir/receipts.stdout" 0 plugin)" == "$plugin" ]] ||
    failures+=("квитанция UI плагина $(rc "$dir/receipts.stdout" 0 plugin), а не $plugin")
  [[ "$(rc "$dir/receipts.stdout" 0 error)" == нет ]] ||
    failures+=("квитанция UI — отказ $(rc "$dir/receipts.stdout" 0 error)")
}

# В B без демо-списка меню «Демо: список» и shellioctl panel open demo-list от одного сигнала: одна панель.
item_open_race() {
  local item=open-race dir="$run/open-race" cp co cw cr up uo uw ur next outcomes
  failures=()
  log "== $item"
  show_bench "$dir" "$wb_b" "$ws2"
  layout_get "$profile" "$dir/before" "$wb_b" || fatal "$item: query.layout не прочитан"
  [[ -z "$(panels_of "$dir/before.stdout" demo-list)" ]] ||
    fatal "условие пункта: в B уже есть демо-список $(panels_of "$dir/before.stdout" demo-list)"
  race_run "$item" "$dir" "Демо: список" demo-list || return 0
  next=$((RACE_REV + 1))
  cp="$(jv "$dir/cli.stdout" ok.value.panel 2>/dev/null || echo нет)"
  co="$(jv "$dir/cli.stdout" ok.value.outcome 2>/dev/null || echo нет)"
  cw="$(jv "$dir/cli.stdout" ok.value.workbench 2>/dev/null || echo нет)"
  cr="$(jv "$dir/cli.stdout" ok.revision 2>/dev/null || echo нет)"
  up="$(rc "$dir/receipts.stdout" 0 panel)"
  uo="$(rc "$dir/receipts.stdout" 0 outcome)"
  uw="$(rc "$dir/receipts.stdout" 0 workbench)"
  ur="$(rc "$dir/receipts.stdout" 0 revision)"
  [[ "$cp" != нет && "$cp" == "$up" ]] || failures+=("панели разные: CLI $cp, квитанция UI $up")
  [[ "$cw" == "$wb_b" && "$uw" == "$wb_b" ]] ||
    failures+=("рабочая область: CLI $cw, квитанция UI $uw, ожидалась B $wb_b")
  outcomes="$(printf '%s\n%s\n' "$co" "$uo" | sort | tr '\n' ' ')"
  [[ "$outcomes" == "created focused " ]] || failures+=("исходы: CLI $co, квитанция UI $uo, ожидались created и focused")
  [[ "$cr" == "$next" && "$ur" == "$next" ]] ||
    failures+=("ревизии квитанций: CLI $cr, UI $ur, ожидалась $next у обеих")
  if events_after "$profile" "$dir/events" "$RACE_REV"; then
    [[ "$EVENTS" == "1|1|$next" ]] ||
      failures+=("события после курсора $RACE_REV: «${EVENTS}» (событий|panel_add|ревизии), ожидалось «1|1|${next}»")
  else
    failures+=("$WHY")
  fi
  settle_layout "$profile" "$dir/settle" "$wb_b" "$next" || failures+=("$WHY")
  wait_tabs "$profile" "$dir/tabs" "Демо: список" 1 || failures+=("$WHY")
  layout_get "$profile" "$dir/after" "$wb_b" || fatal "$item: query.layout не прочитан"
  [[ "$(panels_of "$dir/after.stdout" demo-list)" == "$cp" ]] ||
    failures+=("демо-списки B в чтениях «$(panels_of "$dir/after.stdout" demo-list)», в ответах $cp")
  conclude "$item" "AXPress «Демо: список» и shellioctl panel open demo-list --all --workbench B от одного сигнала: обе квитанции успешны, один PanelId $cp в B, исходы CLI $co и UI $uo, после курсора $RACE_REV одно событие с одним panel_add и ревизией $next, обе квитанции несут $next, в окне одна вкладка «Демо: список»"
}

# То же с «Терминал» (Multi): две новые панели.
item_race_multi() {
  local item=race-multi dir="$run/race-multi" cp co cr up uo ur tabs before want_tabs rev adds
  failures=()
  log "== $item"
  show_bench "$dir" "$wb_b" "$ws2"
  layout_get "$profile" "$dir/before" "$wb_b" || fatal "$item: query.layout не прочитан"
  before="$(panels_of "$dir/before.stdout" terminal)"
  ui_read "$profile" "$dir/before.inspect" inspect || fatal "$item: ui.inspect не прочитан"
  tabs="$(window_tabs "$dir/before.inspect.stdout" "Терминал")" || fatal "$item: вкладки окна не сосчитаны"
  want_tabs=$((tabs + 2))
  race_run "$item" "$dir" "Терминал" terminal || return 0
  cp="$(jv "$dir/cli.stdout" ok.value.panel 2>/dev/null || echo нет)"
  co="$(jv "$dir/cli.stdout" ok.value.outcome 2>/dev/null || echo нет)"
  cr="$(jv "$dir/cli.stdout" ok.revision 2>/dev/null || echo нет)"
  up="$(rc "$dir/receipts.stdout" 0 panel)"
  uo="$(rc "$dir/receipts.stdout" 0 outcome)"
  ur="$(rc "$dir/receipts.stdout" 0 revision)"
  [[ "$cp" != нет && "$up" != нет && "$cp" != "$up" ]] || failures+=("панели не две разные: CLI $cp, квитанция UI $up")
  [[ " $before " != *" $cp "* && " $before " != *" $up "* ]] ||
    failures+=("панель из ответов уже была в B до действия: CLI $cp, UI $up, до действия «${before}»")
  [[ "$co" == created && "$uo" == created ]] || failures+=("исходы: CLI $co, квитанция UI $uo, ожидались оба created")
  if events_after "$profile" "$dir/events" "$RACE_REV"; then
    adds="${EVENTS#*|}"
    adds="${adds%%|*}"
    [[ "$EVENTS" != reset && "$adds" == 2 ]] ||
      failures+=("события после курсора $RACE_REV: «${EVENTS}» (событий|panel_add|ревизии), ожидалось ровно два panel_add")
  else
    failures+=("$WHY")
  fi
  rev="$RACE_REV"
  [[ "$cr" =~ ^[0-9]+$ ]] && ((cr > rev)) && rev="$cr"
  [[ "$ur" =~ ^[0-9]+$ ]] && ((ur > rev)) && rev="$ur"
  settle_layout "$profile" "$dir/settle" "$wb_b" "$rev" || failures+=("$WHY")
  wait_tabs "$profile" "$dir/tabs" "Терминал" "$want_tabs" || failures+=("$WHY")
  conclude "$item" "AXPress «Терминал» и shellioctl panel open terminal --all --workbench B от одного сигнала: две разные панели $cp и $up, оба исхода created, после курсора $RACE_REV ровно два panel_add (${EVENTS}), вкладок «Терминал» в окне $want_tabs — на две больше, чем до действия ($tabs)"
}

# layout split <terminal> --open demo-list при существующем демо-списке A: отказ singleton_exists без изменений.
item_singleton_split() {
  local item=singleton-split dir="$run/singleton-split" term rev before after code reason
  failures=()
  log "== $item"
  layout_get "$profile" "$dir/before" "$wb_a" || fatal "$item: query.layout не прочитан"
  [[ -n "$demo" && "$(panels_of "$dir/before.stdout" demo-list)" == "$demo" ]] ||
    fatal "условие пункта: в A нет демо-списка open-cli: «$(panels_of "$dir/before.stdout" demo-list)»"
  term="$(panels_of "$dir/before.stdout" terminal)"
  term="${term%% *}"
  [[ -n "$term" ]] || fatal "условие пункта: в A нет панели terminal"
  rev="$(jv "$dir/before.stdout" ok.revision)" || fatal "$item: в query.layout нет ревизии"
  before="$(layout_canon "$dir/before.stdout")"
  code=0
  ctl "$profile" "$dir/split" --json --timeout "$CTL_TIMEOUT_MS" layout split "$term" --open demo-list \
    --workspace "$ws1" || code=$?
  reason="$(jv "$dir/split.stdout" core_error.details.reason 2>/dev/null || echo нет)"
  [[ $code -ne 0 && "$reason" == singleton_exists ]] ||
    failures+=("layout split --open demo-list: код $code, причина $reason, ожидался отказ singleton_exists: $(oneline "$dir/split.stdout" | cut -c 1-200)")
  [[ "$(jv "$dir/split.stdout" core_error.details.panel 2>/dev/null || echo нет)" == "$demo" ]] ||
    failures+=("отказ называет панель $(jv "$dir/split.stdout" core_error.details.panel 2>/dev/null || echo нет), а не демо-список $demo")
  layout_get "$profile" "$dir/after" "$wb_a" || fatal "$item: query.layout не прочитан"
  after="$(layout_canon "$dir/after.stdout")"
  [[ "$after" == "$before" ]] || failures+=("раскладка A после отказа другая (before.stdout, after.stdout)")
  [[ "$(jv "$dir/after.stdout" ok.revision || echo ?)" == "$rev" ]] ||
    failures+=("ревизия после отказа $(jv "$dir/after.stdout" ok.revision || echo ?), до него $rev")
  if events_after "$profile" "$dir/events" "$rev"; then
    [[ "$EVENTS" == "0|0|" ]] || failures+=("события после курсора $rev: «${EVENTS}», ожидалось ни одного")
  else
    failures+=("$WHY")
  fi
  conclude "$item" "layout split $term --open demo-list при демо-списке $demo в A: отказ singleton_exists с его PanelId, раскладка и ревизия $rev прежние, событий после курсора нет"
}

# Выбор «Открыть в новой части» у части центра A с одной вкладкой терминала при открытом демо-списке: «Терминал»
# первым и доступен, занятый singleton недоступен с пометкой в названии; Escape ничего не меняет.
item_picker_singleton() {
  local item=picker-singleton dir="$run/picker-singleton" term rev last0 strip parts file taken ax
  failures=()
  log "== $item"
  [[ -n "$demo" ]] || fatal "условие пункта: панели демо-списка нет: open-cli её не создал"
  show_bench "$dir" "$wb_a" "$ws1"
  layout_get "$profile" "$dir/start" "$wb_a" || fatal "$item: query.layout не прочитан"
  if [[ "$(place_of "$dir/start.stdout" "$demo")" != right.* ]]; then
    core_cmd "$profile" "$dir/demo-right" panel move "$demo" --zone right --workspace "$ws1" ||
      fatal "условие пункта: $WHY"
    layout_get "$profile" "$dir/start" "$wb_a" || fatal "$item: query.layout не прочитан"
  fi
  term="$(tabs_of "$dir/start.stdout" center 0)"
  [[ "$(parts_count "$dir/start.stdout" center)" == 1 && "$term" != "?" && "$term" != *" "* &&
    " $(panels_of "$dir/start.stdout" terminal) " == *" $term "* ]] ||
    fatal "условие пункта: в центре A не одна часть с единственной вкладкой терминала: частей $(parts_count "$dir/start.stdout" center), вкладки «${term}»"
  core_cmd "$profile" "$dir/focus" panel focus "$term" --workspace "$ws1" || fatal "условие пункта: $WHY"
  settle_layout "$profile" "$dir/settle" "$wb_a" "$REV" || fatal "$item: $WHY"
  read_all "$profile" "$dir" before "$wb_a" "$owner_pid" || fatal "условие пункта: $WHY"
  [[ "$(focus_of "$dir/before.layout.stdout")" == "$term" ]] ||
    fatal "условие пункта: фокус на $(focus_of "$dir/before.layout.stdout"), а не на терминале $term"
  [[ "$(place_of "$dir/before.layout.stdout" "$demo")" == right.* ]] ||
    fatal "условие пункта: демо-список в «$(place_of "$dir/before.layout.stdout" "$demo")», а не в правой зоне"
  rev="$(jv "$dir/before.layout.stdout" ok.revision)" || fatal "$item: в query.layout нет ревизии"
  parts="$(parts_count "$dir/before.layout.stdout" center)"
  receipts_last "$profile" "$dir/receipts-before" || fatal "$item: $WHY"
  last0="$LAST"
  strip="$(strip_label center 0 1)"
  front_ok "$owner_pid" "$dir" || fatal "условие пункта: $WHY"
  strip_press "$owner_pid" "$strip" "Разделить" "$dir/press" || {
    record "$item" fail "$WHY"
    return 0
  }
  wait_picker "$profile" "$dir/open" true || {
    record "$item" fail "«Разделить» строки «${strip}»: $WHY"
    return 0
  }
  file="$dir/open.stdout"
  taken="Демо: список $PICKER_TAKEN"
  [[ "$(comp "$file" layout.picker 'state.menu.items[0].id' || echo ?)" == terminal ]] ||
    failures+=("первый пункт выбора — $(comp "$file" layout.picker 'state.menu.items[0].id' || echo ?), а не terminal")
  [[ "$(comp "$file" layout.picker 'state.menu.items[0].title' || echo ?)" == "Терминал" ]] ||
    failures+=("название первого пункта «$(comp "$file" layout.picker 'state.menu.items[0].title' || echo ?)», а не «Терминал»")
  [[ "$(comp "$file" layout.picker 'state.menu.items[0].enabled' || echo ?)" == true ]] ||
    failures+=("пункт «Терминал» недоступен")
  [[ "$(comp "$file" layout.picker 'state.menu.items[id=demo-list].title' || echo нет)" == "$taken" ]] ||
    failures+=("пункт demo-list называется «$(comp "$file" layout.picker 'state.menu.items[id=demo-list].title' || echo нет)», а не «${taken}»")
  [[ "$(comp "$file" layout.picker 'state.menu.items[id=demo-list].enabled' || echo ?)" == false ]] ||
    failures+=("пункт «${taken}» доступен")
  # AX открытого выбора: System Events отдаёт меню не всегда; отданное судится, неотданное — запись в причине.
  ax="$(osascript -l JavaScript -e "$AX_PICKER_JS" "$owner_pid" 2>"$dir/picker.ax.stderr" || true)"
  printf '%s\n' "$ax" >"$dir/picker.ax.json"
  ax="$(/usr/bin/perl -COEA -Mutf8 -MJSON::PP -e 'my ($file, $first, $taken) = @ARGV; open my $fh, "<", $file or exit 0;
    local $/; my $menus = eval { JSON::PP->new->utf8->decode(<$fh>) }; exit 0 unless ref $menus eq "ARRAY";
    for my $m (@$menus) {
      next unless @$m && ($m->[0]{title} // "") eq $first;
      my ($hit) = grep { ($_->{title} // "") eq $taken } @$m;
      print $m->[0]{enabled} && $hit && !$hit->{enabled} ? "ok\n" : "bad\n";
      exit 0;
    }' "$dir/picker.ax.json" "Терминал" "$taken" 2>/dev/null || true)"
  case "$ax" in
    ok) ax="AX выбора: «Терминал» первым и доступен, «${taken}» недоступен" ;;
    bad)
      failures+=("AX выбора расходится с ui.inspect: $(oneline "$dir/picker.ax.json" | cut -c 1-300)")
      ax="AX выбора расходится"
      ;;
    *) ax="System Events меню выбора не отдал (picker.ax.json), пункты судятся по ui.inspect" ;;
  esac
  se_key "$KEY_ESC" "$dir/keys.log" || failures+=("Escape не отправлен: $(oneline "$dir/keys.log")")
  wait_picker "$profile" "$dir/closed" false || {
    close_picker "$profile" "$dir"
    record "$item" fail "Escape: $WHY"
    return 0
  }
  layout_get "$profile" "$dir/escaped" "$wb_a" || fatal "$item: query.layout не прочитан"
  [[ "$(parts_count "$dir/escaped.stdout" center)" == "$parts" ]] ||
    failures+=("после Escape частей центра $(parts_count "$dir/escaped.stdout" center), до — $parts")
  [[ "$(jv "$dir/escaped.stdout" ok.revision || echo ?)" == "$rev" ]] ||
    failures+=("после Escape ревизия $(jv "$dir/escaped.stdout" ok.revision || echo ?), до — $rev")
  if events_after "$profile" "$dir/events" "$rev"; then
    [[ "$EVENTS" == "0|0|" ]] || failures+=("события после курсора $rev: «${EVENTS}», ожидалось ни одного")
  else
    failures+=("$WHY")
  fi
  receipts_last "$profile" "$dir/receipts-after" || fatal "$item: $WHY"
  [[ "$LAST" == "$last0" ]] || failures+=("выбор и Escape добавили квитанцию UI: $last0 → $LAST")
  conclude "$item" "«Разделить» строки «${strip}» в A при демо-списке в правой зоне: в выборе «Терминал» первым и доступен, «${taken}» недоступен (ui.inspect layout.picker; ${ax}); Escape — частей центра $parts, ревизия $rev и номер квитанции UI $last0 прежние, событий после курсора нет"
}

# AXPress «Вид → Открыть панель → Демо: поле» в A: квитанция created, панель в центре, ввод у поля; x доходит до
# поля, Escape возвращает пустой черновик без ревизии.
item_open_menu_native() {
  local item=open-menu-native dir="$run/open-menu-native" last0 rev bench name
  failures=()
  log "== $item"
  show_bench "$dir" "$wb_a" "$ws1"
  for name in a a2 b; do
    case "$name" in
      a) bench="$wb_a" ;;
      a2) bench="$wb_a2" ;;
      *) bench="$wb_b" ;;
    esac
    layout_get "$profile" "$dir/before-$name" "$bench" || fatal "$item: query.layout не прочитан"
    [[ -z "$(panels_of "$dir/before-$name.stdout" demo-native)" ]] ||
      fatal "условие пункта: демо-поле уже открыто: $(panels_of "$dir/before-$name.stdout" demo-native)"
  done
  receipts_last "$profile" "$dir/receipts-before" || fatal "$item: $WHY"
  last0="$LAST"
  press_open_item "$owner_pid" "$NATIVE_TITLE" "$dir/press" || {
    record "$item" fail "$WHY"
    return 0
  }
  wait_receipts "$profile" "$dir/receipts" "$last0" 1 || {
    record "$item" fail "меню закрылось без квитанции: $WHY"
    return 0
  }
  native="$(rc "$dir/receipts.stdout" 0 panel)"
  rev="$(rc "$dir/receipts.stdout" 0 revision)"
  [[ "$(jv "$dir/receipts.stdout" 'ok.ui.receipts.receipts[#]' || echo ?)" == 1 ]] ||
    failures+=("новых квитанций UI $(jv "$dir/receipts.stdout" 'ok.ui.receipts.receipts[#]' || echo ?), ожидалась одна")
  [[ "$(rc "$dir/receipts.stdout" 0 plugin)" == demo-native ]] ||
    failures+=("квитанция плагина $(rc "$dir/receipts.stdout" 0 plugin), а не demo-native")
  [[ "$(rc "$dir/receipts.stdout" 0 outcome)" == created ]] ||
    failures+=("исход квитанции $(rc "$dir/receipts.stdout" 0 outcome) (отказ $(rc "$dir/receipts.stdout" 0 error)), ожидался created")
  [[ "$(rc "$dir/receipts.stdout" 0 workbench)" == "$wb_a" ]] ||
    failures+=("рабочая область квитанции $(rc "$dir/receipts.stdout" 0 workbench), а не A $wb_a")
  if [[ ! "$rev" =~ ^[0-9]+$ || "$native" == нет ]]; then
    native=""
    record "$item" fail "в квитанции нет панели или ревизии: $(oneline "$dir/receipts.stdout")"
    return 0
  fi
  settle_layout "$profile" "$dir/settle" "$wb_a" "$rev" || fatal "$item: $WHY"
  wait_inspect "$profile" "$dir/wait-field" "layout.panel.$native.field" first_responder true ||
    failures+=("ввод не у поля демо: $WHY")
  read_all "$profile" "$dir" opened "$wb_a" "$owner_pid" || fatal "условие пункта: $WHY"
  [[ "$(panels_of "$dir/opened.layout.stdout" demo-native)" == "$native" ]] ||
    failures+=("демо-поле в чтениях «$(panels_of "$dir/opened.layout.stdout" demo-native)», в квитанции $native")
  [[ "$(place_of "$dir/opened.layout.stdout" "$native")" == center.* ]] ||
    failures+=("панель в «$(place_of "$dir/opened.layout.stdout" "$native")», а не в центре")
  [[ "$(focus_of "$dir/opened.layout.stdout")" == "$native" ]] ||
    failures+=("фокус на $(focus_of "$dir/opened.layout.stdout"), а не на демо-поле $native")
  [[ "$(pfact "$dir/opened.inspect.stdout" "$native" variant || echo нет)" == center ]] ||
    failures+=("вариант $(pfact "$dir/opened.inspect.stdout" "$native" variant || echo нет), ожидался center")
  [[ -z "$FRAMES" ]] || failures+=("кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  front_ok "$owner_pid" "$dir" || fatal "условие пункта: $WHY"
  se_keystroke x "$dir/keys.log" || failures+=("keystroke не отправлен: $(oneline "$dir/keys.log")")
  wait_field "$profile" "$dir/typed" "$native" x || failures+=("ввод x: $WHY")
  wait_ax_field "$owner_pid" "$dir/typed.ax.json" x || failures+=("ввод x: $WHY")
  se_key "$KEY_ESC" "$dir/keys.log" || failures+=("Escape не отправлен: $(oneline "$dir/keys.log")")
  wait_field "$profile" "$dir/escaped" "$native" "" || failures+=("Escape: $WHY")
  if panel_get "$profile" "$dir/panel" "$native"; then
    [[ "$(pv "$dir/panel.stdout" data.value || echo ?)" == "" && "$(pv "$dir/panel.stdout" data.edits || echo ?)" == 0 ]] ||
      failures+=("query.panel: данные $(pv "$dir/panel.stdout" data || echo ?), ожидались пустое значение и 0 правок")
  else
    failures+=("query.panel не прочитан: $(oneline "$dir/panel.stdout") $(oneline "$dir/panel.stderr")")
  fi
  revision_of "$dir/after" "$wb_a"
  [[ "$NOW_REV" == "$rev" ]] ||
    failures+=("ввод и Escape дали ревизию $(jv "$dir/after.stdout" ok.revision || echo ?), после открытия была $rev")
  conclude "$item" "AXPress «Вид → Открыть панель → ${NATIVE_TITLE}» в A: квитанция ui.receipts $((last0 + 1)) — panel.open demo-native created, панель $native в центре с фокусом, вариант center, first_responder у поля в ui.inspect, кадры совпали с ui.inspect и AX; x дошёл до поля (ui.inspect и AXValue), Escape вернул пустой черновик, ревизия $rev прежняя"
}

# Правки поля: Return сохраняет одной ревизией, Escape возвращает черновик без ревизии, 257 символов — отказ в
# подписи при целом черновике, значение по CLI с кириллицей видно в подписи, повтор — без ревизии.
item_native_edit() {
  local item=native-edit dir="$run/native-edit" rev0 rev1 long saved cyr
  failures=()
  log "== $item"
  [[ -n "$native" ]] || {
    record "$item" fail "панели демо-поля нет: open-menu-native её не создал"
    return 0
  }
  show_bench "$dir" "$wb_a" "$ws1"
  native_focus "$dir" "$wb_a" || fatal "условие пункта: $WHY"
  wait_ax_field "$owner_pid" "$dir/empty.ax.json" "" || fatal "условие пункта: черновик не пуст: $WHY"
  revision_of "$dir/start" "$wb_a"
  rev0="$NOW_REV"

  saved="Сохранено: shellio-check · правок 1"
  se_keystroke shellio-check "$dir/keys.log" || failures+=("keystroke не отправлен: $(oneline "$dir/keys.log")")
  wait_field "$profile" "$dir/typed" "$native" shellio-check || failures+=("ввод: $WHY")
  se_key "$KEY_RETURN" "$dir/keys.log" || failures+=("Return не отправлен: $(oneline "$dir/keys.log")")
  wait_caption "$profile" "$dir/saved" "$native" "$saved" || failures+=("Return: $WHY")
  if panel_get "$profile" "$dir/panel-saved" "$native"; then
    [[ "$(pv "$dir/panel-saved.stdout" data.value || echo ?)" == shellio-check &&
      "$(pv "$dir/panel-saved.stdout" data.edits || echo ?)" == 1 ]] ||
      failures+=("после Return данные $(pv "$dir/panel-saved.stdout" data || echo ?), ожидались shellio-check и 1 правка")
  else
    failures+=("query.panel после Return не прочитан: $(oneline "$dir/panel-saved.stderr")")
  fi
  revision_of "$dir/after-save" "$wb_a"
  rev1="$NOW_REV"
  [[ "$rev1" == $((rev0 + 1)) ]] || failures+=("Return дал ревизию $rev1 после $rev0, ожидалась одна ревизия")

  se_keystroke zz "$dir/keys.log" || failures+=("keystroke не отправлен: $(oneline "$dir/keys.log")")
  wait_field "$profile" "$dir/edited" "$native" shellio-checkzz || failures+=("правка без Return: $WHY")
  se_key "$KEY_ESC" "$dir/keys.log" || failures+=("Escape не отправлен: $(oneline "$dir/keys.log")")
  wait_field "$profile" "$dir/escaped" "$native" shellio-check || failures+=("Escape: $WHY")
  revision_of "$dir/after-escape" "$wb_a"
  [[ "$NOW_REV" == "$rev1" ]] ||
    failures+=("правка и Escape дали ревизию $(jv "$dir/after-escape.stdout" ok.revision || echo ?), до них $rev1")

  long="$(printf 'abcdefghij%.0s' 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25)abcdefg"
  [[ ${#long} -eq 257 ]] || fatal "$item: строка проверки не из 257 символов: ${#long}"
  field_clear "$dir/keys.log" || failures+=("очистка поля не отправлена: $(oneline "$dir/keys.log")")
  wait_field "$profile" "$dir/cleared" "$native" "" || failures+=("очистка поля: $WHY")
  se_keystroke "$long" "$dir/keys.log" || failures+=("keystroke не отправлен: $(oneline "$dir/keys.log")")
  wait_field "$profile" "$dir/long" "$native" "$long" || failures+=("ввод 257 символов: $WHY")
  se_key "$KEY_RETURN" "$dir/keys.log" || failures+=("Return не отправлен: $(oneline "$dir/keys.log")")
  wait_caption "$profile" "$dir/refused" "$native" "$TOO_LONG" || failures+=("Return с 257 символами: $WHY")
  wait_field "$profile" "$dir/refused-field" "$native" "$long" || failures+=("черновик после отказа: $WHY")
  wait_ax_field "$owner_pid" "$dir/refused.ax.json" "$long" "$TOO_LONG" || failures+=("после отказа: $WHY")
  if panel_get "$profile" "$dir/panel-refused" "$native"; then
    [[ "$(pv "$dir/panel-refused.stdout" data.value || echo ?)" == shellio-check &&
      "$(pv "$dir/panel-refused.stdout" data.edits || echo ?)" == 1 ]] ||
      failures+=("после отказа данные $(pv "$dir/panel-refused.stdout" data || echo ?), ожидались прежние")
  else
    failures+=("query.panel после отказа не прочитан: $(oneline "$dir/panel-refused.stderr")")
  fi
  revision_of "$dir/after-refusal" "$wb_a"
  [[ "$NOW_REV" == "$rev1" ]] ||
    failures+=("отказ дал ревизию $(jv "$dir/after-refusal.stdout" ok.revision || echo ?), до него $rev1")

  cyr="Сохранено: проверка · правок 2"
  if core_cmd "$profile" "$dir/set" panel command "$native" demo-native.set --args '{"value":"проверка"}' --all; then
    [[ "$REV" == $((rev1 + 1)) ]] || failures+=("demo-native.set по CLI дал ревизию $REV после $rev1")
    wait_caption "$profile" "$dir/cyr" "$native" "$cyr" || failures+=("значение по CLI: $WHY")
    wait_ax_field "$owner_pid" "$dir/cyr.ax.json" "$long" "$cyr" || failures+=("значение по CLI: $WHY")
    if core_cmd "$profile" "$dir/set-again" panel command "$native" demo-native.set --args '{"value":"проверка"}' --all; then
      [[ "$REV" == $((rev1 + 1)) ]] || failures+=("повтор того же значения дал ревизию $REV, до него $((rev1 + 1))")
    else
      failures+=("повтор demo-native.set: $WHY")
    fi
  else
    failures+=("demo-native.set по CLI: $WHY")
  fi
  # Черновик из 257 символов следующим пунктам не нужен: Escape возвращает поле к сохранённому.
  se_key "$KEY_ESC" "$dir/keys.log" || true
  wait_field "$profile" "$dir/reset" "$native" "проверка" || failures+=("Escape после отказа: $WHY")
  conclude "$item" "черновик пуст по AX; shellio-check и Return — data.value shellio-check, edits 1, одна ревизия $rev1; правка без Return и Escape — поле вернулось к сохранённому без ревизии; 257 символов и Return — подпись «${TOO_LONG}», черновик из 257 символов в поле (ui.inspect и AX), данные и ревизия прежние; demo-native.set по CLI с кириллицей — подпись «${cyr}», повтор — ревизия прежняя"
}

# Поле по очереди в правой, нижней зоне и левой плитке: вариант по зоне, поле и подпись по AX, правка проходит в
# каждой зоне, тело одно.
item_native_zones() {
  local item=native-zones dir="$run/native-zones" zone want edits caption component got seen=""
  failures=()
  log "== $item"
  [[ -n "$native" ]] || {
    record "$item" fail "панели демо-поля нет: open-menu-native её не создал"
    return 0
  }
  show_bench "$dir" "$wb_a" "$ws1"
  ui_read "$profile" "$dir/before.inspect" inspect || fatal "$item: ui.inspect не прочитан"
  component="$(pfact "$dir/before.inspect.stdout" "$native" component || echo нет)"
  panel_get "$profile" "$dir/panel-before" "$native" || fatal "$item: query.panel не прочитан"
  edits="$(pv "$dir/panel-before.stdout" data.edits || echo ?)"
  [[ "$edits" =~ ^[0-9]+$ ]] || fatal "условие пункта: у демо-поля нет счётчика правок: $(oneline "$dir/panel-before.stdout")"
  for zone in right bottom left; do
    want=stack
    [[ "$zone" != bottom ]] || want=row
    core_cmd "$profile" "$dir/move-$zone" panel move "$native" --zone "$zone" --all || {
      failures+=("перенос в зону $zone: $WHY")
      continue
    }
    settle_layout "$profile" "$dir/settle-$zone" "$wb_a" "$REV" || fatal "$item: $WHY"
    wait_inspect "$profile" "$dir/variant-$zone" "layout.panel.$native" state.panel.variant "$want" ||
      failures+=("зона $zone: $WHY")
    wait_inspect "$profile" "$dir/field-$zone" "layout.panel.$native.field" first_responder true ||
      failures+=("зона $zone: ввод не у поля после переноса: $WHY")
    read_all "$profile" "$dir" "$zone" "$wb_a" "$owner_pid" || fatal "условие пункта: $WHY"
    [[ "$(place_of "$dir/$zone.layout.stdout" "$native")" == "$zone".* ]] ||
      failures+=("после переноса панель в «$(place_of "$dir/$zone.layout.stdout" "$native")», а не в зоне $zone")
    [[ -z "$FRAMES" ]] || failures+=("кадры в зоне $zone: $(printf '%s' "$FRAMES" | tr '\n' ';')")
    front_ok "$owner_pid" "$dir" || fatal "условие пункта: $WHY"
    field_clear "$dir/keys.log" || failures+=("зона $zone: очистка поля не отправлена")
    wait_field "$profile" "$dir/cleared-$zone" "$native" "" || failures+=("зона $zone: очистка поля: $WHY")
    se_keystroke "z-$zone" "$dir/keys.log" || failures+=("зона $zone: keystroke не отправлен")
    wait_field "$profile" "$dir/typed-$zone" "$native" "z-$zone" || failures+=("зона $zone: ввод: $WHY")
    se_key "$KEY_RETURN" "$dir/keys.log" || failures+=("зона $zone: Return не отправлен")
    edits=$((edits + 1))
    caption="Сохранено: z-$zone · правок $edits"
    wait_caption "$profile" "$dir/saved-$zone" "$native" "$caption" || failures+=("зона $zone: правка: $WHY")
    wait_ax_field "$owner_pid" "$dir/$zone.ax.json" "z-$zone" "$caption" || failures+=("зона $zone: $WHY")
    if panel_get "$profile" "$dir/panel-$zone" "$native"; then
      [[ "$(pv "$dir/panel-$zone.stdout" data.value || echo ?)" == "z-$zone" ]] ||
        failures+=("зона $zone: data.value $(pv "$dir/panel-$zone.stdout" data.value || echo ?), ожидалось z-$zone")
    else
      failures+=("зона $zone: query.panel не прочитан: $(oneline "$dir/panel-$zone.stderr")")
    fi
    seen="${seen:+$seen, }$zone — $want"
  done
  ui_read "$profile" "$dir/after.inspect" inspect || fatal "$item: ui.inspect не прочитан"
  ui_read "$profile" "$dir/after.stats" stats || fatal "$item: ui.stats не прочитан"
  got="$(pfact "$dir/after.inspect.stdout" "$native" component || echo нет)"
  [[ "$component" != нет && "$got" == "$component" ]] ||
    failures+=("ComponentId контейнера $got, до переносов $component")
  [[ "$(built_of "$dir/after.stats.stdout" "$native")" == 1 ]] ||
    failures+=("bodies_built демо-поля $(built_of "$dir/after.stats.stdout" "$native"), ожидалось 1")
  # Следующие пункты ждут поле в центре.
  core_cmd "$profile" "$dir/move-center" panel move "$native" --zone center --all || fatal "$item: возврат в центр: $WHY"
  settle_layout "$profile" "$dir/settle-center" "$wb_a" "$REV" || fatal "$item: $WHY"
  conclude "$item" "демо-поле по очереди в зонах: $seen (вариант в ui.inspect), кадры совпали с ui.inspect и AX; после каждого переноса ввод у поля, правка и Return прошли, поле и подпись видны по AX; ComponentId контейнера $component прежний, bodies_built 1"
}

# panel open demo-native --all --workbench B при поле в A с черновиком: переезд с тем же PanelId, контейнером,
# полем, данными и черновиком.
item_perapp_move() {
  local item=perapp-move dir="$run/perapp-move" component field data got draft=move-draft
  failures=()
  log "== $item"
  [[ -n "$native" ]] || {
    record "$item" fail "панели демо-поля нет: open-menu-native её не создал"
    return 0
  }
  show_bench "$dir" "$wb_a" "$ws1"
  layout_get "$profile" "$dir/start" "$wb_a" || fatal "$item: query.layout не прочитан"
  [[ -n "$(place_of "$dir/start.stdout" "$native")" ]] || fatal "условие пункта: демо-поля $native нет в A"
  native_focus "$dir" "$wb_a" || fatal "условие пункта: $WHY"
  field_clear "$dir/keys.log" || fatal "условие пункта: очистка поля не отправлена"
  wait_field "$profile" "$dir/cleared" "$native" "" || fatal "условие пункта: $WHY"
  se_keystroke "$draft" "$dir/keys.log" || fatal "условие пункта: keystroke не отправлен"
  wait_field "$profile" "$dir/before.inspect" "$native" "$draft" || fatal "условие пункта: черновик: $WHY"
  component="$(pfact "$dir/before.inspect.stdout" "$native" component || echo нет)"
  field="$(comp "$dir/before.inspect.stdout" "layout.panel.$native.field" object || echo нет)"
  panel_get "$profile" "$dir/panel-before" "$native" || fatal "$item: query.panel не прочитан"
  data="$(data_canon "$dir/panel-before.stdout")"

  core_cmd "$profile" "$dir/open" panel open demo-native --all --workbench "$wb_b" || {
    record "$item" fail "$WHY"
    return 0
  }
  [[ "$(jv "$dir/open.stdout" ok.value.outcome || echo ?)" == moved ]] ||
    failures+=("исход $(jv "$dir/open.stdout" ok.value.outcome || echo ?), ожидался moved")
  [[ "$(jv "$dir/open.stdout" ok.value.panel || echo ?)" == "$native" ]] ||
    failures+=("ответ называет панель $(jv "$dir/open.stdout" ok.value.panel || echo ?), а не $native")
  [[ "$(jv "$dir/open.stdout" ok.value.workbench || echo ?)" == "$wb_b" ]] ||
    failures+=("ответ называет рабочую область $(jv "$dir/open.stdout" ok.value.workbench || echo ?), а не B $wb_b")
  layout_get "$profile" "$dir/moved-a" "$wb_a" || fatal "$item: query.layout не прочитан"
  layout_get "$profile" "$dir/moved-b" "$wb_b" || fatal "$item: query.layout не прочитан"
  [[ -z "$(panels_of "$dir/moved-a.stdout" demo-native)" ]] ||
    failures+=("в раскладке A осталась панель $(panels_of "$dir/moved-a.stdout" demo-native)")
  [[ "$(panels_of "$dir/moved-b.stdout" demo-native)" == "$native" ]] ||
    failures+=("в раскладке B демо-поле «$(panels_of "$dir/moved-b.stdout" demo-native)», ожидалась $native")

  show_bench "$dir" "$wb_b" "$ws2"
  wait_inspect "$profile" "$dir/after.inspect" "layout.panel.$native" visible true ||
    failures+=("демо-поле в окне B: $WHY")
  ui_read "$profile" "$dir/after.stats" stats || fatal "$item: ui.stats не прочитан"
  got="$(pfact "$dir/after.inspect.stdout" "$native" component || echo нет)"
  [[ "$component" != нет && "$got" == "$component" ]] ||
    failures+=("ComponentId контейнера $got, до переезда $component")
  got="$(comp "$dir/after.inspect.stdout" "layout.panel.$native.field" object || echo нет)"
  [[ "$field" != нет && "$got" == "$field" ]] || failures+=("объект вложенного поля $got, до переезда $field")
  got="$(comp "$dir/after.inspect.stdout" "layout.panel.$native.field" state.text_field.value || echo нет)"
  [[ "$got" == "$draft" ]] || failures+=("черновик в поле «${got:0:40}», до переезда «${draft}»")
  [[ "$(built_of "$dir/after.stats.stdout" "$native")" == 1 ]] ||
    failures+=("bodies_built демо-поля $(built_of "$dir/after.stats.stdout" "$native"), ожидалось 1")
  if panel_get "$profile" "$dir/panel-after" "$native"; then
    [[ "$(data_canon "$dir/panel-after.stdout")" == "$data" ]] ||
      failures+=("данные после переезда $(data_canon "$dir/panel-after.stdout"), до него $data")
    [[ "$(pv "$dir/panel-after.stdout" panel.workbench || echo ?)" == "$wb_b" ]] ||
      failures+=("query.panel: рабочая область $(pv "$dir/panel-after.stdout" panel.workbench || echo ?), а не B")
  else
    failures+=("query.panel после переезда не прочитан: $(oneline "$dir/panel-after.stderr")")
  fi
  conclude "$item" "panel open demo-native --all --workbench B при поле в A с черновиком «${draft}»: moved, тот же PanelId $native, данные $data прежние, в раскладке A панели нет, в B есть; после workbench open B ComponentId контейнера $component и объект вложенного поля $field прежние, bodies_built 1, черновик в поле сохранён"
}

# Внешний вызов panel.open плагина PerApp с областью воркспейса: отказ scope_requires_all без изменений.
item_perapp_scope() {
  local item=perapp-scope dir="$run/perapp-scope" rev code reason
  failures=()
  log "== $item"
  revision_of "$dir/before" "$wb_b"
  rev="$NOW_REV"
  code=0
  ctl "$profile" "$dir/open" --json --timeout "$CTL_TIMEOUT_MS" panel open demo-native --workspace "$ws1" \
    --workbench "$wb_a" || code=$?
  reason="$(jv "$dir/open.stdout" core_error.details.reason 2>/dev/null || echo нет)"
  [[ $code -ne 0 && "$reason" == scope_requires_all ]] ||
    failures+=("panel open demo-native --workspace W1: код $code, причина $reason, ожидался отказ scope_requires_all: $(oneline "$dir/open.stdout" | cut -c 1-200)")
  revision_of "$dir/after" "$wb_b"
  [[ "$NOW_REV" == "$rev" ]] ||
    failures+=("ревизия после отказа $(jv "$dir/after.stdout" ok.revision || echo ?), до него $rev")
  if events_after "$profile" "$dir/events" "$rev"; then
    [[ "$EVENTS" == "0|0|" ]] || failures+=("события после курсора $rev: «${EVENTS}», ожидалось ни одного")
  else
    failures+=("$WHY")
  fi
  conclude "$item" "panel open demo-native --workspace W1 --workbench A: отказ scope_requires_all, ревизия $rev прежняя, событий после курсора нет"
}

# Меню «Демо: поле» в B при фокусе на демо-списке правой зоны и терминале в центре: поле переезжает из A, его
# закрытие возвращает фокус демо-списку, а не терминалу центра.
item_perapp_return() {
  local item=perapp-return dir="$run/perapp-return" demo_b term_b last0 rev focus ax
  failures=()
  log "== $item"
  [[ -n "$native" ]] || {
    record "$item" fail "панели демо-поля нет: open-menu-native её не создал"
    return 0
  }
  core_cmd "$profile" "$dir/home" panel open demo-native --all --workbench "$wb_a" || fatal "условие пункта: $WHY"
  layout_get "$profile" "$dir/home-a" "$wb_a" || fatal "$item: query.layout не прочитан"
  [[ "$(panels_of "$dir/home-a.stdout" demo-native)" == "$native" ]] ||
    fatal "условие пункта: демо-поле не в A: «$(panels_of "$dir/home-a.stdout" demo-native)»"
  show_bench "$dir" "$wb_b" "$ws2"
  layout_get "$profile" "$dir/start" "$wb_b" || fatal "$item: query.layout не прочитан"
  demo_b="$(panels_of "$dir/start.stdout" demo-list)"
  [[ -n "$demo_b" && "$demo_b" != *" "* ]] ||
    fatal "условие пункта: в B не один демо-список open-race: «${demo_b}»"
  if [[ "$(place_of "$dir/start.stdout" "$demo_b")" != right.* ]]; then
    core_cmd "$profile" "$dir/demo-right" panel move "$demo_b" --zone right --all || fatal "условие пункта: $WHY"
  fi
  core_cmd "$profile" "$dir/focus" panel focus "$demo_b" --all || fatal "условие пункта: $WHY"
  settle_layout "$profile" "$dir/settle" "$wb_b" "$REV" || fatal "$item: $WHY"
  layout_get "$profile" "$dir/before" "$wb_b" || fatal "$item: query.layout не прочитан"
  term_b="$(tabs_of "$dir/before.stdout" center 0)"
  term_b="${term_b##* }"
  [[ "$term_b" != "?" && " $(panels_of "$dir/before.stdout" terminal) " == *" $term_b "* ]] ||
    fatal "условие пункта: центр B не занят терминалом: вкладки «$(tabs_of "$dir/before.stdout" center 0)»"
  [[ "$(focus_of "$dir/before.stdout")" == "$demo_b" && "$(place_of "$dir/before.stdout" "$demo_b")" == right.* ]] ||
    fatal "условие пункта: фокус на $(focus_of "$dir/before.stdout"), демо-список $demo_b в «$(place_of "$dir/before.stdout" "$demo_b")»"
  front_ok "$owner_pid" "$dir" || fatal "условие пункта: $WHY"
  receipts_last "$profile" "$dir/receipts-before" || fatal "$item: $WHY"
  last0="$LAST"
  press_open_item "$owner_pid" "$NATIVE_TITLE" "$dir/press" || {
    record "$item" fail "$WHY"
    return 0
  }
  wait_receipts "$profile" "$dir/receipts" "$last0" 1 || {
    record "$item" fail "меню закрылось без квитанции: $WHY"
    return 0
  }
  rev="$(rc "$dir/receipts.stdout" 0 revision)"
  [[ "$(rc "$dir/receipts.stdout" 0 outcome)" == moved ]] ||
    failures+=("исход квитанции $(rc "$dir/receipts.stdout" 0 outcome) (отказ $(rc "$dir/receipts.stdout" 0 error)), ожидался moved")
  [[ "$(rc "$dir/receipts.stdout" 0 panel)" == "$native" ]] ||
    failures+=("квитанция называет панель $(rc "$dir/receipts.stdout" 0 panel), а не $native")
  [[ "$(rc "$dir/receipts.stdout" 0 workbench)" == "$wb_b" ]] ||
    failures+=("рабочая область квитанции $(rc "$dir/receipts.stdout" 0 workbench), а не B $wb_b")
  [[ "$rev" =~ ^[0-9]+$ ]] || {
    record "$item" fail "в квитанции нет ревизии: $(oneline "$dir/receipts.stdout")"
    return 0
  }
  settle_layout "$profile" "$dir/moved-settle" "$wb_b" "$rev" || fatal "$item: $WHY"
  read_all "$profile" "$dir" moved "$wb_b" "$owner_pid" || fatal "условие пункта: $WHY"
  [[ "$(place_of "$dir/moved.layout.stdout" "$native")" == center.* ]] ||
    failures+=("поле в «$(place_of "$dir/moved.layout.stdout" "$native")», а не в центре B")
  [[ "$(focus_of "$dir/moved.layout.stdout")" == "$native" ]] ||
    failures+=("после переезда фокус на $(focus_of "$dir/moved.layout.stdout"), а не на поле $native")
  [[ -z "$FRAMES" ]] || failures+=("кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  if panel_get "$profile" "$dir/panel" "$native"; then
    [[ "$(pv "$dir/panel.stdout" opened_from || echo ?)" == "$demo_b" ]] ||
      failures+=("opened_from $(pv "$dir/panel.stdout" opened_from || echo ?), а не демо-список $demo_b")
  else
    failures+=("query.panel не прочитан: $(oneline "$dir/panel.stderr")")
  fi

  core_cmd "$profile" "$dir/close" panel close "$native" --all || {
    record "$item" fail "$WHY"
    return 0
  }
  settle_layout "$profile" "$dir/closed-settle" "$wb_b" "$REV" || fatal "$item: $WHY"
  layout_get "$profile" "$dir/closed" "$wb_b" || fatal "$item: query.layout не прочитан"
  [[ -z "$(panels_of "$dir/closed.stdout" demo-native)" ]] ||
    failures+=("после закрытия в B осталось демо-поле $(panels_of "$dir/closed.stdout" demo-native)")
  focus="$(focus_of "$dir/closed.stdout")"
  [[ "$focus" == "$demo_b" ]] ||
    failures+=("после закрытия фокус на ${focus:-нет} (терминал центра — $term_b), а не на демо-списке $demo_b")
  wait_inspect "$profile" "$dir/closed.inspect" "layout.panel.$demo_b.list" first_responder true ||
    failures+=("ввод не у списка демо: $WHY")
  osascript -l JavaScript -e "$AX_TEXTS_JS" "$owner_pid" >"$dir/closed.ax.json" 2>"$dir/closed.ax.stderr" ||
    fatal "AX окна pid $owner_pid не прочитан: $(oneline "$dir/closed.ax.stderr")"
  ax="$(/usr/bin/perl -COEA -Mutf8 -MJSON::PP -e 'open my $fh, "<", $ARGV[0] or exit 2; local $/;
    my $a = eval { JSON::PP->new->utf8->decode(<$fh>) } or exit 2;
    my ($f) = grep { ($_->{role} // "") eq "focus" } @$a; print $f->{description} // "нет", "\n"' \
    "$dir/closed.ax.json" 2>/dev/null || echo нет)"
  [[ "$ax" == "AXTable|Демо: список" ]] ||
    failures+=("AX: ввод у «${ax}», а не у таблицы «Демо: список»")
  native=""
  conclude "$item" "меню «${NATIVE_TITLE}» в B при фокусе на демо-списке $demo_b правой зоны: квитанция moved, поле в центре B рядом с терминалом $term_b, opened_from — демо-список; panel close поля вернул фокус демо-списку по query.layout, ui.inspect (first_responder списка) и AX (AXFocusedUIElement — таблица «Демо: список»), а не терминалу центра"
}

# Клавиши демо-списка в варианте drill: Down, Down выбирают строку, Return открывает детали и отдаёт им ввод, первый
# Esc закрывает детали и возвращает ввод списку, второй — demo-list.back. Команд CLI между клавишами нет.
item_drill_keys() {
  local item=drill-keys dir="$run/drill-keys" list details got
  failures=()
  log "== $item"
  [[ -n "$demo" ]] || {
    record "$item" fail "панели демо-списка нет: open-cli её не создал"
    return 0
  }
  list="layout.panel.$demo.list"
  details="layout.panel.$demo.details"
  show_bench "$dir" "$wb_a" "$ws1"
  demo_reset "$dir" a || fatal "условие пункта: $WHY"
  core_cmd "$profile" "$dir/open" panel open demo-list --args '{"key":"c"}' --workspace "$ws1" ||
    fatal "условие пункта: $WHY"
  core_cmd "$profile" "$dir/focus" panel focus "$demo" --workspace "$ws1" || fatal "условие пункта: $WHY"
  settle_layout "$profile" "$dir/settle" "$wb_a" "$REV" || fatal "$item: $WHY"
  wait_inspect "$profile" "$dir/start.inspect" "$list" state.virtual_list.selected c || fatal "условие пункта: $WHY"
  wait_inspect "$profile" "$dir/start.inspect" "$list" first_responder true ||
    fatal "условие пункта: ввод не у списка демо: $WHY"
  got="$(pfact "$dir/start.inspect.stdout" "$demo" variant || echo нет)"
  [[ "$got" == drill ]] || fatal "условие пункта: вариант демо-списка $got, пункт судит drill"
  panel_get "$profile" "$dir/start.panel" "$demo" || fatal "$item: query.panel не прочитан"
  [[ "$(pv "$dir/start.panel.stdout" data.history || echo ?)" == '["a"]' &&
    "$(pv "$dir/start.panel.stdout" data.details || echo ?)" == false ]] ||
    fatal "условие пункта: история a → c не собрана: $(data_canon "$dir/start.panel.stdout")"
  front_ok "$owner_pid" "$dir" || fatal "условие пункта: $WHY"

  se_key "$KEY_DOWN" "$dir/keys.log" || failures+=("первый Down не отправлен")
  wait_panel "$profile" "$dir/down1.panel" "$demo" data.key d || failures+=("после первого Down: $WHY")
  wait_inspect "$profile" "$dir/down1.inspect" "$list" state.virtual_list.selected d ||
    failures+=("после первого Down в окне: $WHY")
  se_key "$KEY_DOWN" "$dir/keys.log" || failures+=("второй Down не отправлен")
  wait_panel "$profile" "$dir/down2.panel" "$demo" data.key e || failures+=("после второго Down: $WHY")
  wait_inspect "$profile" "$dir/down2.inspect" "$list" state.virtual_list.selected e ||
    failures+=("после второго Down в окне: $WHY")

  se_key "$KEY_RETURN" "$dir/keys.log" || failures+=("Return не отправлен")
  wait_panel "$profile" "$dir/return.panel" "$demo" data.details true || failures+=("после Return: $WHY")
  wait_inspect "$profile" "$dir/return.inspect" "$details" first_responder true ||
    failures+=("после Return ввод не у списка деталей: $WHY")
  [[ "$(comp "$dir/return.inspect.stdout" "$details" visible || echo нет)" == true ]] ||
    failures+=("после Return список деталей не видим")
  [[ "$(comp "$dir/return.inspect.stdout" "$details" state.virtual_list.label || echo нет)" == "Детали: Эпсилон" ]] ||
    failures+=("после Return детали — «$(comp "$dir/return.inspect.stdout" "$details" state.virtual_list.label || echo нет)», а не строки e")
  pane_hidden "$dir/return.inspect.stdout" "$list" ||
    failures+=("после Return список строк остался видим: детали не вглубь")
  got="$(pfact "$dir/return.inspect.stdout" "$demo" variant || echo нет)"
  [[ "$got" == drill ]] || failures+=("после Return вариант $got, а не drill")
  [[ "$(pv "$dir/return.panel.stdout" data.key || echo ?)" == e ]] ||
    failures+=("после Return data.key $(pv "$dir/return.panel.stdout" data.key || echo ?), ожидался e")

  se_key "$KEY_ESC" "$dir/keys.log" || failures+=("первый Esc не отправлен")
  wait_panel "$profile" "$dir/esc1.panel" "$demo" data.details false || failures+=("после первого Esc: $WHY")
  wait_inspect "$profile" "$dir/esc1.inspect" "$list" first_responder true ||
    failures+=("после первого Esc ввод не у списка: $WHY")
  [[ "$(comp "$dir/esc1.inspect.stdout" "$list" state.virtual_list.selected || echo нет)" == e ]] ||
    failures+=("после первого Esc в окне выбрана $(comp "$dir/esc1.inspect.stdout" "$list" state.virtual_list.selected || echo нет), а не e")
  pane_hidden "$dir/esc1.inspect.stdout" "$details" ||
    failures+=("после первого Esc список деталей остался видим")
  [[ "$(pv "$dir/esc1.panel.stdout" data.key || echo ?)" == e &&
    "$(pv "$dir/esc1.panel.stdout" data.history || echo ?)" == '["a"]' ]] ||
    failures+=("первый Esc тронул выбор или историю: $(data_canon "$dir/esc1.panel.stdout")")

  se_key "$KEY_ESC" "$dir/keys.log" || failures+=("второй Esc не отправлен")
  wait_panel "$profile" "$dir/esc2.panel" "$demo" data.key a || failures+=("после второго Esc: $WHY")
  wait_inspect "$profile" "$dir/esc2.inspect" "$list" state.virtual_list.selected a ||
    failures+=("после второго Esc в окне: $WHY")
  [[ "$(pv "$dir/esc2.panel.stdout" data.history || echo ?)" == "[]" ]] ||
    failures+=("после второго Esc история $(pv "$dir/esc2.panel.stdout" data.history || echo ?), ожидалась пустая")
  # Следующие пункты ждут список с выбранной a, пустой историей и закрытыми деталями.
  mkdir -p "$dir/after"
  demo_reset "$dir/after" a || fatal "$item: возврат: $WHY"
  conclude "$item" "история a → c через panel open --args, ввод у списка варианта drill; Down, Down — data.key и выбор в окне d, затем e; Return — data.details true, детали «Эпсилон» вглубь, first_responder у списка деталей; первый Esc — детали закрыты, список с выбранной e и first_responder у него, история прежняя; второй Esc — demo-list.back: выбрана a, история пуста; команд CLI между клавишами нет"
}

# Демо-список по очереди в центре, нижней зоне и левой плитке: вариант по таблице плагина для класса размера из
# query.layout, регионы представления в окне, то же тело.
item_zones_demo() {
  local item=zones-demo dir="$run/zones-demo" zone part size want component got seen="" regions
  failures=()
  log "== $item"
  [[ -n "$demo" ]] || {
    record "$item" fail "панели демо-списка нет: open-cli её не создал"
    return 0
  }
  show_bench "$dir" "$wb_a" "$ws1"
  demo_reset "$dir" a || fatal "условие пункта: $WHY"
  ui_read "$profile" "$dir/before.inspect" inspect || fatal "$item: ui.inspect не прочитан"
  component="$(pfact "$dir/before.inspect.stdout" "$demo" component || echo нет)"
  for zone in center bottom left; do
    core_cmd "$profile" "$dir/move-$zone" panel move "$demo" --zone "$zone" --all || {
      failures+=("перенос в зону $zone: $WHY")
      continue
    }
    settle_layout "$profile" "$dir/settle-$zone" "$wb_a" "$REV" || fatal "$item: $WHY"
    layout_get "$profile" "$dir/geometry-$zone" "$wb_a" || fatal "$item: query.layout не прочитан"
    part="$(part_with "$dir/geometry-$zone.stdout" "$demo")"
    [[ "$part" == "$zone "* ]] || {
      failures+=("после переноса демо-список не активная вкладка части зоны $zone: «${part}»")
      continue
    }
    [[ "$part" == *" false" ]] || fatal "условие пункта: в зоне $zone тело демо-списка меньше min_size: $part"
    size="$(printf '%s' "$part" | awk '{ print $3 }')"
    want="$(demo_variant "$zone" "$size")"
    [[ "$want" != "?" ]] || fatal "$item: неизвестный класс размера $size зоны $zone"
    wait_inspect "$profile" "$dir/variant-$zone" "layout.panel.$demo" state.panel.variant "$want" ||
      failures+=("зона $zone, класс $size: $WHY")
    read_all "$profile" "$dir" "$zone" "$wb_a" "$owner_pid" || fatal "условие пункта: $WHY"
    [[ -z "$FRAMES" ]] || failures+=("кадры в зоне $zone: $(printf '%s' "$FRAMES" | tr '\n' ';')")
    got="$(pfact "$dir/$zone.inspect.stdout" "$demo" size || echo нет)"
    [[ "$got" == "$size" ]] || failures+=("зона $zone: класс размера в ui.inspect $got, в query.layout $size")
    regions="$(demo_regions "$dir/$zone.inspect.stdout" "$demo" "$want" 2>&1)" ||
      fatal "$item: регионы зоны $zone не сверены: $regions"
    [[ -z "$regions" ]] || failures+=("регионы варианта $want в зоне $zone: $(printf '%s' "$regions" | tr '\n' ';')")
    [[ "$(list_fact "$dir/$zone.inspect.stdout" "$demo" list selected || echo нет)" == a ]] ||
      failures+=("зона $zone: в окне выбрана $(list_fact "$dir/$zone.inspect.stdout" "$demo" list selected || echo нет), а не a")
    seen="${seen:+$seen, }$zone ($size) — $want"
  done
  ui_read "$profile" "$dir/after.inspect" inspect || fatal "$item: ui.inspect не прочитан"
  ui_read "$profile" "$dir/after.stats" stats || fatal "$item: ui.stats не прочитан"
  got="$(pfact "$dir/after.inspect.stdout" "$demo" component || echo нет)"
  [[ "$component" != нет && "$got" == "$component" ]] ||
    failures+=("ComponentId контейнера $got, до переносов $component")
  [[ "$(built_of "$dir/after.stats.stdout" "$demo")" == 1 ]] ||
    failures+=("bodies_built демо-списка $(built_of "$dir/after.stats.stdout" "$demo"), ожидалось 1")
  # Следующие пункты ждут демо-список в правой зоне.
  core_cmd "$profile" "$dir/move-right" panel move "$demo" --zone right --all || fatal "$item: возврат в правую зону: $WHY"
  settle_layout "$profile" "$dir/settle-right" "$wb_a" "$REV" || fatal "$item: $WHY"
  conclude "$item" "демо-список по очереди в зонах: $seen — вариант по таблице плагина для класса размера query.layout в ui.inspect, список и детали в окне стоят по регионам варианта, кадры совпали с ui.inspect и AX; ComponentId контейнера $component прежний, bodies_built 1"
}

# Геометрия пункта stub-expand P1.4: в правой зоне свой терминал, правая зона скрыта, центр — три равные части,
# демо-список переносится в одну из них, правая зона показывается. До показа тело части демо не меньше min_size,
# после — меньше: заглушка «Развернуть» при живом теле, её кнопка разворачивает панель с прежней выбранной строкой.
item_stub_min_size() {
  local item=stub-min-size dir="$run/stub-min-size" side added="" n source part zone size w h stub index component got
  local key rev
  failures=()
  log "== $item"
  [[ -n "$demo" ]] || {
    record "$item" fail "панели демо-списка нет: open-cli её не создал"
    return 0
  }
  show_bench "$dir" "$wb_a" "$ws1"
  demo_reset "$dir" c || fatal "условие пункта: $WHY"
  layout_get "$profile" "$dir/start" "$wb_a" || fatal "$item: query.layout не прочитан"
  [[ "$(place_of "$dir/start.stdout" "$demo")" == right.* ]] ||
    fatal "условие пункта: демо-список не в правой зоне: «$(place_of "$dir/start.stdout" "$demo")»"
  core_cmd "$profile" "$dir/side" panel open terminal --zone right --workspace "$ws1" || fatal "условие пункта: $WHY"
  side="$(jv "$dir/side.stdout" ok.value.panel)" || fatal "условие пункта: panel open terminal не назвал панель"
  core_cmd "$profile" "$dir/hide" layout hide right --workbench "$wb_a" --workspace "$ws1" ||
    fatal "условие пункта: $WHY"
  layout_get "$profile" "$dir/center" "$wb_a" || fatal "$item: query.layout не прочитан"
  n="$(parts_count "$dir/center.stdout" center)"
  [[ "$n" =~ ^[1-3]$ ]] || fatal "условие пункта: в центре A $n частей, пункт делит до трёх"
  while ((n < 3)); do
    layout_get "$profile" "$dir/split-from-$n" "$wb_a" || fatal "$item: query.layout не прочитан"
    source="$(active_of "$dir/split-from-$n.stdout" center $((n - 1)))"
    core_cmd "$profile" "$dir/split-$n" layout split "$source" --open terminal --workspace "$ws1" ||
      fatal "условие пункта: $WHY"
    got="$(jv "$dir/split-$n.stdout" ok.value.panel)" || fatal "условие пункта: деление не назвало новую панель"
    added="${added:+$added }$got"
    n=$((n + 1))
  done
  core_cmd "$profile" "$dir/equal" layout equal --zone center --workbench "$wb_a" --workspace "$ws1" ||
    fatal "условие пункта: $WHY"
  core_cmd "$profile" "$dir/move" panel move "$demo" --zone center --all || fatal "условие пункта: $WHY"
  settle_layout "$profile" "$dir/settle-wide" "$wb_a" "$REV" || fatal "$item: $WHY"
  wait_inspect "$profile" "$dir/wide-list" "layout.panel.$demo.list" state.virtual_list.selected c ||
    fatal "условие пункта: $WHY"
  read_all "$profile" "$dir" wide "$wb_a" "$owner_pid" || fatal "условие пункта: $WHY"
  part="$(part_with "$dir/wide.layout.stdout" "$demo")"
  read -r zone index size w h stub <<<"$part"
  [[ "$part" == "center "* && "$(parts_count "$dir/wide.layout.stdout" center)" == 3 ]] ||
    fatal "условие пункта: демо-список не активная вкладка одной из трёх частей центра: «${part}»"
  awk -v w="$w" -v h="$h" -v W="$DEMO_MIN_W" -v H="$DEMO_MIN_H" 'BEGIN { exit !(w >= W && h >= H) }' ||
    fatal "условие пункта: при скрытой правой зоне тело части демо ${w}×${h} меньше ${DEMO_MIN_W}×${DEMO_MIN_H}"
  [[ "$stub" == false ]] || failures+=("query.layout: тело ${w}×${h} не меньше min_size, а stub true")
  [[ -z "$FRAMES" ]] || failures+=("кадры трёх частей: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  component="$(pfact "$dir/wide.inspect.stdout" "$demo" component || echo нет)"
  key="$(list_fact "$dir/wide.inspect.stdout" "$demo" list selected || echo нет)"

  core_cmd "$profile" "$dir/show" layout show right --workbench "$wb_a" --workspace "$ws1" || {
    record "$item" fail "$WHY"
    return 0
  }
  rev="$REV"
  settle_layout "$profile" "$dir/settle-stubbed" "$wb_a" "$rev" || fatal "$item: $WHY"
  read_all "$profile" "$dir" stubbed "$wb_a" "$owner_pid" || fatal "условие пункта: $WHY"
  part="$(part_with "$dir/stubbed.layout.stdout" "$demo")"
  [[ "$part" == "center $index "* ]] || fatal "условие пункта: после показа правой зоны демо-список не в части center.$index: «${part}»"
  read -r zone index size w h stub <<<"$part"
  awk -v w="$w" -v h="$h" -v W="$DEMO_MIN_W" -v H="$DEMO_MIN_H" 'BEGIN { exit !(w < W || h < H) }' ||
    fatal "условие пункта: при показанной правой зоне тело части демо ${w}×${h} не меньше ${DEMO_MIN_W}×${DEMO_MIN_H}"
  [[ "$stub" == true ]] || failures+=("query.layout: тело ${w}×${h} меньше min_size ${DEMO_MIN_W}×${DEMO_MIN_H}, а stub false")
  got="layout.part.center.$index.stub"
  [[ "$(comp "$dir/stubbed.inspect.stdout" "$got" visible || echo нет)" == true ]] ||
    failures+=("ui.inspect: заглушка части center.$index не видима")
  [[ "$(comp "$dir/stubbed.inspect.stdout" "$got" class || echo нет)" == ExpandStub ]] ||
    failures+=("ui.inspect: узел заглушки — $(comp "$dir/stubbed.inspect.stdout" "$got" class || echo нет), а не ExpandStub")
  [[ "$(comp "$dir/stubbed.inspect.stdout" "$got" label || echo нет)" == "$DEMO_TIGHT" ]] ||
    failures+=("ui.inspect: подпись заглушки — $(comp "$dir/stubbed.inspect.stdout" "$got" label || echo нет)")
  [[ "$(comp "$dir/stubbed.inspect.stdout" "$got.expand" visible || echo нет)" == true ]] ||
    failures+=("ui.inspect: кнопка «Развернуть» заглушки не видима")
  [[ "$(pfact "$dir/stubbed.inspect.stdout" "$demo" shown || echo нет)" == false ]] ||
    failures+=("ui.inspect: контейнер демо-списка под заглушкой не скрыт")
  [[ "$(pfact "$dir/stubbed.inspect.stdout" "$demo" status || echo нет)" == ready ]] ||
    failures+=("ui.inspect: статус под заглушкой $(pfact "$dir/stubbed.inspect.stdout" "$demo" status || echo нет)")
  [[ "$(pfact "$dir/stubbed.inspect.stdout" "$demo" component || echo нет)" == "$component" ]] ||
    failures+=("ComponentId контейнера $(pfact "$dir/stubbed.inspect.stdout" "$demo" component || echo нет), до заглушки $component")
  ui_read "$profile" "$dir/stubbed.stats" stats || fatal "$item: ui.stats не прочитан"
  [[ "$(built_of "$dir/stubbed.stats.stdout" "$demo")" == 1 ]] ||
    failures+=("под заглушкой bodies_built демо-списка $(built_of "$dir/stubbed.stats.stdout" "$demo"), ожидалось 1")
  [[ -z "$FRAMES" ]] || failures+=("кадры заглушек: $(printf '%s' "$FRAMES" | tr '\n' ';')")

  front_ok "$owner_pid" "$dir" || fatal "условие пункта: $WHY"
  if stub_press "$owner_pid" "$dir/stubbed.layout.stdout" center "$index" "$dir/press" &&
    wait_layout "$profile" "$dir/wait-zoom" "$wb_a" layout.zoomed "$demo"; then
    got="$(jv "$dir/wait-zoom.stdout" ok.revision || echo ?)"
    [[ "$got" == $((rev + 1)) ]] || failures+=("кнопка заглушки: ревизия $rev → $got, ожидалась одна команда")
    settle_layout "$profile" "$dir/settle-zoom" "$wb_a" "$got" || fatal "$item: $WHY"
    wait_inspect "$profile" "$dir/zoom-list" "layout.panel.$demo.list" visible true ||
      failures+=("после «Развернуть» список демо: $WHY")
    read_all "$profile" "$dir" zoomed "$wb_a" "$owner_pid" || fatal "условие пункта: $WHY"
    [[ "$(lv "$dir/zoomed.layout.stdout" frames.zoomed.stub || echo ?)" == false ]] ||
      failures+=("развёрнутая панель демо тоже под заглушкой")
    [[ "$(pfact "$dir/zoomed.inspect.stdout" "$demo" shown || echo нет)" == true ]] ||
      failures+=("ui.inspect: контейнер развёрнутого демо-списка не показан")
    [[ "$(list_fact "$dir/zoomed.inspect.stdout" "$demo" list selected || echo нет)" == "$key" ]] ||
      failures+=("после «Развернуть» выбрана $(list_fact "$dir/zoomed.inspect.stdout" "$demo" list selected || echo нет), до заглушки $key")
    [[ "$(pfact "$dir/zoomed.inspect.stdout" "$demo" component || echo нет)" == "$component" ]] ||
      failures+=("после «Развернуть» ComponentId контейнера $(pfact "$dir/zoomed.inspect.stdout" "$demo" component || echo нет), до заглушки $component")
    ui_read "$profile" "$dir/zoomed.stats" stats || fatal "$item: ui.stats не прочитан"
    [[ "$(built_of "$dir/zoomed.stats.stdout" "$demo")" == 1 ]] ||
      failures+=("после «Развернуть» bodies_built демо-списка $(built_of "$dir/zoomed.stats.stdout" "$demo"), ожидалось 1")
    [[ -z "$FRAMES" ]] || failures+=("кадры развёрнутой панели: $(printf '%s' "$FRAMES" | tr '\n' ';')")
    core_cmd "$profile" "$dir/unzoom" layout zoom --off --workbench "$wb_a" --workspace "$ws1" ||
      fatal "$item: возврат: $WHY"
  else
    failures+=("кнопка «Развернуть» заглушки части center.$index: $WHY")
  fi
  # Следующие пункты ждут демо-список в правой зоне и центр без панелей пункта.
  core_cmd "$profile" "$dir/back" panel move "$demo" --zone right --all || fatal "$item: возврат: $WHY"
  for got in $added $side; do
    core_cmd "$profile" "$dir/close-$got" panel close "$got" --workspace "$ws1" || fatal "$item: возврат: $WHY"
  done
  settle_layout "$profile" "$dir/settle-back" "$wb_a" "$REV" || fatal "$item: $WHY"
  conclude "$item" "в правой зоне свой терминал, правая зона скрыта, центр — три равные части, демо-список в части center.$index: тело не меньше ${DEMO_MIN_W}×${DEMO_MIN_H} без заглушки; layout show right: тело ${w}×${h} меньше min_size, stub в query.layout, заглушка ExpandStub «${DEMO_TIGHT}» с кнопкой «Развернуть» по ui.inspect и AX, контейнер скрыт, ComponentId $component прежний, bodies_built 1; кнопка заглушки развернула панель одной ревизией, выбрана прежняя $key"
}

# Демо-список в правой зоне с фокусом и выбранной c: снимки светлой и тёмной темы оракулом без маркера.
item_demo_snapshot() {
  local item=demo-snapshot dir="$run/demo-snapshot" place cell tab got
  failures=()
  log "== $item"
  [[ -n "$demo" ]] || {
    record "$item" fail "панели демо-списка нет: open-cli её не создал"
    return 0
  }
  show_bench "$dir" "$wb_a" "$ws1"
  demo_reset "$dir" c || fatal "условие пункта: $WHY"
  core_cmd "$profile" "$dir/focus" panel focus "$demo" --workspace "$ws1" || fatal "условие пункта: $WHY"
  settle_layout "$profile" "$dir/settle" "$wb_a" "$REV" || fatal "$item: $WHY"
  wait_inspect "$profile" "$dir/wait" "layout.panel.$demo.list" state.virtual_list.selected c ||
    fatal "условие пункта: $WHY"
  read_all "$profile" "$dir" start "$wb_a" "$owner_pid" || fatal "условие пункта: $WHY"
  place="$(place_of "$dir/start.layout.stdout" "$demo")"
  got="$(pfact "$dir/start.inspect.stdout" "$demo" variant || echo нет)"
  [[ "$place" == right.* && "$(focus_of "$dir/start.layout.stdout")" == "$demo" && "$got" == drill ]] ||
    fatal "условие пункта: демо-список в «${place}», вариант $got, фокус на $(focus_of "$dir/start.layout.stdout"): пункт снимает вариант drill правой зоны с фокусом"
  [[ -z "$FRAMES" ]] || failures+=("кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  tab="layout.part.$place.tabs.tab.$demo"
  for cell in light dark; do
    ui_cmd "$profile" "$dir/$cell" ui appearance "$cell" || {
      failures+=("$cell: $WHY")
      continue
    }
    expectations "$cell" "$demo" "$dir/$cell.expect.json"
    snapshot "$profile" "$dir" "$cell" "$dir/$cell.expect.json"
    [[ "$snap_code" == 0 ]] || failures+=("$cell: $snap_reason")
    snapshot "$profile" "$dir" "$cell-underline" "underline:$tab:tone_navigation"
    [[ "$snap_code" == 0 ]] || failures+=("$cell, линия под активной вкладкой: $snap_reason")
  done
  ui_cmd "$profile" "$dir/system" ui appearance system || failures+=("возврат оформления: $WHY")
  conclude "$item" "демо-список в правой зоне с фокусом, выбрана c: оракул без маркера в светлой и тёмной теме — фон заголовка, заливка плитки под тоном фокуса и тон выбранной строки (plugins-expect/*.json), линия под активной вкладкой в допуске толщины и цвета; кадры совпали с ui.inspect и AX"
}

# Список с ключом c и историей, поле со значением и вызывающей панелью → app quit → новый запуск на том же профиле:
# PanelId, данные, opened_from, байты строк panel_states и раскладки прежние. Основной владелец остаётся
# остановленным.
item_restore_cold() {
  local item=restore-cold dir="$run/restore-cold" cold caption list0 field0 from0 layout0 rows0 rows1 got
  local rows_re='^[0-9]+\|[0-9A-F]+ [0-9]+\|[0-9A-F]+ [0-9A-F]+$'
  failures=()
  log "== $item"
  [[ -n "$demo" ]] || {
    record "$item" fail "панели демо-списка нет: open-cli её не создал"
    return 0
  }
  caption="Сохранено: $COLD_VALUE · правок 1"
  show_bench "$dir" "$wb_a" "$ws1"
  demo_reset "$dir" a || fatal "условие пункта: $WHY"
  if [[ -n "$native" ]]; then
    core_cmd "$profile" "$dir/close-native" panel close "$native" --all || fatal "условие пункта: $WHY"
    native=""
  fi
  # Повтор с ключом c кладёт a в историю и отдаёт фокус списку: поле, созданное следом, запоминает его вызывающим.
  core_cmd "$profile" "$dir/list" panel open demo-list --args '{"key":"c"}' --workspace "$ws1" ||
    fatal "условие пункта: $WHY"
  core_cmd "$profile" "$dir/field" panel open demo-native --all --workbench "$wb_a" || fatal "условие пункта: $WHY"
  cold="$(jv "$dir/field.stdout" ok.value.panel)" || fatal "условие пункта: panel open demo-native не назвал панель"
  [[ "$(jv "$dir/field.stdout" ok.value.outcome || echo ?)" == created ]] ||
    fatal "условие пункта: демо-поле не создано: исход $(jv "$dir/field.stdout" ok.value.outcome || echo ?)"
  core_cmd "$profile" "$dir/set" panel command "$cold" demo-native.set --args "{\"value\":\"$COLD_VALUE\"}" --all ||
    fatal "условие пункта: $WHY"
  settle_layout "$profile" "$dir/settle" "$wb_a" "$REV" || fatal "$item: $WHY"
  wait_inspect "$profile" "$dir/wait-list" "layout.panel.$demo.list" state.virtual_list.selected c ||
    fatal "условие пункта: $WHY"
  wait_caption "$profile" "$dir/wait-caption" "$cold" "$caption" || fatal "условие пункта: $WHY"
  panel_get "$profile" "$dir/before-list" "$demo" || fatal "$item: query.panel не прочитан"
  panel_get "$profile" "$dir/before-field" "$cold" || fatal "$item: query.panel не прочитан"
  layout_get "$profile" "$dir/before-layout" "$wb_a" || fatal "$item: query.layout не прочитан"
  list0="$(data_canon "$dir/before-list.stdout")"
  field0="$(data_canon "$dir/before-field.stdout")"
  from0="$(pv "$dir/before-field.stdout" opened_from || echo ?)"
  layout0="$(layout_canon "$dir/before-layout.stdout")"
  [[ "$(pv "$dir/before-list.stdout" data.key || echo ?)" == c &&
    "$(pv "$dir/before-list.stdout" data.history || echo ?)" == '["a"]' && "$from0" == "$demo" ]] ||
    fatal "условие пункта: список $list0, opened_from поля $from0, ожидались ключ c с историей [\"a\"] и вызывающий $demo"

  quit_owner "$profile" "$dir" "$owner_pid"
  owner_pid=""
  rows0="$(row_of "$profile" "$demo") $(row_of "$profile" "$cold") $(layout_row "$profile" "$wb_a")"
  printf '%s\n' "$rows0" >"$dir/rows-before.txt"
  [[ "$rows0" =~ $rows_re ]] ||
    failures+=("строки panel_states и workbench_layouts до перезапуска не прочитаны: ${rows0:0:200}")
  launch_normal "$profile" "$dir/relaunch"
  owner_pid="$OWNER_PID"
  settle_layout "$profile" "$dir/relaunch-settle" "$wb_a" 0 || fatal "$item: после перезапуска: $WHY"
  wait_inspect "$profile" "$dir/after-list.inspect" "layout.panel.$demo.list" state.virtual_list.selected c ||
    failures+=("список в окне после перезапуска: $WHY")
  wait_caption "$profile" "$dir/after-caption" "$cold" "$caption" || failures+=("поле в окне после перезапуска: $WHY")
  wait_field "$profile" "$dir/after-field.inspect" "$cold" "$COLD_VALUE" ||
    failures+=("поле в окне после перезапуска: $WHY")
  read_all "$profile" "$dir" after "$wb_a" "$owner_pid" || fatal "условие пункта: после перезапуска: $WHY"
  [[ -z "$FRAMES" ]] || failures+=("после перезапуска кадры: $(printf '%s' "$FRAMES" | tr '\n' ';')")
  [[ "$(panels_of "$dir/after.layout.stdout" demo-list)" == "$demo" ]] ||
    failures+=("панели demo-list после перезапуска «$(panels_of "$dir/after.layout.stdout" demo-list)», до него $demo")
  [[ "$(panels_of "$dir/after.layout.stdout" demo-native)" == "$cold" ]] ||
    failures+=("панели demo-native после перезапуска «$(panels_of "$dir/after.layout.stdout" demo-native)», до него $cold")
  [[ "$(layout_canon "$dir/after.layout.stdout")" == "$layout0" ]] ||
    failures+=("раскладка A по query.layout после перезапуска другая: before-layout.stdout и after.layout.stdout")
  if panel_get "$profile" "$dir/after-list" "$demo"; then
    [[ "$(data_canon "$dir/after-list.stdout")" == "$list0" ]] ||
      failures+=("данные списка после перезапуска $(data_canon "$dir/after-list.stdout"), до него $list0")
    [[ "$(pv "$dir/after-list.stdout" status || echo ?)" == ready ]] ||
      failures+=("статус списка после перезапуска $(pv "$dir/after-list.stdout" status || echo ?)")
  else
    failures+=("query.panel списка после перезапуска не прочитан: $(oneline "$dir/after-list.stderr")")
  fi
  if panel_get "$profile" "$dir/after-field" "$cold"; then
    [[ "$(data_canon "$dir/after-field.stdout")" == "$field0" ]] ||
      failures+=("данные поля после перезапуска $(data_canon "$dir/after-field.stdout"), до него $field0")
    [[ "$(pv "$dir/after-field.stdout" opened_from || echo ?)" == "$from0" ]] ||
      failures+=("opened_from поля после перезапуска $(pv "$dir/after-field.stdout" opened_from || echo ?), до него $from0")
    [[ "$(pv "$dir/after-field.stdout" status || echo ?)" == ready ]] ||
      failures+=("статус поля после перезапуска $(pv "$dir/after-field.stdout" status || echo ?)")
  else
    failures+=("query.panel поля после перезапуска не прочитан: $(oneline "$dir/after-field.stderr")")
  fi
  quit_owner "$profile" "$dir/relaunch" "$owner_pid"
  owner_pid=""
  rows1="$(row_of "$profile" "$demo") $(row_of "$profile" "$cold") $(layout_row "$profile" "$wb_a")"
  printf '%s\n' "$rows1" >"$dir/rows-after.txt"
  [[ "$rows1" == "$rows0" ]] ||
    failures+=("байты строк panel_states и workbench_layouts после перезапуска другие: rows-before.txt и rows-after.txt")
  got="${rows0%% *}"
  conclude "$item" "список $demo с ключом c и историей [\"a\"], поле $cold со значением «${COLD_VALUE}» и opened_from списка → app quit → запуск на том же профиле: те же PanelId, данные $list0 и $field0 и opened_from структурно равны, строки panel_states (список $got) и строка workbench_layouts рабочей области A побайтно прежние, раскладка query.layout равна; в окне выбрана c, поле и подпись показывают значение, кадры совпали с ui.inspect и AX"
}

# Профиль схемы v3 с каноническими панелями демо, данные которых запуск не принимает: заглушки с причиной, тела не
# строятся, байты строк целы после перезапуска.
item_status_stubs() {
  local item=status-stubs dir="$run/status-stubs" state pid rows0 rows turn
  local rows_re='^2\|[0-9A-F]+ 1\|7B$'
  failures=()
  log "== $item"
  if [[ -n "$owner_pid" ]]; then
    quit_owner "$profile" "$dir" "$owner_pid"
    owner_pid=""
  fi
  new_profile
  state="$NEW_PROFILE"
  chmod 700 "$state"
  /usr/bin/sqlite3 "$state/state.db" <"$fixture_status" >"$dir/sqlite.log" 2>&1 ||
    fatal "фикстура v3-status не загружена: $(oneline "$dir/sqlite.log")"
  rows0="$(row_of "$state" "$STATUS_LIST") $(row_of "$state" "$STATUS_FIELD")"
  printf '%s\n' "$rows0" >"$dir/rows-fixture.txt"
  [[ "$rows0" =~ $rows_re ]] ||
    fatal "фикстура v3-status: строки panel_states «${rows0}», ожидались версия 2 у списка и данные { у поля"
  for turn in first second; do
    launch_normal "$state" "$dir/$turn"
    pid="$OWNER_PID"
    if settle_layout "$state" "$dir/$turn-settle" "$STATUS_BENCH" 0; then
      status_stub "$state" "$dir/$turn-list" "$pid" "$STATUS_BENCH" "$STATUS_LIST" state_newer "$STATUS_NEWER" \
        "запуск $turn, демо-список"
      status_stub "$state" "$dir/$turn-field" "$pid" "$STATUS_BENCH" "$STATUS_FIELD" state_corrupt "$STATUS_CORRUPT" \
        "запуск $turn, демо-поле"
    else
      failures+=("запуск $turn: $WHY")
    fi
    quit_owner "$state" "$dir/$turn" "$pid"
    rows="$(row_of "$state" "$STATUS_LIST") $(row_of "$state" "$STATUS_FIELD")"
    printf '%s\n' "$rows" >"$dir/rows-$turn.txt"
    [[ "$rows" == "$rows0" ]] || failures+=("запуск $turn: строки panel_states «${rows}», в фикстуре «${rows0}»")
  done
  conclude "$item" "state.db схемы v3: демо-список с данными версии 2 — заглушка «${STATUS_NEWER}», статус state_newer; демо-поле с данными { — заглушка «${STATUS_CORRUPT}», статус state_corrupt; заглушки без кнопки по ui.inspect и AX, статусы в ui.inspect и query.panel, bodies_built обеих 0; строки panel_states $rows0 побайтно прежние после первого запуска и после перезапуска"
}

# Профиль схемы v2 с двумя панелями demo-native в A и B: старшая в A готова, младшая в B — дубликат с заглушкой;
# panel open переносит каноническую в B, дубликат остаётся заглушкой.
item_legacy_duplicate() {
  local item=legacy-duplicate dir="$run/legacy-duplicate" state pid got
  failures=()
  log "== $item"
  if [[ -n "$owner_pid" ]]; then
    quit_owner "$profile" "$dir" "$owner_pid"
    owner_pid=""
  fi
  new_profile
  state="$NEW_PROFILE"
  chmod 700 "$state"
  /usr/bin/sqlite3 "$state/state.db" <"$fixture_duplicate" >"$dir/sqlite.log" 2>&1 ||
    fatal "фикстура v2-duplicate-demo-native не загружена: $(oneline "$dir/sqlite.log")"
  launch_normal "$state" "$dir/owner"
  pid="$OWNER_PID"
  if ! settle_layout "$state" "$dir/settle-a" "$DUP_A" 0; then
    record "$item" fail "сразу после запуска: $WHY"
    quit_owner "$state" "$dir/owner" "$pid"
    return 0
  fi
  wait_inspect "$state" "$dir/a.inspect" "layout.panel.$DUP_OLD.field" visible true ||
    failures+=("поле старшей панели в окне A: $WHY")
  got="$(pfact "$dir/a.inspect.stdout" "$DUP_OLD" status || echo нет)"
  [[ "$got" == ready ]] || failures+=("ui.inspect: статус старшей панели в A $got, ожидался ready")
  if panel_get "$state" "$dir/a.old" "$DUP_OLD" && panel_get "$state" "$dir/a.new" "$DUP_NEW"; then
    [[ "$(pv "$dir/a.old.stdout" status || echo ?)" == ready ]] ||
      failures+=("query.panel: статус старшей панели $(pv "$dir/a.old.stdout" status || echo ?), ожидался ready")
    [[ "$(pv "$dir/a.new.stdout" status || echo ?)" == duplicate ]] ||
      failures+=("query.panel: статус младшей панели $(pv "$dir/a.new.stdout" status || echo ?), ожидался duplicate")
    [[ "$(pv "$dir/a.new.stdout" canonical || echo ?)" == "$DUP_OLD" ]] ||
      failures+=("query.panel: canonical младшей панели $(pv "$dir/a.new.stdout" canonical || echo ?), ожидалась старшая $DUP_OLD")
  else
    failures+=("query.panel панелей demo-native не прочитан: $(oneline "$dir/a.old.stderr") $(oneline "$dir/a.new.stderr")")
  fi

  core_cmd "$state" "$dir/open-b" workbench open --target "$DUP_B" --workspace "$DUP_WS" || fatal "$item: $WHY"
  settle_layout "$state" "$dir/settle-b" "$DUP_B" "$REV" || fatal "$item: $WHY"
  status_stub "$state" "$dir/b-stub" "$pid" "$DUP_B" "$DUP_NEW" duplicate "$DUP_STUB" "рабочая область B"

  if core_cmd "$state" "$dir/open" panel open demo-native --all --workbench "$DUP_B"; then
    [[ "$(jv "$dir/open.stdout" ok.value.outcome || echo ?)" == moved ]] ||
      failures+=("panel open: исход $(jv "$dir/open.stdout" ok.value.outcome || echo ?), ожидался moved")
    [[ "$(jv "$dir/open.stdout" ok.value.panel || echo ?)" == "$DUP_OLD" ]] ||
      failures+=("panel open назвал панель $(jv "$dir/open.stdout" ok.value.panel || echo ?), а не каноническую $DUP_OLD")
    [[ "$(jv "$dir/open.stdout" ok.value.workbench || echo ?)" == "$DUP_B" ]] ||
      failures+=("panel open назвал рабочую область $(jv "$dir/open.stdout" ok.value.workbench || echo ?), а не B")
    settle_layout "$state" "$dir/settle-moved" "$DUP_B" "$REV" || fatal "$item: $WHY"
    layout_get "$state" "$dir/moved-a" "$DUP_A" || fatal "$item: query.layout не прочитан"
    layout_get "$state" "$dir/moved-b" "$DUP_B" || fatal "$item: query.layout не прочитан"
    [[ -z "$(panels_of "$dir/moved-a.stdout" demo-native)" ]] ||
      failures+=("в раскладке A осталась панель $(panels_of "$dir/moved-a.stdout" demo-native)")
    got=" $(panels_of "$dir/moved-b.stdout" demo-native) "
    [[ "$got" == *" $DUP_OLD "* && "$got" == *" $DUP_NEW "* ]] ||
      failures+=("в раскладке B панели demo-native «${got}», ожидались каноническая и дубликат")
    wait_inspect "$state" "$dir/moved.inspect" "layout.panel.$DUP_OLD.field" visible true ||
      failures+=("поле канонической панели в окне B: $WHY")
    # Каноническая встала активной вкладкой той же части: фокус на дубликате снова показывает его заглушку.
    core_cmd "$state" "$dir/focus-new" panel focus "$DUP_NEW" --all || failures+=("panel focus дубликата: $WHY")
    status_stub "$state" "$dir/moved-stub" "$pid" "$DUP_B" "$DUP_NEW" duplicate "$DUP_STUB" "после переезда канонической"
    if panel_get "$state" "$dir/moved.old" "$DUP_OLD"; then
      [[ "$(pv "$dir/moved.old.stdout" status || echo ?)" == ready ]] ||
        failures+=("query.panel: статус канонической после переезда $(pv "$dir/moved.old.stdout" status || echo ?)")
      [[ "$(pv "$dir/moved.old.stdout" panel.workbench || echo ?)" == "$DUP_B" ]] ||
        failures+=("query.panel: каноническая в рабочей области $(pv "$dir/moved.old.stdout" panel.workbench || echo ?), а не в B")
    else
      failures+=("query.panel канонической после переезда не прочитан: $(oneline "$dir/moved.old.stderr")")
    fi
    got="$(built_of "$dir/moved-stub.stats.stdout" "$DUP_OLD")"
    [[ "$got" == 1 ]] || failures+=("bodies_built канонической панели после переезда $got, ожидалось 1")
  else
    failures+=("panel open demo-native --all --workbench B: $WHY")
  fi
  quit_owner "$state" "$dir/owner" "$pid"
  conclude "$item" "state.db схемы v2 с двумя панелями demo-native: после запуска старшая $DUP_OLD в A — ready с полем в окне, младшая $DUP_NEW в B — duplicate с canonical старшей; в окне B заглушка «${DUP_STUB}» без кнопки по ui.inspect и AX, bodies_built дубликата 0; panel open demo-native --all --workbench B — moved, каноническая в раскладке B, в A её нет, её поле в окне, bodies_built 1; дубликат остался заглушкой"
}

prepare
preconditions
setup_main
item_placeholders_default
item_plugin_catalog
item_open_cli
item_retarget_cli
item_drill_keys
# restore-list оставляет основного владельца остановленным: missing-plugin идёт на собственном профиле, а System
# Events находит окно по имени процесса.
item_restore_list
item_missing_plugin
# Пункты рубежа 2 идут в порядке матрицы после пунктов с собственным профилем: menu-list сначала проверяет
# WorkspaceHome на новом профиле, пока основной владелец остановлен, затем запускает основного владельца снова, и
# он работает до конца прогона. open-menu и empty-workbench-open идут в A2, open-race и race-multi — в B,
# singleton-split и picker-singleton — в A с демо-списком open-cli.
item_menu_list
item_open_menu
item_empty_workbench_open
item_open_race
item_race_multi
item_singleton_split
item_picker_singleton
# Пункты рубежа 3 делят одну панель «Демо: поле» (PerApp) и идут в порядке матрицы: open-menu-native создаёт её в
# A, native-edit и native-zones правят её там же, perapp-move увозит в B, perapp-return возвращает из A в B и
# закрывает.
item_open_menu_native
item_native_edit
item_native_zones
item_perapp_move
item_perapp_scope
item_perapp_return
# Пункты задачи 12 на основном профиле идут в A с демо-списком open-cli: zones-demo и stub-min-size возвращают его в
# правую зону, demo-snapshot снимает его там, restore-cold оставляет основного владельца остановленным. Пункты с
# собственным профилем (status-stubs, legacy-duplicate) идут последними: второго владельца бандла рядом нет.
item_zones_demo
item_stub_min_size
item_demo_snapshot
item_restore_cold
item_status_stubs
item_legacy_duplicate
if [[ -n "$owner_pid" ]]; then
  mkdir -p "$run/cleanup"
  quit_owner "$profile" "$run/cleanup" "$owner_pid"
  owner_pid=""
fi
for i in "${!ITEMS[@]}"; do
  if ((TASK[i] > DONE_TASK)); then
    record "${ITEMS[i]}" not_run "пункт задачи ${TASK[i]}"
  fi
done
record voiceover-speech not_run "речь VoiceOver автоматика не наблюдает, роли и названия проверяют open-cli и menu-list"
completed=1
