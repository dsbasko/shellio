#!/usr/bin/env bash
# Тест наборов zmx пробы: штамп комбинации, сверка, атомарная публикация, блокировка и отказы.
#
# Работает на копии scripts/ и zmx-patches/ во временном каталоге. Исходники zmx и ghostty — поддельные
# ревизии в git-копиях временного кэша, патчи — поддельные под настоящими именами, Zig — поддельный:
# вместо сборки он компилирует крошечный zmx через cc и считает сборки. Настоящие наборы в target/,
# кэш ~/.cache/shellio и сеть не используются.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
src_probe="$(cd "$here/.." && pwd)"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/shellio-test-zmx.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

probe="$tmp/repo/spikes/probe"
scripts="$probe/scripts"
patches="$probe/zmx-patches"
mkdir -p "$probe"
cp -R "$src_probe/scripts" "$scripts"
cp -R "$src_probe/zmx-patches" "$patches"

export SHELLIO_CACHE_DIR="$tmp/cache"
export SHELLIO_PROBE_SETS_DIR="$tmp/sets"
# Любая попытка git уйти в сеть отказывает: разрешён только file://.
export GIT_ALLOW_PROTOCOL=file
export FAKE_ZIG_LOG="$tmp/zig-builds"
unset SHELLIO_PROBE_SET_LOCKED SHELLIO_PROBE_SET_TEST_ABORT_BEFORE_SWAP ZIG_MIRROR
sets="$SHELLIO_PROBE_SETS_DIR"
out="$tmp/out"
checks=0
: >"$FAKE_ZIG_LOG"

fail() {
  echo "FAIL: $*" >&2
  [[ ! -s "$out" ]] || sed 's/^/  | /' "$out" >&2
  exit 1
}

ok() {
  checks=$((checks + 1))
  echo "ok: $*"
}

expect_ok() {
  local desc="$1"
  shift
  "$@" >"$out" 2>&1 || fail "$desc: команда завершилась ошибкой"
  ok "$desc"
}

# Отказ засчитывается только с ожидаемой причиной, чтобы случай не прошёл из-за чужой ошибки.
expect_fail() {
  local desc="$1" reason="$2"
  shift 2
  if "$@" >"$out" 2>&1; then
    fail "$desc: ожидался отказ"
  fi
  grep -qF -- "$reason" "$out" || fail "$desc: в выводе нет «${reason}»"
  ok "$desc"
}

zmx_sh() { "$scripts/zmx.sh" "$@"; }
builds() { wc -l <"$FAKE_ZIG_LOG" | tr -d ' '; }

expect_builds() {
  local want="$1" desc="$2"
  [[ "$(builds)" == "$want" ]] || fail "$desc: сборок $(builds), ожидается $want"
  ok "$desc"
}

# --- настоящий pin: комбинации проходят свои правила без сети ---

for set in $("$src_probe/scripts/zmx.sh" sets); do
  "$src_probe/scripts/zmx.sh" --print-stamp "$set" >"$out" 2>&1 || fail "штамп настоящего набора $set"
done
ok "комбинации настоящего zmx-pin проходят правила"

# --- поддельные исходники ---

git_quiet() {
  GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.invalid \
    GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.invalid \
    GIT_AUTHOR_DATE='2026-01-01T00:00:00Z' GIT_COMMITTER_DATE='2026-01-01T00:00:00Z' \
    git -c commit.gpgsign=false -c init.defaultBranch=main "$@"
}

# Коммитит дерево $1 и кладёт его в git-копию $2 кэша под ref, который читает zmx.sh. Печатает ревизию.
commit_into_mirror() {
  local tree="$1" mirror="$2" rev
  git_quiet -C "$tree" add -A
  git_quiet -C "$tree" commit -q -m fake
  rev="$(git -C "$tree" rev-parse HEAD)"
  [[ -d "$mirror" ]] || git init -q --bare "$mirror"
  git_quiet -C "$tree" push -q "$mirror" "HEAD:refs/shellio/pin/$rev"
  printf '%s\n' "$rev"
}

