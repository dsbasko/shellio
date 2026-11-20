#!/bin/bash
# Сборка и запуск пробы P0.8 probe-resource-accounting (ADR-0034).
#   run.sh scenarios [сценарий …]      сборка release, затем процесс на сценарий и точку оси с
#                                      временным SHELLIO_STATE_DIR в каталоге прогона
#                                      target/probes/resource-accounting/<run-id>/, затем --aggregate.
#                                      Замерные сценарии идут под замерной блокировкой
#                                      ${SHELLIO_CACHE_DIR:-~/.cache/shellio}/measure.lock (захват до
#                                      5 минут). RUN=<run-id> продолжает прогон по его status: готовые
#                                      процессы пропускаются, а при другой сборке или других
#                                      параметрах заводится новый run-id. POINTS сужает точки осей.
#   run.sh measure-exec <каталог> <команда …>
#                                      команда под замерной блокировкой; печатает lock-busy или
#                                      exit:<код> (проверяет тест логики маркера)
# Код выхода: 0 — все процессы дали итог, 1 — ошибка исполнения, 3 — есть процессы «не начат»:
# повтор следующим вызовом с тем же RUN.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CARGO="${CARGO:-cargo}"
PACKAGE=probe-resource-accounting
BIN="$ROOT/target/release/probe-resources"
OUT_BASE="$ROOT/target/probes/resource-accounting"
CACHE_DIR="${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}"
MEASURE_LOCK="$CACHE_DIR/measure.lock"
LOCK_TIMEOUT="${SHELLIO_MEASURE_LOCK_TIMEOUT:-300}"
# Коды совпадают с runstate::EXIT_NOT_STARTED и runstate::EXIT_BUILD_MISMATCH.
EXIT_NOT_STARTED=3
EXIT_BUILD_MISMATCH=4
MARKER=.measure-lock-start

die() {
    echo "run.sh: $*" >&2
    exit 1
}

# Команда под замерной блокировкой. Код 75 занятой блокировки совпадает с EX_TEMPFAIL самой
# команды, поэтому команда под блокировкой сначала удаляет маркер старта: маркер на месте и код
# 75 — блокировка не взята, маркера нет — код принадлежит команде. Итог — в MEASURE_OUTCOME.
measure_exec() {
    local dir="$1" status=0
    shift
    local marker="$dir/$MARKER"
    mkdir -p "$CACHE_DIR" "$dir"
    : >"$marker"
    /usr/bin/lockf -s -k -t "$LOCK_TIMEOUT" "$MEASURE_LOCK" \
        /bin/sh -c 'rm -f "$1" && shift && exec "$@"' sh "$marker" "$@" || status=$?
    if [ -e "$marker" ]; then
        rm -f "$marker"
        if [ "$status" -eq 75 ]; then
            MEASURE_OUTCOME=lock-busy
            return 0
        fi
        die "не удалось взять замерную блокировку $MEASURE_LOCK (lockf завершился с кодом $status)"
    fi
    MEASURE_OUTCOME="exit:$status"
}

build_info() {
    local commit rustc_bin=rustc
    commit="$(git -C "$ROOT" rev-parse --short=12 HEAD 2>/dev/null || true)"
    if [ -n "$commit" ] && [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
        commit="$commit-dirty"
    fi
    case "$CARGO" in */*) rustc_bin="$(dirname "$CARGO")/rustc" ;; esac
    export SHELLIO_PROBE_COMMIT="$commit"
    SHELLIO_PROBE_RUSTC="$(cd "$ROOT" && "$rustc_bin" -V 2>/dev/null || true)"
    export SHELLIO_PROBE_RUSTC
}

new_run_id() {
    echo "$(date +%Y%m%d-%H%M%S)-$$"
}

# Каталог прогона: новый или продолжение RUN со сверкой сборки и параметров.
prepare_run() {
    local sha="$1" status=0
    RUN_ID="${RUN:-}"
    if [ -z "$RUN_ID" ]; then
        RUN_ID="$(new_run_id)"
    fi
    case "$RUN_ID" in
        */* | . | .. | *[[:space:]]*) die "RUN «$RUN_ID» должен быть именем каталога" ;;
    esac
    RUN_DIR="$OUT_BASE/$RUN_ID"
    mkdir -p "$RUN_DIR"
    "$BIN" --fingerprint "$RUN_DIR" --sha256 "$sha" --profile release || status=$?
    if [ "$status" -eq "$EXIT_BUILD_MISMATCH" ]; then
        RUN_ID="$(new_run_id)"
        RUN_DIR="$OUT_BASE/$RUN_ID"
        echo "run.sh: сборка или параметры расходятся с прогоном ${RUN}, новый run-id $RUN_ID" >&2
        mkdir -p "$RUN_DIR"
        "$BIN" --fingerprint "$RUN_DIR" --sha256 "$sha" --profile release
    elif [ "$status" -ne 0 ]; then
        die "сверка сборки прогона $RUN_DIR завершилась с кодом $status"
    fi
}

ERRORS=0
PENDING=0

