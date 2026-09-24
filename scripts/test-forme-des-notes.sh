#!/usr/bin/env bash
# Contre-epreuve du controle de FORME des notes de version (#4190).
#
# Ce que ce banc doit etablir, et rien d'autre :
#
#   1. une note en PROSE sous des titres thematiques — la forme reellement
#      publiee jusqu'au 22/09/2026 — rend ZERO item, donc « Release x.y.z » a
#      l'ecran, et le controle la REFUSE ;
#   2. la meme note remise en forme A rend des items, et le controle l'ACCEPTE ;
#   3. dans la sonde complete, un fil forum PRESENT ne suffit plus : une note
#      illisible fait rougir, et une note lisible fait taire — meme decor, seul
#      le corps change. Sans cette paire, un controle qui refuse tout ou
#      n'accepte rien passerait pour un succes ;
#   4. les tables de mots-cles du lecteur awk sont la copie EXACTE des
#      `TITRES_*` du Rust. C'est la seule chose qui rend la duplication
#      supportable : sans cette garde, une table qui derive rendrait ce banc
#      vert sur une note que le serveur ne saurait pas lire.
#
# Aucun acces reseau : `gh` et `curl` sont des lecteurs de fichiers poses en
# tete de PATH, `MAINTENANT_ISO` fige l'instant, `SANS_ISSUE=1` n'ouvre rien.
#
# Usage : scripts/test-forme-des-notes.sh

set -uo pipefail

ICI=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SONDE="$ICI/../.github/scripts/notes-de-version-watch.sh"
LECTEUR="$ICI/../.github/scripts/forme-des-notes.awk"
RUST="$ICI/../tune-server/src/routes/system/update.rs"
RACINE=$(mktemp -d "${TMPDIR:-/tmp}/tune-4190-forme-XXXXXX")
trap 'rm -rf "$RACINE"' EXIT

rate=0

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

# `compter <cle> <fichier>` — ce que le lecteur annonce pour une rubrique.
compter() {
  awk -f "$LECTEUR" < "$2" | awk -F'\t' -v c="$1" '$1==c{print $2}'
}

# ─────────────────────────────────────────────────────────────────────────────
# Les deux notes du banc.
#
# `notes-en-prose.md` n'est pas une caricature : c'est la structure exacte des
# notes publiees — un chapeau, puis des sections thematiques ou tout le contenu
# est en paragraphes. Les puces qu'elle contient sont sous des titres que le
# serveur ne reconnait pas, ce qui est precisement le piege : il y a des puces,
# et elles ne comptent pas.
#
# Les deux fichiers vivent hors de ce banc parce que le TEST RUST les lit
# aussi (`forme_des_notes_exemples`, dans update.rs). Le lecteur awk est une
# copie de `parse_release_body` ; deux copies qui ne sont jamais confrontees
# aux MEMES entrees derivent en silence. Ici elles le sont, et sur des chiffres
# ecrits des deux cotes : 0 / 0 / 0 pour la prose, 3 / 2 / 3 pour les puces.
# ─────────────────────────────────────────────────────────────────────────────
EXEMPLES="$ICI/../.github/scripts/exemples"
cp "$EXEMPLES/notes-en-prose.md" "$RACINE/prose.md"
cp "$EXEMPLES/notes-en-puces.md" "$RACINE/puces.md"


# ─────────────────────────────────────────────────────────────────────────────
# 1. Le lecteur, seul et hors reseau.
# ─────────────────────────────────────────────────────────────────────────────
echo "1. le lecteur compte ce que le panneau affichera"
verifier "note en prose : aucune nouveaute"     "0" "$(compter features "$RACINE/prose.md")"
verifier "note en prose : aucune amelioration"  "0" "$(compter improvements "$RACINE/prose.md")"
verifier "note en prose : aucune correction"    "0" "$(compter fixes "$RACINE/prose.md")"
verifier "note en puces : 3 nouveautes"         "3" "$(compter features "$RACINE/puces.md")"
verifier "note en puces : 2 ameliorations"      "2" "$(compter improvements "$RACINE/puces.md")"
verifier "note en puces : 3 corrections"        "3" "$(compter fixes "$RACINE/puces.md")"

