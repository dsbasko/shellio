#!/usr/bin/env bash
# Собирает демо-сцену в живом владельце штатными командами shellioctl: проект из корня репозитория в активном
# воркспейсе, рабочая область и панель каждого плагина реестра.
#
#   run-scene.sh build                      собирает shellioctl (release, --locked) и печатает путь бинаря
#   run-scene.sh apply <shellioctl> <сокет> собирает сцену во владельце сокета
#
# apply ничего не создаёт, если в профиле уже есть проект: постоянный профиль make run-keep получает сцену один
# раз. Панель плагина открывается, только если её ещё нет в раскладке рабочей области: навигатор и терминал
# создаёт раскладка по умолчанию, а повторный panel open плагина с кардинальностью multi дал бы вторую панель.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"

TIMEOUT_MS=5000
WORKBENCH_TITLE="main"

die() {
  echo "error: $*" >&2
  exit 1
}

usage() {
  echo "usage: $0 build | apply <shellioctl> <сокет>" >&2
  exit 2
}

build() {
  local cargo="${CARGO:-$HOME/.cargo/bin/cargo}" messages line exe=""
  # rustup и Cargo ищут rust-toolchain.toml и .cargo/config.toml от текущего каталога.
  cd "$root"
  messages="$("$cargo" build --release --locked -p shellioctl --manifest-path "$root/Cargo.toml" \
    --message-format=json-render-diagnostics)" || die "cargo build -p shellioctl завершился ошибкой"
  while IFS= read -r line; do
    [[ "$line" == *'"reason":"compiler-artifact"'* && "$line" != *'"executable":null'* ]] || continue
    [[ "$(plutil -extract target.name raw -o - - <<<"$line")" == shellioctl ]] || continue
    [[ "$(plutil -extract target.kind.0 raw -o - - <<<"$line")" == bin ]] || continue
    [[ -z "$exe" ]] || die "Cargo сообщил больше одного бинаря shellioctl"
    exe="$(plutil -extract executable raw -o - - <<<"$line")" || die "в сообщении Cargo о бинаре shellioctl нет пути"
  done <<<"$messages"
  [[ -n "$exe" ]] || die "Cargo не сообщил о собранном бинаре shellioctl"
  [[ -f "$exe" ]] || die "после сборки нет $exe"
  echo "$exe"
}

# Значения по пути ключей в JSON со stdin, по строке на значение; «*» раскрывает массив. Пути нет — пустой вывод.
jv() {
  /usr/bin/perl -MJSON::PP -e '
    local $/;
    my @nodes = (JSON::PP->new->utf8->decode(<STDIN>));
    for my $key (@ARGV) {
      @nodes = map {
        $key eq "*" ? (ref $_ eq "ARRAY" ? @$_ : ())
          : (ref $_ eq "HASH" && defined $_->{$key} ? ($_->{$key}) : ())
      } @nodes;
    }
    binmode STDOUT, ":utf8";
    print "$_\n" for grep { !ref } @nodes;
  ' "$@"
}

# Ответ shellioctl в RESP; отказ ядра или транспорта — ошибка с текстом ответа.
call() {
  local code=0
  RESP="$("$ctl" --socket "$sock" --json --timeout "$TIMEOUT_MS" "$@" 2>&1)" || code=$?
  [[ $code -eq 0 && -n "$(jv ok revision <<<"$RESP")" ]] ||
    die "shellioctl $*: код $code: $(tr '\n' ' ' <<<"$RESP")"
}

apply() {
  local workspace project workbench plugin present opened=""
  call tree --all
  if [[ -n "$(jv ok value workspaces '*' projects '*' id <<<"$RESP")" ]]; then
    echo "сцена: в профиле уже есть проект, ничего не создаю"
    return 0
  fi
  workspace="$(jv ok value active_workspace <<<"$RESP")"
  [[ -n "$workspace" ]] || die "в ответе tree нет активного воркспейса"

  call project add "$root" --title "$(basename "$root")" --workspace "$workspace"
  project="$(jv ok value project <<<"$RESP")"
  [[ -n "$project" ]] || die "в ответе project add нет проекта"
  call workbench new --target "$project" "$WORKBENCH_TITLE" --workspace "$workspace"
  workbench="$(jv ok value workbench <<<"$RESP")"
  [[ -n "$workbench" ]] || die "в ответе workbench new нет рабочей области"
  call workbench open --target "$workbench" --workspace "$workspace"

  call layout get --workbench "$workbench" --workspace "$workspace"
  present="$(jv ok value panels '*' plugin <<<"$RESP")"
  call plugin list
  while IFS= read -r plugin; do
    [[ -n "$plugin" ]] || continue
    grep -qxF "$plugin" <<<"$present" && continue
    # Плагин с кардинальностью per_app открывается только в области всех воркспейсов.
    call panel open "$plugin" --workbench "$workbench" --all
    opened="${opened:+$opened, }$plugin"
  done < <(jv ok value plugins '*' id <<<"$RESP")
  echo "сцена: рабочая область «${WORKBENCH_TITLE}» проекта $root, открыты панели: ${opened:-нет новых}"
}

case "${1:-}" in
build)
  [[ $# -eq 1 ]] || usage
  build
  ;;
apply)
  [[ $# -eq 3 ]] || usage
  ctl="$2"
  sock="$3"
  [[ -x "$ctl" ]] || die "нет исполняемого $ctl"
  apply
  ;;
*)
  usage
  ;;
esac
