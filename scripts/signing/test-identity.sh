#!/usr/bin/env bash
# Тест create-identity.sh и пути CI на временных keychain.
#
# Каждый keychain создаётся во временном каталоге и в список поиска не добавляется; codesign получает его
# через --keychain. requirement пишется во временный каталог: scripts/signing/requirement.txt, login keychain
# и список поиска пользователя не меняются, тест это проверяет.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd -P)"
ident="$here/create-identity.sh"
NAME="Shellio Dev"
BUNDLE_ID="io.github.dsbasko.shellio"
openssl="${OPENSSL:-openssl}"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/shellio-test-identity.XXXXXX")"
tmp="$(cd "$tmp" && pwd -P)"
keychains=()
cleanup() {
  local kc
  for kc in ${keychains[@]+"${keychains[@]}"}; do
    [[ ! -f "$kc" ]] || security delete-keychain "$kc" 2>/dev/null || true
  done
  rm -rf "$tmp"
}
trap cleanup EXIT

out="$tmp/out"
checks=0
search_list_before="$(security list-keychains -d user)"
repo_requirement="$root/scripts/signing/requirement.txt"
repo_requirement_before="$(shasum -a 256 "$repo_requirement" 2>/dev/null || echo absent)"

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

kc_password="kc-$(uuidgen)"
p12_password="p12-$(uuidgen)"
export SHELLIO_KEYCHAIN_PASSWORD="$kc_password"
export SHELLIO_DEV_P12_PASSWORD="$p12_password"
unset SHELLIO_SIGNING_TEST_ABORT_AFTER_IMPORT

new_keychain() {
  local kc="$tmp/$1.keychain-db"
  security create-keychain -p "$kc_password" "$kc"
  keychains+=("$kc")
  # Без тайм-аута и блокировки при сне: keychain не закрывается посреди теста.
  security set-keychain-settings "$kc"
  printf '%s\n' "$kc"
}

cert_count() {
  local found
  found="$(security find-certificate -a -c "$NAME" "$1" 2>/dev/null || true)"
  grep -c '"labl"<blob>="Shellio Dev"' <<<"$found" || true
}

expect_no_identity() {
  [[ "$(cert_count "$1")" -eq 0 ]] || fail "$2: в keychain появился сертификат $NAME"
}

sha1_from_out() {
  sed -n 's/^SHA-1: //p' "$out"
}

requirement_for() {
  printf 'designated => identifier "%s" and certificate leaf = H"%s"\n' \
    "$BUNDLE_ID" "$(tr 'A-F' 'a-f' <<<"$1")"
}

# Подпись тестового бинаря по SHA-1 из указанного keychain, как это делает сборка бандла.
sign_probe() {
  local kc="$1" sha1="$2" req="$3" probe="$tmp/probe" info dr
  rm -f "$probe"
  cp /usr/bin/true "$probe"
  codesign --force --sign "$sha1" --keychain "$kc" --identifier "$BUNDLE_ID" --timestamp=none \
    -r "$req" "$probe" >"$out" 2>&1 || fail "подпись тестового бинаря ключом $sha1"
  codesign --verify --strict "$probe" >"$out" 2>&1 || fail "codesign --verify --strict тестового бинаря"
  info="$(codesign -dvvv "$probe" 2>&1)"
  grep -qx "Authority=$NAME" <<<"$info" || fail "подпись тестового бинаря не от $NAME"
  ! grep -q '^Signature=adhoc' <<<"$info" || fail "подпись тестового бинаря ad-hoc"
  dr="$(codesign -d -r- "$probe" 2>&1 | sed -n 's/^designated => /&/p')"
  [[ "$dr" == "$(csreq -r "$req" -t)" ]] || fail "requirement подписи «${dr}» не совпадает с $req"
}

kc1="$(new_keychain main)"
backup="$tmp/backup/shellio-dev.p12"
mkdir -p "$tmp/backup"
req1="$tmp/req1/requirement.txt"
mkdir -p "$tmp/req1"

inside="$root/scripts/signing/test-backup.p12"
expect_fail "резервная копия внутри репозитория" "внутри репозитория" \
  "$ident" --keychain "$kc1" --export "$inside" --requirement "$req1"
[[ ! -e "$inside" ]] || fail "отказ оставил $inside"
expect_no_identity "$kc1" "резервная копия внутри репозитория"

expect_fail "неверный пароль keychain" "не разблокирован" \
  env SHELLIO_KEYCHAIN_PASSWORD=wrong "$ident" --keychain "$kc1" --export "$backup" --requirement "$req1"
[[ ! -e "$backup" ]] || fail "отказ по паролю keychain оставил резервную копию"
expect_no_identity "$kc1" "неверный пароль keychain"

expect_fail "короткий пароль .p12" "короче" \
  env SHELLIO_DEV_P12_PASSWORD=short "$ident" --keychain "$kc1" --export "$backup" --requirement "$req1"
[[ ! -e "$backup" ]] || fail "отказ по паролю .p12 оставил резервную копию"

