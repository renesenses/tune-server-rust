#!/usr/bin/env bash
# #3210 — aucune image Tune OS n'écrit /etc/asound.conf.
#
# Le fichier épinglait la carte 0 sous un commentaire qui promettait l'USB ;
# sur un mini-PC la carte 0 est le HDA interne. Tune ouvre ses périphériques
# par nom et n'en avait pas besoin ; l'image RPi n'en a jamais eu. Ce témoin
# garde qu'aucun constructeur d'image ne le réintroduise.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Les lignes de commentaire ne comptent pas : une garde de texte satisfaite
# par le commentaire qui l'explique ne garde rien.
hors_commentaires() { grep -v '^[[:space:]]*#' "$1"; }

for builder in build-nuc-image.sh build-rpi4-image.sh build-sunxi-image.sh; do
    if hors_commentaires "${SCRIPT_DIR}/${builder}" | grep -q '/etc/asound\.conf'; then
        echo "${builder} écrit encore /etc/asound.conf (#3210)" >&2
        exit 1
    fi
    if hors_commentaires "${SCRIPT_DIR}/${builder}" | grep -Eq '^defaults\.(pcm|ctl)\.card'; then
        echo "${builder} épingle encore une carte ALSA par défaut (#3210)" >&2
        exit 1
    fi
done

echo "Tune OS asound.conf: tests passed"
