#!/bin/bash
# Сборка и запуск пробы P0.4 probe-session-survival (ADR-0034, ADR-0035).
#   run.sh scenarios [сценарий …]  сборка или сверка наборов zmx (scripts/zmx.sh ensure) и набора
#                                  libghostty-vt (scripts/vt.sh ensure), сверка привязок, clippy и
#                                  тесты с функцией vt, сборка раннера; для versions ещё хост
#                                  предыдущего поколения (функции vt и proto-v1) в отдельный
#                                  каталог cargo target/proto-v1, затем каждый
#                                  сценарий на каждом своём кандидате отдельным процессом через
#                                  --launch под caffeinate -dimsu (временный SHELLIO_STATE_DIR
#                                  создаёт --launch), выход в
#                                  target/probes/session-survival/<время>-<pid>/, затем --aggregate
#   run.sh surface [сценарий …]    сценарии в поверхности libghostty: сборка или сверка набора
#                                  libghostty-vt и раннер с функцией vt, который строит байты сцен в
#                                  каталог процесса (--scenes); затем сверка набора ghostty, clippy
#                                  и тесты с функцией surface, поколения бандла пробы, подписанные
#                                  Shellio Dev (вспомогательные бинари внутри подписываются раньше
#                                  бандла), в bundle/generations/<N>/; перед каждым процессом
#                                  сценария установленный бандл bundle/ — копия поколения 1. Каждый
#                                  сценарий через --launch бинаря установленного бандла, затем
#                                  --aggregate. Для update ещё поколение 2, пустой runtime/ и
#                                  generations.json в каталоге процесса. Набор ghostty читает,
#                                  блокировку vendor/ghostty.lock держит цель make
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
# Хост предыдущего поколения (G1, G2): свой каталог cargo, чтобы не заменить текущий хост.
HOST_V1_TARGET="$ROOT/target/proto-v1"
OUT_BASE="$ROOT/target/probes/session-survival"
# Каталог наборов пробы — как в scripts/zmx.sh и scripts/vt.sh.
SETS_DIR="${SHELLIO_PROBE_SETS_DIR:-$OUT_BASE}"
ZMX_SH="$ROOT/spikes/probe-session-survival/scripts/zmx.sh"
VT_SH="$ROOT/spikes/probe-session-survival/scripts/vt.sh"
VT_BINDINGS="$ROOT/spikes/probe-session-survival/scripts/vt-bindings.sh"
ALL_SCENARIOS="smoke lifecycle capi restore queries io geometry versions"
SURFACE_SCENARIOS="report-policy bridge update"
SURFACE_BIN="$ROOT/target/release/probe-session-surface"
FIXTURE_BIN="$ROOT/target/release/probe-session-fixture"
HOST_BIN="$ROOT/target/release/probe-session-host"
ATTACH_BIN="$ROOT/target/release/probe-session-attach"
APP_NAME="probe-session-surface.app"
APP="$OUT_BASE/bundle/$APP_NAME"
# Подписанные поколения бандла (G3) и рантаймы поколений: раскладка matrix::update.
GEN_DIR="$OUT_BASE/bundle/generations"
RUNTIME_DIR="$OUT_BASE/runtime"
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

