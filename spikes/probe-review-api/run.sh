#!/bin/bash
# Сборка и запуск пробы P0.7 probe-review-api (ADR-0034).
#   run.sh fake [сценарий …]   столбец fake (make probe-review), forge-fake на loopback
#   run.sh live [сценарий …]   живые столбцы (make probe-review-live), HOSTS сужает столбцы
#   run.sh with-lock <команда> служебная: команда под замерной блокировкой с маркером старта
# Пустой список сценариев — все сценарии цели. SCENARIOS (аргументы) и HOSTS сужают оси;
# неизвестное имя или пустое пересечение — отказ до блокировки и сборки.
#
# Весь рецепт идёт под замерной блокировкой ${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}/measure.lock
# (/usr/bin/lockf -k -t 300, план P0.7, «Замерная блокировка»): уборка docker, сборка, процессы,
# агрегирование. Внутренняя команда под блокировкой сначала удаляет маркер старта: маркер на месте
# и код 75 — блокировка не взята («не начат»), маркера нет — код принадлежит рецепту.
#
# Коды выхода: 0 — без ошибок исполнения; 1 — ошибка исполнения; 2 — неверный вызов;
# 4 — «не начат»: замерная блокировка не взята за срок, повтор в следующей итерации.
#
# Каталог прогона target/probes/review-api/<время>-<pid>/: run.json (серия, SHA-256 бинаря, пары,
# уборка docker), каталог процесса <сценарий>-<столбец>/ на пару (result.json, stdout.txt,
# stderr.txt), done.json после последнего процесса, summary.json и summary.md агрегатора.
# SERIES объединяет вызовы в одну серию (по умолчанию — имя каталога прогона); серия собирается
# probe-review --aggregate --pairs "<столбец>:<сценарий> …" --out <каталог> <каталог прогона>….
# Процесс сценария получает временный SHELLIO_STATE_DIR, PROBE_REVIEW_SERIES и PROBE_REVIEW_BUILD.
#
# В начале рецепта docker info со сроком 10 с: доступен — удаляются контейнеры прежних прогонов
# с меткой shellio.probe.run-id, и их не должно остаться; недоступен — уборка «не наблюдалась»
# (not_observed), fake идёт дальше.
#
# Переменные для тестов: PROBE_REVIEW_LOCK_TIMEOUT (срок захвата, с), PROBE_REVIEW_DOCKER (команда
# docker), SHELLIO_CACHE_DIR (каталог блокировки).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SELF="$ROOT/spikes/probe-review-api/run.sh"
CARGO="${CARGO:-cargo}"
PACKAGE=probe-review-api
BIN="$ROOT/target/release/probe-review"
OUT_BASE="$ROOT/target/probes/review-api"

# Сценарии по столбцам; совпадают с probe-review --list (проверяет тест tests/run_sh.rs).
FAKE_HOSTS="fake"
FAKE_SCENARIOS="smoke"
LIVE_HOSTS="github_cloud gitlab_cloud gitlab_ce ghes"
LIVE_SCENARIOS=""

EXIT_ERROR=1
EXIT_USAGE=2
EXIT_NOT_STARTED=4
LOCK="${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}/measure.lock"
LOCK_TIMEOUT="${PROBE_REVIEW_LOCK_TIMEOUT:-300}"
DOCKER="${PROBE_REVIEW_DOCKER:-docker}"
DOCKER_DEADLINE=10
DOCKER_LABEL="shellio.probe.run-id"

die() {
    echo "run.sh: $*" >&2
    exit "$EXIT_ERROR"
}

usage() {
    echo "run.sh: $*" >&2
    echo "использование: run.sh fake [сценарий …] | run.sh live [сценарий …] | run.sh with-lock <команда>" >&2
    exit "$EXIT_USAGE"
}

# Команда со сроком: perl alarm переживает exec, по сроку команда получает SIGALRM.
deadline() {
    local secs="$1"
    shift
    /usr/bin/perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$secs" "$@"
}

contains() {
    case " $1 " in
        *" $2 "*) return 0 ;;
    esac
    return 1
}

# Сужение оси: «select_axis <ось> <фильтр> <известные имена> <что>»; печатает пересечение.
select_axis() {
    local axis="$1" filter="$2" known="$3" what="$4" name selected=""
    for name in $filter; do
        contains "$known" "$name" || usage "неизвестное имя $what «${name}»"
    done
    for name in $axis; do
        if [ -z "$filter" ] || contains "$filter" "$name"; then
            selected="$selected${selected:+ }$name"
        fi
    done
    printf '%s' "$selected"
}

