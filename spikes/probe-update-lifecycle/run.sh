#!/bin/bash
# Сборка и запуск пробы P0.5 probe-update-lifecycle (ADR-0034).
#   run.sh scenarios [сценарий …]  сборка бинарей пакета, затем каждый сценарий отдельным процессом через
#                                  --launch под caffeinate -dimsu (временный SHELLIO_STATE_DIR
#                                  создаёт --launch), выход в
#                                  target/probes/update-lifecycle/<время>-<pid>/, затем --aggregate
# Набор ghostty проба не читает и блокировку vendor/ghostty.lock не берёт: собирается только её
# пакет. Ненулевой код — только при ошибке исполнения; fail и not_run показывает сводка.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CARGO="${CARGO:-cargo}"
PACKAGE=probe-update-lifecycle
BIN="$ROOT/target/release/probe-update"
OUT_BASE="$ROOT/target/probes/update-lifecycle"
ALL_SCENARIOS="smoke host"

die() {
    echo "run.sh: $*" >&2
    exit 1
}

# Сведения о сборке и версия Homebrew для environment в result.json: --launch передаёт их процессу
# сценария (launch::BUILD_COMMIT, BUILD_RUSTC, VERSIONS). Homebrew только читается: brew --version
# без автообновления и аналитики, префикс не меняется.
build_info() {
    local commit rustc_bin=rustc brew_version=""
    commit="$(git -C "$ROOT" rev-parse --short=12 HEAD 2>/dev/null || true)"
    if [ -n "$commit" ] && [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
        commit="$commit-dirty"
    fi
    case "$CARGO" in */*) rustc_bin="$(dirname "$CARGO")/rustc" ;; esac
    export SHELLIO_PROBE_COMMIT="$commit"
    SHELLIO_PROBE_RUSTC="$(cd "$ROOT" && "$rustc_bin" -V 2>/dev/null || true)"
    export SHELLIO_PROBE_RUSTC
    if command -v brew >/dev/null 2>&1; then
        brew_version="$(HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 brew --version 2>/dev/null | head -n 1 || true)"
    fi
    export SHELLIO_PROBE_VERSION_HOMEBREW="$brew_version"
}

# Варианты сценария: имя — строчные буквы и цифры, оно входит в имя каталога.
variants_of() {
    case "$1" in
        *) echo base ;;
    esac
}

known_scenario() {
    case " $ALL_SCENARIOS " in
        *" $1 "*) return 0 ;;
    esac
    return 1
}

# Код возврата — код --launch: ошибка запуска, процесса сценария или оставшиеся процессы.
run_scenario() {
    local scenario="$1" variant="$2" out="$3" code=0
    caffeinate -dimsu "$BIN" --launch -- --scenario "$scenario" --variant "$variant" --out "$out" \
        >"$out/stdout.txt" 2>"$out/stderr.txt" || code=$?
    return "$code"
}

cmd_scenarios() {
    local scenarios="${*:-$ALL_SCENARIOS}"
    local run_dir scenario variant variants name out code total=0 failed=0 aggregate=0
    for scenario in $scenarios; do
        known_scenario "$scenario" || die "неизвестный сценарий «${scenario}», известны: $ALL_SCENARIOS"
    done
    # Все бинари пакета: раннер, хост, attach-клиент и нагрузка лежат в одном каталоге, процесс
    # сценария находит их рядом с собой.
    (cd "$ROOT" && "$CARGO" build --release -p "$PACKAGE" --locked --bins)
    [ -x "$BIN" ] || die "нет раннера $BIN"
    build_info
    mkdir -p "$OUT_BASE"
    run_dir="$OUT_BASE/$(date +%Y%m%d-%H%M%S)-$$"
    mkdir "$run_dir"
    for scenario in $scenarios; do
        # Отказ внутри $(…) завершает только подстановку: результат присваивается отдельно.
        variants="$(variants_of "$scenario")"
        for variant in $variants; do
            name="$scenario-$variant"
            out="$run_dir/$name"
            mkdir "$out"
            total=$((total + 1))
            code=0
            run_scenario "$scenario" "$variant" "$out" || code=$?
            if [ "$code" -eq 0 ]; then
                echo "ok      $name"
            else
                failed=$((failed + 1))
                echo "ошибка  $name (код $code)"
                sed 's/^/    /' "$out/stdout.txt" "$out/stderr.txt"
            fi
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

case "${1:-}" in
    scenarios)
        shift
        cmd_scenarios "$@"
        ;;
    *)
        die "использование: run.sh scenarios [сценарий …]"
        ;;
esac
