#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tune-os-password.sh
source "${SCRIPT_DIR}/tune-os-password.sh"

first="$(generate_password)"
second="$(generate_password)"
[[ "$first" =~ ^[0-9a-f]{24}$ ]]
[[ "$second" =~ ^[0-9a-f]{24}$ ]]
[[ "$first" != "$second" ]]

# Generate the fixture with the same libc crypt(3) implementation that the
# policy probes. This remains portable to macOS; Linux validation separately
# exercises the SHA-512/yescrypt-family shadow form used by Debian images.
legacy_hash="$(perl -e 'print crypt("tune", "Tu")')"
password_matches_legacy "$legacy_hash"
if password_matches_legacy "$(perl -e 'print crypt("autre", "Tu")')"; then
    echo "un mot de passe personnalisé a été pris pour l'ancien défaut" >&2
    exit 1
fi

for builder in build-nuc-image.sh build-rpi4-image.sh build-sunxi-image.sh; do
    if grep -Eq "echo ['\"]tune:tune" "${SCRIPT_DIR}/${builder}"; then
        echo "${builder} contient encore l'identifiant public tune/tune" >&2
        exit 1
    fi
    grep -q 'tune-os-password --first-boot' "${SCRIPT_DIR}/${builder}"
    grep -q 'Requires=tune-first-boot-password.service' "${SCRIPT_DIR}/${builder}"
    grep -q 'chroot.*systemctl enable tune-first-boot-password.service' \
        "${SCRIPT_DIR}/${builder}"
done

# --premier-acces (#5617): runtime copy published while the password is due,
# removed with every other copy once shadow records a change. The policy is
# exercised on a copy whose absolute paths point into a throwaway tree, with
# getent stubbed; the real script is never pointed elsewhere.
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
sed -e "s#/var/lib/tune-os#${work}/state#" \
    -e "s#/etc/issue.d/#${work}/issue.d/#" \
    -e "s#/run/tune#${work}/run#" \
    "${SCRIPT_DIR}/tune-os-password.sh" > "${work}/policy.sh"
# A fresh bash: the policy sourced above already holds its readonly paths.
bash -s -- "$work" <<'PREMIER_ACCES'
set -euo pipefail
work="$1"
fail() { echo "premier-acces: $*" >&2; exit 1; }
# shellcheck source=/dev/null
source "${work}/policy.sh"
stub_last_change=0
getent() { printf 'tune:x:%s:0:99999:7:::\n' "$stub_last_change"; }
migrate_legacy() { :; }

publish_runtime
[[ ! -e "${work}/run/premier-mot-de-passe" ]] || fail "copie publiée sans mot de passe en attente"

install -d "${work}/state" "${work}/issue.d"
printf '%s\n' 0123456789abcdef01234567 > "${work}/state/initial-ssh-password"
: > "${work}/issue.d/90-tune-initial-password.issue"
premier_acces
cmp -s "${work}/state/initial-ssh-password" "${work}/run/premier-mot-de-passe" \
    || fail "mot de passe en attente non publié dans /run/tune"
mode="$(stat -c %a "${work}/run/premier-mot-de-passe" 2>/dev/null \
    || stat -f %Lp "${work}/run/premier-mot-de-passe")"
[[ "$mode" == 600 ]] || fail "copie de /run/tune lisible au-delà de root (mode $mode)"

stub_last_change=20366
premier_acces
[[ ! -e "${work}/run/premier-mot-de-passe" ]] || fail "copie de /run/tune restée après le changement"
[[ ! -e "${work}/state/initial-ssh-password" ]] || fail "secret initial resté après le changement"
[[ ! -e "${work}/issue.d/90-tune-initial-password.issue" ]] \
    || fail "avis de console resté après le changement"
PREMIER_ACCES

# main must be reached exactly as in production (#5617): the server pipes
# this file into `/bin/bash -s -- <mode>`; images call it by path. A probe
# mode stops at main's first check (root) or at its usage line: both prove
# main ran, neither touches the account.
for how in stdin path; do
    if [[ "$how" == stdin ]]; then
        out="$(/bin/bash -s -- --sonde < "${SCRIPT_DIR}/tune-os-password.sh" 2>&1)" || true
    else
        out="$(/bin/bash "${SCRIPT_DIR}/tune-os-password.sh" --sonde 2>&1)" || true
    fi
    if [[ "$out" == *"unbound variable"* ]] \
        || [[ "$out" != *"exécutée par root"* && "$out" != *"usage:"* ]]; then
        echo "politique lancée par ${how} : main jamais atteint (${out})" >&2
        exit 1
    fi
done

echo "Tune OS password policy: tests passed"
