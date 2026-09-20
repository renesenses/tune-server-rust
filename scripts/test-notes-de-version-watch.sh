#!/usr/bin/env bash
# Contre-epreuve de `.github/scripts/notes-de-version-watch.sh` (#4461).
#
# Une sonde qu'on n'a jamais vue rougir ne prouve rien : « elle ne s'alarme
# plus sur le moissonneur » se demontre en la regardant se taire sur un tag
# `moissonneur-*` ET rougir, dans le MEME etat de forum, sur un `v0.9.x` sans
# fil. Un filtre qui ecarte tout donnerait le premier resultat sans le second.
#
# Le decor est celui du 20/09/2026, celui qui a rempli #4461 de douze
# commentaires : cinq releases `moissonneur-v0.9.155` a `v0.9.159` publiees,
# aucune avec de fil de notes.
#
# Aucun acces reseau : `gh` et `curl` sont remplaces par des lecteurs de
# fichiers poses en tete de PATH, et `MAINTENANT_ISO` fige l'instant de
# reference. `SANS_ISSUE=1` : la sonde diagnostique sans rien ouvrir.
#
# Usage : scripts/test-notes-de-version-watch.sh

set -uo pipefail

ICI=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SONDE="$ICI/../.github/scripts/notes-de-version-watch.sh"
RACINE=$(mktemp -d "${TMPDIR:-/tmp}/tune-4461-contre-epreuve-XXXXXX")
trap 'rm -rf "$RACINE"' EXIT

rate=0

# ─────────────────────────────────────────────────────────────────────────────
# Les doublures. `gh release list` rend le contenu de $DECOR/releases.json ;
# `curl` ecrit $DECOR/fils.json la ou la sonde l'attend et repond 200.
# ─────────────────────────────────────────────────────────────────────────────
mkdir -p "$RACINE/bin"

cat > "$RACINE/bin/gh" <<'SH'
#!/usr/bin/env bash
# Seule `gh release list` est jouee : la sonde n'appelle rien d'autre tant que
# SANS_ISSUE=1.
case "$1 ${2:-}" in
  "release list") cat "$DECOR/releases.json" ;;
  *) echo "gh inattendu : $*" >&2; exit 97 ;;
esac
SH

cat > "$RACINE/bin/curl" <<'SH'
#!/usr/bin/env bash
# `curl -s -o FICHIER -w '%{http_code}' -m 30 -H ... URL`
sortie=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) sortie="$2"; shift 2 ;;
    *) shift ;;
  esac
done
cat "$DECOR/fils.json" > "$sortie"
printf '200'
SH

chmod +x "$RACINE/bin/gh" "$RACINE/bin/curl"

MAINTENANT="2026-09-20T18:00:00Z"

# ─────────────────────────────────────────────────────────────────────────────
# Le decor : les releases du 18 au 20/09/2026, telles que GitHub les rend.
# `poser_decor <nom> <releases.json> <fils.json>`
# ─────────────────────────────────────────────────────────────────────────────
poser_decor() {
  local nom="$1"
  DECOR="$RACINE/$nom"
  mkdir -p "$DECOR"
  cat > "$DECOR/releases.json"
  # Le plancher de la page de fils doit redescendre sous la fenetre de 72 h,
  # sans quoi la sonde se tait par prudence et le rouge attendu n'arrive pas.
  cat > "$DECOR/fils.json" <<'JSON'
{"threads":[
 {"title":"Tune v0.9.159 — la version qui repare l'interface","type":"release",
  "created_at":"2026-09-20T15:36:11+00:00","is_pinned":true},
 {"title":"Tune v0.9.158 — une seule interface","type":"release",
  "created_at":"2026-09-19T21:05:45+00:00","is_pinned":false},
 {"title":"Tune v0.9.156 — les greffons","type":"release",
  "created_at":"2026-09-19T11:49:41+00:00","is_pinned":false},
 {"title":"Tune v0.9.155 — elle repare la mise a jour","type":"release",
  "created_at":"2026-09-18T14:30:47+00:00","is_pinned":false},
 {"title":"Tune v0.9.154 — Notes de version","type":"release",
  "created_at":"2026-09-17T09:48:35+00:00","is_pinned":false}
]}
JSON
}

# `jouer <nom>` — pose la sortie de la sonde dans $SORTIE et son etat dans
# $ETAT. Pas de `$(jouer …)` : la substitution de commande ouvre un sous-shell,
# et l'affectation de $ETAT y resterait enfermee.
jouer() {
  local nom="$1"
  DECOR="$RACINE/$nom" PATH="$RACINE/bin:$PATH" \
  FORUM_TOKEN=x GITHUB_REPOSITORY=renesenses/tune-server-rust GH_TOKEN=x \
  SANS_ISSUE=1 MAINTENANT_ISO="$MAINTENANT" \
    bash "$SONDE" > "$RACINE/$nom.sortie" 2>&1
  ETAT=$?
  SORTIE=$(cat "$RACINE/$nom.sortie")
}

verifier() {
  local titre="$1" attendu="$2" obtenu="$3"
  if [ "$attendu" = "$obtenu" ]; then
    printf '  OK   %s\n' "$titre"
  else
    printf '  RATE %s\n       attendu : %s\n       obtenu  : %s\n' \
      "$titre" "$attendu" "$obtenu"
    rate=1
  fi
}

