#!/usr/bin/env bash
# Pre-version ou « Latest » ? Rend `true` ou `false` sur la sortie standard.
#
# Usage : scripts/canal-de-release.sh <tag> <manifeste>
#         scripts/canal-de-release.sh --autotest
#
# Regle, dans cet ordre :
#   1. le manifeste du train porte `"prerelease": <booleen>` : il tranche ;
#   2. sinon, un `-` dans le tag fait une pre-version (v0.9.0-rc2), un tag
#      X.Y.Z nu une release stable.
#
# Pourquoi le manifeste : le 29/09/2026, Bertrand decide que la 1.0.0-rc1 part
# chez TOUT LE MONDE — release « Latest », Docker :latest, Homebrew, .deb —
# alors qu'elle porte un suffixe. release.yml est declenche par le tag et n'a
# pas d'entree ; le manifeste `.release/<tag>.json` est fige sur le commit
# tague, revu en PR, et deja lu par release.yml, le controleur et la
# promotion. C'est la seule source qui soit a la fois explicite et versionnee.
#
# Lu par : release.yml (drapeau du brouillon) et promote-release.yml (qui
# refuse un brouillon dont le drapeau differe, et saute les canaux stables
# pour une pre-version).
set -euo pipefail

canal() {
  local tag="$1" plan="$2" defaut=false valeur
  case "$tag" in *-*) defaut=true ;; esac
  if [ ! -f "$plan" ]; then
    printf '%s\n' "$defaut"
    return 0
  fi
  valeur="$(jq -r --arg d "$defaut" '
    if has("prerelease") then
      (if (.prerelease | type) == "boolean" then (.prerelease | tostring) else "invalide" end)
    else $d end' "$plan")" || { echo "::error::$plan illisible" >&2; return 1; }
  case "$valeur" in
    true|false) printf '%s\n' "$valeur" ;;
    *) echo "::error::$plan : \"prerelease\" doit etre un booleen JSON (true/false), pas une chaine" >&2; return 1 ;;
  esac
}

autotest() {
  local d echecs=0
  d="$(mktemp -d)"; trap 'rm -rf "$d"' RETURN
  attendu() {
    local tag="$1" contenu="$2" voulu="$3" obtenu
    if [ "$contenu" = ABSENT ]; then rm -f "$d/m.json"; else printf '%s' "$contenu" > "$d/m.json"; fi
    obtenu="$(canal "$tag" "$d/m.json" 2>/dev/null || echo ERREUR)"
    if [ "$obtenu" = "$voulu" ]; then
      echo "ok: $tag $contenu -> $obtenu"
    else
      echo "ECHEC: $tag $contenu -> $obtenu (attendu $voulu)"; echecs=$((echecs + 1))
    fi
  }
  attendu v0.9.169        ABSENT                                   false
  attendu v0.9.169        '{"version":"0.9.169"}'                  false
  attendu v1.0.0-rc1      '{"version":"1.0.0-rc1"}'                true
  attendu v1.0.0-rc1      '{"version":"1.0.0-rc1","prerelease":false}' false
  attendu v1.0.0-rc2      '{"version":"1.0.0-rc2","prerelease":true}'  true
  attendu v1.0.0          '{"version":"1.0.0","prerelease":true}'  true
  attendu v1.0.0-rc0-test ABSENT                                   true
  # Une chaine n'est pas un booleen : "false" passerait pour vrai ailleurs.
  attendu v1.0.0-rc1      '{"prerelease":"false"}'                 ERREUR
  attendu v1.0.0-rc1      '{"prerelease":null}'                    ERREUR
  attendu v1.0.0-rc1      'pas du json'                            ERREUR
  [ "$echecs" -eq 0 ] && echo "autotest : tout est vert" || { echo "autotest : $echecs echec(s)"; return 1; }
}

if [ "${1:-}" = --autotest ]; then
  autotest
  exit $?
fi
[ $# -eq 2 ] || { echo "usage : $0 <tag> <manifeste> | --autotest" >&2; exit 2; }
canal "$1" "$2"
