#!/usr/bin/env bash
# Тест штампа, сверки, публикации и блокировки набора ghostty на поддельном наборе.
#
# Работает на копии scripts/ во временном каталоге, чтобы менять патчи и build.sh. Набор, кэш исходников
# и блокировка тоже временные: настоящий vendor/, кэш ~/.cache/shellio и сеть не используются.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
src_root="$(cd "$here/../.." && pwd)"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/shellio-test-publish.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

repo="$tmp/repo"
scripts="$repo/scripts/ghostty"
patches="$repo/scripts/ghostty-patches"
mkdir -p "$repo/scripts"
cp -R "$src_root/scripts/ghostty" "$scripts"
cp -R "$src_root/scripts/ghostty-patches" "$patches"

export SHELLIO_CACHE_DIR="$tmp/cache"
export SHELLIO_GHOSTTY_VENDOR="$tmp/vendor/ghostty"
unset SHELLIO_GHOSTTY_LOCKED SHELLIO_GHOSTTY_TEST_ABORT_BEFORE_STAMP
set_dir="$SHELLIO_GHOSTTY_VENDOR"
out="$tmp/out"
checks=0

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

vendor_sh() { "$scripts/vendor.sh" "$@"; }
lock_sh() { "$scripts/lock.sh" "$@"; }

# Поддельная ревизия лежит в кэше исходников под тем же ref, что использует build.sh.
mirror="$SHELLIO_CACHE_DIR/ghostty-src.git"
git init -q --bare "$mirror"
header_blob="$(printf 'fake ghostty header\n' | git --git-dir="$mirror" hash-object -w --stdin)"
include_tree="$(printf '100644 blob %s\tghostty.h\n' "$header_blob" | git --git-dir="$mirror" mktree)"
root_tree="$(printf '040000 tree %s\tinclude\n' "$include_tree" | git --git-dir="$mirror" mktree)"
fake_rev="$(GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.invalid \
  GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.invalid \
  GIT_AUTHOR_DATE='2026-01-01T00:00:00Z' GIT_COMMITTER_DATE='2026-01-01T00:00:00Z' \
  git --git-dir="$mirror" -c commit.gpgsign=false commit-tree "$root_tree" -m fake)"
git --git-dir="$mirror" update-ref "refs/shellio/pin/$fake_rev" "$fake_rev"
sed -i '' "s/^GHOSTTY_REV=.*/GHOSTTY_REV=$fake_rev/" "$scripts/pin"

# Поддельный каталог сборки в раскладке набора, со скрытым файлом и именем с пробелом, как у настоящего.
make_build() {
  local dir="$1"
  mkdir -p "$dir/lib" "$dir/include" "$dir/share/ghostty/shell-integration/zsh" \
    "$dir/share/ghostty/themes" "$dir/share/terminfo/78" "$dir/share/terminfo/67"
  printf 'object\n' >"$tmp/member"
  (cd "$tmp" && ar -rc "$dir/lib/libghostty.a" member 2>/dev/null)
  git --git-dir="$mirror" show "$fake_rev:include/ghostty.h" >"$dir/include/ghostty.h"
  printf 'zshenv\n' >"$dir/share/ghostty/shell-integration/zsh/.zshenv"
  printf 'palette = 0=#000000\n' >"$dir/share/ghostty/themes/Fake Dark"
  printf 'xterm-ghostty\n' >"$dir/share/terminfo/78/xterm-ghostty"
  printf 'ghostty\n' >"$dir/share/terminfo/67/ghostty"
  printf 'fake license\n' >"$dir/LICENSE"
}

build="$tmp/build"
make_build "$build"

# Каждый случай с порчей начинается со свежего набора, который проходит сверку.
fresh_set() {
  lock_sh "$scripts/vendor.sh" publish "$build" >"$out" 2>&1 || fail "публикация поддельного набора"
  vendor_sh --verify >"$out" 2>&1 || fail "сверка свежего набора"
}

stamp_now() { vendor_sh --print-stamp; }

# --- штамп ---

base_stamp="$(stamp_now)"
[[ "$(stamp_now)" == "$base_stamp" ]] || fail "штамп не повторяется"
ok "штамп повторяется"
for key in ghostty_rev ghostty_version_string zig_sha256 build_flags patches_sha256 build_sh_sha256 \
  arch developer_dir xcodebuild_version sdk_version sdk_build metal_version; do
  grep -q "^$key=." <<<"$base_stamp" || fail "в штампе нет $key"
done
grep -qx "ghostty_rev=$fake_rev" <<<"$base_stamp" || fail "в штампе не ревизия из pin"
ok "в штампе есть все входы сборки"

