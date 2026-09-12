#!/usr/bin/env bash
# Relire un tag qui vient d'etre cree, en distinguant « absent » de « pas
# encore visible ».
#
# Le controleur de release cree les quatre tags du train par
# `POST repos/…/git/refs`, puis les relit immediatement pour verifier qu'ils
# pointent bien ou il faut. Cette relecture est la SEULE garde contre un tag
# divergent invisible : elle ne doit pas etre affaiblie.
#
# Mais elle confondait deux etats tres differents. Run 33522674458, publication
# de la v0.9.130 : le tag `v0.9.130` a bien ete cree sur `tune-web-client`, la
# relecture immediate a rendu « absent », et le controleur s'est arrete AVANT
# `universal`, `os` et `server`. Le tag existait — relu trente secondes plus
# tard, sur le bon SHA. L'API GitHub n'est pas coherente en lecture-apres-
# ecriture a cette echelle de temps.
#
# Resultat : un tag orphelin dans un depot, trois depots sans tag, un train a
# reprendre a la main.
#
# Ce fichier apporte la reprise qui manquait, et RIEN d'autre :
#   * « absent » (1) et « illisible » (2) sont reessayes, puis restent des
#     ECHECS DURS si le tag demeure introuvable au bout ;
#   * un tag DIVERGENT (3) est refuse du PREMIER coup — un tag qui pointe
#     ailleurs est un fait, pas une latence, et le reessayer ne ferait que
#     retarder le meme verdict.
#
# Plafond par defaut : 5 tentatives espacees d'une seconde, soit 4 secondes
# d'attente au pire. L'echec reste rapide ; c'est la reprise a la main d'un
# train coupe en deux qui coutait une heure.
#
# S'utilise SOURCE depuis un workflow :
#
#   source scripts/relire-tag-avec-reprise.sh
#   relu="$(relire_tag_avec_reprise "$sha" cible_tag "$repo" "$tag")" && etat=0 || etat=$?
#
# Et s'eprouve seul : `bash scripts/relire-tag-avec-reprise.sh --autotest`.

set -euo pipefail

# Reglables pour l'autotest et pour un depot particulierement lent.
RELIRE_TAG_ESSAIS="${RELIRE_TAG_ESSAIS:-5}"
RELIRE_TAG_PAUSE="${RELIRE_TAG_PAUSE:-1}"

# relire_tag_avec_reprise <sha_attendu> <lecteur> [arguments du lecteur...]
#
# <lecteur> est une commande qui ecrit le SHA du tag sur la sortie standard et
# rend, exactement comme `cible_tag` du controleur :
#   0  le tag est lu
#   1  le tag est absent       — peut n'etre qu'une latence de propagation
#   2  le depot est illisible  — peut n'etre qu'une defaillance passagere
#
# Rend :
#   0  lu, et conforme au SHA attendu (le SHA est ecrit sur la sortie standard)
#   1  toujours absent au bout des tentatives
#   2  depot toujours illisible au bout des tentatives
#   3  tag DIVERGENT — immediat, jamais reessaye
relire_tag_avec_reprise() {
  local attendu="$1"
  shift
  local essai=1 relu etat

  if [ "$RELIRE_TAG_ESSAIS" -lt 1 ]; then
    echo "relire_tag_avec_reprise: RELIRE_TAG_ESSAIS=$RELIRE_TAG_ESSAIS, il en faut au moins 1" >&2
    return 2
  fi

  while :; do
    relu="$("$@")" && etat=0 || etat=$?

    if [ "$etat" -eq 0 ]; then
      printf '%s\n' "$relu"
      [ "$relu" = "$attendu" ] && return 0
      # Divergent : verdict immediat. C'est precisement ce que la garde
      # d'origine protegeait, et ce que la reprise ne doit surtout pas noyer.
      return 3
    fi

    if [ "$essai" -ge "$RELIRE_TAG_ESSAIS" ]; then
      echo "relire_tag_avec_reprise: toujours introuvable apres $essai tentative(s) (etat=$etat)" >&2
      return "$etat"
    fi

    echo "relire_tag_avec_reprise: tentative $essai/$RELIRE_TAG_ESSAIS sans reponse (etat=$etat), nouvel essai dans ${RELIRE_TAG_PAUSE}s" >&2
    sleep "$RELIRE_TAG_PAUSE"
    essai=$((essai + 1))
  done
}