run_unit() {
    local unit="$1" scenario="$2" kind="$3" point="$4"
    local dir="$RUN_DIR/$unit" state_dir status=0 attempt
    rm -rf "$dir"
    mkdir -p "$dir"
    state_dir="$(mktemp -d "${TMPDIR:-/tmp}/shellio-probe-resources.XXXXXX")"
    set -- /usr/bin/env SHELLIO_STATE_DIR="$state_dir" /usr/bin/caffeinate -i \
        "$BIN" --scenario "$scenario" --out "$dir"
    if [ -n "$point" ]; then
        set -- "$@" --point "$point"
    fi
    echo "== $unit"
    if [ "$kind" = measured ]; then
        measure_exec "$dir" "$@"
    else
        "$@" || status=$?
        MEASURE_OUTCOME="exit:$status"
    fi
    rm -rf "$state_dir"
    case "$MEASURE_OUTCOME" in
        lock-busy) attempt=lock-busy ;;
        "exit:0") attempt=started ;;
        "exit:$EXIT_NOT_STARTED") attempt=noisy ;;
        *)
            echo "run.sh: $unit: ошибка исполнения ($MEASURE_OUTCOME)" >&2
            "$BIN" --status-set "$RUN_DIR" "$unit" error
            ERRORS=$((ERRORS + 1))
            return 0
            ;;
    esac
    if [ "$attempt" = started ] && [ ! -f "$dir/result.json" ]; then
        echo "run.sh: $unit: процесс завершился без result.json" >&2
        "$BIN" --status-set "$RUN_DIR" "$unit" error
        ERRORS=$((ERRORS + 1))
        return 0
    fi
    set -- "$BIN" --record "$OUT_BASE/not-started" --unit "$unit" --attempt "$attempt" \
        --out "$dir" --scenario "$scenario"
    if [ -n "$point" ]; then
        set -- "$@" --point "$point"
    fi
    status=0
    "$@" || status=$?
    case "$status" in
        0) "$BIN" --status-set "$RUN_DIR" "$unit" done ;;
        "$EXIT_NOT_STARTED")
            echo "run.sh: $unit: не начат ($attempt), повтор следующим вызовом с RUN=$RUN_ID" >&2
            "$BIN" --status-set "$RUN_DIR" "$unit" not-started
            PENDING=$((PENDING + 1))
            ;;
        *) die "$unit: учёт попытки завершился с кодом $status" ;;
    esac
}

contains() {
    local needle="$1" item
    shift
    for item in "$@"; do
        [ "$item" = "$needle" ] && return 0
    done
    return 1
}

scenarios() {
    local requested="$*" line name kind points point selected="" units="" sha unit agg=0
    (cd "$ROOT" && "$CARGO" build --release --locked -p "$PACKAGE")
    [ -x "$BIN" ] || die "нет бинаря $BIN"
    sha="$(shasum -a 256 "$BIN" | cut -d' ' -f1)"
    export SHELLIO_PROBE_BINARY_SHA256="$sha"
    build_info

    local catalog
    catalog="$("$BIN" --list)"
    if [ -n "$requested" ]; then
        for name in $requested; do
            printf '%s\n' "$catalog" | cut -d' ' -f1 | grep -qx -- "$name" ||
                die "неизвестный сценарий «${name}»: $(printf '%s\n' "$catalog" | cut -d' ' -f1 | tr '\n' ' ')"
        done
    fi
    # Процессы: «unit scenario kind point» по строке; POINTS сужает оси до сборки списка.
    local plan=""
    while read -r name kind points; do
        if [ -n "$requested" ] && ! contains "$name" $requested; then
            continue
        fi
        if [ -z "$points" ]; then
            plan="$plan$name $name $kind -"$'\n'
            continue
        fi
        selected=""
        for point in $points; do
            if [ -z "${POINTS:-}" ] || contains "$point" ${POINTS}; then
                selected="$selected $point"
                plan="$plan$name-$point $name $kind $point"$'\n'
            fi
        done
        [ -n "$selected" ] || die "POINTS «${POINTS:-}» не пересекается с точками $name: $points"
    done <<<"$catalog"
    if [ -n "${POINTS:-}" ]; then
        for point in $POINTS; do
            printf '%s' "$plan" | cut -d' ' -f4 | grep -qx -- "$point" || die "неизвестная точка «${point}»"
        done
    fi

    prepare_run "$sha"
    echo "прогон $RUN_ID: $RUN_DIR"
    units="$(printf '%s' "$plan" | cut -d' ' -f1 | tr '\n' ' ')"
    local pending
    pending="$("$BIN" --status-pending "$RUN_DIR" $units | tr '\n' ' ')"
    while read -r unit name kind point; do
        [ -n "$unit" ] || continue
        contains "$unit" $pending || { echo "== $unit: готов в прогоне $RUN_ID, пропуск"; continue; }
        [ "$point" = - ] && point=""
        run_unit "$unit" "$name" "$kind" "$point"
    done <<<"$plan"

    "$BIN" --aggregate "$RUN_DIR" || agg=$?
    echo "сводка: $RUN_DIR/summary.json"
    if [ "$ERRORS" -gt 0 ] || [ "$agg" -ne 0 ]; then
        die "ошибки исполнения: процессов $ERRORS, сводка — код $agg"
    fi
    if [ "$PENDING" -gt 0 ]; then
        echo "run.sh: не начато процессов: $PENDING; повтор: RUN=$RUN_ID" >&2
        exit "$EXIT_NOT_STARTED"
    fi
}

case "${1:-}" in
    scenarios)
        shift
        scenarios "$@"
        ;;
    measure-exec)
        shift
        [ $# -ge 2 ] || die "measure-exec: нужны каталог и команда"
        measure_exec "$@"
        echo "$MEASURE_OUTCOME"
        ;;
    *)
        die "использование: run.sh scenarios [сценарий …] | run.sh measure-exec <каталог> <команда …>"
        ;;
esac
