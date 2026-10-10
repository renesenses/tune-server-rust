#!/usr/bin/env bash
# #4770 — deux fabrications d'image sur un même hôte n'ont plus le même
# dossier de travail ; TUNE_OS_WORK_DIR le fixe quand on le demande.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib-work-dir.sh
source "${SCRIPT_DIR}/lib-work-dir.sh"

racine="$(mktemp -d)"
trap 'rm -rf "$racine"' EXIT
export TMPDIR="$racine"
unset TUNE_OS_WORK_DIR

# 1. Deux fabrications : deux dossiers distincts, chacun sous TMPDIR.
a="$(tune_os_work_dir tune-os-build)"
b="$(tune_os_work_dir tune-os-build)"
[[ -d "$a" && -d "$b" ]] || { echo "dossier de travail non créé" >&2; exit 1; }
[[ "$a" != "$b" ]] || { echo "deux fabrications partagent $a" >&2; exit 1; }
[[ "$a" == "$racine"/tune-os-build.* ]] || { echo "hors de TMPDIR : $a" >&2; exit 1; }

# 2. Le nettoyage de l'une ne touche pas l'autre.
touch "$b/temoin"
tune_os_work_dir_nettoyer "$a"
[[ ! -e "$a" ]] || { echo "dossier jetable non supprimé : $a" >&2; exit 1; }
[[ -e "$b/temoin" ]] || { echo "le nettoyage a effacé l'autre fabrication" >&2; exit 1; }

# 3. TUNE_OS_WORK_DIR fixe le dossier, et n'est jamais supprimé à la sortie.
fixe="$racine/fixe"
mkdir -p "$fixe"
[[ "$(TUNE_OS_WORK_DIR="$fixe/" tune_os_work_dir tune-os-build)" == "$fixe" ]]
TUNE_OS_WORK_DIR="$fixe" tune_os_work_dir_nettoyer "$fixe"
[[ -d "$fixe" ]] || { echo "TUNE_OS_WORK_DIR supprimé à la sortie" >&2; exit 1; }

# 4. Refus des racines qu'un rm -rf viderait, et des chemins relatifs.
for refuse in / /tmp /tmp/ relatif; do
    if TUNE_OS_WORK_DIR="$refuse" tune_os_work_dir tune-os-build >/dev/null 2>&1; then
        echo "TUNE_OS_WORK_DIR=$refuse accepté" >&2
        exit 1
    fi
done

# 5. Les trois scripts passent par la bibliothèque : plus de nom fixe.
for builder in build-nuc-image.sh build-rpi4-image.sh build-sunxi-image.sh; do
    f="${SCRIPT_DIR}/${builder}"
    if grep -Eq '^[[:space:]]*WORK_DIR="?/tmp/' "$f"; then
        echo "${builder} garde un WORK_DIR au nom fixe sous /tmp" >&2
        exit 1
    fi
    grep -q 'source "${SCRIPT_DIR}/lib-work-dir.sh"' "$f" \
        || { echo "${builder} ne charge pas lib-work-dir.sh" >&2; exit 1; }
    grep -Eq '^WORK_DIR="\$\(tune_os_work_dir tune-os-build[a-z-]*\)"' "$f" \
        || { echo "${builder} ne prend pas son WORK_DIR de tune_os_work_dir" >&2; exit 1; }
    grep -q 'tune_os_work_dir_nettoyer "\$WORK_DIR"' "$f" \
        || { echo "${builder} ne nettoie pas son dossier de travail" >&2; exit 1; }
done

echo "test-tune-os-work-dir : OK"
