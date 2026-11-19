#!/bin/bash
# Сборка и запуск пробы P0.6 probe-diff-view (ADR-0034). Источник — spikes/probe-appkit-shell/run.sh
# (P0.2), упрощён: процесс на сценарий, вариант отрисовки и режим вместо вариантов сплита.
#   run.sh scenarios [сценарий …]   каждый процесс под caffeinate, выход в
#                                   target/probes/diff-view/<время>-<pid>/, затем --aggregate
#                                   пишет summary.json и сводку выводов
#   run.sh demo [аргументы пробы …]  демонстрация без стенда и result.json: --variant, --mode и
#                                   --fixture; с --fixture real run.sh готовит фикстуру, как
#                                   перед сценариями, и передаёт пробе --fixtures
# VARIANTS (по умолчанию «a b») и MODES («unified side») сужают оси сценариев вида, FIXTURES
# («added modified sparse wrap real threads») — ось фикстур: у open и open-warm «added modified
# sparse wrap real», у frames «added modified threads wrap» (wrap только в side), у anchor
# «threads wrap», у pairs «modified wrap». Сценарий берёт
# пересечение FIXTURES со своим списком, пустое пересечение — отказ. FRAMES_PATH (none,
# xctrace, screencapturekit; по умолчанию none) — путь замера кадров frames.
# Перед сценариями открытия с фикстурой real run.sh готовит её через git без сети в
# target/probes/diff-view/fixtures/real/ (status.txt, diff.patch, manifest.tsv, blobs/) и пишет
# время чтения из git. Без закреплённых коммитов фикстура получает status=not_run с причиной.
# Затем отдельный процесс --reference пишет эталоны фикстур в
# target/probes/diff-view/fixtures/<имя>/reference.json, вне измеряемых процессов.
# Набор ghostty проба не читает, поэтому блокировка vendor/ghostty.lock не нужна.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CARGO="${CARGO:-cargo}"
BIN="$ROOT/target/release/probe-diff-view"
OUT_BASE="$ROOT/target/probes/diff-view"
FIXTURES_DIR="$OUT_BASE/fixtures"
ALL_SCENARIOS="smoke highlight calibrate open-smoke ax open open-warm frames memory keys idle anchor selection threads pairs bounded zones theme accept"
ALL_VARIANTS="a b"
ALL_MODES="unified side"
ALL_FIXTURES="added modified sparse wrap real threads"
ALL_FRAMES_PATHS="none xctrace screencapturekit"
# Свежих процессов open на фикстуру, вариант и режим, как open::RUNS.
OPEN_RUNS=11
# Фикстура real: настоящий diff проекта между закреплёнными коммитами.
REAL_OLD=d6893dcc4b973671a8fc79d320db868010e7c604
REAL_NEW=cc81b21e03bb3d7b0707a1cf14d9a8b30551181b
REAL_PATHSPEC=spikes/probe-appkit-shell/src/

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
# «view» — процесс на каждую пару варианта и режима, «open» — также на каждую фикстуру,
# «variant» — процесс на вариант, режим меняется внутри процесса (memory).
axes_of() {
    case "$1" in
        smoke | highlight | calibrate) echo stand ;;
        open-smoke | ax | keys | idle | selection | threads | bounded | zones | theme | accept) echo view ;;
        open | open-warm | frames | anchor | pairs) echo open ;;
        memory) echo variant ;;
        *) die "неизвестный сценарий «${1}», известны: $ALL_SCENARIOS" ;;
    esac
}

# Фикстуры сценария с осью фикстур, как Scenario::fixtures в args.rs.
fixtures_of() {
    case "$1" in
        open | open-warm) echo "added modified sparse wrap real" ;;
        frames) echo "added modified threads wrap" ;;
        anchor) echo "threads wrap" ;;
        pairs) echo "modified wrap" ;;
        *) echo "" ;;
    esac
}

# Пара фикстуры и режима не идёт процессом сценария, как Scenario::accepts в args.rs.
skip_cell() {
    [ "$1" = frames ] && [ "$2" = wrap ] && [ "$3" = unified ]
}

