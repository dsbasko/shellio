#!/bin/bash
# Сборка и запуск пробы P0.7 probe-review-api (ADR-0034).
#   run.sh fake [сценарий …]   столбец fake (make probe-review), forge-fake на loopback
#   run.sh live [сценарий …]   живые столбцы (make probe-review-live), HOSTS сужает столбцы
#   run.sh with-lock <команда> служебная: команда под замерной блокировкой с маркером старта
# Пары строятся по сценариям каждого столбца (SCENARIOS_<столбец>). Пустой список сценариев — все
# сценарии столбцов цели. SCENARIOS (аргументы) и HOSTS сужают пары; неизвестное имя или пустое
# пересечение — отказ до блокировки и сборки.
#
# Весь рецепт идёт под замерной блокировкой ${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}/measure.lock
# (/usr/bin/lockf -k -t 300, план P0.7, «Замерная блокировка»): уборка docker, сборка, процессы,
# агрегирование. Внутренняя команда под блокировкой сначала удаляет маркер старта: маркер на месте
# и код 75 — блокировка не взята («не начат»), маркера нет — код принадлежит рецепту.
#
# Коды выхода: 0 — без ошибок исполнения; 1 — ошибка исполнения; 2 — неверный вызов;
# 4 — «не начат»: замерная блокировка не взята за срок, повтор в следующей итерации.
#
# Каталог прогона target/probes/review-api/<время>-<pid>/: run.json (серия, SHA-256 бинаря, пары,
# уборка docker), каталог процесса <сценарий>-<столбец>/ на пару (result.json, stdout.txt,
# stderr.txt), done.json после последнего процесса, summary.json и summary.md агрегатора.
# SERIES объединяет вызовы в одну серию (по умолчанию — имя каталога прогона); серия собирается
# probe-review --aggregate --pairs "<столбец>:<сценарий> …" --out <каталог> <каталог прогона>….
# Процесс сценария получает временный SHELLIO_STATE_DIR, PROBE_REVIEW_SERIES и PROBE_REVIEW_BUILD.
#
# В начале рецепта docker info со сроком 10 с: доступен — удаляются контейнеры прежних прогонов
# с меткой shellio.probe.run-id, и их не должно остаться; недоступен — уборка «не наблюдалась»
# (not_observed), fake идёт дальше.
#
# Локальный GitLab CE (пары gitlab_ce цели live): gitlab/up.sh поднимает контейнер с меткой
# shellio.probe.run-id=<имя каталога прогона> и томами в <каталог прогона>/gitlab-ce/volumes,
# probe-review --gitlab-ce-provision настраивает его (токен root password grant, иначе
# gitlab-rails runner). Не поднялся (docker, память, образ, готовность) — пары gitlab_ce идут в
# skipped run.json с причиной (not_run); ошибка настройки — ошибка исполнения. Контейнер и тома
# удаляются в конце рецепта и после ошибки или сигнала (trap), итог — gitlab_ce в done.json.
# Процесс сценария gitlab_ce получает PROBE_REVIEW_GITLAB_CE (путь gitlab-ce.json) и
# PROBE_REVIEW_GITLAB_CE_TOKENS (токены ролей) окружением.
#
# Переменные для тестов: PROBE_REVIEW_LOCK_TIMEOUT (срок захвата, с), PROBE_REVIEW_DOCKER (команда
# docker), SHELLIO_CACHE_DIR (каталог блокировки).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SELF="$ROOT/spikes/probe-review-api/run.sh"
CARGO="${CARGO:-cargo}"
PACKAGE=probe-review-api
BIN="$ROOT/target/release/probe-review"
OUT_BASE="$ROOT/target/probes/review-api"

# Сценарии по столбцам; совпадают с probe-review --list <столбец> (проверяет тест tests/run_sh.rs).
FAKE_HOSTS="fake"
LIVE_HOSTS="github_cloud gitlab_cloud gitlab_ce ghes"
SCENARIOS_fake="smoke gh-local-create gh-pending-cycle gh-pending-foreign gh-anchors gh-viewed gl-draft-publish gl-foreign-draft gl-approve-sha gl-request-changes gl-anchors head-change journal-crash reconcile-ambiguous reconcile-foreign reconcile-deleted reconcile-paged partial capabilities"
SCENARIOS_github_cloud="gh-local-create gh-pending-cycle gh-pending-foreign gh-anchors gh-viewed reconcile-foreign capabilities"
SCENARIOS_gitlab_cloud="gl-draft-publish gl-foreign-draft gl-approve-sha gl-request-changes gl-anchors head-change journal-crash reconcile-ambiguous reconcile-deleted reconcile-paged partial capabilities"
SCENARIOS_gitlab_ce="gl-draft-publish gl-foreign-draft gl-request-changes gl-anchors gl-approve-sha head-change capabilities"
SCENARIOS_ghes="gh-local-create gh-pending-cycle gh-pending-foreign gh-anchors gh-viewed reconcile-foreign capabilities"

