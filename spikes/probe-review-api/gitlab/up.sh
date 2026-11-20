#!/bin/bash
# Подъём локального GitLab CE пробы P0.7 (план, «Локальный GitLab CE»). Вызывает только run.sh
# под замерной блокировкой; контейнер удаляет run.sh (trap), а не этот скрипт: контейнер живёт
# дольше up.sh — до конца процессов сценариев.
#
#   up.sh <образ> <run-id> <каталог подъёма>
#
# Окружение: GITLAB_ROOT_PASSWORD (пароль root прогона, в аргументы docker не попадает),
# PROBE_REVIEW_DOCKER (команда docker), PROBE_REVIEW_GITLAB_DEADLINE (срок готовности, с,
# по умолчанию 900), PROBE_REVIEW_PULL_DEADLINE (срок docker pull, с, по умолчанию 900).
#
# Итог в каталоге подъёма: url — корень инсталляции http://127.0.0.1:<порт>; not_run — причина
# not_run клеток gitlab_ce (docker недоступен, мало памяти, нет образа arm64, GitLab не
# поднялся); container.log — журнал контейнера, если GitLab не поднялся. Тома — volumes/{config,
# logs,data} в каталоге подъёма. Код 0 — url или not_run записан; другой код — ошибка исполнения.
set -euo pipefail

[ $# -eq 3 ] || {
    echo "up.sh: ждутся образ, run-id и каталог подъёма" >&2
    exit 2
}
IMAGE="$1"
RUN_ID="$2"
DIR="$3"
DOCKER="${PROBE_REVIEW_DOCKER:-docker}"
LABEL="shellio.probe.run-id"
NAME="shellio-probe-review-$RUN_ID"
PLATFORM="linux/arm64"
READY_DEADLINE="${PROBE_REVIEW_GITLAB_DEADLINE:-900}"
PULL_DEADLINE="${PROBE_REVIEW_PULL_DEADLINE:-900}"
# Свободная память ВМ docker не меньше 6 ГБ (план, «Предусловия»).
MIN_AVAILABLE_KB=$((6 * 1024 * 1024))
POLL=10
CURL=/opt/gitlab/embedded/bin/curl

[ -n "${GITLAB_ROOT_PASSWORD:-}" ] || {
    echo "up.sh: не задан GITLAB_ROOT_PASSWORD" >&2
    exit 1
}
[[ "$RUN_ID" =~ ^[A-Za-z0-9._-]+$ ]] || {
    echo "up.sh: run-id «$RUN_ID»" >&2
    exit 2
}
mkdir -p "$DIR"

deadline() {
    local secs="$1"
    shift
    /usr/bin/perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$secs" "$@"
}

not_run() {
    printf '%s\n' "$*" >"$DIR/not_run"
    echo "up.sh: not_run: $*" >&2
    exit 0
}

save_log() {
    deadline 60 "$DOCKER" logs --timestamps "$NAME" >"$DIR/container.log" 2>&1 || true
}

command -v "$DOCKER" >/dev/null 2>&1 || not_run "docker недоступен: команды docker нет"
deadline 10 "$DOCKER" info >/dev/null 2>&1 || not_run "docker недоступен: docker info не ответил за 10 с"

if ! deadline 30 "$DOCKER" image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "up.sh: docker pull $IMAGE" >&2
    deadline "$PULL_DEADLINE" "$DOCKER" pull --platform "$PLATFORM" "$IMAGE" >&2 ||
        not_run "образ GitLab CE не получен (docker pull)"
fi
arch="$(deadline 30 "$DOCKER" image inspect --format '{{.Os}}/{{.Architecture}}' "$IMAGE")" ||
    not_run "образ GitLab CE не читается (docker image inspect)"
[ "$arch" = "$PLATFORM" ] || not_run "нет образа arm64: локальный образ $arch"

# Свободная память ВМ: MemAvailable её /proc/meminfo из контейнера того же образа.
available="$(deadline 120 "$DOCKER" run --rm --label "$LABEL=$RUN_ID" --platform "$PLATFORM" \
    --entrypoint /bin/sh "$IMAGE" -c 'grep MemAvailable /proc/meminfo')" ||
    not_run "память ВМ docker не прочитана"
available="$(printf '%s' "$available" | awk '{print $2}')"
[[ "$available" =~ ^[0-9]+$ ]] || not_run "память ВМ docker не разобрана"
echo "up.sh: свободно в ВМ docker $((available / 1024)) МиБ" >&2
[ "$available" -ge "$MIN_AVAILABLE_KB" ] ||
    not_run "мало памяти ВМ docker: свободно $((available / 1024)) МиБ, нужно $((MIN_AVAILABLE_KB / 1024)) МиБ"

mkdir -p "$DIR/volumes/config" "$DIR/volumes/logs" "$DIR/volumes/data"
started="$(date +%s)"
# Порт на хосте выбирает docker (127.0.0.1::80): external_url не задаётся, nginx слушает 80.
# Проверки здоровья GitLab отвечают только адресам из monitoring_whitelist (127.0.0.0/8),
# поэтому /-/readiness опрашивается изнутри контейнера, а порт хоста — запросом API.
deadline 120 "$DOCKER" run -d --name "$NAME" --label "$LABEL=$RUN_ID" --platform "$PLATFORM" \
    --shm-size 256m -p 127.0.0.1::80 \
    -e GITLAB_ROOT_PASSWORD \
    -e GITLAB_OMNIBUS_CONFIG="prometheus_monitoring['enable'] = false" \
    -v "$DIR/volumes/config:/etc/gitlab" \
    -v "$DIR/volumes/logs:/var/log/gitlab" \
    -v "$DIR/volumes/data:/var/opt/gitlab" \
    "$IMAGE" >"$DIR/container.id"

port="$(deadline 30 "$DOCKER" port "$NAME" 80/tcp | head -n 1)"
[[ "$port" =~ ^127\.0\.0\.1:[0-9]+$ ]] || {
    echo "up.sh: порт контейнера не на 127.0.0.1: «$port»" >&2
    exit 1
}
url="http://$port"
echo "up.sh: GitLab CE $url, ждём готовности до $READY_DEADLINE с" >&2

while :; do
    state="$(deadline 30 "$DOCKER" inspect --format '{{.State.Status}}' "$NAME" 2>/dev/null || echo unknown)"
    if [ "$state" != running ]; then
        save_log
        not_run "GitLab не поднялся: контейнер в состоянии $state"
    fi
    if deadline 20 "$DOCKER" exec "$NAME" "$CURL" -fsS --max-time 10 \
        'http://localhost/-/readiness?all=1' >/dev/null 2>&1; then
        api="$(/usr/bin/curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$url/api/v4/version" || true)"
        if [ "$api" = 401 ] || [ "$api" = 200 ]; then
            break
        fi
    fi
    elapsed=$(($(date +%s) - started))
    if [ "$elapsed" -ge "$READY_DEADLINE" ]; then
        save_log
        not_run "GitLab не поднялся за $READY_DEADLINE с"
    fi
    sleep "$POLL"
done
echo "up.sh: GitLab CE готов за $(($(date +%s) - started)) с" >&2
printf '%s\n' "$url" >"$DIR/url"
