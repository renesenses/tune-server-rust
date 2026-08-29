#!/bin/sh
# Qu'est-ce qui est mergé sur release/v0.9 mais ABSENT du binaire publié ?
#
# La ligne de release avance en continu ; les tags sortent par à-coups. L'écart
# entre les deux est normal — ce qui ne l'est pas, c'est d'annoncer à un
# testeur un correctif qui n'est pas dans la version qu'il télécharge. Vécu
# assez souvent pour mériter sa mémoire : « correctif mergé APRÈS le tag = pas
# dans la version qui sort ».
#
# Point de comparaison : la dernière release PUBLIÉE, pas le dernier tag.
# Un tag peut exister sans release, ou porter une release en brouillon, ou
# avoir été abandonné après coup (v0.9.66, v0.9.72, v0.9.77). Seul ce qui est
# publié est téléchargeable, donc seul cela compte pour un testeur.
#
# Usage :
#   scripts/unreleased.sh              liste ce qui attend une publication
#   scripts/unreleased.sh 1765         dit si CETTE PR est téléchargeable
#
# Sort toujours 0 : c'est un outil de décision, pas une barrière. Un écart non
# vide est l'état normal d'une ligne vivante.
set -e

REPO="renesenses/tune-server-rust"
LINE="origin/release/v0.9"

git fetch -q origin --tags

# Dernière release publiée : ni brouillon, ni pré-version.
TAG=$(gh release list --repo "$REPO" --limit 30 --json tagName,isDraft,isPrerelease \
        --jq 'map(select(.isDraft == false and .isPrerelease == false)) | .[0].tagName' 2>/dev/null || true)

if [ -z "$TAG" ] || [ "$TAG" = "null" ]; then
    echo "Impossible de lire la dernière release publiée (réseau ? droits gh ?)."
    echo "Sans ce point de comparaison, ce script ne peut rien affirmer — et"
    echo "une absence de réponse n'est pas une preuve d'absence d'écart."
    exit 0
fi

if ! git rev-parse -q --verify "refs/tags/$TAG" >/dev/null 2>&1; then
    echo "La release publiée $TAG n'a pas de tag local malgré le fetch."
    exit 0
fi

# --- Mode « cette PR est-elle téléchargeable ? » --------------------------
if [ -n "$1" ]; then
    PR="$1"
    SHA=$(git log --format='%H %s' "$TAG" | grep -m1 "(#$PR)" | cut -d' ' -f1 || true)
    if [ -n "$SHA" ]; then
        echo "#$PR est dans $TAG (publiée) — annonçable."
        exit 0
    fi
    SHA=$(git log --format='%H %s' "$LINE" | grep -m1 "(#$PR)" | cut -d' ' -f1 || true)
    if [ -n "$SHA" ]; then
        echo "#$PR est mergée sur release/v0.9 mais PAS dans $TAG."
        echo "Elle n'est dans aucun binaire téléchargeable : ne pas l'annoncer"
        echo "comme disponible, et prévenir avant tout retest."
        exit 0
    fi
    echo "#$PR est introuvable sur $LINE."
    echo "Elle est peut-être encore ouverte, ou mergée sur main seulement —"
    echo "auquel cas elle ne sera JAMAIS livrée. Vérifier par CONTENU."
    exit 0
fi

# --- Mode liste ----------------------------------------------------------
COUNT=$(git rev-list --count "$TAG..$LINE")
if [ "$COUNT" -eq 0 ]; then
    echo "Rien en attente : release/v0.9 est exactement $TAG (publiée)."
    exit 0
fi

echo "$COUNT commit(s) mergés après $TAG — donc dans AUCUN binaire publié :"
echo
git log --format='  %h  %s' "$TAG..$LINE"
echo
echo "Ne pas annoncer ces correctifs comme disponibles. Pour en vérifier un :"
echo "  scripts/unreleased.sh <numéro de PR>"