expect_ok "создание идентичности" "$ident" --keychain "$kc1" --export "$backup" --requirement "$req1"
sha1="$(sha1_from_out)"
[[ "$sha1" =~ ^[0-9A-F]{40}$ ]] || fail "SHA-1 не напечатан"
[[ "$(cat "$req1")" == "$(requirement_for "$sha1")" ]] || fail "requirement не описывает сертификат $sha1"
[[ "$(stat -f %Lp "$backup")" == 600 ]] || fail "резервная копия доступна не только владельцу"
[[ "$(cert_count "$kc1")" -eq 1 ]] || fail "в keychain не один сертификат $NAME"
ok "SHA-1, requirement и резервная копия"

cert="$tmp/cert.pem"
security find-certificate -c "$NAME" -p "$kc1" >"$cert"
ext="$("$openssl" x509 -in "$cert" -noout -ext basicConstraints,keyUsage,extendedKeyUsage)"
for want in 'CA:FALSE' 'Digital Signature' 'Code Signing'; do
  grep -qx "[[:space:]]*$want" <<<"$ext" || fail "в сертификате нет «${want}»"
done
"$openssl" x509 -in "$cert" -noout -checkend $((19 * 365 * 86400)) >/dev/null || fail "срок сертификата меньше 19 лет"
ok "назначение и срок сертификата"

existing="$tmp/backup/second.p12"
expect_fail "повторное создание отказывает" "уже есть сертификат" \
  "$ident" --keychain "$kc1" --export "$existing" --requirement "$tmp/req-second.txt"
[[ ! -e "$existing" ]] || fail "повторное создание записало резервную копию"
[[ "$(cert_count "$kc1")" -eq 1 ]] || fail "повторное создание добавило сертификат"

sign_probe "$kc1" "$sha1" "$req1"
ok "тестовый бинарь подписывается по SHA-1 и проходит --verify --strict"

# Путь CI: резервная копия импортируется во временный keychain только средствами security.
kc_ci="$(new_keychain ci)"
security import "$backup" -k "$kc_ci" -f pkcs12 -P "$p12_password" -T /usr/bin/codesign >"$out" 2>&1 ||
  fail "security import резервной копии"
security set-key-partition-list -S apple-tool:,apple: -s -k "$kc_password" "$kc_ci" >"$out" 2>&1 ||
  fail "partition list во временном keychain CI"
sign_probe "$kc_ci" "$sha1" "$req1"
# make bundle в CI подписывает через sign.sh с SHELLIO_SIGNING_KEYCHAIN; копия sign.sh читает req1 рядом с собой.
mkdir -p "$tmp/signing"
cp "$here/sign.sh" "$tmp/signing/sign.sh"
cp "$req1" "$tmp/signing/requirement.txt"
rm -f "$tmp/probe"
cp /usr/bin/true "$tmp/probe"
expect_ok "sign.sh подписывает из keychain CI" \
  env SHELLIO_SIGNING_KEYCHAIN="$kc_ci" "$tmp/signing/sign.sh" "$tmp/probe"
codesign --verify --strict "$tmp/probe" >"$out" 2>&1 || fail "подпись sign.sh не проходит codesign --verify --strict"
grep -qx "Authority=$NAME" <<<"$(codesign -dvvv "$tmp/probe" 2>&1)" || fail "подпись sign.sh не от $NAME"
[[ "$(codesign -d -r- "$tmp/probe" 2>&1 | sed -n 's/^designated => /&/p')" == "$(csreq -r "$req1" -t)" ]] ||
  fail "requirement подписи sign.sh не совпадает с $req1"
security delete-keychain "$kc_ci"
[[ ! -e "$kc_ci" ]] || fail "временный keychain CI не удалён"
ok "как в CI: .p12 импортируется во временный keychain, бинарь подписывается, keychain удаляется"

kc_part="$(new_keychain partial)"
req_part="$tmp/req-partial.txt"
expect_fail "настройка, прерванная после импорта" "прервана после импорта" \
  env SHELLIO_SIGNING_TEST_ABORT_AFTER_IMPORT=1 \
  "$ident" --keychain "$kc_part" --from-p12 "$backup" --requirement "$req_part"
grep -qF -- "--finish" "$out" || fail "отказ после импорта не подсказывает --finish"
[[ ! -e "$req_part" ]] || fail "прерванная настройка записала requirement"
expect_fail "повторное восстановление поверх импортированного ключа" "уже есть сертификат" \
  "$ident" --keychain "$kc_part" --from-p12 "$backup" --requirement "$req_part"
expect_ok "--finish завершает настройку" "$ident" --keychain "$kc_part" --finish --requirement "$req_part"
[[ "$(sha1_from_out)" == "$sha1" ]] || fail "--finish сообщил другой SHA-1"
cmp -s "$req_part" "$req1" || fail "--finish записал другой requirement"
[[ "$(cert_count "$kc_part")" -eq 1 ]] || fail "--finish добавил сертификат"
sign_probe "$kc_part" "$sha1" "$req_part"
ok "после --finish бинарь подписывается без нового сертификата"

kc_restore="$(new_keychain restore)"
expect_ok "восстановление из резервной копии" \
  "$ident" --keychain "$kc_restore" --from-p12 "$backup" --requirement "$req1"
