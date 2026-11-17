#!/bin/bash
# Сборка и запуск пробы P0.4 probe-session-survival (ADR-0034, ADR-0035).
#   run.sh scenarios [сценарий …]  сборка или сверка наборов zmx (scripts/zmx.sh ensure) и набора
#                                  libghostty-vt (scripts/vt.sh ensure), сверка привязок, clippy и
#                                  тесты с функцией vt, сборка раннера, затем каждый
#                                  сценарий на каждом своём кандидате отдельным процессом через
#                                  --launch под caffeinate -dimsu (временный SHELLIO_STATE_DIR
#                                  создаёт --launch), выход в
#                                  target/probes/session-survival/<время>-<pid>/, затем --aggregate
#   run.sh surface [сценарий …]    сценарии в поверхности libghostty: сборка или сверка набора
#                                  libghostty-vt и раннер с функцией vt, который строит байты сцен в
#                                  каталог процесса (--scenes); затем сверка набора ghostty, clippy
#                                  и тесты с функцией surface, бандл пробы, подписанный Shellio Dev
#                                  (фикстура внутри подписывается раньше бандла), каждый сценарий
#                                  через --launch бинаря бандла, затем --aggregate. Набор ghostty
#                                  читает, блокировку vendor/ghostty.lock держит цель make
#   run.sh check <функции>         clippy --all-targets -D warnings и тесты с функциями через запятую
#   run.sh bindings                сборка или сверка набора vt и генерация src/vt/bindings.rs
#   run.sh bindings-check          сборка или сверка набора vt и сверка src/vt/bindings.rs с ним
# Функции vt и surface одним вызовом cargo не включаются: архивы libghostty-vt.a и libghostty.a не
# попадают в одну линковку, а headless-команда не строит shellio-sys-ghostty и не читает
# vendor/ghostty/. Headless-сценарии набор ghostty не читают, блокировку не берут.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CARGO="${CARGO:-cargo}"
PACKAGE=probe-session-survival
BIN="$ROOT/target/release/probe-session"
OUT_BASE="$ROOT/target/probes/session-survival"
# Каталог наборов пробы — как в scripts/zmx.sh и scripts/vt.sh.
SETS_DIR="${SHELLIO_PROBE_SETS_DIR:-$OUT_BASE}"
ZMX_SH="$ROOT/spikes/probe-session-survival/scripts/zmx.sh"
VT_SH="$ROOT/spikes/probe-session-survival/scripts/vt.sh"
VT_BINDINGS="$ROOT/spikes/probe-session-survival/scripts/vt-bindings.sh"
ALL_SCENARIOS="smoke lifecycle capi restore queries io geometry versions"
SURFACE_SCENARIOS="report-policy"
SURFACE_BIN="$ROOT/target/release/probe-session-surface"
FIXTURE_BIN="$ROOT/target/release/probe-session-fixture"
APP_NAME="probe-session-surface.app"
APP="$OUT_BASE/bundle/$APP_NAME"
APP_BIN="$APP/Contents/MacOS/probe-session-surface"
VENDOR="${SHELLIO_GHOSTTY_VENDOR:-$ROOT/vendor/ghostty}"
REQUIREMENT="$ROOT/scripts/signing/requirement.txt"
KEYCHAIN="${SHELLIO_SIGNING_KEYCHAIN:-}"
BUNDLE_ID="io.github.dsbasko.shellio.probe.session-survival"
FIXTURE_ID="$BUNDLE_ID.fixture"

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
        sum="$(shasum -a 256 <"$SETS_DIR/$set/stamp")"
        zig="$(sed -n 's/^zmx_zig_version=//p' "$SETS_DIR/$set/stamp")"
        export "SHELLIO_PROBE_VERSION_$var=$version" "SHELLIO_PROBE_STAMP_$var=${sum%% *}"
    done
    export SHELLIO_PROBE_VERSION_ZIG_ZMX="$zig"
    # Бинари наборов процесс сценария берёт отсюда: launch::SETS_DIR.
    SHELLIO_PROBE_SETS_DIR="$(cd "$SETS_DIR" && pwd)"
    export SHELLIO_PROBE_SETS_DIR
}

# Набор libghostty-vt: сборка или сверка до любого вызова cargo с функцией vt, затем версия, Zig и
# SHA-256 штампа для environment в result.json.
prepare_vt_set() {
    local version sum zig
    "$VT_SH" ensure
    version="$("$VT_SH" version)"
    sum="$(shasum -a 256 <"$SETS_DIR/vt/stamp")"
    zig="$(sed -n 's/^zig_version=//p' "$SETS_DIR/vt/stamp")"
    export SHELLIO_PROBE_VERSION_VT="$version" SHELLIO_PROBE_STAMP_VT="${sum%% *}" \
        SHELLIO_PROBE_VERSION_ZIG_VT="$zig"
}

