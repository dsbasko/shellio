#!/usr/bin/env bash
# Сценарий make e2e «сверка с прототипом» (ADR-0037): пункты матрицы приёмки плана prototype-conformance на
# подписанном бандле и временном профиле.
#
#   conformance.sh [<бандл>]
#
# Бандл по умолчанию — target/bundle/shellio.app, его собирает и проверяет цель make e2e. shellioctl и оракул
# shellio-e2e-oracle сценарий собирает сам (release, --locked). Профиль — новый каталог /tmp/shellio-e2e.*.
# Подготовка — штатный CLI по файлу сцены conformance/scenes/p15.json: воркспейс, проект, рабочая область,
# panel open с зоной для каждой вкладки сцены, которой нет в раскладке по умолчанию, panel select активной вкладки
# каждой части и panel focus части с фокусом. Собранная раскладка сверяется со сценой по query.layout: порядок
# вкладок по плагинам, активная вкладка и фокус; расхождение — ошибка исполнения.
#
# Состояние реестра собирается из чистой сцены по своему рецепту conformance/recipes/<состояние>.json: размер окна
# рецепта задаёт служебная команда ui window size и проверяет ui.window, шаги выполняет штатный CLI
# (hide_zone и show_zone — layout hide|show, split — layout split --open, zoom — layout zoom, focus — panel focus,
# move — panel move, с new_part — panel move-part к последней части зоны с краем after, close_tab — panel close),
# постусловия рецепта сверяются с query.layout. Левая зона сцены — колонка навигатора (часть 1) и части левой
# плитки (части 2 и дальше). Состояние с шагами или своим размером окна оставляет сцену изменённой: следующее
# состояние получает новый профиль и нового владельца.
#
# Шаги клавиатуры идут в порядке рецепта вместе с командами: key — аккорды записи keymap по одному через
# System Events (key code с модификаторами, владелец на переднем плане), после последнего аккорда шага — запись
# ui.receipts исполненного действия не дольше KEY_GRACE_MS; keymap — текст шага пишется в
# config/keymap.toml профиля и применяется командой keymap reload; focus_field — panel focus активной панели части,
# first_responder поля этой панели в ui.inspect, клавиша Tab — только если панель сама фокус полю не отдала; filter — ui filter с текстом шага; query — ui palette open --query с текстом шага у открытой палитры. Шаг, который
# продукт не исполнил (отказ команды, поле без фокуса), — fail пункта, а не пропуск.
#
# Шаги split_refused, open_menu и open_picker — нажатия в окне, а не команды: штатным CLI шаг только переводит
# фокус в свою часть, а само нажатие идёт через AX в каждой ячейке после её осей и до снимка. split_refused —
# AXPress «Разделить» строки вкладок части и сообщение строки статуса; open_menu — фоновый AXPress «Действия
# панели» и открытое меню «…» по ui.inspect; open_picker — AXPress «Разделить» и открытый выбор «Открыть в новой
# части». Меню и выбор после снимка ячейки закрывает Escape. Такой шаг в рецепте — последний.
#
# Состояния поверхности palette (реестр) судятся снимком композиции: палитра — отдельное окно, поэтому снимается
# область окна владельца screencapture -R (подкоманда оракула region), а не окно -l. Шаги key и query рецепта
# идут в каждой ячейке после её осей: сначала основа — два совпавших снимка области при закрытой палитре, затем
# шаги со свидетелем каждого — новой подписью ui.overlay (вид, запрос, категория, выбранная строка). Перед каждым
# снимком оракул pair сверяет пару окон со списком окон экрана (CGWindowListCopyWindowInfo через JXA): чужое окно
# любого уровня, кроме указателя мыши (screencapture без -C его не снимает),
# над областью или другой экран — ошибка исполнения, палитра позади владельца или не на экране — fail. Принятый
# conform снимок судит controls против основы (заливка плашки, тень вниз, фон за полем тени). После снимка
# палитру закрывает Escape, закрытие подтверждают ui.overlay и pair.
#
# Состояния which-key (WHICH_STATES) судятся снимком окна, как поверхность layout, но шаг key их рецепта — префикс:
# действия он не исполняет и записи ui.receipts не оставляет. Шаг идёт в каждой ячейке после её осей: до него
# ui.overlay без наложения и без which_key, после — which_key.prefix рецепта не дольше MIRROR_MS (свидетель шага).
# После снимка ui.overlay ещё показывает подсказку, иначе снимок не годится; затем Escape снимает префикс, и
# which_key из ui.overlay уходит.
#
# Шаг hints рецепта (состояния меток) тоже идёт в каждой ячейке после её осей: до него ui.overlay без наложения,
# затем клавиша f и знаки набора кодами через System Events, после каждой — ui.overlay с видом hints и набором до
# этой клавиши не дольше MIRROR_MS (свидетель шага). Состав меток по порядку (текст и ключ цели) сверяется с
# паспортом: текст — узлы hints.item.{n}, цель номера n — n-я цель композиции паспорта в порядке прототипа;
# расхождение — fail. После снимка ui.overlay ещё показывает метки с тем же набором, иначе снимок не годится;
# затем Escape закрывает метки, и вид из ui.overlay уходит.
#
# Каталог прогона — target/e2e/conformance/<время>-<pid>/: result.json (пункт, обязательность, статус, причина,
# путь сырого вывода относительно каталога прогона), журнал сценария log и каталог каждого пункта. У пункта
# conform-<состояние> — подкаталог ячейки с паспортом, чтениями ui.inspect и ui.window, PNG и отчётом оракула.
# Пробы предусловий пишут в preconditions/, уборка — в cleanup/.
#
# Предусловия — как у plugins.sh, их отсутствие — ошибка исполнения «предусловие: …» до первого пункта: открытый
# сеанс GUI, разблокированный экран, запущенный Finder, бинарь бандла не запущен, запуск из процесса-потомка
# приложения терминала; у терминала-предка Screen Recording и Accessibility с Automation для System Events;
# латинский источник ввода; AeroSpace и другие оконные менеджеры выключены. Пункты conform-* судятся в окне
# размера рецепта (1180×720, если рецепт не задаёт своего) на экране с масштабом 2, иначе пункт — ошибка
# исполнения «условие пункта».
#
# Пункт conform-<состояние> судит четыре ячейки (светлая и тёмная тема, обычный и повышенный контраст): ячейку
# задают ui appearance и ui cell, сценарий ждёт оси в ui.window на новом поколении и снимает окно в режиме без
# маркера, пока оракул conform не примет пару снимков подряд. Код 0 во всех ячейках — pass, код 3 — fail с
# перечнем расхождений из отчёта, код 4 и код 1 оракула — ошибка исполнения пункта. Отказ продукта в шаге
# рецепта и невыполненное постусловие — fail с причиной: оракул такую ячейку не судит. Нажатие, после которого
# окно не показало ожидаемого (сообщения, меню, выбора), — тоже fail, остальные ячейки пункта не судятся. Без
# conformance/calibration/calibration.json (make calibrate) оракул не вызывается: пункты conform-* получают
# not_run «нет калибровки».
#
# Рубежи накопительные (план, «Матрица приёмки»): пункт, чья задача ещё не выполнена, получает not_run
# «пункт задачи N» и необязателен.
#
# Сигналы и уборка адресуют только PID, полученные через hello сокета прогона и сверенные с бинарём бандла
# прогона. Внешние условия опрашиваются с крайним сроком, фиксированных пауз вместо условия нет.
#
# Выход — после записи result.json и уборки: 0 — все обязательные пункты pass, 1 — ошибка исполнения, 3 — ошибки
# исполнения нет, но хотя бы один обязательный пункт не pass. Необязательные пункты на код не влияют.
set -Eeuo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
cargo="${CARGO:-$HOME/.cargo/bin/cargo}"
scene="$root/conformance/scenes/p15.json"
registry="$root/conformance/registry.toml"
calibration="$root/conformance/calibration/calibration.json"
passports="$root/conformance/passports"

FINDER_ID="com.apple.finder"
LSAPPINFO=/usr/bin/lsappinfo
SRGB_PROFILE="/System/Library/ColorSync/Profiles/sRGB Profile.icc"
# Оконные менеджеры, которые переставляют окна сами (P0.2).
WINDOW_MANAGERS=(AeroSpace yabai Amethyst)
LATIN_LAYOUTS=(com.apple.keylayout.ABC com.apple.keylayout.US com.apple.keylayout.British
  com.apple.keylayout.British-PC)

# Сроки в секундах и миллисекундах.
OWNER_START_S=20
EXIT_S=10
KILL_S=5
SECOND_LAUNCH_S=20
MIRROR_MS=3000
# Срок новой ячейки в ui.window (план, «Гонка ячейки»).
SETTLE_MS=3000
# Фоновый AXPress меню «…» ждёт закрытия меню: срок его выхода, секунды.
MENU_OSA_S=20
KEY_ESC=53
# Срок ожидания следа клавиши шага рецепта: записи кольца квитанций и фокуса в поле без Tab.
KEY_GRACE_MS=1000
POLL=0.1
CTL_TIMEOUT_MS=2000

# Окно, в котором сняты паспорта, и масштаб экрана, на котором судятся снимки.
WINDOW_W=1180
WINDOW_H=720
SCALE=2

# Ячейки: имя паспорта, выбор ui appearance, аргумент ui cell --contrast и значение оси контраста в ui.window.
CELLS=(
  "light-normal|light|normal|normal"
  "light-high|light|high|increased"
  "dark-normal|dark|normal|normal"
  "dark-high|dark|high|increased"
)

# Пункты матрицы по порядку плана и задача, которая делает пункт обязательным.
ITEMS=(conform-chrome-titlebar conform-chrome-statusline cell-override fonts-bundled conform-layout-default
  conform-layout-all-zones conform-layout-left-plate conform-layout-split-right conform-layout-split-center
  conform-layout-split-bottom conform-layout-zoom conform-tabs-side conform-tabs-center conform-tabs-half
  conform-tabs-wide ck-coverage conform-layout-stub conform-layout-empty-workbench conform-menu-panel
  conform-picker-new-part conform-status-split-no-room conform-panel-filter conform-status-mode-nav
  conform-status-input-field conform-status-wrong-axis conform-status-keymap-error voiceover-speech
  conform-palette-groups conform-palette-tab-narrowed conform-palette-no-results conform-whichkey-leader
  conform-whichkey-ctrlw conform-hints-full conform-hints-partial)
# Пункты палитры плана command-overlays судит снимок композиции, пункты which-key — снимок окна после префикса,
# пункты меток — снимок окна после клавиши f и набора.
TASK=(10 10 10 10 11 11 11 11 11 11 11 11 11 11 11 11 12 12 12 12 12 12 12 12 12 12 0 13 13 13 14 14 15 15)
# Задача, до которой сценарий доведён: пункты более поздних задач не судятся.
DONE_TASK=15
# Состояния, чей шаг key — префикс со свидетелем ui.overlay (which_key), а не действие с записью ui.receipts.
WHICH_STATES=(whichkey-leader whichkey-ctrlw)
# Плашка клавиши поиска: её шрифт судит пункт fonts-bundled.
KBD_CK="chrome.search.kbd"
MONO_FAMILY="JetBrains Mono"

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

run="$root/target/e2e/conformance/$(date +%Y%m%d-%H%M%S)-$$"
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

# Значения, которые функции отдают через глобальные переменные: $(…) потерял бы known_pids.
HELLO_PID=""
OWNER_PID=""
NEW_PROFILE=""
WHY=""
GEN=""
REV=""
BUILT=""