gen="$tmp/gen"
mkdir -p "$gen/ghostty/src/terminal" "$gen/zmx/src"
git_quiet init -q "$gen/ghostty"
printf 'fake ghostty license\n' >"$gen/ghostty/LICENSE"
printf 'const formatter = 1;\n' >"$gen/ghostty/src/terminal/formatter.zig"
ghostty_rev="$(commit_into_mirror "$gen/ghostty" "$SHELLIO_CACHE_DIR/zmx/ghostty-src.git")"

git_quiet init -q "$gen/zmx"
printf 'fake zmx license\n' >"$gen/zmx/LICENSE"
cat >"$gen/zmx/build.zig.zon" <<EOF
.{
    .name = .zmx,
    .dependencies = .{
        .ghostty = .{
            .url = "git+https://github.com/ghostty-org/ghostty#$ghostty_rev",
            .hash = "fake",
        },
    },
}
EOF
printf 'const loop = 1;\nconst version = 1;\n' >"$gen/zmx/src/main.zig"
zmx_rev="$(commit_into_mirror "$gen/zmx" "$SHELLIO_CACHE_DIR/zmx/zmx-src.git")"

# Поддельный патч под настоящим именем: правка в дереве $1, diff против ревизии, затем откат.
make_patch() {
  local tree="$1" file="$2" edit="$3"
  (cd "$tree" && eval "$edit")
  git -C "$tree" add -A
  git -C "$tree" diff --cached >"$file"
  git -C "$tree" reset -q --hard HEAD
  [[ -s "$file" ]] || fail "пустой поддельный патч $file"
}

