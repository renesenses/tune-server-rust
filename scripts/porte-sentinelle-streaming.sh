#!/usr/bin/env bash
# Porte de release #7148 : aucun tag sans preuve REELLE que les services de
# streaming jouent avec le binaire candidat.
#
# Le 26/10/2026, la v1.0.0-rc8.1 a ete taguee avec un correctif TIDAL prouve
# contre un faux CDN ; sur le .18, toutes les URL de segments partaient avec
# `amp;` : 403 partout. Les tests ne parlaient a aucun vrai service.
#
# La preuve vient du .18 : sa sentinelle (`POST /api/v1/sentinelle/streaming/
# sonder`, lancee toutes les heures par `scripts/sentinelle-streaming/`)
# resout et DECODE un titre de reference par service connecte, puis publie le
# statut de commit `sentinelle/streaming` sur le SHA dont le binaire est
# construit (`TUNE_GIT_SHA`). Cette porte exige, sur le SHA candidat :
#
#   * le DERNIER statut `sentinelle/streaming` (un vert ancien ne rachete pas
#     un rouge recent) ;
#   * a l'etat `success` ;
#   * pose il y a moins de SENTINELLE_AGE_MAX_H heures (12 par defaut) ;
#   * par un auteur de SENTINELLE_AUTEURS (`renesenses` par defaut).
#
# Le statut peut etre porte par un AUTRE commit (le .18 a teste la tete de la
# RC, la fusion vers main a cree un nouveau SHA) : il est alors accepte si et
# seulement si son ARBRE est celui du candidat — meme contenu, octet pour
# octet. Ce commit se donne en troisieme argument.
#
#   bash scripts/porte-sentinelle-streaming.sh <depot> <sha-candidat> [sha-teste]
#   bash scripts/porte-sentinelle-streaming.sh --autotest
#
# Sortie 0 : porte ouverte. Sortie 1 : refus, la raison sur stderr (::error::).
# Toute reponse d'API illisible est un REFUS : une situation inconnue bloque.

set -euo pipefail

CONTEXTE="sentinelle/streaming"

porte_sentinelle() {
  local repo="$1" candidat="$2" teste="${3:-}" age_max_h="${SENTINELLE_AGE_MAX_H:-12}"
  local auteurs="${SENTINELLE_AUTEURS:-renesenses}" maintenant="${SENTINELLE_MAINTENANT:-$(date -u +%s)}"
  local cible="$candidat"

  if ! [[ "$candidat" =~ ^[0-9a-f]{40}$ ]]; then
    echo "::error::porte sentinelle : SHA candidat invalide ($candidat)" >&2
    return 1
  fi
  if [ -n "$teste" ] && [ "$teste" != "$candidat" ]; then
    if ! [[ "$teste" =~ ^[0-9a-f]{40}$ ]]; then
      echo "::error::porte sentinelle : SHA teste invalide ($teste)" >&2
      return 1
    fi
    local arbre_candidat arbre_teste
    arbre_candidat="$(gh api "repos/$repo/commits/$candidat" --jq .commit.tree.sha)" || {
      echo "::error::porte sentinelle : arbre de $candidat illisible" >&2
      return 1
    }
    arbre_teste="$(gh api "repos/$repo/commits/$teste" --jq .commit.tree.sha)" || {
      echo "::error::porte sentinelle : arbre de $teste illisible" >&2
      return 1
    }
    if [ -z "$arbre_candidat" ] || [ "$arbre_candidat" != "$arbre_teste" ]; then
      echo "::error::porte sentinelle : $teste (arbre $arbre_teste) n'a pas le contenu du candidat $candidat (arbre $arbre_candidat)" >&2
      return 1
    fi
    cible="$teste"
  fi

  local statuts dernier
  statuts="$(gh api "repos/$repo/commits/$cible/statuses?per_page=100")" || {
    echo "::error::porte sentinelle : statuts de $cible illisibles" >&2
    return 1
  }
  # L'API rend les statuts du plus recent au plus ancien.
  dernier="$(jq -c --arg c "$CONTEXTE" '[.[] | select(.context == $c)] | first // empty' <<<"$statuts")" || {
    echo "::error::porte sentinelle : reponse de statuts illisible" >&2
    return 1
  }
  if [ -z "$dernier" ]; then
    echo "::error::porte sentinelle : aucun statut $CONTEXTE sur $cible. Le binaire construit depuis ce SHA (TUNE_GIT_SHA=$cible) doit tourner sur le .18 et sa sentinelle passer au vert." >&2
    return 1
  fi
  local etat auteur quand description pose age_s
  etat="$(jq -r .state <<<"$dernier")"
  auteur="$(jq -r '.creator.login // ""' <<<"$dernier")"
  quand="$(jq -r .updated_at <<<"$dernier")"
  description="$(jq -r '.description // ""' <<<"$dernier")"
  if [ "$etat" != success ]; then
    echo "::error::porte sentinelle : dernier statut $CONTEXTE sur $cible = $etat ($quand) : $description" >&2
    return 1
  fi
  local trouve=0 a
  IFS=',' read -r -a liste <<<"$auteurs"
  for a in "${liste[@]}"; do
    [ "$a" = "$auteur" ] && trouve=1
  done
  if [ "$trouve" != 1 ]; then
    echo "::error::porte sentinelle : statut pose par « $auteur », qui n'est pas dans SENTINELLE_AUTEURS ($auteurs)" >&2
    return 1
  fi
  pose="$(date -u -d "$quand" +%s 2>/dev/null || python3 -c 'import sys,datetime;print(int(datetime.datetime.strptime(sys.argv[1],"%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc).timestamp()))' "$quand")" || {
    echo "::error::porte sentinelle : date illisible ($quand)" >&2
    return 1
  }
  age_s=$((maintenant - pose))
  if [ "$age_s" -gt $((age_max_h * 3600)) ]; then
    echo "::error::porte sentinelle : le vert de $CONTEXTE date de $((age_s / 3600)) h ($quand), plus de $age_max_h h. Relancer la sentinelle sur le .18." >&2
    return 1
  fi
  echo "porte sentinelle ouverte : $CONTEXTE=success sur $cible, par $auteur, il y a $((age_s / 60)) min : $description"
}