# Команда под замерной блокировкой с маркером старта (как scripts/ghostty/lock.sh).
with_lock() {
    local started status=0
    mkdir -p "$(dirname "$LOCK")" || die "не создан каталог блокировки $(dirname "$LOCK")"
    started="$(mktemp "${TMPDIR:-/tmp}/probe-review-lock.XXXXXX")"
    /usr/bin/lockf -k -t "$LOCK_TIMEOUT" "$LOCK" \
        /bin/sh -c 'rm -f "$1" && shift && exec "$@"' sh "$started" "$@" || status=$?
    if [ -e "$started" ]; then
        rm -f "$started"
        if [ "$status" -eq 75 ]; then
            echo "run.sh: не начат: замерная блокировка $LOCK занята дольше $LOCK_TIMEOUT с, повтор в следующей итерации" >&2
            exit "$EXIT_NOT_STARTED"
        fi
        die "не удалось взять блокировку $LOCK (lockf: код $status)"
    fi
    case "$status" in
        75 | "$EXIT_NOT_STARTED")
            die "рецепт под блокировкой вернул код $status: это ошибка исполнения, а не «не начат»"
            ;;
    esac
    return "$status"
}

json_string() {
    local text="$1"
    [[ ! "$text" =~ [[:cntrl:]] ]] || die "управляющий символ в строке для JSON: $text"
    text="${text//\\/\\\\}"
    text="${text//\"/\\\"}"
    printf '"%s"' "$text"
}

json_words() {
    local word first=1
    printf '['
    for word in $1; do
        [ "$first" -eq 1 ] || printf ', '
        first=0
        json_string "$word"
    done
    printf ']'
}

# Уборка контейнеров прежних прогонов; итог — DOCKER_STATUS, DOCKER_REMOVED, DOCKER_DETAIL.
docker_cleanup() {
    local ids left
    DOCKER_STATUS=not_observed
    DOCKER_REMOVED=0
    DOCKER_DETAIL=""
    if ! command -v "$DOCKER" >/dev/null 2>&1; then
        DOCKER_DETAIL="docker не установлен"
        return 0
    fi
    if ! deadline "$DOCKER_DEADLINE" "$DOCKER" info >/dev/null 2>&1; then
        DOCKER_DETAIL="docker info не ответил за $DOCKER_DEADLINE с"
        return 0
    fi
    ids="$(deadline "$DOCKER_DEADLINE" "$DOCKER" ps -aq --filter "label=$DOCKER_LABEL")" ||
        die "docker ps не перечислил контейнеры с меткой $DOCKER_LABEL"
    if [ -n "$ids" ]; then
        # shellcheck disable=SC2086 # ID контейнеров делятся по пробелам
        deadline 120 "$DOCKER" rm -f -v $ids >/dev/null ||
            die "контейнеры прежних прогонов не удалены: $(echo $ids)"
        DOCKER_REMOVED="$(echo "$ids" | wc -w | tr -d ' ')"
    fi
    left="$(deadline "$DOCKER_DEADLINE" "$DOCKER" ps -aq --filter "label=$DOCKER_LABEL")" ||
        die "docker ps не перечислил контейнеры после уборки"
    [ -z "$left" ] || die "после уборки остались контейнеры с меткой $DOCKER_LABEL: $(echo $left)"
    DOCKER_STATUS=cleaned
}

