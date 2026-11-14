#!/bin/bash
# Сборка, подпись и запуск пробы P0.3 probe-ghostty-surface (ADR-0034).
#   run.sh scenarios [сценарий …]   бандл, затем каждый сценарий через --launch под caffeinate,
#                                   выход в target/probes/ghostty-surface/<время>-<pid>/, затем
#                                   --aggregate пишет summary.json и сводку выводов; startup
#                                   идёт первым процессом сразу после сборки
#   run.sh demo [аргументы пробы …]  бандл, затем демонстрационная сцена через --launch с живой
#                                   поверхностью; --defer-create <секунды> откладывает её создание
# Скрипт читает набор ghostty, а блокировку vendor/ghostty.lock на весь рецепт держит цель make.
# Окружение процесса пробы строит --launch: run.sh его не задаёт.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CARGO="${CARGO:-cargo}"
BIN="$ROOT/target/release/probe-ghostty-surface"
OUT_BASE="$ROOT/target/probes/ghostty-surface"
APP_NAME="probe-ghostty-surface.app"
APP="$OUT_BASE/bundle/$APP_NAME"
VENDOR="${SHELLIO_GHOSTTY_VENDOR:-$ROOT/vendor/ghostty}"
REQUIREMENT="$ROOT/scripts/signing/requirement.txt"
KEYCHAIN="${SHELLIO_SIGNING_KEYCHAIN:-}"
BUNDLE_ID="io.github.dsbasko.shellio.probe.ghostty-surface"
ALL_SCENARIOS="startup startup-ls window surface-env zones input ime chord palette gpu-calibration visibility teardown"
# Запусков сценариев старта на вариант: первый в агрегаты прогретых не входит
# (results::STARTUP_SCENARIOS).
STARTUP_RUNS=21
STARTUP_LS_RUNS=11
# Диагностический вариант startup с недопустимым LANG: один запуск без агрегатов (env.rs).
BAD_LANG_VARIANT=badlang
# Вариант teardown с переменной варианта MallocScribble=1 (teardown.rs, SCRIBBLE).
SCRIBBLE_VARIANT=scribble
# Сценарии, которым служба footprint снимает footprint процесса пробы по запросу (gpu.rs).
FOOTPRINT_SCENARIOS="gpu-calibration visibility"
FOOTPRINT_SERVICE=""

die() {
    echo "run.sh: $*" >&2
    exit 1
}

build() {
    (cd "$ROOT" && "$CARGO" build --release --locked -p probe-ghostty-surface)
}