[[ "$(sha1_from_out)" == "$sha1" ]] || fail "восстановление дало другой SHA-1"
[[ "$(cat "$req1")" == "$(requirement_for "$sha1")" ]] || fail "восстановление изменило requirement"

kc_wrong="$(new_keychain wrong-p12)"
expect_fail "неверный пароль резервной копии" "не читается" \
  env SHELLIO_DEV_P12_PASSWORD=wrong-password "$ident" --keychain "$kc_wrong" --from-p12 "$backup" \
  --requirement "$tmp/req-wrong.txt"
expect_no_identity "$kc_wrong" "неверный пароль резервной копии"

other_req="$tmp/req-other.txt"
requirement_for 0000000000000000000000000000000000000000 >"$other_req"
expect_fail "резервная копия другой идентичности" "другой сертификат" \
  "$ident" --keychain "$kc_wrong" --from-p12 "$backup" --requirement "$other_req"
expect_no_identity "$kc_wrong" "резервная копия другой идентичности"

expect_fail "создание при существующем requirement" "уже есть" \
  "$ident" --keychain "$kc_wrong" --export "$tmp/backup/third.p12" --requirement "$req1"
[[ ! -e "$tmp/backup/third.p12" ]] || fail "создание при существующем requirement записало резервную копию"
expect_no_identity "$kc_wrong" "создание при существующем requirement"

# Чужой ключ из .p12 получает ту же метку, и partition list нельзя было бы ограничить ключом Shellio Dev.
kc_shared="$(new_keychain shared)"
"$openssl" req -x509 -newkey rsa:2048 -noenc -days 1 -subj "/CN=Other" \
  -keyout "$tmp/other-key.pem" -out "$tmp/other-cert.pem" >"$out" 2>&1 || fail "чужой сертификат"
"$openssl" pkcs12 -export -name Other -inkey "$tmp/other-key.pem" -in "$tmp/other-cert.pem" \
  -passout pass:other -out "$tmp/other.p12" >"$out" 2>&1 || fail "чужой .p12"
security import "$tmp/other.p12" -k "$kc_shared" -f pkcs12 -P other >"$out" 2>&1 || fail "импорт чужого .p12"
expect_fail "чужой импортированный ключ в keychain" "Imported Private Key" \
  "$ident" --keychain "$kc_shared" --export "$tmp/backup/shared.p12" --requirement "$tmp/req-shared.txt"
[[ ! -e "$tmp/backup/shared.p12" ]] || fail "отказ из-за чужого ключа записал резервную копию"
expect_no_identity "$kc_shared" "чужой импортированный ключ в keychain"

# Если метка ключа не уникальна уже после импорта, partition list не меняется ни у одного ключа.
security import "$backup" -k "$kc_shared" -f pkcs12 -P "$p12_password" -T /usr/bin/codesign >"$out" 2>&1 ||
  fail "импорт резервной копии рядом с чужим ключом"
expect_fail "--finish при общей метке ключей" "нельзя ограничить" \
  "$ident" --keychain "$kc_shared" --finish --requirement "$tmp/req-shared.txt"
[[ ! -e "$tmp/req-shared.txt" ]] || fail "отказ при общей метке записал requirement"

# Путь владельца: оба пароля вводятся в терминале, пароль keychain запрашивает сам security.
kc_tty="$(new_keychain tty)"
cat >"$tmp/tty.exp" <<'EOF'
set timeout 120
lassign $argv script kc export req
spawn env -u SHELLIO_DEV_P12_PASSWORD -u SHELLIO_KEYCHAIN_PASSWORD $script --keychain $kc --export $export --requirement $req
expect {
  -re {p12[^:\n]*: $} { send "$env(TEST_P12_PASSWORD)\r"; exp_continue }
  "password to unlock" { send "$env(TEST_KC_PASSWORD)\r"; exp_continue }
  timeout { exit 124 }
  eof
}
lassign [wait] pid spawnid os_error value
exit $value
EOF
expect_ok "создание с паролями из терминала" \
  env TEST_P12_PASSWORD="$p12_password" TEST_KC_PASSWORD="$kc_password" \
  expect "$tmp/tty.exp" "$ident" "$kc_tty" "$tmp/backup/tty.p12" "$tmp/req-tty.txt"
tty_sha1="$(sha1_from_out | tr -d '\r')"
[[ "$tty_sha1" =~ ^[0-9A-F]{40}$ && "$tty_sha1" != "$sha1" ]] || fail "создание из терминала не дало новый SHA-1"
sign_probe "$kc_tty" "$tty_sha1" "$tmp/req-tty.txt"
ok "идентичность из терминала подписывает бинарь"

[[ "$(security list-keychains -d user)" == "$search_list_before" ]] || fail "список поиска keychain изменился"
[[ "$(shasum -a 256 "$repo_requirement" 2>/dev/null || echo absent)" == "$repo_requirement_before" ]] ||
  fail "тест изменил $repo_requirement"
ok "список поиска keychain и requirement репозитория не изменились"

echo "test-identity: $checks проверок пройдено"