# Les deux notes portent une section « Telechargements » AVEC des puces. Si
# elles etaient comptees, la note en prose passerait le seuil sans qu'une seule
# ligne soit lisible dans le panneau — un vert qui ne garde rien.
echo "   (la section « Telechargements » a des puces dans les DEUX notes :"
echo "    si elles comptaient, la note en prose serait acceptee)"

# ─────────────────────────────────────────────────────────────────────────────
# 2. Le mode `--forme`, celui qu'on passe avant `gh release edit`.
# ─────────────────────────────────────────────────────────────────────────────
echo
echo "2. le mode --forme refuse la prose et accepte les puces"
bash "$SONDE" --forme "$RACINE/prose.md" > "$RACINE/prose.sortie" 2>&1
verifier "etat 1 sur la note en prose" "1" "$?"
grep -q '0 item(s) pour le panneau' "$RACINE/prose.sortie" \
  && printf '  OK   le motif est dit : 0 item\n' \
  || { printf '  RATE le motif du refus n est pas dit\n'; cat "$RACINE/prose.sortie"; rate=1; }

bash "$SONDE" --forme "$RACINE/puces.md" > "$RACINE/puces.sortie" 2>&1
verifier "etat 0 sur la note en puces" "0" "$?"

# Le seuil doit MORDRE : a 9 items exiges, la note en puces (8) est refusee.
# Sans cette verification, un seuil de 0 rendrait le point precedent vert.
ITEMS_MINIMUM=9 bash "$SONDE" --forme "$RACINE/puces.md" > /dev/null 2>&1
verifier "etat 1 sur la meme note avec ITEMS_MINIMUM=9" "1" "$?"

# ─────────────────────────────────────────────────────────────────────────────
# 3. Le bloc traduit ne couvre pas un francais reste en prose.
#
# Le piege : on traduit la note APRES l'avoir ecrite, et le traducteur, lui,
# rend des puces. Un decompte sur le corps entier verrait huit items et se
# tairait, pendant que le panneau francais — celui de presque tous les
# testeurs — reste vide.
# ─────────────────────────────────────────────────────────────────────────────
echo
echo "3. des puces dans le bloc <!-- lang:en --> ne sauvent pas un francais en prose"
{ cat "$RACINE/prose.md"; printf '\n<!-- lang:en -->\n\n'; sed 's/Nouveautes/Features/; s/Ameliorations/Improvements/; s/Corrections/Bug fixes/' "$RACINE/puces.md"; } > "$RACINE/mixte.md"
verifier "le decompte reste celui du bloc francais" "0" "$(compter features "$RACINE/mixte.md")"
bash "$SONDE" --forme "$RACINE/mixte.md" > /dev/null 2>&1
verifier "etat 1 malgre les puces anglaises" "1" "$?"
verifier "les deux langues sont vues" "fr,en" "$(compter langues "$RACINE/mixte.md")"

# ─────────────────────────────────────────────────────────────────────────────
# 4. La sonde complete : meme decor, meme fil forum, seul le corps change.
#
# C'est la moitie qui compte. Le fil de la v0.9.163 EXISTE dans les deux cas :
# si la sonde rougit sur l'un et se tait sur l'autre, c'est la forme, et rien
# d'autre, qui a fait la difference.
# ─────────────────────────────────────────────────────────────────────────────
mkdir -p "$RACINE/bin"

cat > "$RACINE/bin/gh" <<'SH'
#!/usr/bin/env bash
case "$1 ${2:-}" in
  "release list") cat "$DECOR/releases.json" ;;
  # `gh release view TAG --repo … --json body -q .body`
  "release view") cat "$DECOR/corps-$3.md" ;;
  *) echo "gh inattendu : $*" >&2; exit 97 ;;