# Сведения о сборке для environment в result.json. Окружение процесса пробы собирается по белому
# списку, поэтому они лежат в подписанном Info.plist бандла.
build_info() {
    local commit rustc_bin=rustc
    commit="$(git -C "$ROOT" rev-parse --short=12 HEAD 2>/dev/null || true)"
    if [ -n "$commit" ] && [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
        commit="$commit-dirty"
    fi
    case "$CARGO" in */*) rustc_bin="$(dirname "$CARGO")/rustc" ;; esac
    PROBE_COMMIT="$commit"
    PROBE_RUSTC="$(cd "$ROOT" && "$rustc_bin" -V 2>/dev/null || true)"
    PROBE_OBJC2="$(awk '
        /^name = "/ { name = $3; gsub(/"/, "", name) }
        /^version = "/ && name ~ /^(objc2|objc2-[a-z-]+|block2|dispatch2)$/ {
            version = $3; gsub(/"/, "", version); printf "%s%s %s", sep, name, version; sep = ", "
        }' "$ROOT/Cargo.lock")"
    GHOSTTY_REV="$(sed -n 's/^ghostty_rev=//p' "$VENDOR/stamp")"
    [ -n "$GHOSTTY_REV" ] || die "в штампе $VENDOR/stamp нет ghostty_rev"
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
# (ADR-0034, «Уточнено в P0.3»). sign.sh подписывает идентификатором продукта, поэтому не годится.
sign() {
    local target="$1" work="$2" sha1 requirement identities details
    local keychain_args=()
    sha1="$(signing_sha1)"
    requirement="identifier \"$BUNDLE_ID\" and certificate leaf = H\"$sha1\""
    printf 'designated => %s\n' "$requirement" >"$work/requirement.txt"
    [ -z "$KEYCHAIN" ] || keychain_args=(--keychain "$KEYCHAIN")
    # Самоподписанный сертификат не доверен, поэтому идентичность ищется без -v.
    identities="$(security find-identity -p codesigning ${KEYCHAIN:+"$KEYCHAIN"} 2>/dev/null)" ||
        die "security find-identity не выполняется${KEYCHAIN:+ для $KEYCHAIN}"
    grep -q ") $sha1 \"" <<<"$identities" ||
        die "нет идентичности подписи с SHA-1 $sha1${KEYCHAIN:+ в $KEYCHAIN}; восстановление — scripts/signing/create-identity.sh --from-p12"
    codesign --force --sign "$sha1" ${keychain_args[@]+"${keychain_args[@]}"} --identifier "$BUNDLE_ID" \
        --timestamp=none -r "$work/requirement.txt" "$target" ||
        die "codesign не подписал $target идентичностью $sha1"
    codesign --verify --strict --verbose=1 "$target" ||
        die "подпись $target не проходит codesign --verify --strict"
    codesign --verify --strict -R="$requirement" "$target" ||
        die "подпись $target не удовлетворяет требованию $requirement"
    details="$(codesign -d --verbose=2 "$target" 2>&1)" || die "codesign -d не читает подпись $target"
    [ "$(sed -n 's/^Identifier=//p' <<<"$details")" = "$BUNDLE_ID" ] ||
        die "идентификатор подписи $target не $BUNDLE_ID"
    if grep -q '^Signature=adhoc$' <<<"$details"; then
        die "подпись $target ad-hoc, а не идентичностью Shellio Dev"
    fi
}

# Бандл пробы: исполняемый файл, Info.plist со сведениями о сборке, ресурсы ghostty и terminfo
# из проверенного набора рядом (ADR-0003), лицензия ghostty. Собирается во временном каталоге и
# заменяет прежний только подписанным.
bundle() {
    local version staging stage contents resources plist
    "$ROOT/scripts/ghostty/vendor.sh" --verify
    build_info
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
    copy "$BIN" "$contents/MacOS/probe-ghostty-surface"
    copy "$VENDOR/share/ghostty" "$resources/ghostty"
    copy "$VENDOR/share/terminfo" "$resources/terminfo"
    copy "$VENDOR/LICENSE" "$resources/licenses/ghostty.txt"
    plist="$contents/Info.plist"
    copy "$ROOT/spikes/probe-ghostty-surface/Info.plist" "$plist"
    plutil -replace CFBundleShortVersionString -string "$version" "$plist"
    plutil -replace CFBundleVersion -string "$version" "$plist"
    plutil -replace ShellioProbeCommit -string "$PROBE_COMMIT" "$plist"
    plutil -replace ShellioProbeRustc -string "$PROBE_RUSTC" "$plist"
    plutil -replace ShellioProbeObjc2 -string "$PROBE_OBJC2" "$plist"
    plutil -replace ShellioGhosttyRev -string "$GHOSTTY_REV" "$plist"
    plutil -lint "$plist" >/dev/null || die "Info.plist бандла не проходит plutil -lint"
    [ "$(plutil -extract CFBundleIdentifier raw -o - "$plist")" = "$BUNDLE_ID" ] ||
        die "CFBundleIdentifier в Info.plist не $BUNDLE_ID"
    sign "$stage" "$staging"
    rm -rf "$APP"
    mv "$stage" "$APP"
    rm -rf "$staging"
    trap - EXIT
    echo "бандл $APP собран и подписан ($BUNDLE_ID)"
}

# Варианты сценария: имя варианта — строчные буквы и цифры, оно входит в имя каталога. У
# сценариев старта вариант с поверхностью идёт перед базовой линией без неё.
variants_of() {
    case "$1" in
        surface-env) echo "light dark" ;;
        startup) echo "surface bare $BAD_LANG_VARIANT" ;;
        startup-ls) echo "surface bare" ;;
        # Наборы обходов agterm (visibility.rs, Workarounds::VARIANTS).
        visibility) echo "base bounds defer syncdraw all" ;;
        # Обычный прогон и с MallocScribble=1 (teardown.rs, SCRIBBLE).
        teardown) echo "base $SCRIBBLE_VARIANT" ;;
        *) echo "base" ;;
    esac
}

# Номера запусков: у сценариев старта по числу запусков на вариант (каталоги
# <сценарий>-<вариант>-NN), у остальных и у диагностического варианта один запуск без номера («_»).
runs_of() {
    case "$1:$2" in
        startup:"$BAD_LANG_VARIANT") echo _ ;;
        startup:*) seq -f '%02g' 1 "$STARTUP_RUNS" ;;
        startup-ls:*) seq -f '%02g' 1 "$STARTUP_LS_RUNS" ;;
        *) echo _ ;;
    esac
}

# Маршрут запуска процесса пробы.
route_of() {
    case "$1" in
        startup-ls) echo launch-services ;;
        *) echo direct ;;
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

# Конфигурация libghostty процесса сценария в его --out (ghostty/config.rs читает её оттуда).
# Пользовательский ghostty.conf проба не читает; тема условная (ghostty::config::conditional_theme),
# схему выбирает вариант. palette продлевает цвет крайних клеток в поля поверхности, чтобы углы
# плитки на снимке показывали содержимое терминала, а не фон полей.
write_config() {
    cat >"$1/ghostty.conf" <<'EOF_CONFIG'
font-family = Menlo
font-size = 13
scrollback-limit-bytes = 10000000
theme = light:Builtin Light,dark:Builtin Dark
cursor-style-blink = true
confirm-close-surface = false
clipboard-paste-protection = false
EOF_CONFIG
    if [ "${2:-}" = palette ]; then
        echo "window-padding-color = extend-always" >>"$1/ghostty.conf"
    fi
}

needs_footprint() {
    case " $FOOTPRINT_SCENARIOS " in
        *" $1 "*) return 0 ;;
    esac
    return 1
}

# Служба footprint процесса сценария (gpu.rs): запрос «<pid> <метка>» из FIFO footprint.fifo,
# JSON в footprint/<метка>.json (публикуется переименованием), вывод в footprint/<метка>.txt,
# ответ «<метка> <код>» в FIFO footprint.reply. Оба FIFO служба держит открытыми на чтение и
# запись: между запросами у FIFO запроса не пропадает читатель, а ответ не ждёт читателя.
footprint_service() {
    local out="$1" dir="$1/footprint" pid label code
    exec 3<>"$out/footprint.fifo" 4<>"$out/footprint.reply"
    while read -r pid label <&3; do
        if ! [[ "$pid" =~ ^[0-9]+$ && "$label" =~ ^[a-z0-9-]{1,64}$ ]]; then
            echo "неверный запрос: $pid $label" >>"$dir/service.txt"
            continue
        fi
        code=0
        /usr/bin/footprint -j "$dir/.$label.json" "$pid" >"$dir/$label.txt" 2>&1 || code=$?
        if [ "$code" -eq 0 ]; then
            mv "$dir/.$label.json" "$dir/$label.json" || code=1
        fi
        printf '%s %s\n' "$label" "$code" >&4
    done
}

start_footprint_service() {
    local out="$1"
    mkdir "$out/footprint"
    mkfifo "$out/footprint.fifo" "$out/footprint.reply"
    footprint_service "$out" &
    FOOTPRINT_SERVICE=$!
}

# Процесс пробы к этому моменту завершён, запроса в работе нет.
stop_footprint_service() {
    [ -n "$FOOTPRINT_SERVICE" ] || return 0
    kill "$FOOTPRINT_SERVICE" 2>/dev/null || true
    wait "$FOOTPRINT_SERVICE" 2>/dev/null || true
    FOOTPRINT_SERVICE=""
}

# Переменные варианта для --launch --set: scribble у teardown запускается с MallocScribble=1.
set_args_of() {
    case "$1:$2" in
        teardown:"$SCRIBBLE_VARIANT") echo "--set MallocScribble=1" ;;
    esac
}

# Код возврата — код --launch: ошибка запуска, процесса пробы или оставшиеся процессы.
run_scenario() {
    local scenario="$1" variant="$2" out="$3" code=0 set_args
    if needs_footprint "$scenario"; then
        start_footprint_service "$out"
    fi
    set_args="$(set_args_of "$scenario" "$variant")"
    # shellcheck disable=SC2086 # аргументы --set разделяются пробелами
    caffeinate -dimsu "$BIN" --launch --app "$APP" --route "$(route_of "$scenario")" $set_args \
        -- --scenario "$scenario" --variant "$variant" --out "$out" \
        >"$out/stdout.txt" 2>"$out/stderr.txt" || code=$?
    if needs_footprint "$scenario"; then
        stop_footprint_service
        rm -f "$out/footprint.fifo" "$out/footprint.reply"
    fi
    return "$code"
}

# Ненулевой код только при ошибке исполнения хотя бы одного процесса или агрегирования;
# fail и pending-manual на код не влияют, их показывает сводка выводов.
cmd_scenarios() {
    local scenarios="${*:-$ALL_SCENARIOS}"
    local run_dir scenario variant index out code total=0 failed=0 aggregate=0
    build
    bundle
    # bundle снимает свою ловушку EXIT, эта снимает службу footprint при прерывании прогона.
    trap stop_footprint_service EXIT
    mkdir -p "$OUT_BASE"
    run_dir="$OUT_BASE/$(date +%Y%m%d-%H%M%S)-$$"
    mkdir "$run_dir"
    # shellcheck disable=SC2086 # имена сценариев разделяются пробелами
    for scenario in $(startup_first $scenarios); do
        for variant in $(variants_of "$scenario"); do
            for index in $(runs_of "$scenario" "$variant"); do
                [ "$index" = _ ] && index=""
                out="$run_dir/$scenario-$variant${index:+-$index}"
                mkdir "$out"
                write_config "$out" "$scenario"
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

# Демонстрация с живой поверхностью: конфигурация libghostty в своём каталоге прогона.
cmd_demo() {
    local demo_dir
    build
    bundle
    mkdir -p "$OUT_BASE/demo"
    demo_dir="$OUT_BASE/demo/$(date +%Y%m%d-%H%M%S)-$$"
    mkdir "$demo_dir"
    write_config "$demo_dir" demo
    echo "демонстрация: конфигурация $demo_dir/ghostty.conf"
    "$BIN" --launch --app "$APP" -- --config "$demo_dir/ghostty.conf" "$@"
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