profile=""
owner_pid=""
ws=""
wb=""
# Сцена владельца не менялась шагами рецепта и размером окна.
scene_clean=0
# Размер окна состояния и род отказа подготовки состояния: fail — отказ продукта или постусловие, broken — ошибка
# исполнения.
STATE_W=""
STATE_H=""
STEP_KIND=""
# Нажатие состояния (последний шаг рецепта split_refused, open_menu или open_picker), описание строки вкладок его
# части и ключ кнопки меню «…» этой части; пусто — нажатия нет.
PRESS_OP=""
PRESS_STRIP=""
PRESS_MENU_CK=""
# Поверхность состояния по реестру и отложенные шаги открытия палитры (номер шага и строка плана по одной на строку).
STATE_SURFACE=""
PALETTE_STEPS=""
bg_job=""
STATUS_TEXT=""

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
    printf '  "scenario": "conformance",\n'
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

# ID объекта tree с названием title; чтение — <dir>/tree-<name>.stdout.
tree_id() {
  ctl "$profile" "$1/tree-$2" --json tree --all || return 1
  id_by_title "$1/tree-$2.stdout" "$3"
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

trap 'err_at=$LINENO' ERR
trap finish EXIT
trap 'on_signal SIGINT' INT
trap 'on_signal SIGTERM' TERM

# --- ячейка и снимок ---

# ui cell с аргументами; поколение ответа — в GEN, иначе причина в WHY. Ответ — <prefix>.stdout.
cell_cmd() {
  local state="$1" prefix="$2" code=0
  shift 2
  GEN=""
  WHY=""
  ctl "$state" "$prefix" --json --timeout "$CTL_TIMEOUT_MS" ui cell "$@" || code=$?
  if [[ $code -ne 0 ]]; then
    WHY="shellioctl ui cell $*: код $code: $(oneline "$prefix.stdout") $(oneline "$prefix.stderr")"
    return 1
  fi
  if ! GEN="$(jv "$prefix.stdout" ok.ui.cell.generation)"; then
    WHY="shellioctl ui cell $*: нет поколения в ответе: $(oneline "$prefix.stdout")"
    return 1
  fi
}

# Опрос ui.window: светлота и контраст равны ожидаемым на поколении не меньше gen; ответ — <prefix>.stdout,
# иначе WHY.
wait_cell() {
  local prefix="$1" lightness="$2" contrast="$3" gen="$4" deadline seen="" l="" c=""
  WHY=""
  deadline=$(($(now_ms) + SETTLE_MS))
  while :; do
    if ui_read "$profile" "$prefix" window; then
      seen="$(jv "$prefix.stdout" ok.ui.window.generation || echo 0)"
      l="$(jv "$prefix.stdout" ok.ui.window.lightness.value || echo ?)"
      c="$(jv "$prefix.stdout" ok.ui.window.contrast.value || echo ?)"
      if [[ "$l" == "$lightness" && "$c" == "$contrast" && "$seen" =~ ^[0-9]+$ ]] && ((seen >= gen)); then
        return 0
      fi
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $SETTLE_MS мс ui.window не показал ячейку $lightness, $contrast на поколении $gen: светлота $l, контраст $c, поколение ${seen:-?}"
      return 1
    fi
    sleep "$POLL"
  done
}

# Ячейка окна: ui appearance, затем ui cell --contrast, затем оси в ui.window на новом поколении; иначе WHY.
set_cell() {
  local dir="$1" lightness="$2" contrast_arg="$3" contrast="$4"
  ui_cmd "$profile" "$dir/appearance" ui appearance "$lightness" || return 1
  cell_cmd "$profile" "$dir/cell" --contrast "$contrast_arg" || return 1
  wait_cell "$dir/axes" "$lightness" "$contrast" "$GEN"
}

# Окно владельца — обычное окно W×H на экране с масштабом 2; иначе WHY.
window_ok() {
  local file="$1" WINDOW_W="$2" WINDOW_H="$3" w h scale
  w="$(jv "$file" ok.ui.window.frame.width || echo ?)"
  h="$(jv "$file" ok.ui.window.frame.height || echo ?)"
  scale="$(jv "$file" ok.ui.window.backing_scale_factor || echo ?)"
  WHY=""
  if [[ "$(jv "$file" ok.ui.window.mode || echo ?)" != normal ]]; then
    WHY="окно не в обычном режиме: $(jv "$file" ok.ui.window.mode || echo ?)"
  elif ! awk -v w="$w" -v h="$h" -v W="$WINDOW_W" -v H="$WINDOW_H" \
    'BEGIN { exit !(w - W <= 1 && W - w <= 1 && h - H <= 1 && H - h <= 1) }'; then
    WHY="окно ${w}×${h}, а не ${WINDOW_W}×${WINDOW_H}"
  elif ! awk -v s="$scale" -v S="$SCALE" 'BEGIN { exit !(s == S) }'; then
    WHY="окно на экране с масштабом $scale, а не $SCALE"
  fi
  [[ -z "$WHY" ]]
}

# Сверка паспорта состояния с окном в режиме без маркера: снимки повторяются, пока оракул conform не примет пару
# подряд (код 0 или 3) либо срок не истёк (код 4 с retry: false). Код оракула — в snap_code, причина — в
# snap_reason. Сырые файлы — <dir>/<N>.{window.stdout,inspect.stdout,png,report.json}, принятая пара — <dir>/report.json.
snap_code=""
snap_reason=""
conform_snapshot() {
  local state="$1" cell="$2" dir="$3" t0 n=0 number code prev="" elapsed cur
  snap_code=""
  snap_reason=""
  t0="$(now_ms)"
  while :; do
    n=$((n + 1))
    cur="$dir/$n"
    ui_read "$profile" "$cur.window" window || fatal "$state $cell: ui.window не прочитан: $(oneline "$cur.window.stdout")"
    ui_read "$profile" "$cur.inspect" inspect ||
      fatal "$state $cell: ui.inspect не прочитан: $(oneline "$cur.inspect.stdout")"
    number="$(jv "$cur.window.stdout" ok.ui.window.window_number)" || fatal "$state $cell: в ui.window нет window_number"
    /usr/sbin/screencapture -x -o -l "$number" "$cur.raw.png" >"$cur.capture.log" 2>&1 ||
      fatal "$state $cell: screencapture -l $number отказал: $(oneline "$cur.capture.log")"
    [[ -s "$cur.raw.png" ]] || fatal "$state $cell: screencapture -l $number не записал PNG"
    sips --matchTo "$SRGB_PROFILE" "$cur.raw.png" --out "$cur.png" >"$cur.sips.log" 2>&1 ||
      fatal "$state $cell: sips не перевёл PNG в sRGB: $(oneline "$cur.sips.log")"
    if [[ -n "$prev" ]]; then
      elapsed=$(($(now_ms) - t0))
      code=0
      "$oracle_bin" conform --passport "$passports/$state/$cell.json" --inspect "$cur.inspect.stdout" \
        --window "$cur.window.stdout" --png "$cur.png" --registry "$registry" --calibration "$calibration" \
        --previous-window "$prev.window.stdout" --previous-png "$prev.png" --elapsed-ms "$elapsed" \
        >"$cur.report.json" 2>"$cur.oracle.stderr" || code=$?
      case $code in
        0 | 3)
          snap_code=$code
          snap_reason="$(jv "$cur.report.json" reason || oneline "$cur.oracle.stderr")"
          cp "$cur.report.json" "$dir/report.json"
          log "$state $cell: код оракула $code за $n снимк., $snap_reason"
          return 0
          ;;
        4)
          if [[ "$(jv "$cur.report.json" retry || echo false)" != true ]]; then
            snap_code=4
            snap_reason="отрисовка не наблюдалась: $(oneline "$cur.oracle.stderr")"
            log "$state $cell: $snap_reason"
            return 0
          fi
          ;;
        *)
          snap_code=$code
          snap_reason="оракул — ошибка исполнения (код $code): $(oneline "$cur.oracle.stderr")"
          log "$state $cell: $snap_reason"
          return 0
          ;;
      esac
    fi
    prev="$cur"
  done
}