# --------------------------------------------------------------------------
# Contre-epreuves. Chacune imprime une ligne « ok: … » ; le nombre de lignes
# est compte par `tune-server/tests/workflows_bornes.rs`, pour qu'un autotest
# devenu muet ne puisse pas passer pour vert.
# --------------------------------------------------------------------------

_garanties=0
_ok() {
  _garanties=$((_garanties + 1))
  echo "ok: $1"
}
_echouer() {
  echo "ECHEC: $1" >&2
  exit 1
}

# Lecteur factice : lit son scenario dans $_SCENARIO (une issue par ligne,
# « absent », « illisible », ou un SHA), et compte ses appels dans $_COMPTEUR.
_lecteur_factice() {
  local n
  n=$(($(cat "$_COMPTEUR") + 1))
  printf '%s' "$n" > "$_COMPTEUR"
  local ligne
  ligne="$(sed -n "${n}p" "$_SCENARIO")"
  [ -n "$ligne" ] || ligne="$(tail -n 1 "$_SCENARIO")"
  case "$ligne" in
    absent) return 1 ;;
    illisible) return 2 ;;
    *) printf '%s\n' "$ligne"; return 0 ;;
  esac
}

_scenario() {
  printf '%s\n' "$@" > "$_SCENARIO"
  printf '0' > "$_COMPTEUR"
}

_appels() { cat "$_COMPTEUR"; }