rm -f "$patches"/*.patch "$patches"/ghostty/*.patch
make_patch "$gen/ghostty" "$patches/ghostty/0001-formatter-unstyled-blanks.patch" \
  "printf 'const blanks = 1;\n' >>src/terminal/formatter.zig"
make_patch "$gen/zmx" "$patches/0001-explicit-leadership.patch" \
  "printf 'pub const role = 1;\n' >src/role.zig"
make_patch "$gen/zmx" "$patches/0002-private-ghostty-dependency.patch" \
  "sed -i '' -e 's|^            .url = .*|            .path = \"../zmx-ghostty\",|' -e '/\.hash = /d' build.zig.zon"
make_patch "$gen/zmx" "$patches/0003-atomic-socket-publish.patch" \
  "printf 'const staged = 1;\n' >src/socket.zig"
make_patch "$gen/zmx" "$patches/0004-managed-exit-keeps-screen.patch" \
  "printf 'const loop = 1;\nconst role = @import(\"role.zig\");\nconst version = 1;\n' >src/main.zig"

pin="$scripts/zmx-pin"
sed -i '' \
  -e "s|^ZMX_REV=.*|ZMX_REV=$zmx_rev|" \
  -e "s|^ZMX_GHOSTTY_REV=.*|ZMX_GHOSTTY_REV=$ghostty_rev|" \
  -e "s|^ZMX_REPO=.*|ZMX_REPO=https://zmx.invalid/zmx|" \
  -e "s|^ZMX_GHOSTTY_REPO=.*|ZMX_GHOSTTY_REPO=https://ghostty.invalid/ghostty|" \
  "$pin"
cp "$pin" "$tmp/pin.orig"
# shellcheck source=zmx-pin
source "$pin"

# Поддельный Zig установлен так, как его ставит zmx.sh: каталог версии с отметкой о сумме архива.
# Сборка компилирует zmx, чей `version` сообщает, лёг ли 0001 (role.zig) и ghostty-патч, а минимум
# macOS берёт из -Dtarget.
zig_home="$SHELLIO_CACHE_DIR/zmx/zig/$ZMX_ZIG_VERSION"
mkdir -p "$zig_home"
printf '%s\n' "$ZMX_ZIG_SHA256" >"$zig_home/.archive-sha256"
cat >"$tmp/fake-zmx.c" <<'EOF'
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "version") == 0) {
        printf("zmx\t\t%s\nghostty_vt\tfake-%s-%s\nsocket_dir\t/x\nlog_dir\t\t/y\n", ZMX_VERSION, ROLE, BLANKS);
        return 0;
    }
    return 1;
}
EOF
cat >"$zig_home/zig" <<EOF
#!/bin/bash
set -euo pipefail
case "\${1:-}" in
  version) echo "$ZMX_ZIG_VERSION" ;;
  build)
    shift
    [[ "\${1:-}" != --fetch ]] || exit 0
    [[ -d ../zmx-ghostty ]] || { echo "нет ../zmx-ghostty" >&2; exit 1; }
    minos=""
    for arg in "\$@"; do
      case "\$arg" in -Dtarget=aarch64-macos.*) minos="\${arg#-Dtarget=aarch64-macos.}" ;; esac
    done
    role=norole
    [[ ! -f src/role.zig ]] || role=role
    blanks=noblanks
    ! grep -q blanks ../zmx-ghostty/src/terminal/formatter.zig || blanks=blanks
    mkdir -p zig-out/bin
    cc -arch arm64 "-mmacosx-version-min=\$minos" -DZMX_VERSION='"$ZMX_VERSION_STRING"' \
      -DROLE="\"\$role\"" -DBLANKS="\"\$blanks\"" -o zig-out/bin/zmx "$tmp/fake-zmx.c"
    echo build >>"\$FAKE_ZIG_LOG"
    ;;
  *) exit 2 ;;
esac
EOF
chmod +x "$zig_home/zig"

# --- штамп ---

zmx_stamp="$(zmx_sh --print-stamp zmx)"
[[ "$(zmx_sh --print-stamp zmx)" == "$zmx_stamp" ]] || fail "штамп не повторяется"
prev_stamp="$(zmx_sh --print-stamp zmx-prev)"
[[ "$zmx_stamp" != "$prev_stamp" ]] || fail "у наборов одинаковый штамп"
grep -qx "zmx_set=$ZMX_SET_ZMX_PREV" <<<"$prev_stamp" || fail "в штампе zmx-prev не его комбинация"
! grep -q "^zmx_set_zmx" <<<"$prev_stamp" || fail "в штамп zmx-prev попала комбинация zmx"
for key in zmx_repo zmx_rev zmx_ghostty_rev zmx_zig_version zmx_zig_sha256 zmx_target zmx_requires \
  build_flags patches_sha256 ghostty_patches_sha256 recipe_sha256 arch developer_dir sdk_version sdk_build; do
  grep -q "^$key=." <<<"$zmx_stamp" || fail "в штампе нет $key"
done
ok "штамп повторяется, у каждого набора своя комбинация"

expect_fail "неизвестный набор" "неизвестный набор «nope»" zmx_sh --print-stamp nope
expect_fail "имя набора вне алфавита" "не из строчных букв" zmx_sh ensure ../x
[[ ! -e "$sets/../x.lock" ]] || fail "негодное имя набора создало блокировку"
expect_fail "сверка до публикации" "набор не опубликован" zmx_sh --verify zmx

# --- сборка, повтор и вторая комбинация ---

expect_ok "первая сборка zmx" zmx_sh ensure zmx
expect_builds 1 "первая сборка zmx собирает один раз"
expect_ok "сверка zmx" zmx_sh --verify zmx
[[ "$(zmx_sh version zmx)" == "zmx $ZMX_VERSION_STRING; ghostty_vt fake-role-blanks" ]] ||
  fail "версия zmx: $(zmx_sh version zmx)"
ok "zmx собран с 0001 и патчем ghostty"

expect_ok "повторный ensure zmx" zmx_sh ensure zmx
grep -qF "набор zmx актуален" "$out" || fail "повторный ensure не сообщил об актуальном наборе"
expect_builds 1 "повторный запуск не пересобирает"

expect_ok "сборка zmx-prev" zmx_sh ensure zmx-prev
expect_builds 2 "zmx-prev собирается отдельно"
[[ "$(zmx_sh version zmx-prev)" == "zmx $ZMX_VERSION_STRING; ghostty_vt fake-norole-blanks" ]] ||
  fail "версия zmx-prev: $(zmx_sh version zmx-prev)"
ok "zmx-prev собран без 0001 и с патчем ghostty"
expect_ok "zmx не тронут сборкой zmx-prev" zmx_sh --verify zmx

# --- сверка опубликованного набора ---

cp "$sets/zmx/stamp" "$tmp/zmx.stamp"
cp "$sets/zmx-prev/stamp" "$sets/zmx/stamp"
expect_fail "чужой штамп" "штамп набора $sets/zmx не совпадает" zmx_sh --verify zmx
expect_fail "version не читает набор с чужим штампом" "не сходится" zmx_sh version zmx
cp "$tmp/zmx.stamp" "$sets/zmx/stamp"
expect_ok "штамп восстановлен" zmx_sh --verify zmx

touch "$sets/zmx/bin/extra"
expect_fail "лишний файл" "лишний файл: bin/extra" zmx_sh --verify zmx
rm "$sets/zmx/bin/extra"

printf 'x' >>"$sets/zmx/LICENSE"
expect_fail "изменённый файл" "изменён: LICENSE" zmx_sh --verify zmx
mv "$sets/zmx/LICENSE.ghostty" "$tmp/LICENSE.ghostty"
expect_fail "недостающий файл" "нет файла: LICENSE.ghostty" zmx_sh --verify zmx
mv "$tmp/LICENSE.ghostty" "$sets/zmx/LICENSE.ghostty"
expect_ok "ensure пересобирает изменённый набор" zmx_sh ensure zmx
expect_builds 3 "изменённый набор собран заново"
expect_ok "пересобранный набор сходится" zmx_sh --verify zmx

ln -s bin/zmx "$sets/zmx/link"
expect_fail "ссылка в наборе" "не является обычным файлом" zmx_sh --verify zmx
rm "$sets/zmx/link"

# --- изменённые входы ---

cp "$patches/0004-managed-exit-keeps-screen.patch" "$tmp/0004.orig"
printf '\n' >>"$patches/0004-managed-exit-keeps-screen.patch"
expect_fail "изменённый патч 0004 устаревает zmx" "штамп набора $sets/zmx не совпадает" zmx_sh --verify zmx
expect_ok "изменённый патч 0004 не трогает zmx-prev" zmx_sh --verify zmx-prev
cp "$tmp/0004.orig" "$patches/0004-managed-exit-keeps-screen.patch"
expect_ok "восстановленный патч снова сходится" zmx_sh --verify zmx

cp "$patches/ghostty/0001-formatter-unstyled-blanks.patch" "$tmp/ghostty.orig"
printf '\n' >>"$patches/ghostty/0001-formatter-unstyled-blanks.patch"
expect_fail "патч ghostty устаревает zmx" "не совпадает" zmx_sh --verify zmx
expect_fail "патч ghostty устаревает zmx-prev" "не совпадает" zmx_sh --verify zmx-prev
cp "$tmp/ghostty.orig" "$patches/ghostty/0001-formatter-unstyled-blanks.patch"

printf '# изменение рецепта\n' >>"$scripts/zmx.sh"
expect_fail "изменённый рецепт" "не совпадает" zmx_sh --verify zmx
sed -i '' '$d' "$scripts/zmx.sh"
expect_ok "восстановленный рецепт" zmx_sh --verify zmx

# --- правила комбинаций до сборки ---

sed -i '' "s|^ZMX_SET_ZMX_PREV=.*|ZMX_SET_ZMX_PREV=\"0002-private-ghostty-dependency 0003-atomic-socket-publish 0004-managed-exit-keeps-screen\"|" "$pin"
expect_fail "0004 без 0001 отвергается в штампе" "патч 0004-managed-exit-keeps-screen требует 0001-explicit-leadership" \
  zmx_sh --print-stamp zmx-prev
expect_fail "0004 без 0001 отвергается до сборки" "требует 0001-explicit-leadership" zmx_sh ensure zmx-prev
sed -i '' "s|^ZMX_SET_ZMX_PREV=.*|ZMX_SET_ZMX_PREV=\"0004-managed-exit-keeps-screen 0001-explicit-leadership 0002-private-ghostty-dependency\"|" "$pin"
expect_fail "0001 после 0004 отвергается" "требует 0001-explicit-leadership раньше себя" zmx_sh ensure zmx-prev
sed -i '' "s|^ZMX_SET_ZMX_PREV=.*|ZMX_SET_ZMX_PREV=\"0002-private-ghostty-dependency 0002-private-ghostty-dependency\"|" "$pin"
expect_fail "повтор патча отвергается" "указан дважды" zmx_sh ensure zmx-prev
sed -i '' "s|^ZMX_SET_ZMX_PREV=.*|ZMX_SET_ZMX_PREV=\"0002-private-ghostty-dependency 0009-missing\"|" "$pin"
expect_fail "несуществующий патч отвергается" "нет патча zmx-patches/0009-missing.patch" zmx_sh ensure zmx-prev
expect_builds 3 "отвергнутые комбинации не собирались"
cp "$tmp/pin.orig" "$pin"
expect_ok "zmx-prev не тронут отказами" zmx_sh --verify zmx-prev

# Комбинация без 0002 накладывается и собирается, но берёт ghostty не из рабочей копии с патчем.
sed -i '' "s|^ZMX_SET_ZMX_PREV=.*|ZMX_SET_ZMX_PREV=\"0003-atomic-socket-publish\"|" "$pin"
expect_fail "комбинация без 0002 отвергается до сборки" "не из рабочей копии ../zmx-ghostty" zmx_sh ensure zmx-prev
expect_builds 3 "комбинация без 0002 не собиралась"
cp "$tmp/pin.orig" "$pin"

# --- применимость патчей к чистому дереву ---

cp "$patches/0003-atomic-socket-publish.patch" "$tmp/0003.orig"
cat >"$patches/0003-atomic-socket-publish.patch" <<'EOF'
diff --git a/src/main.zig b/src/main.zig
--- a/src/main.zig
+++ b/src/main.zig
@@ -1,2 +1,2 @@
-const nothing = 1;
+const staged = 1;
 const version = 1;
EOF
expect_fail "ненакладывающийся патч" "патч 0003-atomic-socket-publish.patch не накладывается на zmx $zmx_rev (после: 0002-private-ghostty-dependency.patch)" \
  zmx_sh ensure zmx-prev
expect_builds 3 "ненакладывающийся патч остановил сборку до Zig"
cp "$tmp/0003.orig" "$patches/0003-atomic-socket-publish.patch"
expect_ok "zmx-prev сходится после восстановления патча" zmx_sh --verify zmx-prev

# --- блокировка ---

expect_fail "занятая блокировка" "набор zmx занят" /usr/bin/lockf -k -t 0 "$sets/zmx.lock" "$scripts/zmx.sh" ensure zmx
expect_ok "блокировка zmx-prev не зависит от zmx" /usr/bin/lockf -k -t 0 "$sets/zmx.lock" "$scripts/zmx.sh" ensure zmx-prev
expect_builds 3 "отказ блокировки ничего не собрал"

# --- атомарная публикация ---

cp "$sets/zmx-prev/stamp" "$sets/zmx/stamp"
cp -R "$sets/zmx" "$tmp/zmx.before"
expect_fail "публикация прервана перед заменой" "прервана перед заменой" \
  env SHELLIO_PROBE_SET_TEST_ABORT_BEFORE_SWAP=1 "$scripts/zmx.sh" ensure zmx
diff -r "$tmp/zmx.before" "$sets/zmx" >/dev/null || fail "прерванная публикация изменила набор"
compgen -G "$sets/.zmx.new.*" >/dev/null || fail "прерванная публикация не оставила подготовленный набор"
ok "прерванная публикация не трогает опубликованный набор"
expect_ok "следующий ensure доводит публикацию" zmx_sh ensure zmx
expect_builds 5 "после прерванной публикации набор собран заново"
expect_ok "набор после прерванной публикации сходится" zmx_sh --verify zmx
! compgen -G "$sets/.zmx.*" >/dev/null || fail "остались каталоги прерванной публикации: $(ls -a "$sets")"
ok "остатки прерванной публикации убраны"

# --- сверка без Zig и без сети ---

mv "$zig_home" "$tmp/zig.away"
expect_ok "сверка без Zig" zmx_sh --verify zmx
expect_ok "ensure актуального набора без Zig" zmx_sh ensure zmx-prev
mv "$tmp/zig.away" "$zig_home"

# Ревизии нет в кэше: загрузка идёт по адресу из pin, а он недоступен.
sed -i '' "s|^ZMX_REV=.*|ZMX_REV=0123456789abcdef0123456789abcdef01234567|" "$pin"
expect_fail "ревизия не в кэше требует загрузки" "не удалось загрузить 0123456789abcdef0123456789abcdef01234567" zmx_sh ensure zmx
cp "$tmp/pin.orig" "$pin"
expect_builds 5 "неудачная загрузка ничего не собрала"

echo "test-zmx: $checks проверок пройдено"
