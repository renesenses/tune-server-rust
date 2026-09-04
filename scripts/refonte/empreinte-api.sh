#!/usr/bin/env bash
# REF-0 — Empreinte de l'API publique des caisses du cœur, sans nightly.
#
# `cargo public-api` demande une chaîne nightly et n'est pas installé ; on
# relève donc les items `pub` (y compris pub(crate) / pub(super)) par lecture
# du source. Ce n'est pas une preuve de compatibilité, c'est un DÉTECTEUR :
# pour une PR de déplacement pur, l'ensemble des signatures doit être
# identique — seul le fichier porteur change.
#
# Usage :   scripts/refonte/empreinte-api.sh <dossier_de_sortie> [ref]
#   ref : commit à inspecter (défaut HEAD), sans changer de branche.
#
# Sortie dans <dossier> :
#   api-signatures.txt   — « visibilité item signature », trié, dédoublonné
#                          (indépendant du fichier : c'est LUI qu'on compare)
#   api-par-fichier.txt  — « fichier<TAB>signature », pour situer un écart
#
# Limites assumées : une signature sur plusieurs lignes est coupée à la
# première ; les items sous #[cfg(test)] sont comptés (ils bougent avec le
# code, l'ensemble reste stable). Deux items homonymes dans deux modules ne
# font qu'une ligne dans api-signatures.txt — api-par-fichier.txt les sépare.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
out="${1:?dossier de sortie requis}"
ref="${2:-HEAD}"
mkdir -p "$out"

caisses=(tune-core/src tune-server/src)

git grep -n -E '^[[:space:]]*pub(\([a-z]+\))?[[:space:]]+(async[[:space:]]+|unsafe[[:space:]]+|const[[:space:]]+|extern[[:space:]]+"C"[[:space:]]+)*(fn|struct|enum|trait|type|const|static|mod|use)([[:space:]<(:;=]|$)' \
  "$ref" -- "${caisses[@]/%//*.rs}" \
| sed -E "s#^${ref}:##" \
| awk -F: '
  {
    fichier=$1; ligne=$3; for(i=4;i<=NF;i++) ligne=ligne ":" $i
    sub(/^[ \t]+/, "", ligne)
    # coupe au corps ou à la fin d item ; garde les bornes de generiques
    sub(/[ \t]*(\{|;|[ \t]where([ \t]|$)).*$/, "", ligne)
    gsub(/[ \t]+/, " ", ligne)
    print fichier "\t" ligne
  }' \
| sort > "$out/api-par-fichier.txt"

cut -f2 "$out/api-par-fichier.txt" | sort -u > "$out/api-signatures.txt"

printf '%6d signatures distinctes, %6d occurrences (%s)\n' \
  "$(wc -l < "$out/api-signatures.txt")" "$(wc -l < "$out/api-par-fichier.txt")" "$ref" >&2
