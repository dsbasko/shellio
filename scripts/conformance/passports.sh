#!/usr/bin/env bash
# Пересъём паспортов сверки с прототипом (ADR-0037): make passports [STATES="…"].
# Каждый паспорт снимается двумя свежими процессами браузера; набор публикуется, только если все
# выбранные ячейки сняты и оба съёма каждой совпали побайтно вместе с масками значков
# (<ячейка>.icons/<ключ>.png рядом с паспортом; прежние маски ячейки заменяются целиком). Несовпадение оставляет оба файла в
# target/passports/<время>-<pid>/ и паспорта репозитория не трогает. Непереснятые паспорта
# проверяются на годность. После набора пишется инвентарь обычного мира прототипа
# conformance/inventory.json (два запуска, побайтное сравнение). Коды: 0 — записано, 1 — ошибка исполнения, 2 — неверный вызов.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
CACHE=${SHELLIO_CACHE_DIR:-$HOME/.cache/shellio}
LOCK="$CACHE/passports.lock"

if [ -z "${SHELLIO_PASSPORTS_LOCKED:-}" ]; then
  mkdir -p "$CACHE"
  rc=0
  SHELLIO_PASSPORTS_LOCKED=1 /usr/bin/lockf -s -t 0 "$LOCK" "$0" "$@" || rc=$?
  if [ "$rc" = 75 ]; then
    echo "passports: блокировка $LOCK занята другим съёмом" >&2
    exit 1
  fi
  exit "$rc"
fi

cd "$ROOT"
"$ROOT/scripts/conformance/tools.sh" chrome >&2
CHROME=$("$ROOT/scripts/conformance/tools.sh" chrome-path)
cargo build --locked -q -p shellio-conformance --bin shellio-snap
SNAP="$ROOT/target/debug/shellio-snap"

read -r -a NAMES <<< "${STATES:-}"
rc=0
LIST=$("$SNAP" states --root "$ROOT" "${NAMES[@]+"${NAMES[@]}"}") || rc=$?
[ "$rc" = 0 ] || exit "$rc"
if [ -z "$LIST" ]; then
  echo "passports: в реестре нет состояний" >&2
  exit 2
fi

RUN="$ROOT/target/passports/$(date +%Y%m%d-%H%M%S)-$$"
mkdir -p "$RUN"
echo "passports: выход $RUN" >&2

while read -r state recipe cell surface; do
  mkdir -p "$RUN/$state"
  for shot in 1 2; do
    if ! "$SNAP" --chrome "$CHROME" --root "$ROOT" --recipe "$recipe" --cell "$cell" --surface "$surface" \
        --out "$RUN/$state/$cell.$shot.json" 2> "$RUN/$state/$cell.$shot.log"; then
      cat "$RUN/$state/$cell.$shot.log" >&2
      echo "passports: $state [$cell] съём $shot не удался" >&2
      exit 1
    fi
  done
  if ! cmp -s "$RUN/$state/$cell.1.json" "$RUN/$state/$cell.2.json"; then
    echo "passports: $state [$cell] два съёма разошлись: $RUN/$state/$cell.{1,2}.json" >&2
    exit 1
  fi
  if [ -d "$RUN/$state/$cell.1.icons" ] || [ -d "$RUN/$state/$cell.2.icons" ]; then
    if ! diff -r "$RUN/$state/$cell.1.icons" "$RUN/$state/$cell.2.icons" > /dev/null 2>&1; then
      echo "passports: $state [$cell] маски значков двух съёмов разошлись: $RUN/$state/$cell.{1,2}.icons" >&2
      exit 1
    fi
  fi
  echo "passports: $state [$cell] два съёма совпали" >&2
done <<< "$LIST"

while read -r state recipe cell surface; do
  dest="$ROOT/conformance/passports/$state"
  mkdir -p "$dest"
  cp "$RUN/$state/$cell.1.json" "$dest/.$cell.json.part"
  rm -rf "$dest/.$cell.icons.part" "$dest/.$cell.icons.old"
  if [ -d "$RUN/$state/$cell.1.icons" ]; then
    cp -R "$RUN/$state/$cell.1.icons" "$dest/.$cell.icons.part"
  fi
  if [ -d "$dest/$cell.icons" ]; then
    mv "$dest/$cell.icons" "$dest/.$cell.icons.old"
  fi
  if [ -d "$dest/.$cell.icons.part" ]; then
    mv "$dest/.$cell.icons.part" "$dest/$cell.icons"
  fi
  rm -rf "$dest/.$cell.icons.old"
  mv "$dest/.$cell.json.part" "$dest/$cell.json"
done <<< "$LIST"

"$SNAP" check --root "$ROOT"
echo "passports: набор опубликован" >&2

for shot in 1 2; do
  if ! "$SNAP" inventory --chrome "$CHROME" --root "$ROOT" --out "$RUN/inventory.$shot.json" 2> "$RUN/inventory.$shot.log"; then
    cat "$RUN/inventory.$shot.log" >&2
    echo "passports: инвентарь, запуск $shot не удался" >&2
    exit 1
  fi
done
if ! cmp -s "$RUN/inventory.1.json" "$RUN/inventory.2.json"; then
  echo "passports: два запуска инвентаря разошлись: $RUN/inventory.{1,2}.json" >&2
  exit 1
fi
cp "$RUN/inventory.1.json" "$ROOT/conformance/.inventory.json.part"
mv "$ROOT/conformance/.inventory.json.part" "$ROOT/conformance/inventory.json"
echo "passports: инвентарь записан" >&2