# Хост с замороженным протоколом v1 для G1 и G2 Rust-хоста: отдельным вызовом cargo в свой
# каталог, путь — процессу сценария (launch::HOST_V1).
build_host_v1() {
    cargo_with vt,proto-v1 build --release --bin probe-session-host --target-dir "$HOST_V1_TARGET"
    export SHELLIO_PROBE_HOST_V1="$HOST_V1_TARGET/release/probe-session-host"
    [ -x "$SHELLIO_PROBE_HOST_V1" ] || die "нет хоста v1 $SHELLIO_PROBE_HOST_V1"
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

# Поколение бандла пробы (раздел плана «Поверхность»): «bundle <поколение> <путь бандла>».
# Бинарь поверхности, фикстура рядом с ним (её ищет стенд), хост, attach-клиент и копия zmx
# текущего набора (их поколение копирует в runtime/<поколение>/, задача 16), ресурсы ghostty и
# terminfo из проверенного набора, лицензии ghostty и zmx. CFBundleVersion — номер поколения.
# Собирается во временном каталоге и заменяет прежний только подписанным; вспомогательные бинари
# подписываются своими идентификаторами раньше бандла, бинарь набора zmx на месте не меняется.
bundle() {
    local generation="$1" dest="$2" version staging stage contents resources plist helper zmx_set
    [[ "$generation" =~ ^[1-9][0-9]*$ ]] || die "bundle: поколение «${generation}» не число"
    zmx_set="$SETS_DIR/zmx"
    version="$(awk '
        /^\[/ { in_section = ($0 == "[workspace.package]") }
        in_section && /^version *= *"/ { sub(/^version *= *"/, ""); sub(/".*/, ""); print; exit }
    ' "$ROOT/Cargo.toml")"
    [ -n "$version" ] || die "в [workspace.package] Cargo.toml нет version"
    mkdir -p "$(dirname "$dest")"
    staging="$(mktemp -d "$(dirname "$dest")/.staging.XXXXXX")"
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
    copy "$HOST_BIN" "$contents/MacOS/probe-session-host"
    copy "$ATTACH_BIN" "$contents/MacOS/probe-session-attach"
    copy "$zmx_set/bin/zmx" "$contents/MacOS/zmx"
    copy "$VENDOR/share/ghostty" "$resources/ghostty"
    copy "$VENDOR/share/terminfo" "$resources/terminfo"
    copy "$VENDOR/LICENSE" "$resources/licenses/ghostty.txt"
    copy "$zmx_set/LICENSE" "$resources/licenses/zmx.txt"
    copy "$zmx_set/LICENSE.ghostty" "$resources/licenses/zmx-ghostty.txt"
    plist="$contents/Info.plist"
    copy "$ROOT/spikes/probe-session-survival/Info.plist" "$plist"
    plutil -replace CFBundleShortVersionString -string "$version" "$plist"
    plutil -replace CFBundleVersion -string "$generation" "$plist"
    plutil -lint "$plist" >/dev/null || die "Info.plist бандла не проходит plutil -lint"
    [ "$(plutil -extract CFBundleIdentifier raw -o - "$plist")" = "$BUNDLE_ID" ] ||
        die "CFBundleIdentifier в Info.plist не $BUNDLE_ID"
    sign "$contents/MacOS/probe-session-fixture" "$FIXTURE_ID" "$staging"
    for helper in host attach; do
        sign "$contents/MacOS/probe-session-$helper" "$BUNDLE_ID.$helper" "$staging"
    done
    sign "$contents/MacOS/zmx" "$BUNDLE_ID.zmx" "$staging"
    sign "$stage" "$BUNDLE_ID" "$staging"
    rm -rf "$dest"
    mv "$stage" "$dest"
    rm -rf "$staging"
    trap - EXIT
    echo "бандл $dest собран и подписан ($BUNDLE_ID, поколение $generation)"
}

# Установленный бандл $APP — копия поколения «$1» из $GEN_DIR; прежний удаляется только после
# готовой копии.
install_generation() {
    local source="$GEN_DIR/$1/$APP_NAME" staging
    [ -d "$source" ] || die "нет поколения $1 бандла: $source"
    mkdir -p "$(dirname "$APP")"
    staging="$(mktemp -d "$(dirname "$APP")/.install.XXXXXX")"
    ditto --norsrc --noextattr --noacl "$source" "$staging/$APP_NAME" ||
        die "не удалось скопировать поколение $1 бандла"
    rm -rf "$APP"
    mv "$staging/$APP_NAME" "$APP"
    rm -rf "$staging"
}

# generations.json процесса update (matrix::update::Manifest): установленный бандл, корень
# рантаймов и подписанные поколения.
write_generations() {
    cat >"$1/generations.json" <<EOF_GENERATIONS
{
  "installed": "$APP",
  "runtime": "$RUNTIME_DIR",
  "generations": [
    { "generation": 1, "app": "$GEN_DIR/1/$APP_NAME" },
    { "generation": 2, "app": "$GEN_DIR/2/$APP_NAME" }
  ]
}
EOF_GENERATIONS
}

# Конфигурация libghostty процесса сценария в его --out (surface::scene::CONFIG_FILE).
# Пользовательский ghostty.conf проба не читает. Поля окна нулевые: сетка 80×24 задаётся точным
# размером в пикселях. Интеграция shell выключена, курсор не мигает. OSC 52 чтения и записи идут в
# колбэки без подтверждения: настоящий буфер обмена колбэки GUI моста не трогают (D3, D4).
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
clipboard-read = allow
clipboard-write = allow
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

# Кандидаты сценария поверхности: «-» — сценарий стенда без кандидата.
surface_backends_of() {
    case "$1" in
        report-policy) echo - ;;
        bridge | update) echo zmx rust ;;
        *) die "у сценария поверхности $1 не заданы кандидаты" ;;
    esac
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
    local run_dir scenario backend name out code total=0 failed=0 aggregate=0
    local backend_args=()
    for scenario in $scenarios; do
        known_surface_scenario "$scenario" ||
            die "неизвестный сценарий поверхности «${scenario}», известны: $SURFACE_SCENARIOS"
    done
    prepare_zmx_sets
    prepare_vt_set
    "$VT_BINDINGS" --check
    # Хост нужен сценариям с кандидатом; бандл его не содержит, путь — launch::HOST_DIR.
    cargo_with vt build --release --bin probe-session --bin probe-session-host
    "$ROOT/scripts/ghostty/vendor.sh" --verify
    check_features surface
    cargo_with surface build --release
    build_info
    surface_facts
    bundle 1 "$GEN_DIR/1/$APP_NAME"
    case " $scenarios " in
        *" update "*) bundle 2 "$GEN_DIR/2/$APP_NAME" ;;
    esac
    SHELLIO_PROBE_HOST_DIR="$(dirname "$SURFACE_BIN")"
    [ -x "$SHELLIO_PROBE_HOST_DIR/probe-session-host" ] &&
        [ -x "$SHELLIO_PROBE_HOST_DIR/probe-session-attach" ] ||
        die "нет хоста или attach-клиента в $SHELLIO_PROBE_HOST_DIR"
    export SHELLIO_PROBE_HOST_DIR
    mkdir -p "$OUT_BASE"
    run_dir="$OUT_BASE/$(date +%Y%m%d-%H%M%S)-$$"
    mkdir "$run_dir"
    for scenario in $scenarios; do
        for backend in $(surface_backends_of "$scenario"); do
            if [ "$backend" = - ]; then
                name="$scenario-base"
                backend_args=()
            else
                name="$scenario-$backend-base"
                backend_args=(--backend "$backend")
            fi
            out="$run_dir/$name"
            mkdir "$out"
            total=$((total + 1))
            code=0
            # Сценарий update заменяет установленный бандл, поэтому каждый процесс начинает с
            # поколения 1 и пустого runtime/.
            install_generation 1
            if [ "$scenario" = update ]; then
                rm -rf "$RUNTIME_DIR"
                write_generations "$out"
            fi
            if "$BIN" --scenes "$out" >"$out/scenes.txt" 2>&1; then
                write_config "$out"
                caffeinate -dimsu "$APP_BIN" --launch -- --scenario "$scenario" \
                    ${backend_args[@]+"${backend_args[@]}"} --variant base --out "$out" \
                    >"$out/stdout.txt" 2>"$out/stderr.txt" || code=$?
            else
                code=$?
                cp "$out/scenes.txt" "$out/stderr.txt"
                : >"$out/stdout.txt"
            fi
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
        lifecycle | io | restore | queries | geometry | versions) echo zmx rust ;;
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
    case " $scenarios " in
        *" versions "*) build_host_v1 ;;
    esac
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