EXIT_ERROR=1
EXIT_USAGE=2
EXIT_NOT_STARTED=4
LOCK="${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}/measure.lock"
LOCK_TIMEOUT="${PROBE_REVIEW_LOCK_TIMEOUT:-300}"
DOCKER="${PROBE_REVIEW_DOCKER:-docker}"
DOCKER_DEADLINE=10
DOCKER_LABEL="shellio.probe.run-id"
UP="$ROOT/spikes/probe-review-api/gitlab/up.sh"
EXIT_ROOT_TOKEN=5

# Личный токен root, если хост отказал в password grant: токен генерирует GitLab, в аргументах
# процессов его нет, stdout — строка «probe-token:<токен>».
CE_ROOT_TOKEN_RUBY="$(
    cat <<'RUBY'
u = User.find_by_username!('root')
a = { name: 'probe-review-root', scopes: [:api], expires_at: 2.days.from_now.to_date }
a[:organization_id] = Organizations::Organization.first.id if PersonalAccessToken.column_names.include?('organization_id')
t = u.personal_access_tokens.create!(a)
puts "probe-token:#{t.token}"
RUBY
)"

# Состояние локального GitLab CE рецепта.
CE_STARTED=0
CE_RUN_ID=""
CE_DIR=""
CE_TOKENS=""
CE_STATUS=not_started
CE_REASON=""
CE_DOWN=not_started

die() {
    echo "run.sh: $*" >&2
    exit "$EXIT_ERROR"
}

usage() {
    echo "run.sh: $*" >&2
    echo "использование: run.sh fake [сценарий …] | run.sh live [сценарий …] | run.sh with-lock <команда>" >&2
    exit "$EXIT_USAGE"
}

# Команда со сроком: perl alarm переживает exec, по сроку команда получает SIGALRM.
deadline() {
    local secs="$1"
    shift
    /usr/bin/perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$secs" "$@"
}

contains() {
    case " $1 " in
        *" $2 "*) return 0 ;;
    esac
    return 1
}

scenarios_of() {
    local var="SCENARIOS_$1"
    printf '%s' "${!var}"
}

# Сужение оси: «select_axis <ось> <фильтр> <известные имена> <что>»; печатает пересечение.
select_axis() {
    local axis="$1" filter="$2" known="$3" what="$4" name selected=""
    for name in $filter; do
        contains "$known" "$name" || usage "неизвестное имя $what «${name}»"
    done
    for name in $axis; do
        if [ -z "$filter" ] || contains "$filter" "$name"; then
            selected="$selected${selected:+ }$name"
        fi
    done
    printf '%s' "$selected"
}

# Команда под замерной блокировкой с маркером старта (как scripts/ghostty/lock.sh).
with_lock() {
    local started status=0
    mkdir -p "$(dirname "$LOCK")" || die "не создан каталог блокировки $(dirname "$LOCK")"
    started="$(mktemp "${TMPDIR:-/tmp}/probe-review-lock.XXXXXX")"
    /usr/bin/lockf -k -t "$LOCK_TIMEOUT" "$LOCK" \
        /bin/sh -c 'rm -f "$1" && shift && exec "$@"' sh "$started" "$@" || status=$?
    if [ -e "$started" ]; then
        rm -f "$started"
        if [ "$status" -eq 75 ]; then
            echo "run.sh: не начат: замерная блокировка $LOCK занята дольше $LOCK_TIMEOUT с, повтор в следующей итерации" >&2
            exit "$EXIT_NOT_STARTED"
        fi
        die "не удалось взять блокировку $LOCK (lockf: код $status)"
    fi
    case "$status" in
        75 | "$EXIT_NOT_STARTED")
            die "рецепт под блокировкой вернул код $status: это ошибка исполнения, а не «не начат»"
            ;;
    esac
    return "$status"
}

json_string() {
    local text="$1"
    [[ ! "$text" =~ [[:cntrl:]] ]] || die "управляющий символ в строке для JSON: $text"
    text="${text//\\/\\\\}"
    text="${text//\"/\\\"}"
    printf '"%s"' "$text"
}