patch_files=("$patches"/*.patch)
real_patch="$(basename "${patch_files[0]}")"
cp "$patches/$real_patch" "$tmp/patch.orig"
cp "$scripts/build.sh" "$tmp/build.sh.orig"

expect_changed() {
  local desc="$1"
  [[ "$(stamp_now)" != "$base_stamp" ]] || fail "$desc не меняет штамп"
  ok "$desc меняет штамп"
}

expect_same() {
  local desc="$1"
  [[ "$(stamp_now)" == "$base_stamp" ]] || fail "$desc меняет штамп"
  ok "$desc не меняет штамп"
}

printf '\n' >>"$patches/$real_patch"
expect_changed "изменённый патч"
cp "$tmp/patch.orig" "$patches/$real_patch"
expect_same "восстановленный патч"

cp "$tmp/patch.orig" "$patches/0099-extra.patch"
expect_changed "добавленный патч"
rm "$patches/0099-extra.patch"

mv "$patches/$real_patch" "$patches/0000-renamed.patch"
expect_changed "переименованный патч"
mv "$patches/0000-renamed.patch" "$patches/$real_patch"

printf '# изменение рецепта\n' >>"$scripts/build.sh"
expect_changed "изменённый build.sh"
cp "$tmp/build.sh.orig" "$scripts/build.sh"

printf '\nправка\n' >>"$patches/README.md"
printf '# правка\n' >>"$scripts/tools.sh"
printf '# правка\n' >>"$scripts/lock.sh"
expect_same "правка README патчей, tools.sh и lock.sh"

mkdir -p "$tmp/bin"
cat >"$tmp/bin/xcrun" <<'EOF'
#!/bin/sh
[ "$1" = metal ] && exit 1
exec /usr/bin/xcrun "$@"
EOF
chmod +x "$tmp/bin/xcrun"
expect_fail "без Metal toolchain штамп не считается" "make setup" env PATH="$tmp/bin:$PATH" "$scripts/vendor.sh" --print-stamp

# --- публикация и сверка ---

expect_fail "сверка без набора" "make ghostty" vendor_sh --verify
expect_fail "публикация без маркера отказывает" "SHELLIO_GHOSTTY_LOCKED" \
  env -u SHELLIO_GHOSTTY_LOCKED "$scripts/vendor.sh" publish "$build"
[[ ! -e "$set_dir" ]] || fail "публикация без маркера создала набор"
ok "публикация без маркера ничего не создаёт"

fresh_set
ok "опубликованный набор проходит сверку"
cmp -s <(printf '%s\n' "$base_stamp") "$set_dir/stamp" || fail "штамп набора не равен --print-stamp"
grep -q '  share/ghostty/shell-integration/zsh/.zshenv$' "$set_dir/manifest" || fail "в манифесте нет скрытого файла"
grep -q '  share/ghostty/themes/Fake Dark$' "$set_dir/manifest" || fail "в манифесте нет файла с пробелом"
grep -qE '  (manifest|stamp)$' "$set_dir/manifest" && fail "манифест перечисляет manifest или stamp"
ok "манифест учитывает скрытые файлы и имена с пробелами"

expect_fail "публикация без маркера поверх набора отказывает" "SHELLIO_GHOSTTY_LOCKED" \
  env -u SHELLIO_GHOSTTY_LOCKED "$scripts/vendor.sh" publish "$build"
expect_ok "набор после отказа без маркера цел" vendor_sh --verify

fresh_set
rm "$set_dir/share/terminfo/78/xterm-ghostty"
expect_fail "удалённый terminfo" "нет файла: share/terminfo/78/xterm-ghostty" vendor_sh --verify

fresh_set
rm "$set_dir/share/ghostty/themes/Fake Dark"
expect_fail "удалённый файл с пробелом" "нет файла: share/ghostty/themes/Fake Dark" vendor_sh --verify

fresh_set
printf 'changed\n' >>"$set_dir/LICENSE"
expect_fail "изменённый файл" "изменён: LICENSE" vendor_sh --verify

fresh_set
printf 'extra\n' >"$set_dir/share/extra"
expect_fail "лишний файл" "лишний файл: share/extra" vendor_sh --verify

fresh_set
printf 'extra\n' >"$set_dir/.hidden"
expect_fail "лишний скрытый файл" "лишний файл: .hidden" vendor_sh --verify

fresh_set
ln -s LICENSE "$set_dir/LICENSE.link"
expect_fail "лишняя ссылка" "LICENSE.link" vendor_sh --verify

fresh_set
rm "$set_dir/manifest"
expect_fail "набор без манифеста" "нет манифеста" vendor_sh --verify

fresh_set
rm "$set_dir/stamp"
expect_fail "набор без штампа" "нет штампа" vendor_sh --verify

fresh_set
printf '\n' >>"$patches/$real_patch"
expect_fail "набор с устаревшим штампом" "штамп набора" vendor_sh --verify
cp "$tmp/patch.orig" "$patches/$real_patch"
expect_ok "возврат патча возвращает набор в силу" vendor_sh --verify

# --- прерванная и отклонённая публикация ---

fresh_set
build2="$tmp/build2"
make_build "$build2"
printf 'second license\n' >"$build2/LICENSE"
expect_fail "публикация, прерванная после шага 2" "прервана" \
  env SHELLIO_GHOSTTY_LOCKED=1 SHELLIO_GHOSTTY_TEST_ABORT_BEFORE_STAMP=1 "$scripts/vendor.sh" publish "$build2"
[[ ! -e "$set_dir/stamp" ]] || fail "прерванная публикация оставила старый штамп"
cmp -s "$build2/LICENSE" "$set_dir/LICENSE" || fail "прерванная публикация не дошла до замены содержимого"
ok "прерванная после шага 2 публикация оставляет набор без штампа"
expect_fail "набор после прерванной публикации не принимается" "нет штампа" vendor_sh --verify
fresh_set
ok "повторная публикация восстанавливает набор"

reject_build() {
  local desc="$1" reason="$2" dir="$3"
  fresh_set
  expect_fail "$desc" "$reason" lock_sh "$scripts/vendor.sh" publish "$dir"
  expect_ok "набор после отказа «${desc}» цел" vendor_sh --verify
}

bad="$tmp/bad-lib"
make_build "$bad"
rm "$bad/lib/libghostty.a"
reject_build "сборка без архива" "нет lib/libghostty.a" "$bad"

bad="$tmp/bad-ar"
make_build "$bad"
printf 'not an archive\n' >"$bad/lib/libghostty.a"
reject_build "архив не читается ar" "не читается ar" "$bad"

bad="$tmp/bad-terminfo"
make_build "$bad"
rm -r "$bad/share/terminfo/78"
reject_build "сборка без terminfo" "нет share/terminfo/78/xterm-ghostty" "$bad"

bad="$tmp/bad-themes"
make_build "$bad"
rm "$bad/share/ghostty/themes/Fake Dark"
reject_build "сборка без тем" "пуст share/ghostty/themes" "$bad"

bad="$tmp/bad-link"
make_build "$bad"
ln -s LICENSE "$bad/COPYING"
reject_build "сборка со ссылкой" "COPYING" "$bad"

bad="$tmp/bad-stamp"
make_build "$bad"
printf 'x\n' >"$bad/stamp"
reject_build "сборка с занятым именем stamp" "имя занято набором" "$bad"

bad="$tmp/bad-header"
make_build "$bad"
printf 'other header\n' >"$bad/include/ghostty.h"
reject_build "заголовок не из собранного дерева" "ghostty.h" "$bad"

cat >"$patches/0002-header.patch" <<'EOF'
diff --git a/include/ghostty.h b/include/ghostty.h
--- a/include/ghostty.h
+++ b/include/ghostty.h
@@ -1 +1,2 @@
 fake ghostty header
+patched line
EOF
expect_fail "заголовок без наложенного на него патча" "ghostty.h" lock_sh "$scripts/vendor.sh" publish "$build"
patched="$tmp/build-patched"
make_build "$patched"
printf 'patched line\n' >>"$patched/include/ghostty.h"
expect_ok "заголовок с наложенным патчем публикуется" lock_sh "$scripts/vendor.sh" publish "$patched"
rm "$patches/0002-header.patch"

inner="$tmp/inner"
make_build "$inner"
expect_fail "набор внутри каталога сборки" "внутри каталога сборки" \
  env SHELLIO_GHOSTTY_VENDOR="$inner/set" "$scripts/lock.sh" "$scripts/vendor.sh" publish "$inner"
[[ -f "$inner/LICENSE" ]] || fail "отказ задел каталог сборки"
outer="$tmp/outer"
make_build "$outer/build"
expect_fail "каталог сборки внутри набора" "внутри набора" \
  env SHELLIO_GHOSTTY_VENDOR="$outer" "$scripts/lock.sh" "$scripts/vendor.sh" publish "$outer/build"
[[ -f "$outer/build/LICENSE" ]] || fail "отказ задел каталог сборки внутри набора"

# --- блокировка ---

lock_sh sh -c 'printf "%s" "$SHELLIO_GHOSTTY_LOCKED"' >"$out" 2>&1 || fail "команда под блокировкой"
[[ "$(cat "$out")" == 1 ]] || fail "под блокировкой нет SHELLIO_GHOSTTY_LOCKED=1"
[[ -f "$set_dir.lock" ]] || fail "файл блокировки $set_dir.lock не создан или удалён"
ok "обёртка выставляет маркер и оставляет файл блокировки"

expect_fail "вторая обёртка при занятой блокировке" "занят" \
  lock_sh "$scripts/lock.sh" sh -c "touch '$tmp/inner-ran'"
[[ ! -e "$tmp/inner-ran" ]] || fail "команда второй обёртки выполнилась"
ok "команда второй обёртки не выполняется"

status=0
lock_sh sh -c 'exit 75' >"$out" 2>&1 || status=$?
[[ $status -eq 75 ]] || fail "код 75 команды не передан, получено $status"
grep -q "занят" "$out" && fail "код 75 команды выдан за занятую блокировку"
ok "код 75 самой команды не считается занятой блокировкой"

status=0
lock_sh sh -c 'exit 3' >"$out" 2>&1 || status=$?
[[ $status -eq 3 ]] || fail "код команды не передан, получено $status"
ok "обёртка передаёт код команды"

expect_ok "блокировка освобождается после команды" lock_sh true

echo "test-publish: $checks проверок прошли"
