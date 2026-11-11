#!/usr/bin/env bash
# Постоянная идентичность подписи Shellio Dev (ADR-0019): самоподписанный сертификат OpenSSL с назначением
# Code Signing и его закрытый ключ в keychain с доступом для /usr/bin/codesign.
#
#   create-identity.sh --export <файл.p12> [общие параметры]
#       создаёт ключ и сертификат, сохраняет резервную копию .p12 по пути вне репозитория, импортирует её
#   create-identity.sh --from-p12 <файл.p12> [общие параметры]
#       восстанавливает идентичность из резервной копии, новый сертификат не создаётся
#   create-identity.sh --finish [общие параметры]
#       завершает настройку уже импортированной идентичности после частичного сбоя
#
# Общие параметры:
#   --keychain <путь>     keychain, по умолчанию login keychain
#   --requirement <путь>  куда записать requirement, по умолчанию scripts/signing/requirement.txt
#
# Пароль .p12 берётся из SHELLIO_DEV_P12_PASSWORD, иначе запрашивается в терминале. Пароль keychain
# берётся из SHELLIO_KEYCHAIN_PASSWORD, иначе его запрашивает сам security. OPENSSL задаёт OpenSSL 3
# или новее, без неё используется openssl из PATH.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd -P)"

NAME="Shellio Dev"
BUNDLE_ID="io.github.dsbasko.shellio"
KEY_BITS=3072
DAYS=7305
P12_ITER=200000
P12_MIN_PASSWORD=12
PARTITIONS="apple-tool:,apple:"
# Такую метку macOS даёт каждому закрытому ключу, импортированному из .p12.
IMPORTED_KEY_LABEL="Imported Private Key"

after_import=""

die() {
  echo "error: $*" >&2
  [[ -z "$after_import" ]] ||
    echo "сертификат и ключ уже в keychain: настройку завершает create-identity.sh --finish (README)" >&2
  exit 1
}

usage() {
  sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

mode=""
p12=""
keychain=""
requirement="$root/scripts/signing/requirement.txt"

set_mode() {
  [[ -z "$mode" ]] || die "задан больше чем один режим"
  mode="$1"
}

while (($#)); do
  case "$1" in
    --export)
      set_mode create
      p12="${2:-}"
      shift 2 || usage
      ;;
    --from-p12)
      set_mode restore
      p12="${2:-}"
      shift 2 || usage
      ;;
    --finish)
      set_mode finish
      shift
      ;;
    --keychain)
      keychain="${2:-}"
      shift 2 || usage
      ;;
    --requirement)
      requirement="${2:-}"
      shift 2 || usage
      ;;
    -h | --help) usage ;;
    *) die "неизвестный параметр $1" ;;
  esac
done
[[ -n "$mode" ]] || usage
[[ "$mode" == finish || -n "$p12" ]] || die "не указан файл .p12"

[[ "$(uname -s)" == Darwin ]] || die "идентичность подписи создаётся только на macOS"

openssl="${OPENSSL:-openssl}"
openssl_version="$("$openssl" version 2>/dev/null)" || die "не найден $openssl"
# LibreSSL из /usr/bin шифрует .p12 устаревшими алгоритмами, поэтому нужен OpenSSL 3 или новее.
[[ "$openssl_version" =~ ^OpenSSL\ ([3-9]|[1-9][0-9]+)\. ]] ||
  die "нужен OpenSSL 3 или новее, найден «${openssl_version}» ($(command -v "$openssl")); задайте OPENSSL"

if [[ -z "$keychain" ]]; then
  keychain="$(security login-keychain | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//')"
fi
[[ -f "$keychain" ]] || die "keychain $keychain не найден"

req_dir="$(cd "$(dirname "$requirement")" 2>/dev/null && pwd -P)" ||
  die "каталог для requirement $(dirname "$requirement") не найден"
requirement="$req_dir/$(basename "$requirement")"

umask 077
tmp="$(mktemp -d "${TMPDIR:-/tmp}/shellio-identity.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

# Закрытые ключи keychain строками «SHA-1 открытого ключа<TAB>метка». Метка печатается в кавычках,
# если она текстовая; SHA-1 открытого ключа macOS хранит как application label ключа.
private_keys() {
  { security find-key -t private "$keychain" 2>/dev/null || true; } | awk '
    function flush() { if (seen) printf "%s\t%s\n", hash, label }
    /^keychain: / { flush(); seen = 1; hash = ""; label = "" }
    /^ *0x00000001 <blob>=/ { label = $0; sub(/^ *0x00000001 <blob>=/, "", label) }
    /^ *0x00000006 <blob>=0x/ { hash = $0; sub(/^ *0x00000006 <blob>=0x/, "", hash); sub(/ .*/, "", hash) }
    END { flush() }'
}