# ─────────────────────────────────────────────────────────────────────────────
# 1. Les cinq releases du moissonneur, sans aucun fil : la sonde doit se taire.
#
# C'est l'etat REEL du 20/09/2026, celui qui alimentait #4461. Toutes les
# versions de Tune de la fenetre ont leur fil ; seules les `moissonneur-*`
# n'en ont pas.
# ─────────────────────────────────────────────────────────────────────────────
echo "1. moissonneur-v0.9.155..159 sans fil — la sonde se tait"
poser_decor moisson <<'JSON'
[
 {"tagName":"v0.9.159","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:33:11Z"},
 {"tagName":"moissonneur-v0.9.159","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T13:46:36Z"},
 {"tagName":"v0.9.158","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-19T21:02:18Z"},
 {"tagName":"moissonneur-v0.9.158","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-19T19:35:26Z"},
 {"tagName":"moissonneur-v0.9.157","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-19T16:34:00Z"},
 {"tagName":"v0.9.156","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-19T11:46:16Z"},
 {"tagName":"moissonneur-v0.9.156","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-19T10:13:22Z"},
 {"tagName":"v0.9.155","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-18T13:59:41Z"},
 {"tagName":"moissonneur-v0.9.155","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-18T12:32:52Z"}
]
JSON
jouer moisson
verifier "etat de sortie 0 (aucune version sans fil)" "0" "$ETAT"
if printf '%s' "$SORTIE" | grep -q 'moissonneur-'; then
  if printf '%s' "$SORTIE" | grep -q 'Releases ecartees'; then
    printf '  OK   les tags moissonneur sont dits comme ECARTES, pas accuses\n'
  else
    printf '  RATE un tag moissonneur apparait ailleurs que dans les ecartees\n%s\n' "$SORTIE"
    rate=1
  fi
fi
# « sans note de version » tout court se lirait aussi dans le message de PAIX
# (« OK — aucune version publiee sans note de version. ») : la garde serait
# satisfaite par la phrase qui dit le contraire de ce qu'elle cherche. C'est
# l'en-tete du CRI qu'on interroge.
if printf '%s' "$SORTIE" | grep -q 'Versions publiees sans fil de notes'; then
  printf '  RATE la sonde crie encore\n%s\n' "$SORTIE"
  rate=1
else
  printf '  OK   aucun cri\n'
fi
# Les cinq tags doivent etre NOMMES sur la ligne des ecartees — pas n'importe
# ou dans la sortie, ou le corps d'issue de l'ancienne sonde les contenait
# aussi. Un filtre muet laisserait passer une future famille de tags sans que
# personne le voie.
LIGNE_ECARTEES=$(printf '%s\n' "$SORTIE" | grep '^Releases ecartees')
for v in 155 156 157 158 159; do
  printf '%s' "$LIGNE_ECARTEES" | grep -q "moissonneur-v0\.9\.$v" \
    || { printf '  RATE moissonneur-v0.9.%s nest pas dit comme ecarte\n' "$v"; rate=1; }
done
[ "$rate" -eq 0 ] && printf '  OK   les cinq tags ecartes sont nommes\n'

# ─────────────────────────────────────────────────────────────────────────────
# 2. Le meme decor, plus une v0.9.160 de TUNE sans fil : la sonde doit rougir.
#
# C'est la moitie qui compte. Sans elle, le point 1 serait satisfait par une
# sonde qui ne regarde plus rien du tout.
# ─────────────────────────────────────────────────────────────────────────────
echo
echo "2. une v0.9.x de Tune sans fil, dans le MEME decor — la sonde rougit"
poser_decor tune <<'JSON'
[
 {"tagName":"v0.9.160","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:40:00Z"},
 {"tagName":"moissonneur-v0.9.160","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:20:00Z"},
 {"tagName":"v0.9.159","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:33:11Z"},
 {"tagName":"moissonneur-v0.9.159","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T13:46:36Z"},
 {"tagName":"v0.9.155","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-18T13:59:41Z"},
 {"tagName":"moissonneur-v0.9.155","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-18T12:32:52Z"}
]
JSON
jouer tune
verifier "etat de sortie 1 (une version sans fil)" "1" "$ETAT"
printf '%s' "$SORTIE" | grep -q 'v0\.9\.160' \
  && printf '  OK   la v0.9.160 est accusee\n' \
  || { printf '  RATE la v0.9.160 nest pas accusee\n%s\n' "$SORTIE"; rate=1; }
# Le tableau du corps d'issue ne doit contenir QUE la v0.9.160.
LIGNES=$(printf '%s' "$SORTIE" | grep -c '^| `v0\.9\.160` |')
verifier "la v0.9.160 figure une fois dans le tableau" "1" "$LIGNES"
MOISS=$(printf '%s' "$SORTIE" | grep -c '^| `moissonneur-')
verifier "aucune ligne moissonneur dans le tableau" "0" "$MOISS"

# ─────────────────────────────────────────────────────────────────────────────
# 3. Le delai de grace tient toujours pour Tune : une version trop fraiche ne
#    s'accuse pas. Le filtre ne doit pas avoir court-circuite les gardes qui
#    suivent.
# ─────────────────────────────────────────────────────────────────────────────
echo
echo "3. une v0.9.x de Tune publiee il y a 10 min — la sonde patiente"
poser_decor grace <<'JSON'
[
 {"tagName":"v0.9.161","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T17:50:00Z"},
 {"tagName":"v0.9.159","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:33:11Z"},
 {"tagName":"v0.9.155","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-18T13:59:41Z"}
]
JSON
jouer grace
verifier "etat de sortie 0 (delai de grace non echu)" "0" "$ETAT"

echo
if [ "$rate" -eq 0 ]; then
  echo "Contre-epreuve #4461 : tout est vert."
else
  echo "Contre-epreuve #4461 : ECHEC."
fi
exit "$rate"