esac
SH

cat > "$RACINE/bin/curl" <<'SH'
#!/usr/bin/env bash
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

MAINTENANT="2026-09-23T18:00:00Z"

poser_decor() {
  local nom="$1" corps="$2"
  DECOR="$RACINE/$nom"
  mkdir -p "$DECOR"
  cat > "$DECOR/releases.json" <<'JSON'
[
 {"tagName":"v0.9.163","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-23T09:00:00Z"},
 {"tagName":"moissonneur-v0.9.163","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-23T08:30:00Z"},
 {"tagName":"v0.9.162","isDraft":false,"isPrerelease":false,"publishedAt":"2026-09-22T10:00:00Z"}
]
JSON
  # La v0.9.163 ET la v0.9.162 ont leur fil : le rapprochement est vert dans
  # les deux cas, donc tout rouge vient de la forme.
  cat > "$DECOR/fils.json" <<'JSON'
{"threads":[
 {"title":"Tune v0.9.163 — Notes de version","type":"release",
  "created_at":"2026-09-23T10:00:00+00:00","is_pinned":false},
 {"title":"Tune v0.9.162 — Notes de version","type":"release",
  "created_at":"2026-09-22T11:00:00+00:00","is_pinned":false},
 {"title":"Tune v0.9.154 — Notes de version","type":"release",
  "created_at":"2026-09-17T09:48:35+00:00","is_pinned":false}
]}
JSON
  cp "$corps" "$DECOR/corps-v0.9.163.md"
}

jouer() {
  local nom="$1"
  DECOR="$RACINE/$nom" PATH="$RACINE/bin:$PATH" \
  FORUM_TOKEN=x GITHUB_REPOSITORY=renesenses/tune-server-rust GH_TOKEN=x \
  SANS_ISSUE=1 MAINTENANT_ISO="$MAINTENANT" \
    bash "$SONDE" > "$RACINE/$nom.sortie" 2>&1
  ETAT=$?
  SORTIE=$(cat "$RACINE/$nom.sortie")
}

echo
echo "4a. v0.9.163 annoncee sur le forum, mais en PROSE — la sonde rougit"
poser_decor sonde-prose "$RACINE/prose.md"
jouer sonde-prose
verifier "etat de sortie 1" "1" "$ETAT"
printf '%s' "$SORTIE" | grep -q 'Notes illisibles par le panneau' \
  && printf '  OK   le cri nomme la forme, pas un fil manquant\n' \
  || { printf '  RATE le cri ne parle pas de la forme\n%s\n' "$SORTIE"; rate=1; }
printf '%s' "$SORTIE" | grep -q 'Versions publiees sans fil de notes' \
  && { printf '  RATE la sonde accuse un fil manquant, alors que le fil existe\n'; rate=1; } \
  || printf '  OK   aucun fil n est accuse a tort\n'

echo
echo "4b. la MEME version, le MEME fil, une note en PUCES — la sonde se tait"
poser_decor sonde-puces "$RACINE/puces.md"
jouer sonde-puces
verifier "etat de sortie 0" "0" "$ETAT"
printf '%s' "$SORTIE" | grep -q 'Notes illisibles par le panneau' \
  && { printf '  RATE la sonde crie encore sur une note lisible\n%s\n' "$SORTIE"; rate=1; } \
  || printf '  OK   aucun cri\n'

echo
echo "4c. la v0.9.162, publiee AVANT la regle, n est pas jugee"
# Les notes deja publiees ne sont pas reecrites (decision du 23/09/2026). Son
# corps n'est meme pas demande : le `gh` double rendrait une erreur si elle
# l'etait, et un « corps illisible » apparaitrait dans la sortie.
printf '%s' "$SORTIE" | grep -q 'v0\.9\.162' \
  && { printf '  RATE la v0.9.162 est jugee alors qu elle precede FORME_DEPUIS\n'; rate=1; } \
  || printf '  OK   la v0.9.162 est hors perimetre\n'