json_words() {
    local word first=1
    printf '['
    for word in $1; do
        [ "$first" -eq 1 ] || printf ', '
        first=0
        json_string "$word"
    done
    printf ']'
}

# Уборка контейнеров прежних прогонов; итог — DOCKER_STATUS, DOCKER_REMOVED, DOCKER_DETAIL.
docker_cleanup() {
    local ids left
    DOCKER_STATUS=not_observed
    DOCKER_REMOVED=0
    DOCKER_DETAIL=""
    if ! command -v "$DOCKER" >/dev/null 2>&1; then
        DOCKER_DETAIL="docker не установлен"
        return 0
    fi
    if ! deadline "$DOCKER_DEADLINE" "$DOCKER" info >/dev/null 2>&1; then
        DOCKER_DETAIL="docker info не ответил за $DOCKER_DEADLINE с"
        return 0
    fi
    ids="$(deadline "$DOCKER_DEADLINE" "$DOCKER" ps -aq --filter "label=$DOCKER_LABEL")" ||
        die "docker ps не перечислил контейнеры с меткой $DOCKER_LABEL"
    if [ -n "$ids" ]; then
        # shellcheck disable=SC2086 # ID контейнеров делятся по пробелам
        deadline 120 "$DOCKER" rm -f -v $ids >/dev/null ||
            die "контейнеры прежних прогонов не удалены: $(echo $ids)"
        DOCKER_REMOVED="$(echo "$ids" | wc -w | tr -d ' ')"
    fi
    left="$(deadline "$DOCKER_DEADLINE" "$DOCKER" ps -aq --filter "label=$DOCKER_LABEL")" ||
        die "docker ps не перечислил контейнеры после уборки"
    [ -z "$left" ] || die "после уборки остались контейнеры с меткой $DOCKER_LABEL: $(echo $left)"
    DOCKER_STATUS=cleaned
}

# Удаление контейнера и томов GitLab CE: из trap и в конце рецепта, повторный вызов ничего не
# делает. Итог — CE_DOWN: removed или failed с причиной в CE_REASON.
ce_down() {
    local ids left ok=1
    [ "$CE_STARTED" -eq 1 ] || return 0
    CE_STARTED=0
    if ids="$(deadline "$DOCKER_DEADLINE" "$DOCKER" ps -aq --filter "label=$DOCKER_LABEL=$CE_RUN_ID")"; then
        if [ -n "$ids" ]; then
            # shellcheck disable=SC2086 # ID контейнеров делятся по пробелам
            deadline 180 "$DOCKER" rm -f -v $ids >/dev/null || ok=0
        fi
        left="$(deadline "$DOCKER_DEADLINE" "$DOCKER" ps -aq --filter "label=$DOCKER_LABEL=$CE_RUN_ID")" || ok=0
        [ -z "${left:-}" ] || ok=0
    else
        ok=0
    fi
    [ -z "$CE_DIR" ] || rm -rf "$CE_DIR/volumes" || ok=0
    CE_TOKENS=""
    if [ "$ok" -eq 1 ]; then
        CE_DOWN=removed
        echo "GitLab CE: контейнер и тома удалены"
    else
        CE_DOWN=failed
        echo "run.sh: GitLab CE: удаление контейнера $CE_RUN_ID или томов не подтверждено" >&2
    fi
}

on_signal() {
    trap - INT TERM HUP
    exit "$1"
}

ce_root_token() {
    local out token
    out="$(deadline 600 "$DOCKER" exec "shellio-probe-review-$CE_RUN_ID"         gitlab-rails runner "$CE_ROOT_TOKEN_RUBY" 2>"$CE_DIR/rails-runner.log")" || return 1
    token="$(printf '%s\n' "$out" | sed -n 's/^probe-token://p' | tail -n 1)"
    [[ "$token" =~ ^[A-Za-z0-9._-]{16,}$ ]] || return 1
    printf '%s' "$token"
}