# SHA-1 сертификата Shellio Dev из designated requirement продукта (ADR-0019).
signing_sha1() {
    local sha1
    [ -f "$REQUIREMENT" ] || die "нет $REQUIREMENT: идентичность Shellio Dev создаёт scripts/signing/create-identity.sh"
    sha1="$(sed -n 's/.*certificate leaf = H"\([0-9a-fA-F]\{40\}\)".*/\1/p' "$REQUIREMENT")"
    [[ "$sha1" =~ ^[0-9a-fA-F]{40}$ ]] || die "в $REQUIREMENT нет SHA-1 сертификата"
    printf '%s' "$sha1" | tr '[:lower:]' '[:upper:]'
}

# Подпись идентичностью Shellio Dev под своим идентификатором и своим designated requirement
# (ADR-0034, «Уточнено в P0.3»): «sign <путь> <идентификатор> <рабочий каталог>».
sign() {
    local target="$1" identifier="$2" work="$3" sha1 requirement identities details
    local keychain_args=()
    sha1="$(signing_sha1)"
    requirement="identifier \"$identifier\" and certificate leaf = H\"$sha1\""
    printf 'designated => %s\n' "$requirement" >"$work/requirement.txt"
    [ -z "$KEYCHAIN" ] || keychain_args=(--keychain "$KEYCHAIN")
    # Самоподписанный сертификат не доверен, поэтому идентичность ищется без -v.
    identities="$(security find-identity -p codesigning ${KEYCHAIN:+"$KEYCHAIN"} 2>/dev/null)" ||
        die "security find-identity не выполняется${KEYCHAIN:+ для $KEYCHAIN}"
    grep -q ") $sha1 \"" <<<"$identities" ||
        die "нет идентичности подписи с SHA-1 $sha1${KEYCHAIN:+ в $KEYCHAIN}; восстановление — scripts/signing/create-identity.sh --from-p12"
    codesign --force --sign "$sha1" ${keychain_args[@]+"${keychain_args[@]}"} --identifier "$identifier" \
        --timestamp=none -r "$work/requirement.txt" "$target" ||
        die "codesign не подписал $target идентичностью $sha1"
    codesign --verify --strict --verbose=1 "$target" ||
        die "подпись $target не проходит codesign --verify --strict"
    codesign --verify --strict -R="$requirement" "$target" ||
        die "подпись $target не удовлетворяет требованию $requirement"
    details="$(codesign -d --verbose=2 "$target" 2>&1)" || die "codesign -d не читает подпись $target"
    [ "$(sed -n 's/^Identifier=//p' <<<"$details")" = "$identifier" ] ||
        die "идентификатор подписи $target не $identifier"
    if grep -q '^Signature=adhoc$' <<<"$details"; then
        die "подпись $target ad-hoc, а не идентичностью Shellio Dev"
    fi
}

# Бандл пробы (раздел плана «Поверхность»): бинарь поверхности, фикстура рядом с ним (её ищет
# стенд), ресурсы ghostty и terminfo из проверенного набора, лицензия ghostty. Собирается во
# временном каталоге и заменяет прежний только подписанным; фикстура подписывается раньше бандла.
bundle() {
    local version staging stage contents resources plist
    version="$(awk '
        /^\[/ { in_section = ($0 == "[workspace.package]") }
        in_section && /^version *= *"/ { sub(/^version *= *"/, ""); sub(/".*/, ""); print; exit }
    ' "$ROOT/Cargo.toml")"
    [ -n "$version" ] || die "в [workspace.package] Cargo.toml нет version"
    mkdir -p "$OUT_BASE/bundle"
    staging="$(mktemp -d "$OUT_BASE/bundle/.staging.XXXXXX")"
    # shellcheck disable=SC2064 # путь фиксируется сейчас
    trap "rm -rf '$staging'" EXIT
    stage="$staging/$APP_NAME"
    contents="$stage/Contents"
    resources="$contents/Resources"
    mkdir -p "$contents/MacOS" "$resources/licenses"
    copy() {
        ditto --norsrc --noextattr --noacl "$1" "$2" || die "не удалось скопировать $1"
    }
    copy "$SURFACE_BIN" "$contents/MacOS/probe-session-surface"
    copy "$FIXTURE_BIN" "$contents/MacOS/probe-session-fixture"
    copy "$VENDOR/share/ghostty" "$resources/ghostty"
    copy "$VENDOR/share/terminfo" "$resources/terminfo"
    copy "$VENDOR/LICENSE" "$resources/licenses/ghostty.txt"
    plist="$contents/Info.plist"
    copy "$ROOT/spikes/probe-session-survival/Info.plist" "$plist"
    plutil -replace CFBundleShortVersionString -string "$version" "$plist"
    plutil -replace CFBundleVersion -string "$version" "$plist"
    plutil -lint "$plist" >/dev/null || die "Info.plist бандла не проходит plutil -lint"
    [ "$(plutil -extract CFBundleIdentifier raw -o - "$plist")" = "$BUNDLE_ID" ] ||
        die "CFBundleIdentifier в Info.plist не $BUNDLE_ID"
    sign "$contents/MacOS/probe-session-fixture" "$FIXTURE_ID" "$staging"
    sign "$stage" "$BUNDLE_ID" "$staging"
    rm -rf "$APP"
    mv "$stage" "$APP"
    rm -rf "$staging"
    trap - EXIT
    echo "бандл $APP собран и подписан ($BUNDLE_ID)"
}

# Конфигурация libghostty процесса сценария в его --out (surface::scene::CONFIG_FILE).
# Пользовательский ghostty.conf проба не читает. Поля окна нулевые: сетка 80×24 задаётся точным
# размером в пикселях. Интеграция shell выключена, курсор не мигает.
write_config() {
    cat >"$1/ghostty.conf" <<EOF_CONFIG
font-family = Menlo
font-size = 13
window-padding-x = 0
window-padding-y = 0
window-padding-balance = false
scrollback-limit-bytes = 10000000
theme = light:Builtin Light,dark:Builtin Dark
cursor-style-blink = false
shell-integration = none
confirm-close-surface = false
clipboard-paste-protection = false
EOF_CONFIG
}

# Ревизия и SHA-256 штампа набора ghostty поверхности для environment в result.json
# (launch::SURFACE_FACTS).
surface_facts() {
    local rev sum
    rev="$(sed -n 's/^ghostty_rev=//p' "$VENDOR/stamp")"
    [ -n "$rev" ] || die "в штампе $VENDOR/stamp нет ghostty_rev"
    sum="$(shasum -a 256 <"$VENDOR/stamp")"
    export SHELLIO_PROBE_VERSION_SURFACE="$rev" SHELLIO_PROBE_STAMP_SURFACE="${sum%% *}"
}

known_surface_scenario() {
    case " $SURFACE_SCENARIOS " in
        *" $1 "*) return 0 ;;
    esac
    return 1
}