# Сертификаты keychain с CN ровно «Shellio Dev» в отдельных файлах; печатает их число.
# `find-certificate -c` ищет подстроку, поэтому имя сверяется по сертификату.
keychain_certs() {
  local dir="$1" all count=0 f
  mkdir -p "$dir"
  all="$dir/all.pem"
  security find-certificate -a -c "$NAME" -p "$keychain" >"$all" 2>/dev/null || true
  awk -v dir="$dir" '/BEGIN CERTIFICATE/ { n++ } n { print > (dir "/" n ".pem") }' "$all"
  for f in "$dir"/[0-9]*.pem; do
    [[ -f "$f" ]] || continue
    if [[ "$(subject_of "$f")" == "CN=$NAME" ]]; then
      count=$((count + 1))
      mv "$f" "$dir/match-$count.pem"
    fi
  done
  echo "$count"
}

subject_of() {
  "$openssl" x509 -in "$1" -noout -subject -nameopt RFC2253 | sed 's/^subject=//'
}

sha1_of() {
  "$openssl" x509 -in "$1" -noout -fingerprint -sha1 | sed -e 's/^.*=//' -e 's/://g' | tr 'a-f' 'A-F'
}

pubkey_hash_of() {
  "$openssl" x509 -in "$1" -noout -pubkey |
    "$openssl" rsa -pubin -RSAPublicKey_out -outform DER 2>/dev/null |
    shasum -a 1 | cut -d' ' -f1 | tr 'a-f' 'A-F'
}

# Сертификат обязан быть сертификатом подписи кода Shellio Dev: другой не попадёт в requirement.
check_cert() {
  local cert="$1" ext text
  [[ "$(subject_of "$cert")" == "CN=$NAME" ]] || die "субъект сертификата не CN=$NAME"
  ext="$("$openssl" x509 -in "$cert" -noout -ext basicConstraints,keyUsage,extendedKeyUsage)"
  grep -qx '[[:space:]]*CA:FALSE' <<<"$ext" || die "в сертификате нет basicConstraints CA:FALSE"
  grep -qx '[[:space:]]*Digital Signature' <<<"$ext" ||
    die "keyUsage сертификата не равен digitalSignature"
  grep -qx '[[:space:]]*Code Signing' <<<"$ext" || die "extendedKeyUsage сертификата не равен codeSigning"
  text="$("$openssl" x509 -in "$cert" -noout -text)"
  grep -q 'Public Key Algorithm: rsaEncryption' <<<"$text" || die "ключ сертификата не RSA"
  "$openssl" x509 -in "$cert" -noout -checkend 0 >/dev/null || die "срок действия сертификата истёк"
}

requirement_text() {
  printf 'designated => identifier "%s" and certificate leaf = H"%s"\n' \
    "$BUNDLE_ID" "$(tr 'A-F' 'a-f' <<<"$1")"
}

# requirement.txt описывает одну идентичность: другой сертификат его не переписывает.
check_requirement_file() {
  local expected="$1"
  [[ -e "$requirement" ]] || return 0
  [[ "$(cat "$requirement")" == "$expected" ]] ||
    die "$requirement описывает другой сертификат; новая идентичность меняет designated requirement (ADR-0019)"
}

refuse_existing_identity() {
  local count labels
  count="$(keychain_certs "$tmp/existing")"
  ((count == 0)) ||
    die "в $keychain уже есть сертификат «${NAME}», новый не создаётся; незавершённую настройку завершает --finish"
  # Partition list меняется только по метке ключа: фильтры -a, -D и -j у set-key-partition-list
  # не ограничивают выбор. Поэтому импортированный ключ должен остаться единственным с такой меткой.
  labels="$(private_keys | cut -f2)"
  if grep -qxF "\"$IMPORTED_KEY_LABEL\"" <<<"$labels"; then
    die "в $keychain уже есть закрытый ключ с меткой «${IMPORTED_KEY_LABEL}»: partition list нельзя ограничить ключом $NAME (README)"
  fi
}