# Фикстуры из FIXTURES, которые принимает сценарий, в порядке FIXTURES.
scenario_fixtures() {
    local scenario="$1" given="$2" fixture picked=""
    for fixture in $given; do
        case " $(fixtures_of "$scenario") " in
            *" $fixture "*) picked="$picked $fixture" ;;
        esac
    done
    [ -n "$picked" ] || die "FIXTURES: у сценария $scenario нет фикстур из «${given}», его фикстуры: $(fixtures_of "$scenario")"
    echo "${picked# }"
}

# Сценарии с эталоном подсветки и `accept` с фикстурой real, как Scenario::needs_fixtures в
# args.rs.
needs_fixtures() {
    case "$1" in
        highlight | open | open-warm | accept) return 0 ;;
        *) return 1 ;;
    esac
}

# Миллисекунды настенных часов: у /bin/bash 3.2 нет EPOCHREALTIME, у date в macOS нет %N.
now_ms() {
    perl -MTime::HiRes=time -e 'printf("%.3f\n", time() * 1000)'
}

# Фикстура real через git без сети: diff с параметрами синтетических фикстур, список файлов и
# blob обеих сторон. Чтение из git — работа P3.1, его время пишется в status.txt без вердикта.
prepare_real() {
    local dir="$FIXTURES_DIR/real" commit started finished calls=0 count=0
    local line meta path old_sha new_sha status sha
    rm -rf "$dir"
    mkdir -p "$dir/blobs"
    for commit in "$REAL_OLD" "$REAL_NEW"; do
        if ! git -C "$ROOT" cat-file -e "$commit^{commit}" 2>/dev/null; then
            printf 'status=not_run\nreason=нет закреплённых коммитов: %s нет в git-копии\n' \
                "$commit" >"$dir/status.txt"
            echo "real: not_run, нет коммита $commit"
            return 0
        fi
    done
    started="$(now_ms)"
    git -C "$ROOT" diff -U3 --inter-hunk-context=0 --no-ext-diff --no-textconv --no-color \
        "$REAL_OLD" "$REAL_NEW" -- "$REAL_PATHSPEC" >"$dir/diff.patch"
    git -C "$ROOT" diff --raw --no-abbrev --no-ext-diff --no-textconv \
        "$REAL_OLD" "$REAL_NEW" -- "$REAL_PATHSPEC" >"$dir/raw.txt"
    calls=2
    : >"$dir/manifest.tsv"
    while IFS= read -r line; do
        meta="${line%%$'\t'*}"
        path="${line#*$'\t'}"
        case "$path" in
            *$'\t'* | \"*) die "real: путь не поддержан: $path" ;;
        esac
        # shellcheck disable=SC2086
        set -- $meta
        old_sha="$3"
        new_sha="$4"
        status="$5"
        case "$status" in
            M* | A* | D* | T*) ;;
            *) die "real: состояние «${status}» файла $path не поддержано" ;;
        esac
        for sha in "$old_sha" "$new_sha"; do
            case "$sha" in
                *[!0]*)
                    git -C "$ROOT" cat-file blob "$sha" >"$dir/blobs/$sha"
                    calls=$((calls + 1))
                    ;;
            esac
        done
        case "$old_sha" in *[!0]*) ;; *) old_sha=- ;; esac
        case "$new_sha" in *[!0]*) ;; *) new_sha=- ;; esac
        printf '%s\t%s\t%s\n' "$old_sha" "$new_sha" "$path" >>"$dir/manifest.tsv"
        count=$((count + 1))
    done <"$dir/raw.txt"
    finished="$(now_ms)"
    [ "$count" -gt 0 ] || die "real: в diff нет файлов"
    {
        echo "status=prepared"
        echo "old=$REAL_OLD"
        echo "new=$REAL_NEW"
        echo "pathspec=$REAL_PATHSPEC"
        echo "files=$count"
        echo "git_calls=$calls"
        perl -e 'printf("git_read_ms=%.1f\n", $ARGV[1] - $ARGV[0])' "$started" "$finished"
    } >"$dir/status.txt"
    echo "real: $count файлов, $(grep '^git_read_ms=' "$dir/status.txt")"
}

# Эталоны подсветки отдельным процессом пробы до сценариев, журнал — reference.txt прогона.
write_references() {
    local run_dir="$1" code=0
    "$BIN" --reference "$FIXTURES_DIR" >"$run_dir/reference.txt" 2>&1 || code=$?
    if [ "$code" -ne 0 ]; then
        sed 's/^/    /' "$run_dir/reference.txt" >&2
        die "эталоны подсветки не записаны (код $code)"
    fi
    tail -n 1 "$run_dir/reference.txt"
}

