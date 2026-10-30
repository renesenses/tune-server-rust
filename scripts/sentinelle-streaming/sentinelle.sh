#!/usr/bin/env bash
# Sentinelle de streaming du .18 (#7148) — un tour, lance toutes les heures
# par `tune-sentinelle.timer`.
#
#  1. demande un tour au serveur Tune (`POST /api/v1/sentinelle/streaming/
#     sonder`) : le serveur resout et DECODE un titre de reference par service
#     connecte, par le chemin de la lecture, sans aucun son ;
#  2. garde le rapport (une ligne JSON par tour) dans $ETAT/rapports.jsonl ;
#  3. publie le statut de commit `sentinelle/streaming` sur le SHA du binaire
#     (`binaire.sha`) dans renesenses/tune-server-rust — c'est la preuve que
#     le controleur de release exige avant de poser un tag ;
#  4. previent Bertrand par courriel (Resend, Cci bertrand@mozaiklabs.fr) au
#     PREMIER echec d'un service et a son RETOUR a la normale, et quand le
#     serveur ne repond plus.
#
# Un tour « passe » (une zone joue, un tour deja en cours) ne publie rien et
# n'alerte pas : il ne prouve rien, dans un sens ni dans l'autre.
#
# Secrets : jamais dans ce fichier ni dans l'environnement du service. Ils
# arrivent par les identifiants systemd (`LoadCredential=`), lus dans
# $CREDENTIALS_DIRECTORY, et ne passent jamais sur une ligne de commande
# (curl les lit sur son entree standard, `-K -`) :
#   github-token  jeton a grain fin, depot tune-server-rust, « Commit
#                 statuses: Read and write » et rien d'autre ;
#   resend-key    cle Resend « Sending access » limitee au domaine
#                 mozaiklabs.fr ;
#   tune-admin    (facultatif) jeton d'administrateur Tune, si
#                 l'authentification du serveur est allumee.
# Un secret absent n'arrete pas le tour : l'etape correspondante est sautee
# et le journal le dit.
#
# Reglages (Environment= du service) :
#   SENTINELLE_API      http://127.0.0.1:8888/api/v1
#   SENTINELLE_DEPOT    renesenses/tune-server-rust
#   SENTINELLE_MAIL_A   bertrand@mozaiklabs.fr
#   SENTINELLE_MAIL_DE  Tune sentinelle <sentinelle@mozaiklabs.fr>
#   SENTINELLE_ETAT     $STATE_DIRECTORY, sinon ~/.local/state/tune-sentinelle
#
#   bash sentinelle.sh             un tour
#   bash sentinelle.sh --a-blanc   un tour, sans rien publier ni envoyer
#   bash sentinelle.sh --autotest  les contre-epreuves, sans reseau

set -euo pipefail

API="${SENTINELLE_API:-http://127.0.0.1:8888/api/v1}"
DEPOT="${SENTINELLE_DEPOT:-renesenses/tune-server-rust}"
MAIL_A="${SENTINELLE_MAIL_A:-bertrand@mozaiklabs.fr}"
MAIL_DE="${SENTINELLE_MAIL_DE:-Tune sentinelle <sentinelle@mozaiklabs.fr>}"
MAIL_CCI="bertrand@mozaiklabs.fr"
ETAT="${SENTINELLE_ETAT:-${STATE_DIRECTORY:-$HOME/.local/state/tune-sentinelle}}"
IDENTIFIANTS="${CREDENTIALS_DIRECTORY:-}"
A_BLANC=0

journal() { printf '%s sentinelle: %s\n' "$(date -u +%FT%TZ)" "$*" >&2; }

secret() {
  local f="$IDENTIFIANTS/$1"
  [ -n "$IDENTIFIANTS" ] && [ -r "$f" ] && tr -d '\r\n' <"$f"
}

# ── pur : decrire un rapport ───────────────────────────────────────────────