# Рецепт под блокировкой: «locked <цель> <столбцы> <сценарии>».
locked() {
    local target="$1" hosts="$2" scenarios="$3"
    local run_dir run_name series build pairs="" pair scenario column out state code
    local total=0 failed=0 aggregate=0
    mkdir -p "$OUT_BASE"
    run_name="$(date +%Y%m%d-%H%M%S)-$$"
    run_dir="$OUT_BASE/$run_name"
    mkdir "$run_dir"
    docker_cleanup
    echo "уборка docker: $DOCKER_STATUS${DOCKER_DETAIL:+ ($DOCKER_DETAIL)}, удалено $DOCKER_REMOVED"
    (cd "$ROOT" && "$CARGO" build --release -p "$PACKAGE" --locked --bin probe-review)
    [ -x "$BIN" ] || die "нет бинаря $BIN"
    build="$(shasum -a 256 <"$BIN")" || die "shasum не прочитал $BIN"
    build="${build%% *}"
    series="${SERIES:-$run_name}"
    [[ "$series" =~ ^[A-Za-z0-9._-]+$ ]] || usage "SERIES «${series}»: только буквы, цифры, точка, дефис и подчёркивание"
    for column in $hosts; do
        for scenario in $scenarios; do
            pairs="$pairs${pairs:+ }$column:$scenario"
        done
    done
    {
        printf '{\n'
        printf '  "schema": 1,\n'
        printf '  "series": %s,\n' "$(json_string "$series")"
        printf '  "build": "%s",\n' "$build"
        printf '  "target": "%s",\n' "$target"
        printf '  "started_unix": %s,\n' "$(date +%s)"
        printf '  "pairs": %s,\n' "$(json_words "$pairs")"
        printf '  "skipped": [],\n'
        printf '  "docker": {"status": "%s", "removed": %s, "detail": %s}\n' \
            "$DOCKER_STATUS" "$DOCKER_REMOVED" "$(json_string "$DOCKER_DETAIL")"
        printf '}\n'
    } >"$run_dir/run.json"
    for pair in $pairs; do
        column="${pair%%:*}"
        scenario="${pair#*:}"
        out="$run_dir/$scenario-$column"
        mkdir "$out"
        total=$((total + 1))
        state="$(mktemp -d "${TMPDIR:-/tmp}/probe-review-state.XXXXXX")"
        code=0
        SHELLIO_STATE_DIR="$state" PROBE_REVIEW_SERIES="$series" PROBE_REVIEW_BUILD="$build" \
            "$BIN" --scenario "$scenario" --column "$column" --out "$out" \
            >"$out/stdout.txt" 2>"$out/stderr.txt" || code=$?
        rm -rf "$state"
        if [ "$code" -eq 0 ]; then
            echo "ok      $pair"
        else
            failed=$((failed + 1))
            echo "ошибка  $pair (код $code)"
            sed 's/^/    /' "$out/stdout.txt" "$out/stderr.txt"
        fi
    done
    printf '{\n  "schema": 1,\n  "finished_unix": %s,\n  "errors": %s\n}\n' "$(date +%s)" "$failed" \
        >"$run_dir/done.json"
    echo "прогон: $run_dir"
    echo
    "$BIN" --aggregate --pairs "$pairs" "$run_dir" || aggregate=$?
    [ "$failed" -eq 0 ] || die "ошибки исполнения: $failed из $total"
    [ "$aggregate" -eq 0 ] || die "агрегирование отметило ошибки исполнения (код $aggregate)"
}

# Публичная цель: оси проверяются до блокировки и сборки, рецепт идёт внутренней командой.
cmd_target() {
    local target="$1" hosts_axis scenarios_axis hosts scenarios
    shift
    case "$target" in
        fake) hosts_axis="$FAKE_HOSTS" scenarios_axis="$FAKE_SCENARIOS" ;;
        live) hosts_axis="$LIVE_HOSTS" scenarios_axis="$LIVE_SCENARIOS" ;;
    esac
    [ -z "${SERIES:-}" ] || [[ "$SERIES" =~ ^[A-Za-z0-9._-]+$ ]] ||
        usage "SERIES «${SERIES}»: только буквы, цифры, точка, дефис и подчёркивание"
    hosts="$(select_axis "$hosts_axis" "${HOSTS:-}" "$FAKE_HOSTS $LIVE_HOSTS" "столбца")"
    [ -n "$hosts" ] || usage "пустое пересечение столбцов цели $target ($hosts_axis) и HOSTS «${HOSTS:-}»"
    scenarios="$(select_axis "$scenarios_axis" "$*" "$FAKE_SCENARIOS $LIVE_SCENARIOS" "сценария")"
    [ -n "$scenarios" ] || usage "пустое пересечение сценариев цели $target (${scenarios_axis:-нет}) и SCENARIOS «$*»"
    with_lock "$SELF" locked "$target" "$hosts" "$scenarios"
}

case "${1:-}" in
    fake | live)
        cmd_target "$@"
        ;;
    locked)
        shift
        [ $# -eq 3 ] || usage "locked ждёт цель, столбцы и сценарии"
        locked "$@"
        ;;
    with-lock)
        shift
        [ $# -gt 0 ] || usage "with-lock ждёт команду"
        with_lock "$@"
        ;;
    *)
        usage "неизвестная команда «${1:-}»"
        ;;
esac
