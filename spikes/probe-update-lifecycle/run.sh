#!/bin/bash
# Сборка и запуск пробы P0.5 probe-update-lifecycle (ADR-0034).
#   run.sh bundle                  поколения 1, 2 и кандидат incompatible отдельными вызовами cargo
#                                  в свои каталоги target/probe-update-gen/<N>/ с параметрами
#                                  поколения; бандлы поколений, кандидатов tampered, foreign,
#                                  wrong-id и бандл-контроль .denied, подписанные Shellio Dev
#                                  (foreign — временной идентичностью во временной keychain), и
#                                  bundles.json в target/probes/update-lifecycle/bundle/
#   run.sh scenarios [сценарий …]  сборка бинарей пакета, для сценариев не из стенда ещё бандлы
#                                  (как run.sh bundle), затем каждый сценарий отдельным процессом
#                                  через --launch под caffeinate -dimsu (временный
#                                  SHELLIO_STATE_DIR создаёт --launch), выход в
#                                  target/probes/update-lifecycle/<время>-<pid>/, затем --aggregate
# Набор ghostty проба не читает и блокировку vendor/ghostty.lock не берёт: собирается только её
# пакет. Ненулевой код — только при ошибке исполнения; fail и not_run показывает сводка.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CARGO="${CARGO:-cargo}"
PACKAGE=probe-update-lifecycle
BIN="$ROOT/target/release/probe-update"
OUT_BASE="$ROOT/target/probes/update-lifecycle"
ALL_SCENARIOS="smoke host identity reject-verify owner-basic incompatible swap identity-live reject incompatible-activate window-update owner kill runtimes"
# Сценарии стенда без бандлов: для них run.sh scenarios бандлы не собирает.
STAND_SCENARIOS="smoke host"
# Каталоги cargo поколений: исполняемые файлы поколений различаются константами сборки.
GEN_TARGET="$ROOT/target/probe-update-gen"
BUNDLE_DIR="$OUT_BASE/bundle"
BUNDLE_BINS="probe-update-app probe-update-helper probe-update-host probe-update-attach probe-update-fixture"
BUNDLE_ID="io.github.dsbasko.shellio.probe.update-lifecycle"
APP_NAME="probe-update.app"
DENIED_APP_NAME="probe-update-denied.app"
REQUIREMENT="$ROOT/scripts/signing/requirement.txt"
CREATE_IDENTITY="$ROOT/scripts/signing/create-identity.sh"
KEYCHAIN="${SHELLIO_SIGNING_KEYCHAIN:-}"
TAMPERED_FILE="Contents/Resources/generation.json"

# Временные каталоги и keychain, которые убирает выход run.sh при любом исходе.
CLEANUP_DIRS=()
CLEANUP_KEYCHAINS=()
cleanup() {
    local keychain dir
    for keychain in ${CLEANUP_KEYCHAINS[@]+"${CLEANUP_KEYCHAINS[@]}"}; do
        [ ! -f "$keychain" ] || security delete-keychain "$keychain" 2>/dev/null || true
    done
    for dir in ${CLEANUP_DIRS[@]+"${CLEANUP_DIRS[@]}"}; do
        rm -rf "$dir"
    done
}
trap cleanup EXIT

die() {
    echo "run.sh: $*" >&2
    exit 1
}

lower() {
    printf '%s' "$1" | tr '[:upper:]' '[:lower:]'
}

# Строка JSON: пути и имена keychain без управляющих символов.
json_string() {
    local text="$1"
    [[ ! "$text" =~ [[:cntrl:]] ]] || die "управляющий символ в строке для JSON: $text"
    text="${text//\\/\\\\}"
    text="${text//\"/\\\"}"
    printf '"%s"' "$text"
}

file_sha256() {
    local sum
    sum="$(shasum -a 256 <"$1")" || die "shasum не прочитал $1"
    printf '%s' "${sum%% *}"
}

# SHA-1 сертификата Shellio Dev из designated requirement продукта (ADR-0019), заглавными.
signing_sha1() {
    local sha1
    [ -f "$REQUIREMENT" ] || die "нет $REQUIREMENT: идентичность Shellio Dev создаёт scripts/signing/create-identity.sh"
    sha1="$(sed -n 's/.*certificate leaf = H"\([0-9a-fA-F]\{40\}\)".*/\1/p' "$REQUIREMENT")"
    [[ "$sha1" =~ ^[0-9a-fA-F]{40}$ ]] || die "в $REQUIREMENT нет SHA-1 сертификата"
    printf '%s' "$sha1" | tr '[:lower:]' '[:upper:]'
}