# Подъём и настройка GitLab CE; итог — CE_STATUS ready или not_run с CE_REASON.
ce_up() {
    local run_dir="$1" image password url code=0 tokens root_token
    CE_RUN_ID="$(basename "$run_dir")"
    CE_DIR="$run_dir/gitlab-ce"
    mkdir -p "$CE_DIR"
    if [ "$DOCKER_STATUS" != cleaned ]; then
        CE_STATUS=not_run
        CE_REASON="docker недоступен${DOCKER_DETAIL:+: $DOCKER_DETAIL}"
        return 0
    fi
    image="$("$BIN" --gitlab-ce-image)" || die "закрепление образа GitLab CE не разобрано"
    password="$(/usr/bin/openssl rand -hex 24)" || die "пароль root не создан"
    CE_STARTED=1
    GITLAB_ROOT_PASSWORD="$password" PROBE_REVIEW_DOCKER="$DOCKER" "$UP" "$image" "$CE_RUN_ID" "$CE_DIR" ||
        die "gitlab/up.sh: ошибка исполнения"
    if [ -f "$CE_DIR/not_run" ]; then
        CE_STATUS=not_run
        CE_REASON="$(head -n 1 "$CE_DIR/not_run")"
        return 0
    fi
    url="$(head -n 1 "$CE_DIR/url")"
    tokens="$(GITLAB_ROOT_PASSWORD="$password" "$BIN" --gitlab-ce-provision --url "$url" --out "$CE_DIR")" ||
        code=$?
    if [ "$code" -eq "$EXIT_ROOT_TOKEN" ]; then
        echo "GitLab CE: password grant отклонён, токен root через gitlab-rails runner"
        root_token="$(ce_root_token)" || die "gitlab-rails runner не создал токен root ($CE_DIR/rails-runner.log)"
        code=0
        tokens="$(PROBE_REVIEW_GITLAB_ROOT_TOKEN="$root_token" "$BIN" --gitlab-ce-provision \
            --url "$url" --out "$CE_DIR")" || code=$?
    fi
    [ "$code" -eq 0 ] || die "настройка GitLab CE: ошибка исполнения (код $code)"
    CE_TOKENS="$tokens"
    CE_STATUS=ready
}

# Рецепт под блокировкой: «locked <цель> <пары>».
locked() {
    local target="$1" pairs="$2"
    local run_dir run_name series build pair scenario column out state code
    local run_pairs="" ce_pairs="" skipped="" total=0 failed=0 aggregate=0
    mkdir -p "$OUT_BASE"
    run_name="$(date +%Y%m%d-%H%M%S)-$$"
    run_dir="$OUT_BASE/$run_name"
    mkdir "$run_dir"
    docker_cleanup
    echo "уборка docker: $DOCKER_STATUS${DOCKER_DETAIL:+ ($DOCKER_DETAIL)}, удалено $DOCKER_REMOVED"
    (cd "$ROOT" && "$CARGO" build --release -p "$PACKAGE" --locked --bin probe-review)
    [ -x "$BIN" ] || die "нет бинаря $BIN"
    build="$(shasum -a 256 <"$BIN")" || die "shasum не прочитал $BIN"
    build="${build%% *}"
    series="${SERIES:-$run_name}"
    [[ "$series" =~ ^[A-Za-z0-9._-]+$ ]] || usage "SERIES «${series}»: только буквы, цифры, точка, дефис и подчёркивание"
    for pair in $pairs; do
        if [ "${pair%%:*}" = gitlab_ce ]; then
            ce_pairs="$ce_pairs${ce_pairs:+ }$pair"
        fi
    done
    if [ -n "$ce_pairs" ]; then
        trap ce_down EXIT
        trap 'on_signal 130' INT
        trap 'on_signal 143' TERM
        trap 'on_signal 129' HUP
        ce_up "$run_dir"
        echo "GitLab CE: $CE_STATUS${CE_REASON:+ ($CE_REASON)}"
    fi
    for pair in $pairs; do
        if [ "${pair%%:*}" = gitlab_ce ] && [ "$CE_STATUS" != ready ]; then
            skipped="$skipped${skipped:+, }{\"pair\": $(json_string "$pair"), \"reason\": $(json_string "$CE_REASON")}"
        else
            run_pairs="$run_pairs${run_pairs:+ }$pair"
        fi
    done
    {
        printf '{\n'
        printf '  "schema": 1,\n'
        printf '  "series": %s,\n' "$(json_string "$series")"
        printf '  "build": "%s",\n' "$build"
        printf '  "target": "%s",\n' "$target"
        printf '  "started_unix": %s,\n' "$(date +%s)"
        printf '  "pairs": %s,\n' "$(json_words "$run_pairs")"
        printf '  "skipped": [%s],\n' "$skipped"
        printf '  "docker": {"status": "%s", "removed": %s, "detail": %s}\n' \
            "$DOCKER_STATUS" "$DOCKER_REMOVED" "$(json_string "$DOCKER_DETAIL")"
        printf '}\n'
    } >"$run_dir/run.json"
    for pair in $run_pairs; do
        column="${pair%%:*}"
        scenario="${pair#*:}"
        out="$run_dir/$scenario-$column"
        mkdir "$out"
        total=$((total + 1))
        state="$(mktemp -d "${TMPDIR:-/tmp}/probe-review-state.XXXXXX")"
        code=0
        if [ "$column" = gitlab_ce ]; then
            SHELLIO_STATE_DIR="$state" PROBE_REVIEW_SERIES="$series" PROBE_REVIEW_BUILD="$build" \
                PROBE_REVIEW_GITLAB_CE="$CE_DIR/gitlab-ce.json" PROBE_REVIEW_GITLAB_CE_TOKENS="$CE_TOKENS" \
                "$BIN" --scenario "$scenario" --column "$column" --out "$out" \
                >"$out/stdout.txt" 2>"$out/stderr.txt" || code=$?
        else
            SHELLIO_STATE_DIR="$state" PROBE_REVIEW_SERIES="$series" PROBE_REVIEW_BUILD="$build" \
                "$BIN" --scenario "$scenario" --column "$column" --out "$out" \
                >"$out/stdout.txt" 2>"$out/stderr.txt" || code=$?
        fi
        rm -rf "$state"
        if [ "$code" -eq 0 ]; then
            echo "ok      $pair"
        else
            failed=$((failed + 1))
            echo "ошибка  $pair (код $code)"
            sed 's/^/    /' "$out/stdout.txt" "$out/stderr.txt"
        fi
    done
    ce_down
    if [ "$CE_DOWN" = failed ]; then
        failed=$((failed + 1))
        echo "ошибка  уборка начатого контейнера GitLab CE не подтверждена"
    fi
    printf '{\n  "schema": 1,\n  "finished_unix": %s,\n  "errors": %s,\n  "gitlab_ce": {"status": "%s", "down": "%s"}\n}\n' \
        "$(date +%s)" "$failed" "$CE_STATUS" "$CE_DOWN" >"$run_dir/done.json"
    echo "прогон: $run_dir"
    echo
    "$BIN" --aggregate --pairs "$pairs" "$run_dir" || aggregate=$?
    [ "$failed" -eq 0 ] || die "ошибки исполнения: $failed (процессов $total)"
    [ "$aggregate" -eq 0 ] || die "агрегирование отметило ошибки исполнения (код $aggregate)"
}

