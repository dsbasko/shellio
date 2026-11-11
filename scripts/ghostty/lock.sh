#!/usr/bin/env bash
# Выполняет команду под блокировкой набора ghostty и с маркером SHELLIO_GHOSTTY_LOCKED=1.
#
#   lock.sh <команда> [аргументы]
#
# Блокировка — файл <корень набора>.lock вне заменяемого каталога, по умолчанию vendor/ghostty.lock.
# Файл не удаляется. Занятая блокировка не ожидается: вторая цель сразу завершается ошибкой.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"

vendor="${SHELLIO_GHOSTTY_VENDOR:-$root/vendor/ghostty}"

die() {
  echo "error: $*" >&2
  exit 1
}

[[ $# -gt 0 ]] || {
  echo "usage: $0 <команда> [аргументы]" >&2
  exit 2
}

# Путь блокировки строится так же, как корень набора в vendor.sh publish: физический родитель и имя каталога.
# Поэтому лишние косые черты в конце пути не уводят блокировку внутрь набора.
name="$(basename "$vendor")"
[[ "$name" != / && "$name" != . && "$name" != .. ]] || die "путь набора $vendor должен оканчиваться именем каталога"
mkdir -p "$(dirname "$vendor")"
parent="$(cd "$(dirname "$vendor")" && pwd -P)"
lock="${parent%/}/$name.lock"

# Код 75 занятой блокировки совпадает с EX_TEMPFAIL команды, поэтому команда сначала удаляет отметку о старте.
started="$(mktemp "${TMPDIR:-/tmp}/shellio-ghostty-lock.XXXXXX")"
trap 'rm -f "$started"' EXIT

status=0
SHELLIO_GHOSTTY_LOCKED=1 /usr/bin/lockf -s -k -t 0 "$lock" \
  /bin/sh -c 'rm -f "$1" && shift && exec "$@"' sh "$started" "$@" || status=$?

if [[ -e "$started" ]]; then
  if [[ $status -eq 75 ]]; then
    die "набор ghostty занят другой целью: блокировку $lock держит другой процесс, повторите после его завершения"
  fi
  die "не удалось взять блокировку $lock (lockf завершился с кодом $status)"
fi
exit "$status"