unlock_keychain() {
  [[ -n "${SHELLIO_KEYCHAIN_PASSWORD:-}" ]] || return 0
  security unlock-keychain -p "$SHELLIO_KEYCHAIN_PASSWORD" "$keychain" ||
    die "keychain $keychain не разблокирован паролем из SHELLIO_KEYCHAIN_PASSWORD"
}

read_p12_password() {
  local confirm="$1" again
  if [[ -z "${SHELLIO_DEV_P12_PASSWORD:-}" ]]; then
    [[ -r /dev/tty ]] || die "задайте SHELLIO_DEV_P12_PASSWORD: терминала для запроса пароля нет"
    read -rs -p "пароль .p12: " SHELLIO_DEV_P12_PASSWORD </dev/tty
    echo >&2
    if [[ "$confirm" == confirm ]]; then
      read -rs -p "пароль .p12 ещё раз: " again </dev/tty
      echo >&2
      [[ "$again" == "$SHELLIO_DEV_P12_PASSWORD" ]] || die "пароли .p12 не совпадают"
    fi
  fi
  [[ -n "$SHELLIO_DEV_P12_PASSWORD" ]] || die "пароль .p12 пуст"
  export SHELLIO_DEV_P12_PASSWORD
}

# Резервная копия ключа никогда не попадает в репозиторий и не заменяет существующий файл.
resolve_export_path() {
  local dir
  dir="$(cd "$(dirname "$p12")" 2>/dev/null && pwd -P)" || die "каталог $(dirname "$p12") не найден"
  p12="$dir/$(basename "$p12")"
  case "$dir/" in
    "$root/"*) die "$p12 внутри репозитория: резервная копия ключа хранится вне него" ;;
  esac
  [[ ! -e "$p12" && ! -L "$p12" ]] || die "$p12 уже существует"
}

generate_identity() {
  local cnf="$tmp/cert.cnf"
  cat >"$cnf" <<EOF
[req]
distinguished_name = dn
prompt = no
x509_extensions = v3
[dn]
CN = $NAME
[v3]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
subjectKeyIdentifier = hash
EOF
  "$openssl" req -x509 -newkey "rsa:$KEY_BITS" -noenc -sha256 -days "$DAYS" -config "$cnf" \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" 2>"$tmp/req.log" ||
    { cat "$tmp/req.log" >&2; die "OpenSSL не создал ключ и сертификат"; }
  check_cert "$tmp/cert.pem"
  # Алгоритмы заданы явно: формат резервной копии не зависит от умолчаний версии OpenSSL.
  "$openssl" pkcs12 -export -name "$NAME" -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
    -keypbe AES-256-CBC -certpbe AES-256-CBC -macalg sha256 -iter "$P12_ITER" \
    -passout env:SHELLIO_DEV_P12_PASSWORD -out "$tmp/identity.p12"
  rm -f "$tmp/key.pem"
  # Резервная копия записывается до импорта, чтобы ключ ни в какой момент не жил только в keychain.
  (set -o noclobber && cat "$tmp/identity.p12" >"$p12") || die "не удалось записать $p12"
  chmod 600 "$p12"
  echo "резервная копия ключа: $p12"
}

cert_from_p12() {
  "$openssl" pkcs12 -in "$p12" -nokeys -clcerts -passin env:SHELLIO_DEV_P12_PASSWORD \
    -out "$tmp/cert.pem" 2>"$tmp/p12.log" ||
    { cat "$tmp/p12.log" >&2; die "$p12 не читается с этим паролем"; }
  check_cert "$tmp/cert.pem"
}

import_identity() {
  security import "$p12" -k "$keychain" -f pkcs12 -P "$SHELLIO_DEV_P12_PASSWORD" -T /usr/bin/codesign ||
    die "security не импортировал $p12 в $keychain"
  after_import=1
  if [[ -n "${SHELLIO_SIGNING_TEST_ABORT_AFTER_IMPORT:-}" ]]; then
    die "настройка прервана после импорта (SHELLIO_SIGNING_TEST_ABORT_AFTER_IMPORT)"
  fi
}

