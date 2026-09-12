#!/usr/bin/env bash
# REF-0 — Compare deux relevés (parent d'une PR, tête de la PR).
#
# Règles du chantier de refonte, telles qu'inscrites sur #2219 :
#   - aucune assertion supprimée ni affaiblie → un TEST DISPARU est bloquant ;
#   - une garde qui change de chemin est admise, mais listée nommément ;
#   - pour une PR de déplacement pur, les SIGNATURES publiques sont identiques.
# La baseline est TOUJOURS le parent de la PR, jamais une référence figée.
#
# Usage :   scripts/refonte/comparer.sh <dossier_parent> <dossier_pr>
#   Chaque dossier contient ce que gardes.sh / tests-nominatifs.sh /
#   empreinte-api.sh y ont écrit (un sous-ensemble suffit).
# Sortie : rapport lisible sur stdout ; code 1 si un test ou une signature a
# disparu, 0 sinon. Un code 0 n'est pas une preuve — c'est une contre-épreuve
# forte, à compléter par les portes CI complètes.
set -uo pipefail

parent="${1:?dossier parent}"; pr="${2:?dossier PR}"
bloquant=0

section() { printf '\n## %s\n' "$1"; }
seulement_dans() { comm -23 <(sort -u "$1") <(sort -u "$2"); }

for f in "$parent"/tests-*.txt "$parent"/doctests-*.txt; do
  [ -f "$f" ] || continue
  b="$(basename "$f")"
  [ -f "$pr/$b" ] || { echo "⚠ $b absent côté PR — matrice non relevée"; continue; }
  disparus="$(seulement_dans "$f" "$pr/$b")"
  apparus="$(seulement_dans "$pr/$b" "$f")"
  section "$b : $(wc -l < "$f") → $(wc -l < "$pr/$b")"
  if [ -n "$disparus" ]; then
    echo "✗ tests DISPARUS ($(wc -l <<<"$disparus" | tr -d " ")) — bloquant, chacun à justifier nommément :"
    sed 's/^/    - /' <<<"$disparus"; bloquant=1
  else
    echo "✓ aucun test disparu"
  fi
  [ -n "$apparus" ] && { echo "+ tests apparus ($(wc -l <<<"$apparus" | tr -d " ")) :"; sed 's/^/    + /' <<<"$apparus"; }
done

if [ -f "$parent/gardes.txt" ] && [ -f "$pr/gardes.txt" ]; then
  section "gardes par chemin : $(wc -l < "$parent/gardes.txt") → $(wc -l < "$pr/gardes.txt")"
  # Le numéro de ligne bouge à chaque édition sans que la garde change :
  # on compare « lecteur, forme, fichier lu » avec leur multiplicité.
  # comm exige des entrées triées APRÈS l'ajout du préfixe de multiplicité.
  sans_ligne() { awk -F'\t' 'BEGIN{OFS="\t"}{sub(/:[0-9]+$/,"",$1); print $1,$2,$3}' "$1" | sort | uniq -c | sed -E 's/^ *([0-9]+) /\1×\t/' | sort; }
  d="$(comm -23 <(sans_ligne "$parent/gardes.txt") <(sans_ligne "$pr/gardes.txt"))"
  a="$(comm -13 <(sans_ligne "$parent/gardes.txt") <(sans_ligne "$pr/gardes.txt"))"
  if [ -z "$d$a" ]; then echo "✓ inchangées"; else
    echo "! gardes modifiées — à lister dans la PR (chemin adapté, jamais assertion retirée) :"
    [ -n "$d" ] && sed 's/^/    - /' <<<"$d"
    [ -n "$a" ] && sed 's/^/    + /' <<<"$a"
  fi
fi

if [ -f "$parent/api-signatures.txt" ] && [ -f "$pr/api-signatures.txt" ]; then
  section "signatures publiques : $(wc -l < "$parent/api-signatures.txt") → $(wc -l < "$pr/api-signatures.txt")"
  d="$(seulement_dans "$parent/api-signatures.txt" "$pr/api-signatures.txt")"
  a="$(seulement_dans "$pr/api-signatures.txt" "$parent/api-signatures.txt")"
  if [ -n "$d" ]; then
    echo "✗ signatures DISPARUES ($(wc -l <<<"$d" | tr -d " ")) — bloquant pour un déplacement pur :"
    sed 's/^/    - /' <<<"$d"; bloquant=1
  else
    echo "✓ aucune signature disparue"
  fi
  [ -n "$a" ] && { echo "+ signatures apparues ($(wc -l <<<"$a" | tr -d " ")) :"; sed 's/^/    + /' <<<"$a"; }
  if [ -f "$parent/api-par-fichier.txt" ] && [ -f "$pr/api-par-fichier.txt" ]; then
    deplacees="$(comm -3 <(sort -u "$parent/api-par-fichier.txt") <(sort -u "$pr/api-par-fichier.txt") | wc -l)"
    echo "  ($deplacees lignes fichier↔signature ont bougé : c'est le déplacement lui-même)"
  fi
fi

echo
[ "$bloquant" -eq 0 ] && echo "RÉSULTAT : rien de disparu. Contre-épreuve passée, pas une preuve." \
                      || echo "RÉSULTAT : BLOQUANT — quelque chose a disparu, voir ci-dessus."
exit "$bloquant"