# Сценарии в поверхности. Функции vt и surface идут раздельными вызовами cargo: раннер с vt строит
# сцены, бинарь с surface их прогоняет. Ненулевой код только при ошибке исполнения.
cmd_surface() {
    local scenarios="${*:-$SURFACE_SCENARIOS}"
    local run_dir scenario out code total=0 failed=0 aggregate=0
    for scenario in $scenarios; do
        known_surface_scenario "$scenario" ||
            die "неизвестный сценарий поверхности «${scenario}», известны: $SURFACE_SCENARIOS"
    done
    prepare_vt_set
    "$VT_BINDINGS" --check
    cargo_with vt build --release --bin probe-session
    "$ROOT/scripts/ghostty/vendor.sh" --verify
    check_features surface
    cargo_with surface build --release
    build_info
    surface_facts
    bundle
    mkdir -p "$OUT_BASE"
    run_dir="$OUT_BASE/$(date +%Y%m%d-%H%M%S)-$$"
    mkdir "$run_dir"
    for scenario in $scenarios; do
        out="$run_dir/$scenario-base"
        mkdir "$out"
        total=$((total + 1))
        code=0
        if "$BIN" --scenes "$out" >"$out/scenes.txt" 2>&1; then
            write_config "$out"
            caffeinate -dimsu "$APP_BIN" --launch -- --scenario "$scenario" --variant base \
                --out "$out" >"$out/stdout.txt" 2>"$out/stderr.txt" || code=$?
        else
            code=$?
            cp "$out/scenes.txt" "$out/stderr.txt"
            : >"$out/stdout.txt"
        fi
        if [ "$code" -eq 0 ]; then
            echo "ok      $scenario"
        else
            failed=$((failed + 1))
            echo "ошибка  $scenario (код $code)"
            sed 's/^/    /' "$out/stdout.txt" "$out/stderr.txt"
        fi
    done
    echo "прогон: $run_dir"
    echo
    "$APP_BIN" --aggregate "$run_dir" || aggregate=$?
    if [ "$failed" -ne 0 ]; then
        die "ошибки исполнения: $failed из $total"
    fi
    if [ "$aggregate" -ne 0 ]; then
        die "агрегирование отметило ошибки исполнения (код $aggregate)"
    fi
}

# Кандидаты сценария: «-» — сценарий стенда без кандидата.
backends_of() {
    case "$1" in
        smoke | capi) echo - ;;
        lifecycle | io) echo zmx rust ;;
        restore | queries | geometry | versions) echo zmx ;;
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
    prepare_vt_set
    "$VT_BINDINGS" --check
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
    surface)
        shift
        cmd_surface "$@"
        ;;
    check)
        [ "$#" -eq 2 ] || die "использование: run.sh check <функции через запятую>"
        case ",$2," in *,vt,*) "$VT_SH" ensure ;; esac
        check_features "$2"
        ;;
    bindings)
        [ "$#" -eq 1 ] || die "использование: run.sh bindings"
        "$VT_SH" ensure
        "$VT_BINDINGS"
        ;;
    bindings-check)
        [ "$#" -eq 1 ] || die "использование: run.sh bindings-check"
        "$VT_SH" ensure
        "$VT_BINDINGS" --check
        ;;
    *)
        die "использование: run.sh scenarios [сценарий …] | run.sh surface [сценарий …] | run.sh check <функции> | run.sh bindings | run.sh bindings-check"
        ;;
esac
