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

echo "Tune OS password policy: tests passed"
