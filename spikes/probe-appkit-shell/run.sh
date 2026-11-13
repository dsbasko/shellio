#!/bin/bash
# Сборка и запуск пробы P0.2 probe-appkit-shell (ADR-0034).
#   run.sh scenarios [сценарий …]   каждый сценарий отдельным процессом под caffeinate,
#                                   выход в target/probes/appkit-shell/<время>/, затем
#                                   --aggregate пишет summary.json и сводку выводов
#   run.sh demo [аргументы пробы …]  демонстрационная сцена для ручной работы
# Набор ghostty проба не читает, поэтому блокировка vendor/ghostty.lock не нужна.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CARGO="${CARGO:-cargo}"
BIN="$ROOT/target/release/probe-appkit-shell"
OUT_BASE="$ROOT/target/probes/appkit-shell"
ALL_SCENARIOS="startup launch native-menu parent-click key-delivery readback-check split tabs list tree palette menu theme idle frames cycles"
# Запусков startup на вариант: первый в агрегаты прогретых не входит (results::STARTUP_RUNS).
STARTUP_RUNS=21

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

# Сведения о сборке для environment в result.json: сама проба git и rustc не вызывает.
export_build_info() {
    local commit rustc
    commit="$(git -C "$ROOT" rev-parse --short=12 HEAD 2>/dev/null || true)"
    if [ -n "$commit" ] && [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
        commit="$commit-dirty"
    fi
    local rustc_bin=rustc
    case "$CARGO" in */*) rustc_bin="$(dirname "$CARGO")/rustc" ;; esac
    rustc="$(cd "$ROOT" && "$rustc_bin" -V 2>/dev/null || true)"
    PROBE_COMMIT="$commit"
    PROBE_RUSTC="$rustc"
    PROBE_OBJC2="$(awk '
        /^name = "/ { name = $3; gsub(/"/, "", name) }
        /^version = "/ && name ~ /^(objc2|objc2-[a-z-]+|block2|dispatch2)$/ {
            version = $3; gsub(/"/, "", version); printf "%s%s %s", sep, name, version; sep = ", "
        }' "$ROOT/Cargo.lock")"
    export PROBE_COMMIT PROBE_RUSTC PROBE_OBJC2
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

# startup идёт первым процессом сразу после сборки, остальные — в заданном порядке.
startup_first() {
    local scenario rest=""
    for scenario in "$@"; do
        if [ "$scenario" = startup ]; then
            echo startup
        else
            rest="$rest $scenario"
        fi
    done
    echo "$rest"
}

# Номера запусков: у startup по STARTUP_RUNS на вариант (каталоги <сценарий>-<вариант>-NN),
# у остальных один запуск без номера («_»).
runs_of() {
    if [ "$1" = startup ]; then
        seq -f '%02g' 1 "$STARTUP_RUNS"
    else
        echo _
    fi
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

# Ненулевой код только при ошибке исполнения хотя бы одного процесса или агрегирования;
# fail и pending-manual на код не влияют, их показывает сводка выводов.
cmd_scenarios() {
    local scenarios="${*:-$ALL_SCENARIOS}"
    local run_dir scenario variant index out code total=0 failed=0 aggregate=0
    build
    export_build_info
    mkdir -p "$OUT_BASE"
    run_dir="$OUT_BASE/$(date +%Y%m%d-%H%M%S)-$$"
    mkdir "$run_dir"
    for scenario in $(startup_first $scenarios); do
        for variant in $(variants_of "$scenario"); do
            for index in $(runs_of "$scenario"); do
                [ "$index" = _ ] && index=""
                out="$run_dir/$scenario-$variant${index:+-$index}"
                mkdir "$out"
                total=$((total + 1))
                code=0
                run_scenario "$scenario" "$variant" "$out" || code=$?
                if [ "$code" -eq 0 ]; then
                    echo "ok      $scenario $variant${index:+ $index}"
                else
                    failed=$((failed + 1))
                    echo "ошибка  $scenario $variant${index:+ $index} (код $code)"
                    sed 's/^/    /' "$out/stdout.txt" "$out/stderr.txt"
                fi
            done
        done
    done
    echo "прогон: $run_dir"
    echo
    "$BIN" --aggregate "$run_dir" || aggregate=$?
    if [ "$failed" -ne 0 ]; then
        die "ошибки исполнения: $failed из $total"
    fi
    if [ "$aggregate" -ne 0 ]; then
        die "агрегирование отметило ошибки исполнения (код $aggregate)"
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