# Публичная цель: пары проверяются до блокировки и сборки, рецепт идёт внутренней командой.
cmd_target() {
    local target="$1" hosts_axis hosts column name scenario known="" pairs=""
    shift
    case "$target" in
        fake) hosts_axis="$FAKE_HOSTS" ;;
        live) hosts_axis="$LIVE_HOSTS" ;;
    esac
    [ -z "${SERIES:-}" ] || [[ "$SERIES" =~ ^[A-Za-z0-9._-]+$ ]] ||
        usage "SERIES «${SERIES}»: только буквы, цифры, точка, дефис и подчёркивание"
    hosts="$(select_axis "$hosts_axis" "${HOSTS:-}" "$FAKE_HOSTS $LIVE_HOSTS" "столбца")"
    [ -n "$hosts" ] || usage "пустое пересечение столбцов цели $target ($hosts_axis) и HOSTS «${HOSTS:-}»"
    for column in $FAKE_HOSTS $LIVE_HOSTS; do
        known="$known $(scenarios_of "$column")"
    done
    for name in "$@"; do
        contains "$known" "$name" || usage "неизвестное имя сценария «${name}»"
    done
    for column in $hosts; do
        for scenario in $(scenarios_of "$column"); do
            if [ $# -eq 0 ] || contains "$*" "$scenario"; then
                pairs="$pairs${pairs:+ }$column:$scenario"
            fi
        done
    done
    [ -n "$pairs" ] || usage "пустое пересечение сценариев столбцов $hosts и SCENARIOS «$*»"
    with_lock "$SELF" locked "$target" "$pairs"
}

case "${1:-}" in
    fake | live)
        cmd_target "$@"
        ;;
    locked)
        shift
        [ $# -eq 2 ] || usage "locked ждёт цель и пары"
        locked "$@"
        ;;
    with-lock)
        shift
        [ $# -gt 0 ] || usage "with-lock ждёт команду"
        with_lock "$@"
        ;;
    *)
        usage "неизвестная команда «${1:-}»"
        ;;
esac