# Et la borne doit etre une VRAIE borne : reculee d'un jour, la v0.9.162 entre
# dans le perimetre et son corps est reclame. Sans cette verification, un
# filtre qui ecarte tout passerait pour un succes.
DECOR="$RACINE/sonde-puces" PATH="$RACINE/bin:$PATH" \
FORUM_TOKEN=x GITHUB_REPOSITORY=renesenses/tune-server-rust GH_TOKEN=x \
SANS_ISSUE=1 MAINTENANT_ISO="$MAINTENANT" FORME_DEPUIS=2026-09-22 \
  bash "$SONDE" > "$RACINE/borne.sortie" 2>&1
grep -q 'v0\.9\.162' "$RACINE/borne.sortie" \
  && printf '  OK   avec FORME_DEPUIS=2026-09-22, la v0.9.162 est bien examinee\n' \
  || { printf '  RATE la borne n en est pas une : la v0.9.162 reste ignoree\n'; cat "$RACINE/borne.sortie"; rate=1; }

# Le moissonneur reste hors sujet ici aussi : il n'a pas de notes de version.
#
# `grep moissonneur` sur TOUTE la sortie serait un faux rouge : le
# rapprochement dit deja « Releases ecartees — … moissonneur-v0.9.163 », et
# c'est exactement ce qu'on veut lire. Ce qu'on interroge, c'est la ligne de
# VERDICT de forme, reconnaissable a son indentation de deux espaces.
printf '%s\n' "$SORTIE" | grep -q '^  moissonneur-' \
  && { printf '  RATE un tag moissonneur est juge sur sa forme\n'; rate=1; } \
  || printf '  OK   aucun tag moissonneur n est juge sur sa forme\n'

# ─────────────────────────────────────────────────────────────────────────────
# 5. Les tables de mots-cles : awk et Rust doivent dire la MEME chose.
#
# `forme-des-notes.awk` recopie `TITRES_CORRECTIONS`, `TITRES_AMELIORATIONS` et
# `TITRES_NOUVEAUTES`. Une copie qu'on ne compare jamais derive ; et une table
# qui derive rend ce banc vert sur une note que le serveur ne lit pas.
# ─────────────────────────────────────────────────────────────────────────────
echo
echo "5. les tables du lecteur awk sont la copie exacte des TITRES_* du Rust"

table_rust() {
  awk -v nom="$1" '
    index($0, "const " nom ":") == 1 { dedans = 1; next }
    dedans && index($0, "];") == 1 { exit }
    dedans {
      s = $0
      # Le commentaire de fin de ligne (« // fr, en ») ne contient pas de
      # guillemet : extraire les chaines suffit, sans le retirer.
      while (match(s, /"[^"]*"/)) {
        printf "%s|", substr(s, RSTART + 1, RLENGTH - 2)
        s = substr(s, RSTART + RLENGTH)
      }
    }
    END { printf "\n" }
  ' "$RUST" | sed 's/|$//'
}

table_awk() {
  sed -n "s/^  $1 = \"\(.*\)\"$/\1/p" "$LECTEUR"
}

for paire in "TITRES_CORRECTIONS CORR" "TITRES_AMELIORATIONS AMEL" "TITRES_NOUVEAUTES NOUV"; do
  set -- $paire
  R=$(table_rust "$1")
  A=$(table_awk "$2")
  if [ -z "$R" ]; then
    printf '  RATE %s introuvable dans %s\n' "$1" "$RUST"
    rate=1
  else
    verifier "$1 == $2" "$R" "$A"
  fi
done

echo
if [ "$rate" -eq 0 ]; then
  echo "Contre-epreuve #4190 : tout est vert."
else
  echo "Contre-epreuve #4190 : ECHEC."
fi
exit "$rate"
