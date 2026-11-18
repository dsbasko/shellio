#!/bin/bash
# Сборка и запуск пробы P0.6 probe-diff-view (ADR-0034). Источник — spikes/probe-appkit-shell/run.sh
# (P0.2), упрощён: процесс на сценарий, вариант отрисовки и режим вместо вариантов сплита.
#   run.sh scenarios [сценарий …]   каждый процесс под caffeinate, выход в
#                                   target/probes/diff-view/<время>-<pid>/, затем --aggregate
#                                   пишет summary.json и сводку выводов
#   run.sh demo [аргументы пробы …]  демонстрация для ручной работы
# VARIANTS (по умолчанию «a b») и MODES («unified side») сужают оси сценариев вида.
# Набор ghostty проба не читает, поэтому блокировка vendor/ghostty.lock не нужна.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CARGO="${CARGO:-cargo}"
BIN="$ROOT/target/release/probe-diff-view"
OUT_BASE="$ROOT/target/probes/diff-view"
ALL_SCENARIOS="smoke"
ALL_VARIANTS="a b"
ALL_MODES="unified side"

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

# Значения из списка допустимых, без повторов; пусто — все допустимые.
pick() {
    local name="$1" allowed="$2" given="$3" value seen=""
    if [ -z "${given// /}" ]; then
        echo "$allowed"
        return
    fi
    for value in $given; do
        case " $allowed " in
            *" $value "*) ;;
            *) die "$name: неизвестное значение «${value}», допустимы: $allowed" ;;
        esac
        case " $seen " in
            *" $value "*) die "$name: «${value}» указан дважды" ;;
        esac
        seen="$seen $value"
    done
    echo "${seen# }"
}

build() {
    (cd "$ROOT" && "$CARGO" build --release --locked -p probe-diff-view)
}

# Версии пакетов из Cargo.lock через запятую, имена — расширенное регулярное выражение.
lock_versions() {
    awk -v pattern="$1" '
        /^name = "/ { name = $3; gsub(/"/, "", name) }
        /^version = "/ && name ~ pattern {
            version = $3; gsub(/"/, "", version); printf "%s%s %s", sep, name, version; sep = ", "
        }' "$ROOT/Cargo.lock"
}

# Сведения о сборке и инструментах для environment в result.json: сама проба git, rustc и xcrun
# не вызывает. Пустое значение проба записывает как отсутствующее.
export_build_info() {
    local commit
    commit="$(git -C "$ROOT" rev-parse --short=12 HEAD 2>/dev/null || true)"
    if [ -n "$commit" ] && [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
        commit="$commit-dirty"
    fi
    local rustc_bin=rustc
    case "$CARGO" in */*) rustc_bin="$(dirname "$CARGO")/rustc" ;; esac
    PROBE_COMMIT="$commit"
    PROBE_RUSTC="$(cd "$ROOT" && "$rustc_bin" -V 2>/dev/null || true)"
    PROBE_OBJC2="$(lock_versions '^objc2(-(encode|foundation|app-kit|core-foundation|core-graphics|core-text|quartz-core))?$')"
    PROBE_TREE_SITTER="$(lock_versions '^tree-sitter(-language|-rust)?$')"
    PROBE_XCODE="$(xcodebuild -version 2>/dev/null | paste -sd ' ' - || true)"
    PROBE_XCTRACE="$(xcrun xctrace version 2>/dev/null | paste -sd ' ' - || true)"
    export PROBE_COMMIT PROBE_RUSTC PROBE_OBJC2 PROBE_TREE_SITTER PROBE_XCODE PROBE_XCTRACE
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

# Оси сценария, как Scenario::axes в args.rs: «stand» — один процесс без варианта и режима,
# «view» — процесс на каждую пару варианта и режима.
axes_of() {
    case "$1" in
        smoke) echo stand ;;
        *) die "неизвестный сценарий «${1}», известны: $ALL_SCENARIOS" ;;
    esac
}

# Номера повторных запусков: «_» — один запуск без номера.
runs_of() {
    echo _
}

# Код возврата — код процесса пробы; непустой SHELLIO_STATE_DIR тоже ошибка исполнения.
run_process() {
    local out="$1" code=0
    shift
    new_state_dir
    SHELLIO_STATE_DIR="$STATE_DIR" caffeinate -dimsu \
        "$BIN" "$@" --out "$out" \
        >"$out/stdout.txt" 2>"$out/stderr.txt" || code=$?
    if ! check_state_dir && [ "$code" -eq 0 ]; then
        code=1
    fi
    return "$code"
}

# Ненулевой код только при ошибке исполнения хотя бы одного процесса или агрегирования;
# fail и not_run на код не влияют, их показывает сводка выводов.
cmd_scenarios() {
    local scenarios variants modes
    scenarios="$(pick SCENARIOS "$ALL_SCENARIOS" "$*")"
    variants="$(pick VARIANTS "$ALL_VARIANTS" "${VARIANTS:-}")"
    modes="$(pick MODES "$ALL_MODES" "${MODES:-}")"
    local scenario axes variant mode index name out code total=0 failed=0 aggregate=0 run_dir
    for scenario in $scenarios; do
        axes_of "$scenario" >/dev/null
    done
    build
    export_build_info
    mkdir -p "$OUT_BASE"
    run_dir="$OUT_BASE/$(date +%Y%m%d-%H%M%S)-$$"
    mkdir "$run_dir"
    for scenario in $scenarios; do
        axes="$(axes_of "$scenario")"
        local cells
        if [ "$axes" = stand ]; then
            cells="-:-"
        else
            cells=""
            for variant in $variants; do
                for mode in $modes; do
                    cells="$cells $variant:$mode"
                done
            done
        fi
        for cell in $cells; do
            variant="${cell%%:*}"
            mode="${cell##*:}"
            for index in $(runs_of "$scenario"); do
                [ "$index" = _ ] && index=""
                name="$scenario"
                local flags=(--scenario "$scenario")
                if [ "$variant" != - ]; then
                    name="$name-$variant"
                    flags+=(--variant "$variant")
                fi
                if [ "$mode" != - ]; then
                    name="$name-$mode"
                    flags+=(--mode "$mode")
                fi
                name="$name${index:+-$index}"
                out="$run_dir/$name"
                mkdir "$out"
                total=$((total + 1))
                code=0
                run_process "$out" "${flags[@]}" || code=$?
                if [ "$code" -eq 0 ]; then
                    echo "ok      $name"
                else
                    failed=$((failed + 1))
                    echo "ошибка  $name (код $code)"
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
