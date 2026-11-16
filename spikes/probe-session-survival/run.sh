#!/bin/bash
# Сборка и запуск пробы P0.4 probe-session-survival (ADR-0034, ADR-0035).
#   run.sh scenarios [сценарий …]  сборка или сверка наборов zmx (scripts/zmx.sh ensure), clippy и
#                                  тесты с функцией vt, сборка раннера, затем каждый
#                                  сценарий на каждом своём кандидате отдельным процессом через
#                                  --launch под caffeinate -dimsu (временный SHELLIO_STATE_DIR
#                                  создаёт --launch), выход в
#                                  target/probes/session-survival/<время>-<pid>/, затем --aggregate
#   run.sh check <функции>         clippy --all-targets -D warnings и тесты с функциями через запятую
# Функции vt и surface одним вызовом cargo не включаются: архивы libghostty-vt.a и libghostty.a не
# попадают в одну линковку, а headless-команда не строит shellio-sys-ghostty и не читает
# vendor/ghostty/. Headless-сценарии набор ghostty не читают, блокировку не берут.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CARGO="${CARGO:-cargo}"
PACKAGE=probe-session-survival
BIN="$ROOT/target/release/probe-session"
OUT_BASE="$ROOT/target/probes/session-survival"
ZMX_SH="$ROOT/spikes/probe-session-survival/scripts/zmx.sh"
ALL_SCENARIOS="smoke"

die() {
    echo "run.sh: $*" >&2
    exit 1
}

# cargo с функциями пробы: «cargo_with <функции через запятую> <подкоманда> [аргументы …]».
# Пакет и функции задаёт только эта функция; vt и surface вместе она не пропускает.
cargo_with() {
    local features="$1" subcommand="${2:-}" feature has_vt=0 has_surface=0 arg
    local list=()
    shift 2 || die "cargo_with: нужны функции и подкоманда cargo"
    [ -n "$features" ] || die "нужна хотя бы одна функция пробы"
    IFS=, read -r -a list <<<"$features"
    for feature in "${list[@]}"; do
        case "$feature" in
            vt) has_vt=1 ;;
            surface) has_surface=1 ;;
            proto-v1) ;;
            *) die "неизвестная функция пробы «${feature}»: ожидаются vt, surface, proto-v1" ;;
        esac
    done
    if [ "$has_vt" -eq 1 ] && [ "$has_surface" -eq 1 ]; then
        die "функции vt и surface не включаются одним вызовом cargo: «${features}»"
    fi
    for arg in "$@"; do
        case "$arg" in
            --) break ;;
            --all-features | --features | --features=* | -F | -F* | -p | -p* | --package | --package=* | --workspace)
                die "пакет и функции задаёт cargo_with, аргумент «${arg}» не принимается" ;;
        esac
    done
    (cd "$ROOT" && "$CARGO" "$subcommand" -p "$PACKAGE" --locked --features "$features" "$@")
}

# clippy и тесты с одним набором функций (ADR-0035, «Цель пробы»).
check_features() {
    cargo_with "$1" clippy --all-targets -- -D warnings
    cargo_with "$1" test
}

# Сведения о сборке для environment в result.json: --launch передаёт их процессу сценария.
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

# Наборы zmx: сборка или сверка перед сценариями, затем `zmx version`, Zig и SHA-256 штампа каждого
# набора для environment в result.json. Имена переменных — launch::SET_VERSIONS и SET_STAMPS.
prepare_zmx_sets() {
    local sets set var version sum zig=""
    sets="$("$ZMX_SH" sets)"
    for set in $sets; do
        "$ZMX_SH" ensure "$set"
    done
    # Подстановка внутри export скрыла бы отказ команды, поэтому значения присваиваются отдельно.
    for set in $sets; do
        var="$(printf '%s' "$set" | tr '[:lower:]-' '[:upper:]_')"
        version="$("$ZMX_SH" version "$set")"
        sum="$(shasum -a 256 <"$OUT_BASE/$set/stamp")"
        zig="$(sed -n 's/^zmx_zig_version=//p' "$OUT_BASE/$set/stamp")"
        export "SHELLIO_PROBE_VERSION_$var=$version" "SHELLIO_PROBE_STAMP_$var=${sum%% *}"
    done
    export SHELLIO_PROBE_VERSION_ZIG_ZMX="$zig"
}

# Кандидаты сценария: «-» — сценарий стенда без кандидата.
backends_of() {
    case "$1" in
        smoke) echo - ;;
        *) die "у сценария $1 не заданы кандидаты" ;;
    esac
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
    local scenario="$1" backend="$2" variant="$3" out="$4" code=0
    local backend_args=()
    [ "$backend" = - ] || backend_args=(--backend "$backend")
    caffeinate -dimsu "$BIN" --launch -- --scenario "$scenario" \
        ${backend_args[@]+"${backend_args[@]}"} --variant "$variant" --out "$out" \
        >"$out/stdout.txt" 2>"$out/stderr.txt" || code=$?
    return "$code"
}

# Ненулевой код только при ошибке исполнения хотя бы одного процесса или агрегирования; fail,
# not_run и unsupported на код не влияют, их показывает сводка.
cmd_scenarios() {
    local scenarios="${*:-$ALL_SCENARIOS}"
    local run_dir scenario backend variant name out code total=0 failed=0 aggregate=0
    local backends variants
    # Отказ die внутри $(…) завершает только подстановку: результат присваивается отдельно, тогда
    # set -e останавливает скрипт, а не пропускает сценарий.
    for scenario in $scenarios; do
        known_scenario "$scenario" || die "неизвестный сценарий «${scenario}», известны: $ALL_SCENARIOS"
        backends="$(backends_of "$scenario")"
        variants="$(variants_of "$scenario")"
    done
    prepare_zmx_sets
    check_features vt
    cargo_with vt build --release
    build_info
    mkdir -p "$OUT_BASE"
    run_dir="$OUT_BASE/$(date +%Y%m%d-%H%M%S)-$$"
    mkdir "$run_dir"
    for scenario in $scenarios; do
        backends="$(backends_of "$scenario")"
        variants="$(variants_of "$scenario")"
        for backend in $backends; do
            for variant in $variants; do
                if [ "$backend" = - ]; then
                    name="$scenario-$variant"
                else
                    name="$scenario-$backend-$variant"
                fi
                out="$run_dir/$name"
                mkdir "$out"
                total=$((total + 1))
                code=0
                run_scenario "$scenario" "$backend" "$variant" "$out" || code=$?
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

case "${1:-}" in
    scenarios)
        shift
        cmd_scenarios "$@"
        ;;
    check)
        [ "$#" -eq 2 ] || die "использование: run.sh check <функции через запятую>"
        check_features "$2"
        ;;
    *)
        die "использование: run.sh scenarios [сценарий …] | run.sh check <функции>"
        ;;
esac
