#!/bin/bash
# Сборка, подпись и запуск пробы P0.3 probe-ghostty-surface (ADR-0034).
#   run.sh scenarios [сценарий …]   бандл, затем каждый сценарий через --launch под caffeinate,
#                                   выход в target/probes/ghostty-surface/<время>-<pid>/, затем
#                                   --aggregate пишет summary.json и сводку выводов
#   run.sh demo [аргументы пробы …]  бандл, затем демонстрационная сцена через --launch
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
ALL_SCENARIOS="window"

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

# Варианты сценария: имя варианта — строчные буквы и цифры, оно входит в имя каталога.
variants_of() {
    echo "base"
}

# Код возврата — код --launch: ошибка запуска, процесса пробы или оставшиеся процессы.
run_scenario() {
    local scenario="$1" variant="$2" out="$3" code=0
    caffeinate -dimsu "$BIN" --launch --app "$APP" \
        -- --scenario "$scenario" --variant "$variant" --out "$out" \
        >"$out/stdout.txt" 2>"$out/stderr.txt" || code=$?
    return "$code"
}

# Ненулевой код только при ошибке исполнения хотя бы одного процесса или агрегирования;
# fail и pending-manual на код не влияют, их показывает сводка выводов.
cmd_scenarios() {
    local scenarios="${*:-$ALL_SCENARIOS}"
    local run_dir scenario variant out code total=0 failed=0 aggregate=0
    build
    bundle
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
    build
    bundle
    "$BIN" --launch --app "$APP" -- "$@"
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