set_partition_list() {
  local hash="$1" line label matched pw_args=()
  line="$(private_keys | awk -F '\t' -v h="$hash" '$1 == h')"
  [[ -n "$line" ]] || die "в $keychain нет закрытого ключа сертификата $NAME"
  [[ "$(wc -l <<<"$line")" -eq 1 ]] || die "в $keychain несколько записей ключа $NAME"
  label="${line#*$'\t'}"
  [[ "$label" =~ ^\".+\"$ ]] || die "метка ключа $NAME не текстовая: $label"
  [[ "$(private_keys | awk -F '\t' -v l="$label" '$2 == l' | wc -l)" -eq 1 ]] ||
    die "метка $label есть у нескольких ключей $keychain: partition list нельзя ограничить ключом $NAME (README)"
  label="${label#\"}"
  label="${label%\"}"
  [[ -z "${SHELLIO_KEYCHAIN_PASSWORD:-}" ]] || pw_args=(-k "$SHELLIO_KEYCHAIN_PASSWORD")
  security set-key-partition-list -S "$PARTITIONS" -s -t private -l "$label" ${pw_args[@]+"${pw_args[@]}"} "$keychain" \
    >"$tmp/partition.log" 2>&1 ||
    { sed -n '/^security: /p' "$tmp/partition.log" >&2; die "partition list ключа $NAME не изменён"; }
  matched="$(awk '/^ *0x00000006 <blob>=0x/ { s = $0; sub(/^ *0x00000006 <blob>=0x/, "", s); sub(/ .*/, "", s); print s }' \
    "$tmp/partition.log")"
  [[ "$matched" == "$hash" ]] || die "set-key-partition-list затронул не только ключ $NAME: $(tr '\n' ' ' <<<"$matched")"
}

sign_probe() {
  local sha1="$1" probe="$tmp/probe" got want
  cp /usr/bin/true "$probe"
  codesign --force --sign "$sha1" --keychain "$keychain" --identifier "$BUNDLE_ID" --timestamp=none \
    -r "$requirement" "$probe" 2>"$tmp/sign.log" ||
    { cat "$tmp/sign.log" >&2; die "codesign не подписал тестовый бинарь ключом $sha1"; }
  codesign --verify --strict "$probe" || die "подпись тестового бинаря не проходит codesign --verify --strict"
  got="$(codesign -d -r- "$probe" 2>&1 | sed -n 's/^designated => /&/p')"
  want="$(csreq -r "$requirement" -t)"
  [[ "$got" == "$want" ]] || die "requirement подписи «${got}» не совпадает с «${want}»"
}

# Общий хвост всех режимов: идентичность уже в keychain.
finish_identity() {
  local count cert sha1 hash text identities
  count="$(keychain_certs "$tmp/installed")"
  ((count == 1)) || die "в $keychain $count сертификатов «${NAME}», ожидается один"
  after_import=1
  cert="$tmp/installed/match-1.pem"
  check_cert "$cert"
  sha1="$(sha1_of "$cert")"
  hash="$(pubkey_hash_of "$cert")"
  text="$(requirement_text "$sha1")"
  check_requirement_file "$text"
  identities="$(security find-identity -p codesigning "$keychain")"
  grep -qF " $sha1 " <<<"$identities" ||
    die "в $keychain нет закрытого ключа сертификата $sha1"
  set_partition_list "$hash"
  printf '%s\n' "$text" >"$tmp/requirement.txt"
  csreq -r "$tmp/requirement.txt" -t >/dev/null || die "csreq не принимает requirement"
  mv "$tmp/requirement.txt" "$requirement"
  chmod 644 "$requirement"
  sign_probe "$sha1"
  echo "идентичность $NAME готова в $keychain"
  echo "requirement: $requirement"
  echo "SHA-1: $sha1"
}

echo "OpenSSL: $openssl_version ($(command -v "$openssl"))"
case "$mode" in
  create)
    resolve_export_path
    [[ ! -e "$requirement" ]] ||
      die "$requirement уже есть: новая идентичность меняет designated requirement (ADR-0019), восстановление — --from-p12"
    refuse_existing_identity
    unlock_keychain
    read_p12_password confirm
    ((${#SHELLIO_DEV_P12_PASSWORD} >= P12_MIN_PASSWORD)) ||
      die "пароль .p12 короче $P12_MIN_PASSWORD символов"
    generate_identity
    import_identity
    finish_identity
    ;;
  restore)
    [[ -f "$p12" ]] || die "$p12 не найден"
    refuse_existing_identity
    unlock_keychain
    read_p12_password once
    cert_from_p12
    check_requirement_file "$(requirement_text "$(sha1_of "$tmp/cert.pem")")"
    import_identity
    finish_identity
    ;;
  finish)
    unlock_keychain
    finish_identity
    ;;
esac
