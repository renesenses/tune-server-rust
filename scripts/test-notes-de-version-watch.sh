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

# ─────────────────────────────────────────────────────────────────────────────
# 4. Le faux vert que le fil groupe du moissonneur fabriquerait.
#
# « Moissonneur Roon v0.9.160 a v0.9.161 — Notes de version » contient
# « 0.9.160 » et « 0.9.161 » avec les bornes qu'`annoncee()` exige : le « v »
# qui precede n'est ni un chiffre ni un point. Sans le retrait des fils du
# moissonneur, ce SEUL fil vaudrait annonce pour deux versions du SERVEUR qui
# n'en ont aucune. Ici, la sonde doit accuser les deux.
# ─────────────────────────────────────────────────────────────────────────────
echo
echo "4. un fil du moissonneur nomme v0.9.160 et v0.9.161 — il n'annonce pas Tune"
poser_decor fauxvert <<'JSON'
[
 {"tagName":"v0.9.161","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:45:00Z"},
 {"tagName":"v0.9.160","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:40:00Z"},
 {"tagName":"v0.9.155","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-18T13:59:41Z"}
]
JSON
# Le fil du moissonneur s'ajoute au decor commun, sans rien en retirer.
python3 - "$RACINE/fauxvert/fils.json" <<'PY'
import json, sys
chemin = sys.argv[1]
with open(chemin, encoding="utf-8") as f:
    d = json.load(f)
d["threads"].insert(0, {
    "title": "Moissonneur Roon v0.9.160 a v0.9.161 — Notes de version",
    "type": "release",
    "created_at": "2026-09-20T16:00:00+00:00",
    "is_pinned": False,
})
with open(chemin, "w", encoding="utf-8") as f:
    json.dump(d, f)
PY
jouer fauxvert
verifier "etat de sortie 1 (deux versions de Tune sans fil)" "1" "$ETAT"
for v in 160 161; do
  N=$(printf '%s' "$SORTIE" | grep -c "^| \`v0\.9\.$v\` |")
  verifier "la v0.9.$v est accusee malgre le fil du moissonneur" "1" "$N"
done

# ─────────────────────────────────────────────────────────────────────────────
# 5. L'API du forum refuse `type=release` depuis le 27/09/2026 : un fil de notes
#    naît `discussion`. Porte-t-il le titre de la procédure, il annonce ; un
#    fil de bug ou de discussion qui cite seulement la version, non.
# ─────────────────────────────────────────────────────────────────────────────
ajouter_fil() {
  python3 - "$1" "$2" "$3" <<'PY'
import json, sys
chemin, titre, genre = sys.argv[1:4]
with open(chemin, encoding="utf-8") as f:
    d = json.load(f)
d["threads"].insert(0, {"title": titre, "type": genre,
                        "created_at": "2026-09-20T16:00:00+00:00", "is_pinned": False})
with open(chemin, "w", encoding="utf-8") as f:
    json.dump(d, f)
PY
}
RELEASES_160='[
 {"tagName":"v0.9.160","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:40:00Z"},
 {"tagName":"v0.9.155","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-18T13:59:41Z"}
]'

echo
echo "5. un fil discussion « Tune v0.9.160 — Notes de version » — il annonce"
printf '%s' "$RELEASES_160" | poser_decor discussion
ajouter_fil "$RACINE/discussion/fils.json" "Tune v0.9.160 — Notes de version" discussion
jouer discussion
verifier "etat de sortie 0 (la v0.9.160 a son fil)" "0" "$ETAT"

echo
echo "5b. un fil discussion au titre thematique « Tune v0.9.160 — l'egaliseur … » — il annonce"
printf '%s' "$RELEASES_160" | poser_decor thematique
ajouter_fil "$RACINE/thematique/fils.json" "Tune v0.9.160 — l'égaliseur sans saut de volume, la recherche par dossier" discussion
jouer thematique
verifier "etat de sortie 0 (titre reel des fils .165-.167)" "0" "$ETAT"

echo
echo "6. un fil de bug et une discussion qui citent la v0.9.160 — ils n'annoncent rien"
printf '%s' "$RELEASES_160" | poser_decor bavard
ajouter_fil "$RACINE/bavard/fils.json" "v0.9.160 : plus de son sur le DAC" bug
ajouter_fil "$RACINE/bavard/fils.json" "Vos impressions sur la v0.9.160 ?" discussion
jouer bavard
verifier "etat de sortie 1 (aucun fil de notes)" "1" "$ETAT"
N=$(printf '%s' "$SORTIE" | grep -c '^| `v0\.9\.160` |')
verifier "la v0.9.160 est accusee malgre les fils qui la citent" "1" "$N"

# ─────────────────────────────────────────────────────────────────────────────
# 7. La 1.0.0-rc1 (29/09/2026) part en « Latest » : c'est une version de Tune,
#    elle doit avoir son fil. Et le fil de la rc1 n'annonce PAS la 1.0.0.
# ─────────────────────────────────────────────────────────────────────────────
RELEASES_RC='[
 {"tagName":"v1.0.0-rc1","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:40:00Z"},
 {"tagName":"moissonneur-v1.0.0-rc1","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:30:00Z"},
 {"tagName":"v0.9.155","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-18T13:59:41Z"}
]'