# Идентичность подписи с SHA-1 «$1» есть в keychain «$2» (пусто — список поиска пользователя).
# Самоподписанный сертификат не доверен, поэтому идентичность ищется без -v.
require_identity() {
    local sha1="$1" keychain="$2" identities
    identities="$(security find-identity -p codesigning ${keychain:+"$keychain"} 2>/dev/null)" ||
        die "security find-identity не выполняется${keychain:+ для $keychain}"
    grep -q ") $sha1 \"" <<<"$identities" ||
        die "нет идентичности подписи с SHA-1 $sha1${keychain:+ в $keychain}; восстановление — scripts/signing/create-identity.sh --from-p12"
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

# Поколение «$1» с диапазонами протокола хоста «$2» и attach «$3»: бинари бандла отдельным вызовом
# cargo в свой каталог, константы сборки читает option_env!.
build_generation() {
    local generation="$1" host_proto="$2" attach_proto="$3" bin
    local bins=()
    for bin in $BUNDLE_BINS; do
        bins+=(--bin "$bin")
    done
    (cd "$ROOT" && PROBE_UPDATE_GENERATION="$generation" PROBE_UPDATE_HOST_PROTO="$host_proto" \
        PROBE_UPDATE_ATTACH_PROTO="$attach_proto" "$CARGO" build --release -p "$PACKAGE" --locked \
        --target-dir "$GEN_TARGET/$generation" "${bins[@]}")
}

# Подпись под своим идентификатором и своим designated requirement (ADR-0034, «Уточнено в P0.3»):
# «sign <путь> <идентификатор> <рабочий каталог> <SHA-1> <keychain или пусто>». Требование
# называет лист подписанта: у foreign это временная идентичность.
sign() {
    local target="$1" identifier="$2" work="$3" sha1="$4" keychain="$5" requirement req_file log details
    local keychain_args=()
    requirement="identifier \"$identifier\" and certificate leaf = H\"$(lower "$sha1")\""
    req_file="$work/requirement-$identifier.txt"
    log="$work/codesign.log"
    printf 'designated => %s\n' "$requirement" >"$req_file"
    [ -z "$keychain" ] || keychain_args=(--keychain "$keychain")
    codesign --force --sign "$sha1" ${keychain_args[@]+"${keychain_args[@]}"} --identifier "$identifier" \
        --timestamp=none -r "$req_file" "$target" >"$log" 2>&1 ||
        { sed 's/^/    /' "$log" >&2; die "codesign не подписал $target идентичностью $sha1"; }
    codesign --verify --strict --deep -R="$requirement" "$target" >"$log" 2>&1 ||
        { sed 's/^/    /' "$log" >&2; die "подпись $target не проходит проверку с требованием $requirement"; }
    details="$(codesign -d --verbose=2 "$target" 2>&1)" || die "codesign -d не читает подпись $target"
    [ "$(sed -n 's/^Identifier=//p' <<<"$details")" = "$identifier" ] ||
        die "идентификатор подписи $target не $identifier"
    if grep -q '^Signature=adhoc$' <<<"$details"; then
        die "подпись $target ad-hoc"
    fi
}

cdhash_of() {
    local hash
    hash="$(codesign -d --verbose=4 "$1" 2>&1 | sed -n 's/^CDHash=//p')"
    [[ "$hash" =~ ^[0-9a-f]{40}$ ]] || die "нет CDHash у $1"
    printf '%s' "$hash"
}

# Бандл из бинарей поколения сборки: «assemble <поколение> <основа идентификаторов> <CFBundleName>
# <путь бандла> <рабочий каталог> <SHA-1> <keychain или пусто>». Манифест печатает
# probe-update-app --manifest этого поколения, вспомогательные бинари подписываются своими
# идентификаторами <основа>.<часть> раньше бандла.
assemble() {
    local generation="$1" base="$2" name="$3" dest="$4" work="$5" sha1="$6" keychain="$7"
    local bins="$GEN_TARGET/$1/release" contents plist bin version
    contents="$dest/Contents"
    version="$(awk '
        /^\[/ { in_section = ($0 == "[workspace.package]") }
        in_section && /^version *= *"/ { sub(/^version *= *"/, ""); sub(/".*/, ""); print; exit }
    ' "$ROOT/Cargo.toml")"
    [ -n "$version" ] || die "в [workspace.package] Cargo.toml нет version"
    mkdir -p "$contents/MacOS" "$contents/Resources"
    for bin in $BUNDLE_BINS; do
        [ -x "$bins/$bin" ] || die "нет $bins/$bin поколения $generation"
        ditto --norsrc --noextattr --noacl "$bins/$bin" "$contents/MacOS/$bin" || die "не удалось скопировать $bin"
    done
    "$bins/probe-update-app" --manifest >"$contents/Resources/generation.json" ||
        die "probe-update-app поколения $generation не напечатал манифест"
    plist="$contents/Info.plist"
    ditto --norsrc --noextattr --noacl "$ROOT/spikes/probe-update-lifecycle/Info.plist" "$plist"
    plutil -replace CFBundleShortVersionString -string "$version" "$plist"
    plutil -replace CFBundleVersion -string "$generation" "$plist"
    plutil -replace CFBundleIdentifier -string "$base" "$plist"
    plutil -replace CFBundleName -string "$name" "$plist"
    plutil -lint "$plist" >/dev/null || die "Info.plist $dest не проходит plutil -lint"
    for bin in helper host attach fixture; do
        sign "$contents/MacOS/probe-update-$bin" "$base.$bin" "$work" "$sha1" "$keychain"
    done
    sign "$dest" "$base" "$work" "$sha1" "$keychain"
}

keychain_search_list() {
    security list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//'
}

default_keychain() {
    security default-keychain -d user | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//'
}

json_lines() {
    local line first=1
    printf '['
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        [ "$first" -eq 1 ] || printf ', '
        first=0
        json_string "$line"
    done <<<"$1"
    printf ']'
}

# Кандидат foreign: поколение 2 с теми же идентификаторами, подписанное временной идентичностью с
# тем же CN Shellio Dev и другим сертификатом. Идентичность создаёт scripts/signing/create-identity.sh
# во временной keychain с временными --requirement и --export вне репозитория; после подписи keychain
# удаляется. requirement.txt продукта, список поиска и keychain по умолчанию снимаются до создания,
# во время подписи (временная keychain ещё жива: delete-keychain убрал бы её и из списка поиска) и
# после удаления; расхождение с первым снимком — ошибка исполнения. «foreign_bundle <каталог бандлов> <рабочий каталог> <SHA-1
# закреплённой идентичности>»; SHA-1 временной идентичности — в FOREIGN_SHA1.
foreign_bundle() {
    local staging="$1" work="$2" pinned="$3" tmp keychain log kc_password p12_password sha1
    local req_before list_before default_before req_during list_during default_during
    local req_after list_after default_after
    req_before="$(file_sha256 "$REQUIREMENT")"
    list_before="$(keychain_search_list)"
    default_before="$(default_keychain)"
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/probe-update-foreign.XXXXXX")"
    tmp="$(cd "$tmp" && pwd -P)"
    CLEANUP_DIRS+=("$tmp")
    keychain="$tmp/foreign.keychain-db"
    log="$tmp/create-identity.log"
    kc_password="kc-$(uuidgen)"
    p12_password="p12-$(uuidgen)"
    CLEANUP_KEYCHAINS+=("$keychain")
    security create-keychain -p "$kc_password" "$keychain" || die "временная keychain не создана"
    security set-keychain-settings "$keychain" || die "настройки временной keychain не заданы"
    SHELLIO_KEYCHAIN_PASSWORD="$kc_password" SHELLIO_DEV_P12_PASSWORD="$p12_password" \
        "$CREATE_IDENTITY" --keychain "$keychain" --export "$tmp/foreign.p12" \
        --requirement "$tmp/requirement.txt" </dev/null >"$log" 2>&1 ||
        { sed 's/^/    /' "$log" >&2; die "create-identity.sh не создал временную идентичность"; }
    sha1="$(sed -n 's/^SHA-1: //p' "$log")"
    [[ "$sha1" =~ ^[0-9A-F]{40}$ ]] || die "create-identity.sh не напечатал SHA-1 временной идентичности"
    [ "$sha1" != "$pinned" ] || die "временная идентичность совпала с закреплённой"
    require_identity "$sha1" "$keychain"
    assemble 2 "$BUNDLE_ID" probe-update-app "$staging/foreign/$APP_NAME" "$work" "$sha1" "$keychain"
    req_during="$(file_sha256 "$REQUIREMENT")"
    list_during="$(keychain_search_list)"
    default_during="$(default_keychain)"
    security delete-keychain "$keychain" || die "временная keychain не удалена"
    rm -rf "$tmp"
    req_after="$(file_sha256 "$REQUIREMENT")"
    list_after="$(keychain_search_list)"
    default_after="$(default_keychain)"
    {
        printf '{\n'
        printf '  "leaf": "%s",\n' "$(lower "$sha1")"
        printf '  "requirement_sha256_before": "%s",\n' "$req_before"
        printf '  "requirement_sha256_during": "%s",\n' "$req_during"
        printf '  "requirement_sha256_after": "%s",\n' "$req_after"
        printf '  "search_list_before": %s,\n' "$(json_lines "$list_before")"
        printf '  "search_list_during": %s,\n' "$(json_lines "$list_during")"
        printf '  "search_list_after": %s,\n' "$(json_lines "$list_after")"
        printf '  "default_keychain_before": %s,\n' "$(json_string "$default_before")"
        printf '  "default_keychain_during": %s,\n' "$(json_string "$default_during")"
        printf '  "default_keychain_after": %s\n' "$(json_string "$default_after")"
        printf '}\n'
    } >"$staging/foreign-identity.json"
    [ "$req_before" = "$req_during" ] && [ "$req_before" = "$req_after" ] ||
        die "временная идентичность изменила $REQUIREMENT"
    [ "$list_before" = "$list_during" ] && [ "$list_before" = "$list_after" ] ||
        die "временная идентичность изменила список поиска keychain"
    [ "$default_before" = "$default_during" ] && [ "$default_before" = "$default_after" ] ||
        die "временная идентичность изменила keychain по умолчанию"
    FOREIGN_SHA1="$sha1"
}

# Запись bundles.json: «layout_entry <имя> <поколение> <путь в каталоге сборки> <окончательный путь>
# <идентификатор> <SHA-1 подписанта> [<tampered JSON>]».
layout_entry() {
    local name="$1" generation="$2" staged="$3" final="$4" identifier="$5" sha1="$6" tampered="${7:-}"
    printf '    {"name": "%s", "generation": %s, "app": %s, "identifier": "%s", "signer_leaf": "%s", "cdhash": "%s"%s}' \
        "$name" "$generation" "$(json_string "$final")" "$identifier" "$(lower "$sha1")" \
        "$(cdhash_of "$staged")" "${tampered:+, \"tampered\": $tampered}"
}

# Бандлы стенда (раздел плана «Поколения и кандидаты») собираются в каталоге рядом с bundle/ и
# заменяют его только целиком готовыми; путь — процессу сценария (launch::BUNDLES_DIR).
cmd_bundle() {
    local pinned staging work before after tampered entry index entries=()
    build_generation 1 1..1 1..1
    build_generation 2 1..2 1..2
    build_generation 3 2..2 2..2
    pinned="$(signing_sha1)"
    require_identity "$pinned" "$KEYCHAIN"
    mkdir -p "$OUT_BASE"
    staging="$(mktemp -d "$OUT_BASE/.bundle.XXXXXX")"
    CLEANUP_DIRS+=("$staging")
    work="$staging/.work"
    mkdir "$work"
    assemble 1 "$BUNDLE_ID" probe-update-app "$staging/gen-1/$APP_NAME" "$work" "$pinned" "$KEYCHAIN"
    assemble 2 "$BUNDLE_ID" probe-update-app "$staging/gen-2/$APP_NAME" "$work" "$pinned" "$KEYCHAIN"
    assemble 3 "$BUNDLE_ID" probe-update-app "$staging/incompatible/$APP_NAME" "$work" "$pinned" "$KEYCHAIN"
    mkdir "$staging/tampered"
    ditto --norsrc --noextattr --noacl "$staging/gen-2/$APP_NAME" "$staging/tampered/$APP_NAME"
    before="$(file_sha256 "$staging/tampered/$APP_NAME/$TAMPERED_FILE")"
    printf ' ' >>"$staging/tampered/$APP_NAME/$TAMPERED_FILE"
    after="$(file_sha256 "$staging/tampered/$APP_NAME/$TAMPERED_FILE")"
    [ "$before" != "$after" ] || die "tampered: файл не изменился"
    tampered="{\"file\": \"$TAMPERED_FILE\", \"sha256_before\": \"$before\", \"sha256_after\": \"$after\"}"
    assemble 2 "$BUNDLE_ID.wrong-id" probe-update-app "$staging/wrong-id/$APP_NAME" "$work" "$pinned" "$KEYCHAIN"
    assemble 1 "$BUNDLE_ID.denied" probe-update-denied "$staging/denied/$DENIED_APP_NAME" "$work" "$pinned" "$KEYCHAIN"
    FOREIGN_SHA1=""
    foreign_bundle "$staging" "$work" "$pinned"
    rm -rf "$work"
    # Каждая запись присваивается отдельно: отказ внутри $(…) завершает только подстановку.
    entry="$(layout_entry gen-1 1 "$staging/gen-1/$APP_NAME" "$BUNDLE_DIR/gen-1/$APP_NAME" "$BUNDLE_ID" "$pinned")"
    entries+=("$entry")
    entry="$(layout_entry gen-2 2 "$staging/gen-2/$APP_NAME" "$BUNDLE_DIR/gen-2/$APP_NAME" "$BUNDLE_ID" "$pinned")"
    entries+=("$entry")
    entry="$(layout_entry incompatible 3 "$staging/incompatible/$APP_NAME" "$BUNDLE_DIR/incompatible/$APP_NAME" "$BUNDLE_ID" "$pinned")"
    entries+=("$entry")
    entry="$(layout_entry tampered 2 "$staging/tampered/$APP_NAME" "$BUNDLE_DIR/tampered/$APP_NAME" "$BUNDLE_ID" "$pinned" "$tampered")"
    entries+=("$entry")
    entry="$(layout_entry foreign 2 "$staging/foreign/$APP_NAME" "$BUNDLE_DIR/foreign/$APP_NAME" "$BUNDLE_ID" "$FOREIGN_SHA1")"
    entries+=("$entry")
    entry="$(layout_entry wrong-id 2 "$staging/wrong-id/$APP_NAME" "$BUNDLE_DIR/wrong-id/$APP_NAME" "$BUNDLE_ID.wrong-id" "$pinned")"
    entries+=("$entry")
    entry="$(layout_entry denied 1 "$staging/denied/$DENIED_APP_NAME" "$BUNDLE_DIR/denied/$DENIED_APP_NAME" "$BUNDLE_ID.denied" "$pinned")"
    entries+=("$entry")
    {
        printf '{\n  "bundle_id": "%s",\n  "leaf": "%s",\n  "bundles": [\n' "$BUNDLE_ID" "$(lower "$pinned")"
        for index in "${!entries[@]}"; do
            printf '%s' "${entries[$index]}"
            [ "$index" -eq $((${#entries[@]} - 1)) ] || printf ','
            printf '\n'
        done
        printf '  ]\n}\n'
    } >"$staging/bundles.json"
    rm -rf "$BUNDLE_DIR"
    mv "$staging" "$BUNDLE_DIR"
    export SHELLIO_PROBE_BUNDLES="$BUNDLE_DIR"
    echo "бандлы стенда собраны и подписаны: $BUNDLE_DIR"
}

needs_bundles() {
    local scenario
    for scenario in "$@"; do
        case " $STAND_SCENARIOS " in
            *" $scenario "*) ;;
            *) return 0 ;;
        esac
    done
    return 1
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
    # shellcheck disable=SC2086 # список сценариев делится по пробелам
    if needs_bundles $scenarios; then
        cmd_bundle
    fi
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
    bundle)
        cmd_bundle
        ;;
    scenarios)
        shift
        cmd_scenarios "$@"
        ;;
    *)
        die "использование: run.sh bundle | run.sh scenarios [сценарий …]"
        ;;
esac
