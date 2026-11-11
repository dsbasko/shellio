#!/usr/bin/env bash
# Подписывает бандл или бинарь постоянной идентичностью Shellio Dev (ADR-0019).
#
#   sign.sh <путь>
#
# Идентичность выбирается по SHA-1 сертификата из requirement.txt, а не по имени. Без неё скрипт
# завершается ошибкой: ad-hoc подпись и другая идентичность не подставляются.
# SHELLIO_SIGNING_KEYCHAIN задаёт keychain с идентичностью, например временный keychain CI;
# без неё идентичность ищется в списке поиска пользователя.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

BUNDLE_ID="io.github.dsbasko.shellio"
requirement="$here/requirement.txt"
keychain="${SHELLIO_SIGNING_KEYCHAIN:-}"

die() {
  echo "error: $*" >&2
  exit 1
}

[[ $# -eq 1 && -n "$1" ]] || {
  echo "usage: $0 <путь>" >&2
  exit 2
}
target="$1"
[[ -e "$target" ]] || die "$target не найден"

[[ -f "$requirement" ]] ||
  die "нет $requirement: идентичность Shellio Dev создаёт scripts/signing/create-identity.sh (README рядом)"
sha1="$(sed -n 's/.*certificate leaf = H"\([0-9a-fA-F]\{40\}\)".*/\1/p' "$requirement")"
[[ "$sha1" =~ ^[0-9a-fA-F]{40}$ ]] || die "в $requirement нет SHA-1 сертификата"
sha1="$(printf '%s' "$sha1" | tr '[:lower:]' '[:upper:]')"

keychain_args=()
[[ -z "$keychain" ]] || keychain_args=(--keychain "$keychain")

# Самоподписанный сертификат не доверен, поэтому идентичность ищется без -v.
identities="$(security find-identity -p codesigning ${keychain:+"$keychain"} 2>/dev/null)" ||
  die "security find-identity не выполняется${keychain:+ для $keychain}"
grep -q ") $sha1 \"" <<<"$identities" ||
  die "нет идентичности подписи с SHA-1 $sha1${keychain:+ в $keychain}; восстановление — scripts/signing/create-identity.sh --from-p12"

codesign --force --sign "$sha1" ${keychain_args[@]+"${keychain_args[@]}"} --identifier "$BUNDLE_ID" --timestamp=none \
  -r "$requirement" "$target" ||
  die "codesign не подписал $target идентичностью $sha1"