echo
echo "7. la v1.0.0-rc1 sans fil — elle est accusee"
printf '%s' "$RELEASES_RC" | poser_decor rc_sans_fil
jouer rc_sans_fil
verifier "etat de sortie 1 (la rc1 n'a pas de fil)" "1" "$ETAT"
N=$(printf '%s' "$SORTIE" | grep -c '^| `v1\.0\.0-rc1` |')
verifier "la v1.0.0-rc1 est accusee" "1" "$N"
N=$(printf '%s' "$SORTIE" | grep -c '^| `moissonneur-v1\.0\.0-rc1` |')
verifier "le moissonneur-v1.0.0-rc1 n'est pas accuse" "0" "$N"

echo
echo "7b. la v1.0.0-rc1 avec son fil « Tune v1.0.0-rc1 — Notes de version » — elle a son fil"
printf '%s' "$RELEASES_RC" | poser_decor rc_avec_fil
ajouter_fil "$RACINE/rc_avec_fil/fils.json" "Tune v1.0.0-rc1 — Notes de version" discussion
jouer rc_avec_fil
verifier "etat de sortie 0 (la rc1 a son fil)" "0" "$ETAT"

echo
echo "7c. la v1.0.0 finale et le seul fil de la rc1 — la 1.0.0 est accusee"
printf '%s' '[
 {"tagName":"v1.0.0","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:45:00Z"},
 {"tagName":"v0.9.155","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-18T13:59:41Z"}
]' | poser_decor finale
ajouter_fil "$RACINE/finale/fils.json" "Tune v1.0.0-rc1 — Notes de version" discussion
jouer finale
verifier "etat de sortie 1 (le fil de la rc1 n'annonce pas la 1.0.0)" "1" "$ETAT"

# ─────────────────────────────────────────────────────────────────────────────
# 8. #4811 : le fil de la rc2 s'intitule « Tune 1.0.0-rc2 est disponible »
#    (fil 2144), sans « v » ni « Notes de version ». Il annonce la rc2, et elle
#    seule : pas la 1.0.0 finale. Un fil de bug qui dit « est disponible » en
#    cours de phrase n'annonce rien.
# ─────────────────────────────────────────────────────────────────────────────
RELEASES_RC2='[
 {"tagName":"v1.0.0-rc2","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:40:00Z"},
 {"tagName":"v0.9.155","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-18T13:59:41Z"}
]'

echo
echo "8. la v1.0.0-rc2 et le fil « Tune 1.0.0-rc2 est disponible » — elle a son fil"
for genre in discussion announcement; do
  printf '%s' "$RELEASES_RC2" | poser_decor "disponible_$genre"
  ajouter_fil "$RACINE/disponible_$genre/fils.json" "Tune 1.0.0-rc2 est disponible" "$genre"
  jouer "disponible_$genre"
  verifier "etat de sortie 0 (fil de type $genre)" "0" "$ETAT"
done

echo
echo "8b. la forme avec « v » : « Tune v1.0.0-rc2 est disponible » — elle a son fil"
printf '%s' "$RELEASES_RC2" | poser_decor disponible_v
ajouter_fil "$RACINE/disponible_v/fils.json" "Tune v1.0.0-rc2 est disponible" discussion
jouer disponible_v
verifier "etat de sortie 0" "0" "$ETAT"

echo
echo "8c. la v1.0.0 finale et le seul fil « Tune 1.0.0-rc2 est disponible » — la 1.0.0 est accusee"
printf '%s' '[
 {"tagName":"v1.0.0","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-20T15:45:00Z"},
 {"tagName":"v0.9.155","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-18T13:59:41Z"}
]' | poser_decor disponible_finale
ajouter_fil "$RACINE/disponible_finale/fils.json" "Tune 1.0.0-rc2 est disponible" discussion
jouer disponible_finale
verifier "etat de sortie 1 (le fil de la rc2 n'annonce pas la 1.0.0)" "1" "$ETAT"

echo
echo "8d. un fil de bug « La 1.0.0-rc2 est disponible mais muette » — il n'annonce rien"
printf '%s' "$RELEASES_RC2" | poser_decor disponible_bavard
ajouter_fil "$RACINE/disponible_bavard/fils.json" "La 1.0.0-rc2 est disponible mais muette sur mon DAC" bug
jouer disponible_bavard
verifier "etat de sortie 1 (aucun fil de notes)" "1" "$ETAT"
N=$(printf '%s' "$SORTIE" | grep -c '^| `v1\.0\.0-rc2` |')
verifier "la v1.0.0-rc2 est accusee" "1" "$N"

echo
if [ "$rate" -eq 0 ]; then
  echo "Contre-epreuve #4461 : tout est vert."
else
  echo "Contre-epreuve #4461 : ECHEC."
fi
exit "$rate"