autotest() {
  local atelier
  atelier="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$atelier'" EXIT
  _SCENARIO="$atelier/scenario"
  _COMPTEUR="$atelier/compteur"
  export _SCENARIO _COMPTEUR

  local bon=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  local autre=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
  local relu etat

  RELIRE_TAG_ESSAIS=5
  RELIRE_TAG_PAUSE=0

  # 1. Le cas nominal : lu du premier coup, un seul appel, aucune pause.
  _scenario "$bon"
  relu="$(relire_tag_avec_reprise "$bon" _lecteur_factice)" && etat=0 || etat=$?
  [ "$etat" -eq 0 ] || _echouer "un tag lisible du premier coup doit reussir (etat=$etat)"
  [ "$relu" = "$bon" ] || _echouer "le SHA lu n'est pas rendu ($relu)"
  [ "$(_appels)" = 1 ] || _echouer "le cas nominal a coute $(_appels) lectures au lieu d'une"
  _ok "tag visible immediatement : succes en une seule lecture"

  # 2. LE cas de la v0.9.130 : absent, absent, puis le bon SHA.
  _scenario absent absent "$bon"
  relu="$(relire_tag_avec_reprise "$bon" _lecteur_factice)" && etat=0 || etat=$?
  [ "$etat" -eq 0 ] || _echouer "deux absences puis le bon SHA doivent reussir (etat=$etat)"
  [ "$relu" = "$bon" ] || _echouer "le SHA lu au 3e essai n'est pas rendu ($relu)"
  [ "$(_appels)" = 3 ] || _echouer "il a fallu $(_appels) lectures au lieu de 3"
  _ok "absent deux fois puis visible : succes (le defaut du run 33522674458)"

  # 3. Un depot illisible passagerement est lui aussi rattrape.
  _scenario illisible "$bon"
  relu="$(relire_tag_avec_reprise "$bon" _lecteur_factice)" && etat=0 || etat=$?
  [ "$etat" -eq 0 ] || _echouer "une lecture illisible puis lisible doit reussir (etat=$etat)"
  _ok "depot illisible une fois puis lisible : succes"

  # 4. Toujours absent : ECHEC DUR, et borne au plafond.
  _scenario absent
  relu="$(relire_tag_avec_reprise "$bon" _lecteur_factice)" && etat=0 || etat=$?
  [ "$etat" -eq 1 ] || _echouer "un tag jamais visible doit echouer avec 1, pas $etat"
  [ "$(_appels)" = 5 ] || _echouer "le plafond n'est pas tenu : $(_appels) lectures au lieu de 5"
  _ok "tag jamais visible : echec dur (1), borne a 5 lectures"

  # 5. Toujours illisible : ECHEC DUR distinct, meme plafond.
  _scenario illisible
  relu="$(relire_tag_avec_reprise "$bon" _lecteur_factice)" && etat=0 || etat=$?
  [ "$etat" -eq 2 ] || _echouer "un depot jamais lisible doit echouer avec 2, pas $etat"
  [ "$(_appels)" = 5 ] || _echouer "le plafond n'est pas tenu : $(_appels) lectures au lieu de 5"
  _ok "depot jamais lisible : echec dur (2), borne a 5 lectures"

  # 6. LA garde qu'il ne faut pas affaiblir : un tag divergent est refuse du
  #    premier coup, sans la moindre reprise.
  _scenario "$autre" "$bon"
  relu="$(relire_tag_avec_reprise "$bon" _lecteur_factice)" && etat=0 || etat=$?
  [ "$etat" -eq 3 ] || _echouer "un tag divergent doit rendre 3, pas $etat"
  [ "$(_appels)" = 1 ] || _echouer "un tag divergent a ete RELU $(_appels) fois : la reprise noie la garde"
  [ "$relu" = "$autre" ] || _echouer "le SHA divergent doit etre rendu pour le message d'erreur"
  _ok "tag divergent : refus immediat (3), aucune reprise, SHA fautif rendu"

  # 7. Le plafond est bien celui qu'on croit : deux tentatives suffisent a
  #    echouer si on le regle a deux.
  RELIRE_TAG_ESSAIS=2
  _scenario absent
  relu="$(relire_tag_avec_reprise "$bon" _lecteur_factice)" && etat=0 || etat=$?
  [ "$etat" -eq 1 ] || _echouer "plafond 2 : l'echec doit rester 1, pas $etat"
  [ "$(_appels)" = 2 ] || _echouer "plafond 2 : $(_appels) lectures"
  _ok "le plafond est reglable et respecte a la lettre"

  # 8. Le lecteur recoit ses arguments intacts : c'est ainsi que le controleur
  #    lui passe `cible_tag "$repo" "$tag"`. Les intervertir ou en perdre un
  #    ferait lire un AUTRE tag, et la reprise validerait le mauvais.
  RELIRE_TAG_ESSAIS=5
  local trace="$atelier/arguments"
  _lecteur_traceur() {
    printf '%s\n' "$*" >> "$trace"
    printf '%s\n' "$bon"
  }
  : > "$trace"
  relu="$(relire_tag_avec_reprise "$bon" _lecteur_traceur renesenses/tune-web-client v0.9.130)" \
    && etat=0 || etat=$?
  [ "$etat" -eq 0 ] || _echouer "le lecteur trace doit reussir (etat=$etat)"
  [ "$(cat "$trace")" = "renesenses/tune-web-client v0.9.130" ] \
    || _echouer "les arguments du lecteur sont alteres : $(cat "$trace")"
  _ok "les arguments du lecteur (depot, tag) sont transmis intacts"

  echo "$_garanties garanties verifiees"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  case "${1:-}" in
    --autotest) autotest ;;
    *)
      echo "usage: $0 --autotest    (sinon: se source depuis un workflow)" >&2
      exit 64
      ;;
  esac
fi