autotest() {
  local echecs=0 dossier
  dossier="$(mktemp -d)"
  trap 'rm -rf "$dossier"' RETURN
  local A=1111111111111111111111111111111111111111 B=2222222222222222222222222222222222222222
  local C=3333333333333333333333333333333333333333
  export SENTINELLE_MAINTENANT=1793000000
  local recent ancien
  recent="$(date -u -d @$((SENTINELLE_MAINTENANT - 1800)) +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || python3 -c 'import sys,datetime;print(datetime.datetime.fromtimestamp(int(sys.argv[1]),datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"))' $((SENTINELLE_MAINTENANT - 1800)))"
  ancien="$(date -u -d @$((SENTINELLE_MAINTENANT - 86400)) +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || python3 -c 'import sys,datetime;print(datetime.datetime.fromtimestamp(int(sys.argv[1]),datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"))' $((SENTINELLE_MAINTENANT - 86400)))"

  # Un faux `gh` : les reponses vivent dans des fichiers du dossier d'essai.
  gh() {
    local chemin="$2" f
    f="$dossier/$(printf '%s' "$chemin" | tr '/?=&' '____')"
    [ -f "$f" ] || return 1
    if [ "${3:-}" = --jq ]; then jq -r "$4" "$f"; else cat "$f"; fi
  }
  statuts() { printf '%s' "$2" >"$dossier/repos_r_commits_$1_statuses_per_page_100"; }
  arbre() { printf '{"commit":{"tree":{"sha":"%s"}}}' "$2" >"$dossier/repos_r_commits_$1"; }
  vert() { printf '{"context":"sentinelle/streaming","state":"success","creator":{"login":"%s"},"updated_at":"%s","description":"ok"}' "${2:-renesenses}" "$1"; }
  rouge() { printf '{"context":"sentinelle/streaming","state":"failure","creator":{"login":"renesenses"},"updated_at":"%s","description":"echec: tidal (http)"}' "$1"; }

  cas() {
    local attendu="$1" nom="$2"; shift 2
    local obtenu=0
    porte_sentinelle "$@" >/dev/null 2>&1 || obtenu=1
    if [ "$obtenu" = "$attendu" ]; then
      echo "ok: $nom"
    else
      echo "ECHEC $nom (sortie $obtenu, attendu $attendu)"
      echecs=$((echecs + 1))
    fi
  }

  statuts "$A" "[$(vert "$recent")]"
  cas 0 "vert recent sur le candidat : ouvert" r "$A"
  statuts "$A" "[]"
  cas 1 "aucun statut : refus" r "$A"
  statuts "$A" '[{"context":"ci/autre","state":"success","creator":{"login":"renesenses"},"updated_at":"'"$recent"'"}]'
  cas 1 "un vert d'un AUTRE contexte ne compte pas" r "$A"
  statuts "$A" "[$(rouge "$recent"),$(vert "$ancien")]"
  cas 1 "le dernier est rouge : refus, meme avec un vert plus ancien" r "$A"
  statuts "$A" "[$(vert "$ancien")]"
  cas 1 "vert de 24 h : trop vieux, refus" r "$A"
  statuts "$A" "[$(vert "$recent" intrus)]"
  cas 1 "vert pose par un inconnu : refus" r "$A"
  cas 1 "SHA candidat invalide : refus" r "abc"
  printf 'pas du json' >"$dossier/repos_r_commits_${A}_statuses_per_page_100"
  cas 1 "reponse illisible : refus" r "$A"
  rm -f "$dossier/repos_r_commits_${A}_statuses_per_page_100"
  cas 1 "API en panne : refus" r "$A"

  statuts "$B" "[$(vert "$recent")]"
  arbre "$A" aaaa
  arbre "$B" aaaa
  cas 0 "vert sur un commit de MEME arbre : ouvert" r "$A" "$B"
  arbre "$C" bbbb
  statuts "$C" "[$(vert "$recent")]"
  cas 1 "vert sur un commit d'arbre DIFFERENT : refus" r "$A" "$C"

  unset -f gh
  unset SENTINELLE_MAINTENANT
  if [ "$echecs" -ne 0 ]; then
    echo "autotest : $echecs echec(s)"
    return 1
  fi
  echo "autotest : tous les cas tiennent"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  case "${1:-}" in
    --autotest) autotest ;;
    "") echo "usage: $0 <depot> <sha-candidat> [sha-teste] | --autotest" >&2; exit 2 ;;
    *) porte_sentinelle "$@" ;;
  esac
fi