# Номера повторных запусков: «_» — один запуск без номера. Номер двузначный, как
# ProcessKey::dir_name.
runs_of() {
    case "$1" in
        open)
            local index
            for index in $(seq 1 "$OPEN_RUNS"); do
                printf '%02d ' "$index"
            done
            echo
            ;;
        *) echo _ ;;
    esac
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
    local scenarios variants modes fixtures
    scenarios="$(pick SCENARIOS "$ALL_SCENARIOS" "$*")"
    variants="$(pick VARIANTS "$ALL_VARIANTS" "${VARIANTS:-}")"
    modes="$(pick MODES "$ALL_MODES" "${MODES:-}")"
    fixtures="$(pick FIXTURES "$ALL_FIXTURES" "${FIXTURES:-}")"
    local frames_path
    frames_path="$(pick FRAMES_PATH "$ALL_FRAMES_PATHS" "${FRAMES_PATH:-none}")"
    case "$frames_path" in
        *" "*) die "FRAMES_PATH: нужно одно значение из $ALL_FRAMES_PATHS" ;;
    esac
    local scenario axes variant mode fixture index name out code total=0 failed=0 aggregate=0 run_dir
    for scenario in $scenarios; do
        if [ "$(axes_of "$scenario")" = open ]; then
            scenario_fixtures "$scenario" "$fixtures" >/dev/null
        fi
    done
    build
    export_build_info
    mkdir -p "$OUT_BASE"
    run_dir="$OUT_BASE/$(date +%Y%m%d-%H%M%S)-$$"
    mkdir "$run_dir"
    for scenario in $scenarios; do
        if [ "$scenario" = accept ] || { [ "$(axes_of "$scenario")" = open ] \
            && [[ " $(scenario_fixtures "$scenario" "$fixtures") " == *" real "* ]]; }; then
            prepare_real
            break
        fi
    done
    for scenario in $scenarios; do
        if needs_fixtures "$scenario"; then
            write_references "$run_dir"
            break
        fi
    done
    for scenario in $scenarios; do
        axes="$(axes_of "$scenario")"
        local cells cell_fixtures
        cell_fixtures="-"
        [ "$axes" = open ] && cell_fixtures="$(scenario_fixtures "$scenario" "$fixtures")"
        if [ "$axes" = stand ]; then
            cells="-:-:-"
        elif [ "$axes" = variant ]; then
            cells=""
            for variant in $variants; do
                cells="$cells -:$variant:-"
            done
        else
            cells=""
            for fixture in $cell_fixtures; do
                for variant in $variants; do
                    for mode in $modes; do
                        skip_cell "$scenario" "$fixture" "$mode" && continue
                        cells="$cells $fixture:$variant:$mode"
                    done
                done
            done
        fi
        for cell in $cells; do
            fixture="${cell%%:*}"
            variant="${cell#*:}"
            variant="${variant%%:*}"
            mode="${cell##*:}"
            for index in $(runs_of "$scenario"); do
                [ "$index" = _ ] && index=""
                name="$scenario"
                local flags=(--scenario "$scenario")
                if needs_fixtures "$scenario"; then
                    flags+=(--fixtures "$FIXTURES_DIR")
                fi
                if [ "$scenario" = frames ]; then
                    flags+=(--frames-path "$frames_path")
                fi
                if [ "$fixture" != - ]; then
                    name="$name-$fixture"
                    flags+=(--fixture "$fixture")
                fi
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
    local code=0 arg previous="" real=0
    for arg in "$@"; do
        if [ "$previous" = --fixture ] && [ "$arg" = real ]; then
            real=1
        fi
        previous="$arg"
    done
    build
    if [ "$real" -eq 1 ]; then
        prepare_real
        if grep -q '^status=not_run$' "$FIXTURES_DIR/real/status.txt"; then
            die "демонстрация real: $(sed -n 's/^reason=//p' "$FIXTURES_DIR/real/status.txt")"
        fi
        set -- "$@" --fixtures "$FIXTURES_DIR"
    fi
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
