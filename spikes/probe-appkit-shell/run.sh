#!/bin/bash
# Сборка и запуск пробы P0.2 probe-appkit-shell (ADR-0034).
#   run.sh scenarios [сценарий …]   каждый сценарий отдельным процессом под caffeinate,
#                                   выход в target/probes/appkit-shell/<время>/
#   run.sh demo [аргументы пробы …]  демонстрационная сцена для ручной работы
# Набор ghostty проба не читает, поэтому блокировка vendor/ghostty.lock не нужна.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CARGO="${CARGO:-cargo}"
BIN="$ROOT/target/release/probe-appkit-shell"
OUT_BASE="$ROOT/target/probes/appkit-shell"
ALL_SCENARIOS="launch native-menu parent-click key-delivery"

STATE_DIR=""

die() {
    echo "run.sh: $*" >&2
    exit 1
}

cleanup() {
    if [ -n "$STATE_DIR" ]; then
        rm -rf "$STATE_DIR"
    fi
}
trap cleanup EXIT

build() {
    (cd "$ROOT" && "$CARGO" build --release --locked -p probe-appkit-shell)
}

new_state_dir() {
    STATE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/shellio-probe-state.XXXXXX")"
}

# Временный SHELLIO_STATE_DIR после выхода процесса обязан быть пуст.
check_state_dir() {
    local leftovers
    leftovers="$(ls -A "$STATE_DIR")"
    rm -rf "$STATE_DIR"
    STATE_DIR=""
    if [ -n "$leftovers" ]; then
        echo "run.sh: SHELLIO_STATE_DIR не пуст после выхода процесса:" >&2
        printf '%s\n' "$leftovers" | sed 's/^/    /' >&2
        return 1
    fi
}

# split, startup, idle и frames идут в обоих вариантах сплита, остальные в plain.
variants_of() {
    case "$1" in
        split | startup | idle | frames) echo "sidebar plain" ;;
        *) echo "plain" ;;
    esac
}

# Код возврата — код процесса пробы; непустой SHELLIO_STATE_DIR тоже ошибка исполнения.
run_scenario() {
    local scenario="$1" variant="$2" out="$3" code=0
    new_state_dir
    SHELLIO_STATE_DIR="$STATE_DIR" caffeinate -dimsu \
        "$BIN" --scenario "$scenario" --split "$variant" --out "$out" \
        >"$out/stdout.txt" 2>"$out/stderr.txt" || code=$?
    if ! check_state_dir && [ "$code" -eq 0 ]; then
        code=1
    fi
    return "$code"
}

cmd_scenarios() {
    local scenarios="${*:-$ALL_SCENARIOS}"
    local run_dir scenario variant out code total=0 failed=0
    build
    mkdir -p "$OUT_BASE"
    run_dir="$OUT_BASE/$(date +%Y%m%d-%H%M%S)-$$"
    mkdir "$run_dir"
    for scenario in $scenarios; do
        for variant in $(variants_of "$scenario"); do
            out="$run_dir/$scenario-$variant"
            mkdir "$out"
            total=$((total + 1))
            code=0
            run_scenario "$scenario" "$variant" "$out" || code=$?
            if [ "$code" -eq 0 ]; then
                echo "ok      $scenario $variant"
            else
                failed=$((failed + 1))
                echo "ошибка  $scenario $variant (код $code)"
                sed 's/^/    /' "$out/stdout.txt" "$out/stderr.txt"
            fi
        done
    done
    echo "прогон: $run_dir"
    if [ "$failed" -ne 0 ]; then
        die "ошибки исполнения: $failed из $total"
    fi
}

cmd_demo() {
    local code=0
    build
    new_state_dir
    SHELLIO_STATE_DIR="$STATE_DIR" "$BIN" "$@" || code=$?
    if ! check_state_dir && [ "$code" -eq 0 ]; then
        code=1
    fi
    return "$code"
}

case "${1:-}" in
    scenarios)
        shift
        cmd_scenarios "$@"
        ;;
    demo)
        shift
        cmd_demo "$@"
        ;;
    *)
        die "использование: run.sh scenarios [сценарий …] | run.sh demo [аргументы пробы …]"
        ;;
esac