# L'etat (success/failure) et la description (140 caracteres au plus) du
# statut de commit, d'apres le rapport du serveur.
statut_de_commit() {
  jq -r '
    (if .statut == "ok" then "success" else "failure" end) as $etat
    | ([.resultats[] | select(.etat == "ok") | .service]) as $ok
    | ([.resultats[] | select(.etat == "echec") | "\(.service) (\(.cause // "?"))"]) as $ko
    | ([.resultats[] | select(.etat == "non_couvert") | .service]) as $nc
    | (if $etat == "success"
        then "ok : " + ($ok | join(", "))
        else "echec : " + (if ($ko | length) > 0 then ($ko | join(", ")) else "aucune sonde" end)
       end
       + (if ($nc | length) > 0 then " ; non couverts : " + ($nc | join(", ")) else "" end)) as $d
    | "\($etat)\t\($d[0:140])"'
}

# L'etat par service, pour comparer au dernier etat NOTIFIE.
etats_par_service() {
  jq -c '[.resultats[] | {key: "\(.nature):\(.service)", value: .etat}] | from_entries'
}

# Les changements entre deux cartes d'etats : une ligne par service change.
changements() {
  local avant="$1" apres="$2"
  jq -rn --argjson a "$avant" --argjson b "$apres" '
    ($a + $b | keys) as $k
    | $k[] | select(($a[.] // "absent") != ($b[.] // "absent"))
    | "\(.) : \($a[.] // "absent") -> \($b[.] // "absent")"'
}

# ── effets ─────────────────────────────────────────────────────────────────

demander_un_tour() {
  local jeton
  jeton="$(secret tune-admin || true)"
  if [ -n "$jeton" ]; then
    printf 'header = "Authorization: Bearer %s"\n' "$jeton" |
      curl -sS --fail-with-body -m 900 -X POST -K - "$API/sentinelle/streaming/sonder"
  else
    curl -sS --fail-with-body -m 900 -X POST "$API/sentinelle/streaming/sonder"
  fi
}

publier_le_statut() {
  local sha="$1" etat="$2" description="$3" jeton corps
  jeton="$(secret github-token || true)"
  if [ -z "$jeton" ]; then
    journal "pas d'identifiant github-token : statut non publie"
    return 0
  fi
  corps="$(jq -nc --arg s "$etat" --arg d "$description" \
    '{state: $s, context: "sentinelle/streaming", description: $d}')"
  if [ "$A_BLANC" = 1 ]; then
    journal "a blanc : statut $etat sur $sha : $description"
    return 0
  fi
  printf 'header = "Authorization: Bearer %s"\n' "$jeton" |
    curl -sS --fail-with-body -m 30 -K - \
      -H 'Accept: application/vnd.github+json' \
      -H 'X-GitHub-Api-Version: 2022-11-28' \
      -X POST "https://api.github.com/repos/$DEPOT/statuses/$sha" \
      -d "$corps" >/dev/null
  journal "statut $etat publie sur $sha"
}

envoyer_l_alerte() {
  local sujet="$1" texte="$2" cle corps
  cle="$(secret resend-key || true)"
  if [ -z "$cle" ]; then
    journal "pas d'identifiant resend-key : alerte non envoyee ($sujet)"
    return 1
  fi
  corps="$(jq -nc --arg de "$MAIL_DE" --arg a "$MAIL_A" --arg cci "$MAIL_CCI" \
    --arg s "$sujet" --arg t "$texte" \
    '{from: $de, to: [$a], bcc: [$cci], subject: $s, text: $t}')"
  if [ "$A_BLANC" = 1 ]; then
    journal "a blanc : courriel « $sujet »"
    return 0
  fi
  printf 'header = "Authorization: Bearer %s"\n' "$cle" |
    curl -sS --fail-with-body -m 30 -K - \
      -H 'Content-Type: application/json' \
      -X POST https://api.resend.com/emails -d "$corps" >/dev/null
  journal "alerte envoyee : $sujet"
}

un_tour() {
  mkdir -p "$ETAT"
  local rapport notifie avant apres diff sha ligne etat description
  notifie="$ETAT/etat-notifie.json"
  [ -f "$notifie" ] || echo '{}' >"$notifie"
  avant="$(cat "$notifie")"

  if ! rapport="$(demander_un_tour 2>&1)"; then
    journal "serveur injoignable : $rapport"
    apres="$(jq -c '. + {"serveur:tune": "injoignable"}' <<<"$avant")"
  elif ! jq -e .statut >/dev/null 2>&1 <<<"$rapport"; then
    journal "reponse illisible du serveur"
    apres="$(jq -c '. + {"serveur:tune": "illisible"}' <<<"$avant")"
  else
    printf '%s\n' "$(jq -c . <<<"$rapport")" >>"$ETAT/rapports.jsonl"
    if [ "$(jq -r .statut <<<"$rapport")" = passe ]; then
      journal "tour passe : $(jq -r '.raison // ""' <<<"$rapport")"
      return 0
    fi
    apres="$(etats_par_service <<<"$rapport" | jq -c '. + {"serveur:tune": "ok"}')"
    ligne="$(statut_de_commit <<<"$rapport")"
    etat="${ligne%%$'\t'*}"
    description="${ligne#*$'\t'}"
    sha="$(jq -r '.binaire.sha // empty' <<<"$rapport")"
    journal "tour $etat : $description"
    if [ -n "$sha" ]; then
      publier_le_statut "$sha" "$etat" "$description" || journal "publication du statut en echec"
    else
      journal "binaire sans TUNE_GIT_SHA : aucun statut publiable (la porte de release restera fermee)"
    fi
  fi

  diff="$(changements "$avant" "$apres")"
  if [ -n "$diff" ]; then
    local sujet="Tune sentinelle : changement d'etat"
    grep -qE -- '-> (echec|injoignable|illisible)' <<<"$diff" && sujet="Tune sentinelle : ECHEC"
    local detail
    detail="$(jq . 2>/dev/null <<<"$rapport" || printf '%s' "$rapport")"
    if envoyer_l_alerte "$sujet" "$(printf '%s\n\n%s\n' "$diff" "$detail")"; then
      [ "$A_BLANC" = 1 ] || printf '%s\n' "$apres" >"$notifie"
    fi
  fi
}

autotest() {
  local echecs=0
  verifier() {
    if [ "$2" = "$3" ]; then echo "ok: $1"; else
      echo "ECHEC $1"; echo "  attendu : $3"; echo "  obtenu  : $2"; echecs=$((echecs + 1)); fi
  }
  local vert rouge seul_nc
  vert='{"statut":"ok","resultats":[{"service":"qobuz","nature":"service","etat":"ok"},{"service":"radio","nature":"radio","etat":"ok"},{"service":"spotify","nature":"service","etat":"non_couvert"}]}'
  rouge='{"statut":"echec","resultats":[{"service":"qobuz","nature":"service","etat":"ok"},{"service":"tidal","nature":"service","etat":"echec","cause":"http"}]}'
  seul_nc='{"statut":"echec","resultats":[{"service":"spotify","nature":"service","etat":"non_couvert"}]}'
  verifier "un tour vert donne success" "$(statut_de_commit <<<"$vert")" \
    "$(printf 'success\tok : qobuz, radio ; non couverts : spotify')"
  verifier "un echec HTTP de TIDAL donne failure et le nomme" "$(statut_de_commit <<<"$rouge")" \
    "$(printf 'failure\techec : tidal (http)')"
  verifier "sans aucune sonde : failure" "$(statut_de_commit <<<"$seul_nc")" \
    "$(printf 'failure\techec : aucune sonde ; non couverts : spotify')"
  local a b
  a="$(etats_par_service <<<"$vert")"
  b="$(etats_par_service <<<"$rouge")"
  verifier "le premier echec se dit" "$(changements '{"service:tidal":"ok"}' '{"service:tidal":"echec"}')" \
    "service:tidal : ok -> echec"
  verifier "un echec qui dure ne se redit pas" "$(changements '{"service:tidal":"echec"}' '{"service:tidal":"echec"}')" ""
  verifier "le retour a la normale se dit" "$(changements '{"service:tidal":"echec"}' '{"service:tidal":"ok"}')" \
    "service:tidal : echec -> ok"
  verifier "carte d'etats" "$(changements "$a" "$a")" ""
  [ -n "$(changements "$a" "$b")" ] && echo "ok: deux tours differents se distinguent" ||
    { echo "ECHEC deux tours differents se distinguent"; echecs=$((echecs + 1)); }
  [ "$echecs" -eq 0 ] && echo "autotest : tous les cas tiennent" || { echo "autotest : $echecs echec(s)"; return 1; }
}

case "${1:-}" in
  --autotest) autotest ;;
  --a-blanc) A_BLANC=1; un_tour ;;
  "") un_tour ;;
  *) echo "usage: $0 [--a-blanc|--autotest]" >&2; exit 2 ;;
esac