# Расхождения отчёта conform одной строкой: число и первые пять «ключ поле: ожидание против факта».
mismatches_of() {
  /usr/bin/perl -Mutf8 -MJSON::PP -e '
    my ($file) = @ARGV;
    open my $fh, "<", $file or exit 0;
    local $/;
    my $r = eval { JSON::PP->new->utf8->decode(<$fh>) } or exit 0;
    my @m = @{ $r->{mismatches} // [] };
    my @u = @{ $r->{unjudged} // [] };
    binmode STDOUT, ":utf8";
    my @head = map { "$_->{ck} $_->{field}: $_->{expected} против $_->{actual}" } @m[0 .. ($#m < 4 ? $#m : 4)];
    print "расхождений " . scalar(@m) . (@u ? ", полей без суждения " . scalar(@u) : "") .
      (@head ? ": " . join("; ", @head) : "") . ($#m > 4 ? "; …" : "") . "\n";
  ' "$1"
}

# --- сцена ---

# План сцены по файлу сцены и ответу query.layout. Режим opens печатает «зона плагин» для каждой вкладки сцены,
# которой в раскладке ещё нет, по порядку; check — строки «select <панель>» активной вкладки каждой части и
# «focus <панель>» части с фокусом; verify — расхождения раскладки со сценой, по одному на строку. Раскладка,
# которую сцена не продолжает (другой плагин на месте вкладки, лишняя вкладка или часть), — строка «error: …».
scene_plan() {
  /usr/bin/perl -Mutf8 -MJSON::PP -e '
    my ($mode, $scene_file, $layout_file) = @ARGV;
    sub load { open my $fh, "<", $_[0] or die "$_[0]: $!\n"; local $/; JSON::PP->new->utf8->decode(<$fh>) }
    my $scene = load($scene_file);
    my $value = load($layout_file)->{ok}{value};
    my $layout = $value->{layout};
    my %plugin = map { $_->{id} => $_->{plugin} } @{ $value->{panels} };
    binmode STDOUT, ":utf8";
    my @out;
    my $focus = $scene->{focus};
    for my $zone (@{ $scene->{zones} }) {
      my $name = $zone->{zone};
      my @want = @{ $zone->{parts} };
      # Левая зона сцены с одним навигатором — колонка навигатора продукта, а не часть левой плитки.
      if ($name eq "left" && @want == 1 && @{ $want[0]{tabs} } == 1 && $want[0]{tabs}[0]{plugin} eq "navigator") {
        my $nav = $layout->{navigator};
        push @out, "error: в раскладке нет навигатора" unless defined $nav && ($plugin{$nav} // "") eq "navigator";
        push @out, "error: левая плитка не пуста" if @{ $layout->{zones}{left}{parts} };
        push @out, "focus $nav" if $mode eq "check" && defined $nav && $focus->{zone} eq "left";
        if ($mode eq "verify" && $focus->{zone} eq "left" && ($layout->{focus}[-1] // "") ne ($nav // "")) {
          push @out, "фокус не в колонке навигатора";
        }
        next;
      }
      my @have = @{ $layout->{zones}{$name}{parts} };
      push @out, "error: в зоне $name частей " . scalar(@have) . ", в сцене " . scalar(@want) if @have > @want;
      push @out, "error: сцена делит зону $name на части: деление — шаг рецепта" if @want > 1;
      for my $p (0 .. $#want) {
        my @tabs = @{ $want[$p]{tabs} };
        my @ids = $p <= $#have ? @{ $have[$p]{tabs} } : ();
        push @out, "error: в части " . ($p + 1) . " зоны $name вкладок " . scalar(@ids) . ", в сцене " . scalar(@tabs)
          if @ids > @tabs;
        my $active = 0;
        for my $t (0 .. $#tabs) {
          $active = $t if $tabs[$t]{active};
          if ($t <= $#ids) {
            my $is = $plugin{ $ids[$t] } // "?";
            push @out, ($mode eq "verify" ? "" : "error: ") .
              "вкладка " . ($t + 1) . " части " . ($p + 1) . " зоны $name — $is, в сцене $tabs[$t]{plugin}"
              if $is ne $tabs[$t]{plugin};
          } elsif ($mode eq "opens") {
            push @out, "$name $tabs[$t]{plugin}";
          } else {
            push @out, ($mode eq "verify" ? "" : "error: ") .
              "нет вкладки " . ($t + 1) . " части " . ($p + 1) . " зоны $name ($tabs[$t]{plugin})";
          }
        }
        next if $active > $#ids;
        my $here = $focus->{zone} eq $name && $focus->{part} == $p + 1;
        if ($mode eq "check") {
          push @out, "select $ids[$active]";
          push @out, "focus $ids[$active]" if $here;
        } elsif ($mode eq "verify") {
          push @out, "активная вкладка части " . ($p + 1) . " зоны $name — " . ($have[$p]{active} + 1) .
            ", в сцене " . ($active + 1) if $have[$p]{active} != $active;
          push @out, "фокус не в части " . ($p + 1) . " зоны $name"
            if $here && ($layout->{focus}[-1] // "") ne $ids[$active];
        }
      }
    }
    # Команда focus идёт последней: select другой части фокус не возвращает.
    print "$_\n" for (grep { !/^focus / } @out), (grep { /^focus / } @out);
  ' "$1" "$scene" "$2"
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
  for left in "$scene" "$registry"; do
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

# Пробы предусловий до владельца: оконные менеджеры, источник ввода, Screen Recording.
preconditions() {
  local dir="$run/preconditions" name layout ok preflight pid
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
}

# Новый профиль и владелец со сценой, собранной штатным CLI по файлу сцены. С probe=1 — ещё пробы Automation,
# Accessibility и снимка на окне владельца.
build_scene() {
  local dir="$1" probe="${2:-0}" project dir1 title zone plugin verb panel n=0 name number
  mkdir -p "$dir"
  log "== профиль по сцене $scene"
  new_profile
  profile="$NEW_PROFILE"
  launch_normal "$profile" "$dir"
  owner_pid="$OWNER_PID"
  scene_clean=0
  if ((probe)); then
  name="$(osascript -e "tell application \"System Events\" to get name of (first process whose unix id is $owner_pid)" \
    2>"$dir/system-events.stderr" || true)"
  [[ -n "$name" ]] ||
    fatal "предусловие: System Events не прочитал процесс pid $owner_pid (нет Automation у терминала-предка): $(oneline "$dir/system-events.stderr")"
  osascript -e "tell application \"System Events\" to count windows of (first process whose unix id is $owner_pid)" \
    >"$dir/accessibility.stdout" 2>"$dir/accessibility.stderr" ||
    fatal "предусловие: System Events не прочитал окна pid $owner_pid (нет Accessibility у терминала-предка): $(oneline "$dir/accessibility.stderr")"
  fi
  dir1="$(mktemp -d /tmp/shellio-e2e-project.XXXXXX)"
  profiles+=("$dir1")
  title="$(jv "$scene" workspace)" || fatal "подготовка: в сцене нет названия воркспейса"
  core_cmd "$profile" "$dir/ws" workspace new "$title" --all || fatal "подготовка: $WHY"
  ws="$(tree_id "$dir" ws "$title")" || fatal "подготовка: воркспейс «$title» не найден в tree"
  core_cmd "$profile" "$dir/project" project add "$dir1" --title "conformance-p" --workspace "$ws" ||
    fatal "подготовка: $WHY"
  project="$(tree_id "$dir" project "conformance-p")" || fatal "подготовка: проект не найден в tree"
  core_cmd "$profile" "$dir/wb" workbench new --target "$project" "conformance-wb" --workspace "$ws" ||
    fatal "подготовка: $WHY"
  wb="$(tree_id "$dir" wb "conformance-wb")" || fatal "подготовка: рабочая область не найдена в tree"
  core_cmd "$profile" "$dir/open" workbench open --target "$wb" --workspace "$ws" || fatal "подготовка: $WHY"
  settle_layout "$profile" "$dir/settle" "$wb" "$REV" || fatal "подготовка: $WHY"
  scene_plan opens "$dir/settle.layout.stdout" >"$dir/opens" || fatal "подготовка: план сцены не построен"
  while IFS=' ' read -r zone plugin; do
    [[ "$zone" != error: ]] || fatal "подготовка: сцена не продолжает раскладку по умолчанию: $plugin"
    n=$((n + 1))
    core_cmd "$profile" "$dir/open-$n" panel open "$plugin" --workbench "$wb" --zone "$zone" --all ||
      fatal "подготовка: $WHY"
  done <"$dir/opens"
  layout_get "$profile" "$dir/opened" "$wb" || fatal "подготовка: query.layout не прочитан: $(oneline "$dir/opened.stderr")"
  scene_plan check "$dir/opened.stdout" >"$dir/selects" || fatal "подготовка: план сцены не построен"
  n=0
  while IFS=' ' read -r verb panel; do
    [[ "$verb" != error: ]] || fatal "подготовка: раскладка не собрана по сцене: $panel"
    n=$((n + 1))
    core_cmd "$profile" "$dir/$verb-$n" panel "$verb" "$panel" --all || fatal "подготовка: $WHY"
  done <"$dir/selects"
  settle_layout "$profile" "$dir/scene" "$wb" "$REV" || fatal "подготовка: $WHY"
  scene_plan verify "$dir/scene.layout.stdout" >"$dir/verify" || fatal "подготовка: сцена не сверена"
  [[ ! -s "$dir/verify" ]] || fatal "подготовка: раскладка расходится со сценой: $(oneline "$dir/verify")"
  front_ok "$owner_pid" "$dir" || fatal "подготовка: $WHY"
  if ((probe)); then
    number="$(jv "$dir/scene.window.stdout" ok.ui.window.window_number)" || fatal "подготовка: в ui.window нет window_number"
    /usr/sbin/screencapture -x -o -l "$number" "$dir/probe.png" >"$dir/probe.log" 2>&1 ||
      fatal "предусловие: screencapture -l $number отказал (нет Screen Recording): $(oneline "$dir/probe.log")"
  fi
  scene_clean=1
  log "воркспейс $ws, рабочая область $wb: сцена собрана и сверена с query.layout"
}

# Чистая сцена: владелец с изменённой сценой завершается, сцена собирается в новом профиле.
fresh_scene() {
  local dir="$1"
  ((!scene_clean)) || return 0
  mkdir -p "$dir"
  if [[ -n "$owner_pid" ]]; then
    quit_owner "$profile" "$dir" "$owner_pid"
    owner_pid=""
  fi
  build_scene "$dir"
}

# --- рецепт ---

# План рецепта. count <рецепт> — число шагов; window <рецепт> <W> <H> — размер окна рецепта или W H по умолчанию;
# step <рецепт> <query.layout> <N> <рабочая область> — вызовы shellioctl шага N по одному на строку (без --all)
# либо строка «error: …»; press <рецепт> <query.layout> — нажатие последнего шага строкой «шаг зона часть
# номер-плитки число-плиток» (номер плитки с нуля, как в описании строки вкладок), пусто — нажатия нет;
# verify <рецепт> <query.layout> — невыполненные постусловия по одному на строку. Левая зона сцены — колонка
# навигатора (часть 1) и части левой плитки (части 2 и дальше).
recipe_plan() {
  /usr/bin/perl -Mutf8 -MJSON::PP -e '
    my ($mode, $recipe_file, @rest) = @ARGV;
    sub load { open my $fh, "<", $_[0] or die "$_[0]: $!\n"; local $/; JSON::PP->new->utf8->decode(<$fh>) }
    my $recipe = load($recipe_file);
    my @steps = @{ $recipe->{steps} // [] };
    binmode STDOUT, ":utf8";
    if ($mode eq "count") { print scalar(@steps), "\n"; exit 0 }
    # Текст шага N: keymap.toml шага keymap, текст фильтра шага filter или запроса шага query, без перевода строки в конце.
    if ($mode eq "text") {
      my $s = $steps[ $rest[0] - 1 ] or die "в рецепте нет шага $rest[0]\n";
      defined $s->{text} or die "у шага $rest[0] нет текста\n";
      print $s->{text};
      exit 0;
    }
    # Виртуальные коды клавиш раскладки ANSI и модификаторы System Events.
    my %MOD = (Cmd => "command", Ctrl => "control", Alt => "option", Shift => "shift");
    my %CODE = (
      a => 0, s => 1, d => 2, f => 3, h => 4, g => 5, z => 6, x => 7, c => 8, v => 9, b => 11, q => 12, w => 13,
      e => 14, r => 15, y => 16, t => 17, 1 => 18, 2 => 19, 3 => 20, 4 => 21, 6 => 22, 5 => 23, "=" => 24, 9 => 25,
      7 => 26, "-" => 27, 8 => 28, 0 => 29, "]" => 30, o => 31, u => 32, "[" => 33, i => 34, p => 35, Enter => 36,
      l => 37, j => 38, "\x27" => 39, k => 40, ";" => 41, "\\" => 42, "," => 43, "/" => 44, n => 45, m => 46,
      "." => 47, Tab => 48, Space => 49, "`" => 50, Backspace => 51, Esc => 53, Delete => 117, Left => 123,
      Right => 124, Down => 125, Up => 126,
    );
    my %SHIFTED = (
      "+" => 24, "_" => 27, "}" => 30, "{" => 33, "\x22" => 39, ":" => 41, "|" => 42, "<" => 43, "?" => 44,
      ">" => 47, "~" => 50,
    );
    # Набор шага hints рецепта: знаки после клавиши входа f, пустая строка — ничего не набрано.
    if ($mode eq "typed") {
      my ($s) = grep { $_->{op} eq "hints" } @steps;
      print(($s ? $s->{typed} // "" : "") . "\n");
      exit 0;
    }
    if ($mode eq "window") {
      my $w = $recipe->{window} // {};
      print(($w->{width} // $rest[0]) . " " . ($w->{height} // $rest[1]) . "\n");
      exit 0;
    }
    my $value = load($rest[0])->{ok}{value};
    my $layout = $value->{layout};
    my $hidden = $layout->{hidden} // {};
    my %plugin = map { $_->{id} => $_->{plugin} } @{ $value->{panels} };
    # Части зоны по счёту сцены; visible — только показанные.
    sub parts {
      my ($zone, $visible) = @_;
      my @parts;
      if ($zone eq "left") {
        push @parts, { tabs => [ $layout->{navigator} ], active => 0 }
          if defined $layout->{navigator} && !($visible && $hidden->{navigator});
        push @parts, @{ $layout->{zones}{left}{parts} } unless $visible && $hidden->{plate};
      } elsif ($zone eq "center" || !($visible && $hidden->{$zone})) {
        @parts = @{ $layout->{zones}{$zone}{parts} };
      }
      return @parts;
    }
    sub fail { print "error: $_[0]\n"; exit 0 }
    my %press = map { $_ => 1 } qw(split_refused open_menu open_picker);
    if ($mode eq "press") {
      my $s = $steps[-1];
      exit 0 unless $s && $press{ $s->{op} };
      my $zone = $s->{zone};
      my @plates = @{ $layout->{zones}{$zone}{parts} };
      my $index = $s->{part} - 1 - ($zone eq "left" ? 1 : 0);
      fail("у части $s->{part} зоны $zone нет строки вкладок") if $index < 0 || $index > $#plates;
      print "$s->{op} $zone $s->{part} $index " . scalar(@plates) . "\n";
      exit 0;
    }
    if ($mode eq "step") {
      my ($n, $wb) = @rest[1, 2];
      my $s = $steps[$n - 1] or fail("в рецепте нет шага $n");
      my ($op, $zone) = ($s->{op}, $s->{zone});
      fail("шаг $op — нажатие в окне, в рецепте он должен быть последним") if $press{$op} && $n != @steps;
      if ($op eq "key") {
        my @names = split / /, $s->{keys} // "";
        for my $at (0 .. $#names) {
          my $name = $names[$at];
          my ($rest, @mods) = ($name);
          while ($rest =~ /^(Cmd|Ctrl|Alt|Shift)-(.+)$/) { push @mods, $MOD{$1}; $rest = $2 }
          my $code = $CODE{$rest};
          if (!defined $code && defined $SHIFTED{$rest}) { $code = $SHIFTED{$rest}; push @mods, "shift" }
          if (!defined $code && $rest =~ /^[A-Z]$/) { $code = $CODE{ lc $rest }; push @mods, "shift" }
          fail("аккорд $name не разобран") unless defined $code;
          my %seen;
          @mods = grep { !$seen{$_}++ } @mods;
          # Действие исполняет только последний аккорд шага: прежние — набранный префикс.
          print "!key $code " . (@mods ? join(",", @mods) : "-") . " $name " . ($at == $#names ? "last" : "prefix") . "\n";
        }
        exit 0;
      }
      if ($op eq "hints") {
        my $typed = $s->{typed} // "";
        fail("набор меток $typed — не знаки алфавита asdfjklgh") unless $typed =~ /^[asdfjklgh]*$/;
        print "!hints " . join(" ", map { $CODE{$_} } split //, $typed) . "\n";
        exit 0;
      }
      if ($op eq "keymap") { print "!keymap\n"; exit 0 }
      if ($op eq "filter") { print "!filter\n"; exit 0 }
      if ($op eq "query") { print "!query\n"; exit 0 }
      if ($op eq "hide_zone" || $op eq "show_zone") {
        my $verb = $op eq "hide_zone" ? "hide" : "show";
        fail("центр не скрывается") if $zone eq "center";
        if ($zone eq "left") {
          print "layout $verb navigator --workbench $wb\n";
          print "layout $verb plate --workbench $wb\n" if @{ $layout->{zones}{left}{parts} };
        } else {
          print "layout $verb $zone --workbench $wb\n";
        }
        exit 0;
      }
      my @parts = parts($zone, 0);
      my $part = $parts[ $s->{part} - 1 ] or fail("в зоне $zone нет части $s->{part}");
      my $active = $part->{tabs}[ $part->{active} ];
      my ($tab) = grep { ($plugin{$_} // "") eq ($s->{plugin} // "") } @{ $part->{tabs} };
      if ($op eq "split") { print "layout split $active --open $s->{plugin}\n" }
      elsif ($op eq "zoom") { print "layout zoom $active\n" }
      # Нажатие идёт в части с фокусом: штатным CLI шаг только переводит туда фокус.
      elsif ($op eq "focus" || $press{$op}) { print "panel focus $active\n" }
      elsif ($op eq "focus_field") { print "panel focus $active\n!field $active\n" }
      elsif ($op eq "move" || $op eq "close_tab") {
        fail("в части $s->{part} зоны $zone нет вкладки $s->{plugin}") unless defined $tab;
        if ($op eq "close_tab") { print "panel close $tab\n" }
        elsif ($s->{new_part}) {
          my @to = parts($s->{to}, 0);
          fail("в зоне $s->{to} нет части для края новой части") unless @to;
          print "panel move-part $tab --to $to[-1]{tabs}[ $to[-1]{active} ] --edge after\n";
        } else { print "panel move $tab --zone $s->{to}\n" }
      }
      else { fail("шаг $op сценарий не выполняет") }
      exit 0;
    }
    my $post = $recipe->{post} // {};
    my $comp = $post->{composition};
    my @out;
    if (defined $layout->{zoomed}) {
      my @zones = keys %{ $post->{parts} // {} };
      my $zone = $zones[0];
      if (@zones != 1 || $post->{parts}{$zone} != 1) {
        push @out, "панель развёрнута, а постусловие ждёт части зон " . join(", ", sort @zones);
      } else {
        push @out, "развёрнутая панель не в зоне $zone"
          unless grep { grep { $_ eq $layout->{zoomed} } @{ $_->{tabs} } } parts($zone, 0);
        my $tabs = $comp ? $comp->{zones}{$zone}[0]{tabs} : undef;
        push @out, "развёрнута панель " . ($plugin{ $layout->{zoomed} } // "?") . ", постусловие — " . join(", ", @$tabs)
          if $tabs && !(@$tabs == 1 && $tabs->[0] eq ($plugin{ $layout->{zoomed} } // ""));
      }
    } else {
      for my $zone (qw(left center right bottom)) {
        my @have = parts($zone, 1);
        my @want = $comp ? @{ $comp->{zones}{$zone} // [] } : ();
        my $count = $post->{parts}{$zone} // 0;
        # Пустая рабочая область: сцена называет пустую часть, раскладка продукта части не держит.
        next if !@have && $count == 1 && @want == 1 && !@{ $want[0]{tabs} };
        push @out, "частей зоны $zone — " . scalar(@have) . ", постусловие — $count" if @have != $count;
        for my $p (0 .. $#want) {
          last if $p > $#have;
          my $is = join(", ", map { $plugin{$_} // "?" } @{ $have[$p]{tabs} });
          my $exp = join(", ", @{ $want[$p]{tabs} });
          push @out, "вкладки части " . ($p + 1) . " зоны $zone — $is, постусловие — $exp" if $is ne $exp;
          push @out, "активная вкладка части " . ($p + 1) . " зоны $zone — " . ($have[$p]{active} + 1) .
            ", постусловие — $want[$p]{active}"
            if defined $want[$p]{active} && $have[$p]{active} + 1 != $want[$p]{active};
        }
      }
    }
    if ($comp && $comp->{focus}) {
      my $f = $comp->{focus};
      my @have = parts($f->{zone}, 0);
      my $part = $have[ $f->{part} - 1 ];
      push @out, "фокус не в части $f->{part} зоны $f->{zone}"
        unless $part && ($layout->{focus}[-1] // "") eq $part->{tabs}[ $part->{active} ];
    }
    print "$_\n" for @out;
  ' "$@"
}

# Размер окна в ответе ui.window равен W×H в пределах 1 точки.
size_is() {
  awk -v w="$(jv "$1" ok.ui.window.frame.width || echo 0)" -v h="$(jv "$1" ok.ui.window.frame.height || echo 0)" \
    -v W="$2" -v H="$3" 'BEGIN { exit !(w - W <= 1 && W - w <= 1 && h - H <= 1 && H - h <= 1) }'
}

# Окно владельца размера W×H: ui window size, затем опрос ui.window; иначе WHY. AX размер окна не задаёт: шире
# экрана окно через AXSize не становится.
resize_window() {
  local dir="$1" w="$2" h="$3" deadline
  WHY=""
  ui_read "$profile" "$dir/size-before" window || {
    WHY="ui.window не прочитан: $(oneline "$dir/size-before.stdout")"
    return 1
  }
  ! size_is "$dir/size-before.stdout" "$w" "$h" || return 0
  ui_cmd "$profile" "$dir/resize" ui window size "$w" "$h" || return 1
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$profile" "$dir/size-after" window && size_is "$dir/size-after.stdout" "$w" "$h"; then
      return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс после ui window size окно не стало ${w}×${h}: $(jv "$dir/size-after.stdout" ok.ui.window.frame || echo "ui.window не прочитан")"
      return 1
    fi
    sleep "$POLL"
  done
}

# Строка вкладок окна по описанию: обход дерева AX до глубины 12, внутрь групп вкладок не заходит; AXPress кнопки
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
  const hit = found.buttons().map((b) => ({ element: b, description: prop(() => b.description()) || "" }))
    .find((b) => b.description.startsWith(prefix));
  if (!hit) throw new Error("нет кнопки «" + prefix + "» в строке «" + strip + "»");
  hit.element.actions.byName("AXPress").perform();
  return "pressed " + hit.description;
}'

# Описание строки вкладок плитки index (с нуля) зоны zone из count плиток, как его задаёт адаптер.
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

# AXPress кнопки prefix строки вкладок strip окна владельца; иначе WHY.
strip_press() {
  local strip="$1" prefix="$2" out="$3"
  WHY=""
  osascript -l JavaScript -e "$AX_STRIP_JS" "$owner_pid" "$strip" "$prefix" >"$out.stdout" 2>"$out.stderr" || {
    WHY="AXPress кнопки «${prefix}» строки «${strip}» не выполнен: $(oneline "$out.stderr")"
    return 1
  }
}

# Клавиша key code через System Events; журнал — <out>.
se_key() {
  printf '%s key code %s\n' "$(now_ms)" "$1" >>"$2"
  osascript -e "tell application \"System Events\" to key code $1" >>"$2" 2>&1
}

# Аккорд через System Events: код клавиши и модификаторы через запятую, «-» — без них; журнал — <out>.
se_chord() {
  local code="$1" mods="$2" out="$3" using=""
  if [[ "$mods" != - ]]; then
    using=" using {${mods//,/ down, } down}"
  fi
  printf '%s key code %s%s\n' "$(now_ms)" "$code" "$using" >>"$out"
  osascript -e "tell application \"System Events\" to key code $code$using" >>"$out" 2>&1
}

# Шаг рецепта, который не команда ядра: строка плана «!key код модификаторы аккорд», «!keymap», «!field панель»
# «!filter» или «!query». Отказ — причина в WHY и род в STEP_KIND: fail — продукт шаг не исполнил, broken — ошибка
# исполнения.
keyboard_step() {
  local line="$1" recipe="$2" n="$3" dir="$4" prefix="$5" kind code mods name place panel text deadline node before
  STEP_KIND=broken
  WHY=""
  read -r kind code mods name place <<<"$line"
  case "$kind" in
    '!key')
      front_ok "$owner_pid" "$dir" || return 1
      before=""
      if [[ "$place" == last ]]; then
        ui_read "$profile" "$prefix.receipts-before" receipts &&
          before="$(jv "$prefix.receipts-before.stdout" ok.ui.receipts.last || true)"
        [[ "$before" =~ ^[0-9]+$ ]] || {
          WHY="ui.receipts перед аккордом $name не прочитан: $(oneline "$prefix.receipts-before.stdout")"
          return 1
        }
      fi
      se_chord "$code" "$mods" "$prefix.keys.log" || {
        WHY="аккорд $name не отправлен: $(oneline "$prefix.keys.log")"
        return 1
      }
      # Исполненное клавишей действие пишет запись кольца квитанций при завершении: следующий шаг читает
      # раскладку после неё. Клавиша, которую маршрутизатор пропустил (Esc в поле), записи не оставляет: её
      # ждут не дольше KEY_GRACE_MS.
      if [[ -n "$before" ]]; then
        deadline=$(($(now_ms) + KEY_GRACE_MS))
        until ui_read "$profile" "$prefix.receipts" receipts &&
          [[ "$(jv "$prefix.receipts.stdout" ok.ui.receipts.last || echo 0)" =~ ^[0-9]+$ ]] &&
          (($(jv "$prefix.receipts.stdout" ok.ui.receipts.last) > before)); do
          (($(now_ms) < deadline)) || break
          sleep "$POLL"
        done
      fi
      ;;
    '!keymap')
      mkdir -p "$profile/config"
      recipe_plan text "$recipe" "$n" >"$profile/config/keymap.toml" 2>"$prefix.stderr" || {
        WHY="текст keymap.toml шага не прочитан: $(oneline "$prefix.stderr")"
        return 1
      }
      cp "$profile/config/keymap.toml" "$prefix.keymap.toml"
      ctl "$profile" "$prefix" --json --timeout "$CTL_TIMEOUT_MS" keymap reload || {
        STEP_KIND=fail
        WHY="keymap reload: $(oneline "$prefix.stdout") $(oneline "$prefix.stderr")"
        return 1
      }
      ;;
    '!field')
      panel="$code"
      node="ok.ui.inspect.components[id=layout.panel.$panel.field].first_responder"
      front_ok "$owner_pid" "$dir" || return 1
      # Панель, взявшая фокус, сама отдаёт его своему полю: Tab нужен только виду, который этого не сделал, а у
      # поля с фокусом он увёл бы фокус дальше.
      deadline=$(($(now_ms) + KEY_GRACE_MS))
      until ui_read "$profile" "$prefix" inspect && [[ "$(jv "$prefix.stdout" "$node" || echo ?)" == true ]]; do
        if (($(now_ms) >= deadline)); then
          se_chord 48 - "$prefix.keys.log" || {
            WHY="Tab не отправлен: $(oneline "$prefix.keys.log")"
            return 1
          }
          break
        fi
        sleep "$POLL"
      done
      deadline=$(($(now_ms) + MIRROR_MS))
      until ui_read "$profile" "$prefix" inspect && [[ "$(jv "$prefix.stdout" "$node" || echo ?)" == true ]]; do
        if (($(now_ms) >= deadline)); then
          STEP_KIND=fail
          WHY="за $MIRROR_MS мс поле панели $panel не стало первым ответчиком по ui.inspect"
          return 1
        fi
        sleep "$POLL"
      done
      ;;
    '!filter')
      text="$(recipe_plan text "$recipe" "$n" 2>"$prefix.stderr")" || {
        WHY="текст фильтра шага не прочитан: $(oneline "$prefix.stderr")"
        return 1
      }
      ui_cmd "$profile" "$prefix" ui filter "$text" || {
        STEP_KIND=fail
        return 1
      }
      ;;
    '!query')
      text="$(recipe_plan text "$recipe" "$n" 2>"$prefix.stderr")" || {
        WHY="текст запроса шага не прочитан: $(oneline "$prefix.stderr")"
        return 1
      }
      ui_cmd "$profile" "$prefix" ui palette open --query "$text" || {
        STEP_KIND=fail
        return 1
      }
      ;;
    *)
      WHY="строка плана шага не разобрана: $line"
      return 1
      ;;
  esac
  STEP_KIND=""
}

# Выход фонового osascript до срока MENU_OSA_S от его старта t0; иначе WHY.
await_bg() {
  local job="$1" t0="$2"
  while alive "$job"; do
    if (($(now_ms) - t0 >= MENU_OSA_S * 1000)); then
      kill -TERM "$job" 2>/dev/null || true
      WHY="фоновый AXPress не вышел за $MENU_OSA_S с"
      return 1
    fi
    sleep "$POLL"
  done
  wait "$job" 2>/dev/null || true
}

# Опрос ui.inspect: меню узла по пути path (jv) открыто или закрыто, как в want; последнее чтение —
# <prefix>.stdout, иначе WHY. С job срок — MENU_OSA_S: фоновый osascript до нажатия обходит дерево AX окна, и
# опрос кончается раньше срока, если он уже вышел.
wait_menu() {
  local prefix="$1" path="$2" want="$3" job="${4:-}" limit="$MIRROR_MS" deadline seen=""
  WHY=""
  [[ -z "$job" ]] || limit=$((MENU_OSA_S * 1000))
  deadline=$(($(now_ms) + limit))
  while :; do
    if ui_read "$profile" "$prefix" inspect; then
      seen="$(jv "$prefix.stdout" "$path.state.menu.open" || echo ?)"
      [[ "$seen" == "$want" ]] && return 0
    fi
    if (($(now_ms) >= deadline)) || { [[ -n "$job" ]] && ! alive "$job"; }; then
      WHY="за $limit мс меню $path open — ${seen:-?}, а не $want"
      return 1
    fi
    sleep "$POLL"
  done
}

# Опрос ui.inspect до видимого сообщения строки статуса; текст — в STATUS_TEXT, последнее чтение — <prefix>.stdout,
# иначе WHY.
wait_status() {
  local prefix="$1" deadline node="ok.ui.inspect.components[id=status.message]"
  WHY=""
  STATUS_TEXT=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$profile" "$prefix" inspect && [[ "$(jv "$prefix.stdout" "$node.visible" || echo ?)" == true ]]; then
      STATUS_TEXT="$(jv "$prefix.stdout" "$node.label" || echo null)"
      [[ -n "$STATUS_TEXT" && "$STATUS_TEXT" != null ]] && return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс строка статуса не показала сообщение"
      return 1
    fi
    sleep "$POLL"
  done
}

PICKER_NODE="ok.ui.inspect.components[id=layout.picker]"

# Нажатие состояния в окне владельца: PRESS_OP на строке вкладок PRESS_STRIP. Отказ — причина в WHY и род в
# STEP_KIND: fail — нажатие прошло, а окно не показало ожидаемого, broken — ошибка AX.
press_state() {
  local dir="$1" t0
  STEP_KIND=broken
  front_ok "$owner_pid" "$dir" || return 1
  case "$PRESS_OP" in
    split_refused)
      strip_press "$PRESS_STRIP" "Разделить" "$dir/press" || return 1
      wait_status "$dir/status" || {
        STEP_KIND=fail
        WHY="«Разделить» строки «${PRESS_STRIP}»: $WHY"
        return 1
      }
      printf '%s\n' "$STATUS_TEXT" >"$dir/status.txt"
      ;;
    open_menu)
      t0="$(now_ms)"
      osascript -l JavaScript -e "$AX_STRIP_JS" "$owner_pid" "$PRESS_STRIP" "Действия панели" \
        >"$dir/press.stdout" 2>"$dir/press.stderr" &
      bg_job=$!
      bg_jobs+=("$bg_job")
      wait_menu "$dir/menu" "ok.ui.inspect.components[ck=$PRESS_MENU_CK]" true "$bg_job" || {
        # Вышедший AXPress без открытого меню — отказ AX, живой — меню не открылось.
        alive "$bg_job" && STEP_KIND=fail
        WHY="«Действия панели» строки «${PRESS_STRIP}»: $WHY $(oneline "$dir/press.stderr")"
        se_key "$KEY_ESC" "$dir/keys.log" || true
        await_bg "$bg_job" "$t0" || true
        return 1
      }
      ;;
    open_picker)
      strip_press "$PRESS_STRIP" "Разделить" "$dir/press" || return 1
      wait_menu "$dir/menu" "$PICKER_NODE" true || {
        STEP_KIND=fail
        WHY="«Разделить» строки «${PRESS_STRIP}»: выбор «Открыть в новой части» не открылся: $WHY"
        return 1
      }
      ;;
  esac
}

# Меню или выбор, открытые нажатием состояния, закрывает Escape; иначе WHY.
release_state() {
  local dir="$1" t0
  case "$PRESS_OP" in
    open_menu)
      t0="$(now_ms)"
      se_key "$KEY_ESC" "$dir/keys.log" || {
        WHY="Escape не отправлен: $(oneline "$dir/keys.log")"
        return 1
      }
      await_bg "$bg_job" "$t0" || return 1
      wait_menu "$dir/menu-closed" "ok.ui.inspect.components[ck=$PRESS_MENU_CK]" false
      ;;
    open_picker)
      se_key "$KEY_ESC" "$dir/keys.log" || {
        WHY="Escape не отправлен: $(oneline "$dir/keys.log")"
        return 1
      }
      wait_menu "$dir/menu-closed" "$PICKER_NODE" false
      ;;
  esac
}

# --- снимок композиции палитры ---

# Окна экрана спереди назад: CGWindowListCopyWindowInfo, только показанные, без рабочего стола.
LIST_WINDOWS_JS='ObjC.import("CoreGraphics");
function run() {
  const all = ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo(1 | 16, 0)));
  return JSON.stringify(all.map((w) => ({ kCGWindowNumber: w.kCGWindowNumber, kCGWindowOwnerPID: w.kCGWindowOwnerPID,
    kCGWindowLayer: w.kCGWindowLayer, kCGWindowBounds: w.kCGWindowBounds })));
}'

# Поверхность состояния по реестру.
state_surface() {
  awk -v id="$1" '
    /^\[\[/ { cur = "" }
    $1 == "id" { v = $3; gsub(/"/, "", v); cur = v }
    $1 == "surface" && cur == id { v = $3; gsub(/"/, "", v); print v; exit }
  ' "$registry"
}

# Пара окон владельца перед снимком: ui.window в <prefix>.window.stdout, список окон экрана и оракул pair с
# ожиданием open или closed. Код оракула — в PAIR_CODE (1 — снимок не годится, 3 — дефект палитры), причина — в WHY.
PAIR_CODE=""
palette_pair() {
  local prefix="$1" expect="$2" code=0
  PAIR_CODE=1
  WHY=""
  ui_read "$profile" "$prefix.window" window || {
    WHY="ui.window не прочитан: $(oneline "$prefix.window.stdout")"
    return 1
  }
  osascript -l JavaScript -e "$LIST_WINDOWS_JS" >"$prefix.listed.json" 2>"$prefix.listed.stderr" || {
    WHY="список окон экрана не прочитан: $(oneline "$prefix.listed.stderr")"
    return 1
  }
  "$oracle_bin" pair --window "$prefix.window.stdout" --listed "$prefix.listed.json" --palette "$expect" \
    --scale "$SCALE" >"$prefix.pair.stdout" 2>"$prefix.pair.stderr" || code=$?
  PAIR_CODE=$code
  if [[ $code -ne 0 ]]; then
    WHY="пара окон ($expect), код $code: $(oneline "$prefix.pair.stdout") $(oneline "$prefix.pair.stderr")"
    return 1
  fi
}

# Снимок области окна владельца (кадр ui.window <window>) screencapture -R в sRGB: <cur>.png; сбой — fatal.
region_capture() {
  local what="$1" cur="$2" window="$3" region
  region="$("$oracle_bin" region --window "$window" 2>"$cur.region.stderr")" ||
    fatal "$what: область снимка не построена: $(oneline "$cur.region.stderr")"
  /usr/sbin/screencapture -x -R "$region" "$cur.raw.png" >"$cur.capture.log" 2>&1 ||
    fatal "$what: screencapture -R $region отказал: $(oneline "$cur.capture.log")"
  [[ -s "$cur.raw.png" ]] || fatal "$what: screencapture -R $region не записал PNG"
  sips --matchTo "$SRGB_PROFILE" "$cur.raw.png" --out "$cur.png" >"$cur.sips.log" 2>&1 ||
    fatal "$what: sips не перевёл PNG в sRGB: $(oneline "$cur.sips.log")"
}

# Подпись палитры в ui.overlay: вид наложения, запрос, категория и выбранная строка.
overlay_sig() {
  printf '%s|%s|%s|%s' "$(jv "$1" ok.ui.overlay.kind || echo -)" "$(jv "$1" ok.ui.overlay.palette.query || echo -)" \
    "$(jv "$1" ok.ui.overlay.palette.category || echo -)" "$(jv "$1" ok.ui.overlay.palette.selected || echo -)"
}

# Опрос ui.overlay до подписи, отличной от before, не дольше MIRROR_MS; подпись — в SIG, иначе WHY.
SIG=""
overlay_changed() {
  local prefix="$1" before="$2" deadline
  SIG=""
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$profile" "$prefix" overlay; then
      SIG="$(overlay_sig "$prefix.stdout")"
      [[ "$SIG" != "$before" ]] && return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $MIRROR_MS мс ui.overlay не изменился: ${SIG:-не прочитан}"
      return 1
    fi
    sleep "$POLL"
  done
}

# Основа контрольных пикселей: палитра закрыта по ui.overlay и паре окон, два совпавших снимка области подряд —
# <dir>/base.png. Отказ — причина в WHY и род в STEP_KIND: fail — палитра видима, broken — ошибка исполнения.
palette_base() {
  local state="$1" cell="$2" dir="$3" n=0 cur prev="" deadline
  STEP_KIND=broken
  ui_read "$profile" "$dir/closed-overlay" overlay || {
    WHY="ui.overlay не прочитан: $(oneline "$dir/closed-overlay.stdout")"
    return 1
  }
  if [[ "$(jv "$dir/closed-overlay.stdout" ok.ui.overlay.kind || echo ?)" != null ]]; then
    STEP_KIND=fail
    WHY="до открытия палитры ui.overlay — $(jv "$dir/closed-overlay.stdout" ok.ui.overlay.kind || echo ?)"
    return 1
  fi
  deadline=$(($(now_ms) + SETTLE_MS))
  while :; do
    n=$((n + 1))
    cur="$dir/base-$n"
    if ! palette_pair "$cur" closed; then
      [[ "$PAIR_CODE" == 3 ]] && STEP_KIND=fail
      return 1
    fi
    region_capture "$state $cell: основа" "$cur" "$cur.window.stdout"
    if [[ -n "$prev" ]] && cmp -s "$prev.png" "$cur.png"; then
      cp "$cur.png" "$dir/base.png"
      return 0
    fi
    if (($(now_ms) >= deadline)); then
      WHY="за $SETTLE_MS мс два снимка области до открытия палитры подряд не совпали ($n снимк.)"
      return 1
    fi
    prev="$cur"
  done
}

# Открытие палитры шагами рецепта, отложенными prepare_state (<recipe dir>/palette.steps: номер шага и строка
# плана). Свидетель каждого шага — новая подпись ui.overlay, после шагов наложение — палитра. Отказ — WHY и
# STEP_KIND, как у keyboard_step.
palette_open() {
  local dir="$1" recipe="$2" steps="$3" n line m=0 before
  while IFS=$'\t' read -r n line <&4; do
    m=$((m + 1))
    STEP_KIND=broken
    ui_read "$profile" "$dir/open-$m.before" overlay || {
      WHY="ui.overlay перед шагом $n не прочитан: $(oneline "$dir/open-$m.before.stdout")"
      return 1
    }
    before="$(overlay_sig "$dir/open-$m.before.stdout")"
    keyboard_step "$line" "$recipe" "$n" "$dir" "$dir/open-$m" || {
      WHY="шаг $n рецепта: $WHY"
      return 1
    }
    overlay_changed "$dir/open-$m.overlay" "$before" || {
      STEP_KIND=fail
      WHY="шаг $n рецепта (${line#\!}): $WHY"
      return 1
    }
  done 4<"$steps"
  if [[ "${SIG%%|*}" != palette ]]; then
    STEP_KIND=fail
    WHY="после шагов рецепта ui.overlay — ${SIG%%|*}, а не палитра"
    return 1
  fi
}

# Закрытие палитры после снимка: Escape, ui.overlay без наложения и пара окон без палитры. Отказ — WHY и
# STEP_KIND.
palette_close() {
  local dir="$1" before
  STEP_KIND=broken
  ui_read "$profile" "$dir/close-before" overlay || {
    WHY="ui.overlay перед закрытием не прочитан: $(oneline "$dir/close-before.stdout")"
    return 1
  }
  before="$(overlay_sig "$dir/close-before.stdout")"
  front_ok "$owner_pid" "$dir" || return 1
  se_key "$KEY_ESC" "$dir/close-keys.log" || {
    WHY="Escape не отправлен: $(oneline "$dir/close-keys.log")"
    return 1
  }
  if ! overlay_changed "$dir/close-overlay" "$before" || [[ "${SIG%%|*}" != null ]]; then
    STEP_KIND=fail
    WHY="Escape не закрыл палитру: ${WHY:-ui.overlay — ${SIG%%|*}}"
    return 1
  fi
  if ! palette_pair "$dir/close" closed; then
    [[ "$PAIR_CODE" == 3 ]] && STEP_KIND=fail
    return 1
  fi
}

# Подсказка which-key набранного префикса в ui.overlay: чтение — <prefix>.stdout, префикс — канонический JSON
# want. Нет подсказки или префикс другой — 1 и причина в WHY.
which_shown() {
  local prefix="$1" want="$2" seen
  WHY=""
  ui_read "$profile" "$prefix" overlay || {
    WHY="ui.overlay не прочитан: $(oneline "$prefix.stdout")"
    return 1
  }
  seen="$(jv "$prefix.stdout" ok.ui.overlay.which_key.prefix || echo нет)"
  if [[ "$seen" != "$want" ]]; then
    WHY="ui.overlay — which_key.prefix $seen, а не $want"
    return 1
  fi
}

# Префикс which-key отложенными шагами key рецепта (<recipe dir>/which.steps: номер шага и строка плана). До
# первого шага ui.overlay без наложения и без подсказки, свидетель последнего — which_key.prefix из аккордов шагов
# не дольше MIRROR_MS. Префикс — в WHICH_PREFIX. Отказ — WHY и STEP_KIND, как у keyboard_step.
WHICH_PREFIX=""
which_open() {
  local dir="$1" steps="$2" n line m=0 kind code mods name place chords=() deadline
  STEP_KIND=broken
  WHICH_PREFIX=""
  ui_read "$profile" "$dir/which-before" overlay || {
    WHY="ui.overlay перед префиксом не прочитан: $(oneline "$dir/which-before.stdout")"
    return 1
  }
  if [[ "$(jv "$dir/which-before.stdout" ok.ui.overlay.kind || echo ?)" != null ]] ||
    [[ "$(jv "$dir/which-before.stdout" ok.ui.overlay.which_key || echo null)" != null ]]; then
    STEP_KIND=fail
    WHY="до префикса ui.overlay — $(oneline "$dir/which-before.stdout")"
    return 1
  fi
  while IFS=$'\t' read -r n line <&4; do
    m=$((m + 1))
    read -r kind code mods name place <<<"$line"
    chords+=("$name")
    front_ok "$owner_pid" "$dir" || return 1
    se_chord "$code" "$mods" "$dir/which-$m.keys.log" || {
      WHY="шаг $n рецепта: аккорд $name не отправлен: $(oneline "$dir/which-$m.keys.log")"
      return 1
    }
  done 4<"$steps"
  ((${#chords[@]})) || {
    WHY="в рецепте нет шага key"
    return 1
  }
  WHICH_PREFIX="$(printf '%s\n' "${chords[@]}" | /usr/bin/perl -MJSON::PP -e 'chomp(my @c = <STDIN>); print JSON::PP->new->canonical->encode(\@c)')"
  deadline=$(($(now_ms) + MIRROR_MS))
  until which_shown "$dir/which-shown" "$WHICH_PREFIX"; do
    if (($(now_ms) >= deadline)); then
      STEP_KIND=fail
      WHY="за $MIRROR_MS мс после префикса ${chords[*]} подсказка не появилась: $WHY"
      return 1
    fi
    sleep "$POLL"
  done
}

# Снятие префикса после снимка: Escape, ui.overlay без which_key не дольше MIRROR_MS. Отказ — WHY и STEP_KIND.
which_close() {
  local dir="$1" deadline
  STEP_KIND=broken
  front_ok "$owner_pid" "$dir" || return 1
  se_key "$KEY_ESC" "$dir/which-close-keys.log" || {
    WHY="Escape не отправлен: $(oneline "$dir/which-close-keys.log")"
    return 1
  }
  deadline=$(($(now_ms) + MIRROR_MS))
  while :; do
    if ui_read "$profile" "$dir/which-closed" overlay &&
      [[ "$(jv "$dir/which-closed.stdout" ok.ui.overlay.which_key || echo null)" == null ]] &&
      [[ "$(jv "$dir/which-closed.stdout" ok.ui.overlay.kind || echo ?)" == null ]]; then
      return 0
    fi
    if (($(now_ms) >= deadline)); then
      STEP_KIND=fail
      WHY="за $MIRROR_MS мс Escape не снял префикс: ui.overlay — $(oneline "$dir/which-closed.stdout")"
      return 1
    fi
    sleep "$POLL"
  done
}

# Ожидаемые метки состояния: пары «текст, ключ цели» по порядку целей одной строкой JSON. Текст — typed и remaining
# узлов hints.item.{n} паспорта по номеру цели, цель номера n — n-я цель композиции паспорта в порядке прототипа:
# кнопки заголовка, части зон левой, центральной, нижней и правой (вкладки, затем кнопки строки вкладок), строка
# статуса. Два источника независимы от продукта: номер метки и её цель сводит только порядок.
hints_expect() {
  /usr/bin/perl -MJSON::PP -e '
    open my $fh, "<", $ARGV[0] or die "$ARGV[0]: $!\n";
    local $/;
    my $p = JSON::PP->new->utf8->decode(<$fh>);
    my $c = $p->{composition};
    my @targets = @{ $c->{titlebar} // [] };
    for my $zone (qw(left center bottom right)) {
      my $n = 0;
      for my $part (@{ $c->{zones}{$zone} // [] }) {
        $n++;
        push @targets, map { "zone.$zone.part.$n.tabs.tab.$_" } 1 .. scalar @{ $part->{tabs} // [] };
        push @targets, map { "zone.$zone.part.$n.tabs.$_" } @{ $part->{tools} // [] };
      }
    }
    push @targets, @{ $c->{statusline} // [] };
    my (%text, @order);
    for my $e (@{ $p->{elements} }) {
      next unless $e->{ck} =~ /^hints\.item\.(\d+)(?:\.(typed|remaining))?$/;
      push @order, $1 unless defined $2;
      $text{$1}{$2} = $e->{text} if defined $2;
    }
    my @out;
    for my $n (sort { $a <=> $b } @order) {
      die "у метки $n нет цели в композиции паспорта\n" if $n > @targets;
      push @out, [ ($text{$n}{typed} // "") . ($text{$n}{remaining} // ""), $targets[ $n - 1 ] ];
    }
    print JSON::PP->new->canonical->encode(\@out), "\n";
  ' "$1"
}

# Метки в ui.overlay: чтение — <prefix>.stdout, набор — typed. Нет меток или набор другой — 1 и причина в WHY.
hints_shown() {
  local prefix="$1" typed="$2" seen
  WHY=""
  ui_read "$profile" "$prefix" overlay || {
    WHY="ui.overlay не прочитан: $(oneline "$prefix.stdout")"
    return 1
  }
  seen="$(jv "$prefix.stdout" ok.ui.overlay.kind || echo ?)"
  if [[ "$seen" != hints ]]; then
    WHY="ui.overlay — вид $seen, а не hints"
    return 1
  fi
  seen="$(jv "$prefix.stdout" ok.ui.overlay.hints.typed || echo ?)"
  if [[ "$seen" != "$typed" ]]; then
    WHY="ui.overlay — набор «$seen», а не «$typed»"
    return 1
  fi
}

# Метки отложенным шагом hints рецепта (HINTS_STEP: «!hints коды набора»): до клавиши f ui.overlay без наложения,
# после неё — метки с пустым набором, после каждого знака — набор до этого знака, каждое не дольше MIRROR_MS.
# Затем состав меток (текст и цель по порядку) сверяется с ожиданием паспорта <passport>. Отказ — WHY и STEP_KIND.
hints_open() {
  local dir="$1" recipe="$2" passport="$3" typed want="" code m=0 deadline seen expect
  STEP_KIND=broken
  typed="$(recipe_plan typed "$recipe")" || {
    WHY="набор шага hints не прочитан"
    return 1
  }
  ui_read "$profile" "$dir/hints-before" overlay || {
    WHY="ui.overlay перед метками не прочитан: $(oneline "$dir/hints-before.stdout")"
    return 1
  }
  if [[ "$(jv "$dir/hints-before.stdout" ok.ui.overlay.kind || echo ?)" != null ]]; then
    STEP_KIND=fail
    WHY="до меток ui.overlay — $(oneline "$dir/hints-before.stdout")"
    return 1
  fi
  # Клавиша входа f, затем знаки набора: код каждого — из плана шага.
  for code in 3 ${HINTS_STEP#!hints}; do
    m=$((m + 1))
    ((m == 1)) || want="${typed:0:m-1}"
    front_ok "$owner_pid" "$dir" || return 1
    se_key "$code" "$dir/hints-$m.keys.log" || {
      WHY="клавиша $m меток не отправлена: $(oneline "$dir/hints-$m.keys.log")"
      return 1
    }
    deadline=$(($(now_ms) + MIRROR_MS))
    until hints_shown "$dir/hints-$m" "$want"; do
      if (($(now_ms) >= deadline)); then
        STEP_KIND=fail
        if ((m == 1)); then
          WHY="за $MIRROR_MS мс после f метки не открылись: $WHY"
        else
          WHY="за $MIRROR_MS мс после набора «$want» метки его не показали: $WHY"
        fi
        return 1
      fi
      sleep "$POLL"
    done
  done
  expect="$(hints_expect "$passport" 2>"$dir/hints-expect.stderr")" || {
    WHY="ожидание меток по паспорту не построено: $(oneline "$dir/hints-expect.stderr")"
    return 1
  }
  printf '%s\n' "$expect" >"$dir/hints-expect.json"
  seen="$(/usr/bin/perl -MJSON::PP -e '
    open my $fh, "<", $ARGV[0] or exit 2;
    local $/;
    my $o = JSON::PP->new->utf8->decode(<$fh>)->{ok}{ui}{overlay}{hints}{labels} // [];
    print JSON::PP->new->canonical->encode([ map { [ $_->{text}, $_->{target} ] } @$o ]), "\n";
  ' "$dir/hints-$m.stdout")" || {
    WHY="метки ui.overlay не разобраны"
    return 1
  }
  if [[ "$seen" != "$expect" ]]; then
    STEP_KIND=fail
    WHY="метки ui.overlay $(printf '%s' "$seen" | cut -c 1-300) против паспорта $(printf '%s' "$expect" | cut -c 1-300)"
    return 1
  fi
  HINTS_TYPED="$want"
}

# Отмена меток после снимка: Escape, ui.overlay без наложения не дольше MIRROR_MS. Отказ — WHY и STEP_KIND.
hints_close() {
  local dir="$1" deadline
  STEP_KIND=broken
  front_ok "$owner_pid" "$dir" || return 1
  se_key "$KEY_ESC" "$dir/hints-close-keys.log" || {
    WHY="Escape не отправлен: $(oneline "$dir/hints-close-keys.log")"
    return 1
  }
  deadline=$(($(now_ms) + MIRROR_MS))
  until ui_read "$profile" "$dir/hints-closed" overlay &&
    [[ "$(jv "$dir/hints-closed.stdout" ok.ui.overlay.kind || echo ?)" == null ]]; do
    if (($(now_ms) >= deadline)); then
      STEP_KIND=fail
      WHY="за $MIRROR_MS мс Escape не закрыл метки: ui.overlay — $(oneline "$dir/hints-closed.stdout")"
      return 1
    fi
    sleep "$POLL"
  done
}

# Сверка паспорта палитры по снимку композиции: перед каждым снимком пара окон (палитра над владельцем), снимок
# области окна владельца, conform над парой снимков подряд, затем controls принятого снимка против основы
# <dir>/base.png. Итог — snap_code и snap_reason, как у conform_snapshot; код 3 controls — дефект продукта, его
# причина — в CONTROLS_FAIL.
CONTROLS_FAIL=""
palette_snapshot() {
  local state="$1" cell="$2" dir="$3" t0 n=0 code prev="" elapsed cur
  snap_code=""
  snap_reason=""
  CONTROLS_FAIL=""
  t0="$(now_ms)"
  while :; do
    n=$((n + 1))
    cur="$dir/$n"
    if ! palette_pair "$cur" open; then
      snap_code="$PAIR_CODE"
      snap_reason="$WHY"
      log "$state $cell: $snap_reason"
      return 0
    fi
    ui_read "$profile" "$cur.inspect" inspect ||
      fatal "$state $cell: ui.inspect не прочитан: $(oneline "$cur.inspect.stdout")"
    region_capture "$state $cell" "$cur" "$cur.window.stdout"
    if [[ -n "$prev" ]]; then
      elapsed=$(($(now_ms) - t0))
      code=0
      "$oracle_bin" conform --passport "$passports/$state/$cell.json" --inspect "$cur.inspect.stdout" \
        --window "$cur.window.stdout" --png "$cur.png" --registry "$registry" --calibration "$calibration" \
        --previous-window "$prev.window.stdout" --previous-png "$prev.png" --elapsed-ms "$elapsed" \
        >"$cur.report.json" 2>"$cur.oracle.stderr" || code=$?
      case $code in
        0 | 3)
          snap_code=$code
          snap_reason="$(jv "$cur.report.json" reason || oneline "$cur.oracle.stderr")"
          cp "$cur.report.json" "$dir/report.json"
          log "$state $cell: код оракула $code за $n снимк., $snap_reason"
          code=0
          "$oracle_bin" controls --passport "$passports/$state/$cell.json" --window "$cur.window.stdout" \
            --base "$dir/base.png" --png "$cur.png" >"$dir/controls.json" 2>"$dir/controls.stderr" || code=$?
          log "$state $cell: controls — код $code, $(oneline "$dir/controls.json")"
          case $code in
            0) ;;
            3) CONTROLS_FAIL="контрольные пиксели: $(jv "$dir/controls.json" reason || oneline "$dir/controls.stderr")" ;;
            *)
              snap_code=1
              snap_reason="controls — ошибка исполнения (код $code): $(jv "$dir/controls.json" reason || oneline "$dir/controls.stderr")"
              ;;
          esac
          return 0
          ;;
        4)
          if [[ "$(jv "$cur.report.json" retry || echo false)" != true ]]; then
            snap_code=4
            snap_reason="отрисовка не наблюдалась: $(oneline "$cur.oracle.stderr")"
            log "$state $cell: $snap_reason"
            return 0
          fi
          ;;
        *)
          snap_code=$code
          snap_reason="оракул — ошибка исполнения (код $code): $(oneline "$cur.oracle.stderr")"
          log "$state $cell: $snap_reason"
          return 0
          ;;
      esac
    fi
    prev="$cur"
  done
}

# Состояние рецепта на чистой сцене: размер окна рецепта, шаги штатным CLI, постусловия по query.layout. Размер
# окна — в STATE_W и STATE_H, нажатие последнего шага — в PRESS_OP, PRESS_STRIP и PRESS_MENU_CK. Отказ — причина
# в WHY и род в STEP_KIND: fail — продукт отказал в шаге или постусловие не выполнено, broken — ошибка исполнения.
prepare_state() {
  local state="$1" dir="$2" recipe="$root/conformance/recipes/$1.json" count size n m line rev=0 op zone part index
  local plates
  STEP_KIND=broken
  WHY=""
  PRESS_OP=""
  PRESS_STRIP=""
  PRESS_MENU_CK=""
  mkdir -p "$dir"
  [[ -f "$recipe" ]] || {
    WHY="нет рецепта $recipe"
    return 1
  }
  count="$(recipe_plan count "$recipe")" && size="$(recipe_plan window "$recipe" "$WINDOW_W" "$WINDOW_H")" || {
    WHY="рецепт $recipe не прочитан"
    return 1
  }
  STATE_W="${size% *}"
  STATE_H="${size#* }"
  STATE_SURFACE="$(state_surface "$state")"
  PALETTE_STEPS="$dir/palette.steps"
  : >"$PALETTE_STEPS"
  WHICH_STEPS="$dir/which.steps"
  : >"$WHICH_STEPS"
  HINTS_STEP=""
  STATE_WHICH=0
  if [[ " ${WHICH_STATES[*]} " == *" $state "* ]]; then
    STATE_WHICH=1
  fi
  fresh_scene "$dir/scene"
  if [[ "$size" != "$WINDOW_W $WINDOW_H" ]]; then
    scene_clean=0
  fi
  if ! resize_window "$dir" "$STATE_W" "$STATE_H"; then
    WHY="условие пункта: $WHY"
    return 1
  fi
  for ((n = 1; n <= count; n++)); do
    scene_clean=0
    layout_get "$profile" "$dir/step-$n.layout" "$wb" || {
      WHY="шаг $n: query.layout не прочитан: $(oneline "$dir/step-$n.layout.stderr")"
      return 1
    }
    recipe_plan step "$recipe" "$dir/step-$n.layout.stdout" "$n" "$wb" >"$dir/step-$n.plan" || {
      WHY="шаг $n: план шага не построен"
      return 1
    }
    m=0
    while IFS= read -r line <&3; do
      if [[ "$line" == error:* ]]; then
        WHY="шаг $n рецепта: ${line#error: }"
        return 1
      fi
      m=$((m + 1))
      # Палитру открывают шаги клавиш и запроса в каждой ячейке после её осей: повторный Cmd-k при пустом запросе
      # закрыл бы палитру, открытую до перебора ячеек.
      if [[ "$STATE_SURFACE" == palette && ("$line" == '!key '* || "$line" == '!query') ]]; then
        printf '%s\t%s\n' "$n" "$line" >>"$PALETTE_STEPS"
        continue
      fi
      # Префикс which-key набирается в каждой ячейке после её осей: смена ячейки не должна решать, дожил ли он.
      if ((STATE_WHICH)) && [[ "$line" == '!key '* ]]; then
        printf '%s\t%s\n' "$n" "$line" >>"$WHICH_STEPS"
        continue
      fi
      # Метки открываются в каждой ячейке после её осей, как префикс which-key.
      if [[ "$line" == '!hints'* ]]; then
        HINTS_STEP="$line"
        continue
      fi
      if [[ "$line" == '!'* ]]; then
        if ! keyboard_step "$line" "$recipe" "$n" "$dir" "$dir/step-$n-$m"; then
          WHY="шаг $n рецепта: $WHY"
          return 1
        fi
        STEP_KIND=broken
        continue
      fi
      # shellcheck disable=SC2086
      if ! core_cmd "$profile" "$dir/step-$n-$m" $line --all; then
        if [[ "$(jv "$dir/step-$n-$m.stdout" err.code 2>/dev/null || true)" != "" ]]; then
          STEP_KIND=fail
        fi
        WHY="шаг $n рецепта: $WHY"
        return 1
      fi
      rev="$REV"
    done 3<"$dir/step-$n.plan"
  done
  settle_layout "$profile" "$dir/settled" "$wb" "$rev" || return 1
  recipe_plan verify "$recipe" "$dir/settled.layout.stdout" >"$dir/verify" || {
    WHY="постусловия рецепта не сверены"
    return 1
  }
  if [[ -s "$dir/verify" ]]; then
    STEP_KIND=fail
    WHY="постусловие рецепта не выполнено: $(oneline "$dir/verify")"
    return 1
  fi
  recipe_plan press "$recipe" "$dir/settled.layout.stdout" >"$dir/press.plan" || {
    WHY="нажатие рецепта не разобрано"
    return 1
  }
  if [[ -s "$dir/press.plan" ]]; then
    line="$(head -n 1 "$dir/press.plan")"
    if [[ "$line" == error:* ]]; then
      WHY="нажатие рецепта: ${line#error: }"
      return 1
    fi
    read -r op zone part index plates <<<"$line"
    PRESS_OP="$op"
    PRESS_STRIP="$(strip_label "$zone" "$index" "$plates")"
    PRESS_MENU_CK="zone.$zone.part.$part.tabs.menu"
  fi
  log "$state: окно ${STATE_W}×${STATE_H}, шагов рецепта $count, постусловия выполнены${PRESS_OP:+, нажатие $PRESS_OP на строке «${PRESS_STRIP}»}"
}

# --- пункты матрицы ---

# Состояние реестра в четырёх ячейках: сцена по рецепту состояния, затем сверка каждой ячейки.
item_conform() {
  local state="$1" item="conform-$1" dir entry cell lightness contrast_arg contrast broken=""
  failures=()
  log "== $item"
  if [[ ! -f "$calibration" ]]; then
    record "$item" not_run "нет калибровки: нет файла conformance/calibration/calibration.json (make calibrate)"
    return 0
  fi
  if ! prepare_state "$state" "$run/$item/recipe"; then
    if [[ "$STEP_KIND" == fail ]]; then
      record "$item" fail "$WHY"
    else
      add_error "$item: $WHY"
      record "$item" fail "ошибка исполнения пункта: $WHY"
    fi
    return 0
  fi
  for entry in "${CELLS[@]}"; do
    IFS='|' read -r cell lightness contrast_arg contrast <<<"$entry"
    dir="$run/$item/$cell"
    mkdir -p "$dir"
    if [[ ! -f "$passports/$state/$cell.json" ]]; then
      broken="${broken:+$broken; }$cell: нет паспорта $passports/$state/$cell.json"
      continue
    fi
    cp "$passports/$state/$cell.json" "$dir/passport.json"
    if ! set_cell "$dir" "$lightness" "$contrast_arg" "$contrast"; then
      broken="${broken:+$broken; }$cell: $WHY"
      continue
    fi
    if ! window_ok "$dir/axes.stdout" "$STATE_W" "$STATE_H"; then
      broken="${broken:+$broken; }$cell: условие пункта: $WHY"
      continue
    fi
    if [[ -n "$PRESS_OP" ]] && ! press_state "$dir"; then
      # Окно после несостоявшегося нажатия — не состояние рецепта: остальные ячейки не судятся.
      if [[ "$STEP_KIND" == fail ]]; then
        failures+=("$cell: $WHY")
      else
        broken="${broken:+$broken; }$cell: $WHY"
      fi
      break
    fi
    if [[ "$STATE_SURFACE" == palette ]]; then
      # Окно без открытой палитры — не состояние рецепта: остальные ячейки не судятся.
      if ! palette_base "$state" "$cell" "$dir" || ! palette_open "$dir" "$root/conformance/recipes/$state.json" \
        "$PALETTE_STEPS"; then
        if [[ "$STEP_KIND" == fail ]]; then
          failures+=("$cell: $WHY")
        else
          broken="${broken:+$broken; }$cell: $WHY"
        fi
        break
      fi
      palette_snapshot "$state" "$cell" "$dir"
      [[ -z "$CONTROLS_FAIL" ]] || failures+=("$cell: $CONTROLS_FAIL")
    elif [[ -n "$HINTS_STEP" ]]; then
      # Окно без открытых меток с набором рецепта — не состояние рецепта: остальные ячейки не судятся.
      if ! hints_open "$dir" "$root/conformance/recipes/$state.json" "$passports/$state/$cell.json"; then
        if [[ "$STEP_KIND" == fail ]]; then
          failures+=("$cell: $WHY")
        else
          broken="${broken:+$broken; }$cell: $WHY"
        fi
        break
      fi
      conform_snapshot "$state" "$cell" "$dir"
      if ! hints_shown "$dir/hints-after" "$HINTS_TYPED"; then
        broken="${broken:+$broken; }$cell: после снимка $WHY: снимок не годится"
        break
      fi
    elif ((STATE_WHICH)); then
      # Окно без подсказки набранного префикса — не состояние рецепта: остальные ячейки не судятся.
      if ! which_open "$dir" "$WHICH_STEPS"; then
        if [[ "$STEP_KIND" == fail ]]; then
          failures+=("$cell: $WHY")
        else
          broken="${broken:+$broken; }$cell: $WHY"
        fi
        break
      fi
      conform_snapshot "$state" "$cell" "$dir"
      if ! which_shown "$dir/which-after" "$WHICH_PREFIX"; then
        broken="${broken:+$broken; }$cell: после снимка $WHY: снимок не годится"
        break
      fi
    else
      conform_snapshot "$state" "$cell" "$dir"
    fi
    case "$snap_code" in
      0) ;;
      3)
        if [[ -f "$dir/report.json" ]]; then
          failures+=("$cell: $(mismatches_of "$dir/report.json")")
        else
          failures+=("$cell: $snap_reason")
        fi
        ;;
      *) broken="${broken:+$broken; }$cell: $snap_reason" ;;
    esac
    if [[ -n "$HINTS_STEP" ]] && ! hints_close "$dir"; then
      if [[ "$STEP_KIND" == fail ]]; then
        failures+=("$cell: $WHY")
      else
        broken="${broken:+$broken; }$cell: $WHY"
      fi
      break
    fi
    if ((STATE_WHICH)) && ! which_close "$dir"; then
      if [[ "$STEP_KIND" == fail ]]; then
        failures+=("$cell: $WHY")
      else
        broken="${broken:+$broken; }$cell: $WHY"
      fi
      break
    fi
    if [[ "$STATE_SURFACE" == palette ]] && ! palette_close "$dir"; then
      if [[ "$STEP_KIND" == fail ]]; then
        failures+=("$cell: $WHY")
      else
        broken="${broken:+$broken; }$cell: $WHY"
      fi
      break
    fi
    if [[ -n "$PRESS_OP" ]] && ! release_state "$dir"; then
      broken="${broken:+$broken; }$cell: $WHY"
      break
    fi
  done
  if [[ -n "$broken" ]]; then
    add_error "$item: $broken"
    record "$item" fail "ошибка исполнения пункта: $broken"
    return 0
  fi
  conclude "$item" "conform — код 0 в четырёх ячейках, паспорт, чтения, PNG и отчёт — в каталоге ячейки"
}

# ui cell переопределяет контраст и движение в обычном окне и возвращает их системе; поколение растёт.
item_cell_override() {
  local dir="$run/cell-override" gen0 gen1 gen2 seen axis file
  failures=()
  log "== cell-override"
  ui_read "$profile" "$dir/before" window || fatal "cell-override: ui.window не прочитан"
  gen0="$(jv "$dir/before.stdout" ok.ui.window.generation)" || fatal "cell-override: в ui.window нет поколения"
  if cell_cmd "$profile" "$dir/override" --contrast high --motion reduce; then
    gen1="$GEN"
    ((gen1 > gen0)) || failures+=("поколение после переопределения $gen1 не больше прежнего $gen0")
    ui_read "$profile" "$dir/overridden" window || fatal "cell-override: ui.window не прочитан"
    seen="$(jv "$dir/overridden.stdout" ok.ui.window.generation || echo 0)"
    ((seen >= gen1)) || failures+=("ui.window на поколении $seen раньше ответа ui.cell $gen1")
    for file in "$dir/override.stdout:ok.ui.cell" "$dir/overridden.stdout:ok.ui.window"; do
      for axis in "contrast increased" "motion reduced"; do
        [[ "$(jv "${file%%:*}" "${file#*:}.${axis% *}.value" || echo ?)" == "${axis#* }" &&
          "$(jv "${file%%:*}" "${file#*:}.${axis% *}.source" || echo ?)" == override ]] ||
          failures+=("${file#*:}.${axis% *} после переопределения — $(jv "${file%%:*}" "${file#*:}.${axis% *}" || echo нет), а не ${axis#* } с источником override")
      done
    done
    if cell_cmd "$profile" "$dir/system" --contrast system --motion system; then
      gen2="$GEN"
      ((gen2 > gen1)) || failures+=("поколение после возврата $gen2 не больше прежнего $gen1")
      ui_read "$profile" "$dir/returned" window || fatal "cell-override: ui.window не прочитан"
      seen="$(jv "$dir/returned.stdout" ok.ui.window.generation || echo 0)"
      ((seen >= gen2)) || failures+=("ui.window на поколении $seen раньше ответа ui.cell $gen2")
      for file in "$dir/system.stdout:ok.ui.cell" "$dir/returned.stdout:ok.ui.window"; do
        for axis in contrast motion; do
          [[ "$(jv "${file%%:*}" "${file#*:}.$axis.source" || echo ?)" == system ]] ||
            failures+=("${file#*:}.$axis после возврата — $(jv "${file%%:*}" "${file#*:}.$axis" || echo нет), источник не system")
        done
      done
    else
      failures+=("$WHY")
    fi
  else
    failures+=("$WHY")
  fi
  conclude cell-override "ui.cell и ui.window: контраст increased и движение reduced с источником override, затем источник system; поколения $gen0 → ${gen1:-?} → ${gen2:-?}"
}

# Плашка ⌘K нарисована вложенным JetBrains Mono: семейство и файл шрифта из бандла прогона.
item_fonts_bundled() {
  local dir="$run/fonts-bundled" family file fonts
  failures=()
  log "== fonts-bundled"
  ui_read "$profile" "$dir/inspect" inspect || fatal "fonts-bundled: ui.inspect не прочитан"
  family="$(jv "$dir/inspect.stdout" "ok.ui.inspect.components[ck=$KBD_CK].style.font.family" || true)"
  file="$(jv "$dir/inspect.stdout" "ok.ui.inspect.components[ck=$KBD_CK].style.font.file" || true)"
  fonts="file://$app/Contents/Resources/Fonts/"
  [[ "$family" == "$MONO_FAMILY" ]] || failures+=("семейство шрифта $KBD_CK — «${family:-не читается}», а не «$MONO_FAMILY»")
  [[ "$file" == "$fonts"* ]] || failures+=("файл шрифта $KBD_CK — ${file:-не читается}, а не из $fonts")
  conclude fonts-bundled "$KBD_CK: семейство $family, файл $file"
}

# Ключи сверки состояния layout-all-zones: состав сцены найден, у каждого сверяемого узла ключ из таблицы
# ключей. Судит подкоманда coverage оракула по одному чтению ui.inspect: калибровка и снимок ей не нужны,
# ключи от ячейки не зависят.
item_ck_coverage() {
  local state=layout-all-zones dir="$run/ck-coverage" code=0
  failures=()
  log "== ck-coverage"
  if ! prepare_state "$state" "$dir/recipe"; then
    if [[ "$STEP_KIND" == fail ]]; then
      record ck-coverage fail "$WHY"
    else
      add_error "ck-coverage: $WHY"
      record ck-coverage fail "ошибка исполнения пункта: $WHY"
    fi
    return 0
  fi
  cp "$passports/$state/light-normal.json" "$dir/passport.json"
  if ! ui_read "$profile" "$dir/inspect" inspect; then
    add_error "ck-coverage: ui.inspect не прочитан: $(oneline "$dir/inspect.stderr")"
    record ck-coverage fail "ошибка исполнения пункта: ui.inspect не прочитан"
    return 0
  fi
  "$oracle_bin" coverage --passport "$passports/$state/light-normal.json" --inspect "$dir/inspect.stdout" \
    --registry "$registry" >"$dir/report.json" 2>"$dir/oracle.stderr" || code=$?
  case $code in
    0) ;;
    3) failures+=("$(mismatches_of "$dir/report.json")") ;;
    *)
      add_error "ck-coverage: оракул — ошибка исполнения (код $code): $(oneline "$dir/oracle.stderr")"
      record ck-coverage fail "ошибка исполнения пункта: оракул вышел кодом $code"
      return 0
      ;;
  esac
  conclude ck-coverage "coverage — код 0: состав сцены $state найден, у каждого сверяемого узла ключ из таблицы ключей; паспорт, чтение и отчёт — в каталоге пункта"
}

# --- ход сценария ---

prepare
preconditions
build_scene "$run/preconditions/setup" 1
item_cell_override
item_fonts_bundled
item_ck_coverage
# Состояния без шагов рецепта идут первыми на одной сцене, остальные получают новый профиль.
for state in chrome-titlebar chrome-statusline layout-all-zones layout-default layout-left-plate layout-split-right \
  layout-split-center layout-split-bottom layout-zoom tabs-side tabs-center tabs-half tabs-wide layout-stub \
  layout-empty-workbench menu-panel picker-new-part status-split-no-room panel-filter status-mode-nav \
  status-input-field status-wrong-axis status-keymap-error palette-groups palette-tab-narrowed palette-no-results \
  whichkey-leader whichkey-ctrlw hints-full hints-partial; do
  item_conform "$state"
done
if [[ -n "$owner_pid" ]]; then
  mkdir -p "$run/cleanup"
  cell_cmd "$profile" "$run/cleanup/cell" --contrast system --motion system || log "уборка: $WHY"
  quit_owner "$profile" "$run/cleanup" "$owner_pid"
  owner_pid=""
fi
for i in "${!ITEMS[@]}"; do
  if ((TASK[i] > DONE_TASK)); then
    record "${ITEMS[i]}" not_run "пункт задачи ${TASK[i]}"
  fi
done
record voiceover-speech not_run "речь VoiceOver автоматика не наблюдает"
completed=1
