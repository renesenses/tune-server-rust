#!/usr/bin/env bash
# Determine si une execution doit lancer la batterie complete de CI.
#
# Sortie unique : `rapide` ou `complet`. Toute entree inconnue est traitee
# comme `complet` : une panne du routage peut couter des runners, jamais
# supprimer silencieusement une verification.

set -euo pipefail

profil_ci() {
  local evenement="${1:-}"
  local base_ref="${2:-}"
  local forcer_complet="${3:-false}"

  if [ "$evenement" != "pull_request" ]; then
    printf '%s\n' complet
    return
  fi

  if [ "$forcer_complet" = "true" ]; then
    printf '%s\n' complet
    return
  fi

  case "$base_ref" in
    batch/*) printf '%s\n' rapide ;;
    *) printf '%s\n' complet ;;
  esac
}

autotest() {
  local echecs=0

  verifier() {
    local attendu="$1"
    local evenement="$2"
    local base_ref="$3"
    local forcer_complet="$4"
    local obtenu

    obtenu="$(profil_ci "$evenement" "$base_ref" "$forcer_complet")"
    if [ "$obtenu" != "$attendu" ]; then
      printf 'ECHEC: evenement=%s base=%s force=%s: attendu=%s obtenu=%s\n' \
        "$evenement" "$base_ref" "$forcer_complet" "$attendu" "$obtenu" >&2
      echecs=$((echecs + 1))
    fi
  }

  # Seule une PR de correctif explicitement dirigee vers un lot est rapide.
  verifier rapide pull_request batch/v0.9-audio false

  # L'operateur peut forcer les contrats complets sur un correctif risque.
  verifier complet pull_request batch/v0.9-audio true

  # Une integration de lot a release/v0.9 et toute PR directe restent completes.
  verifier complet pull_request release/v0.9 false
  verifier complet pull_request main false
  verifier complet pull_request feature/empilee false

  # Les pushes, lancements manuels et entrees incompletes sont fail-closed.
  verifier complet push batch/v0.9-audio false
  verifier complet workflow_dispatch "" false
  verifier complet pull_request "" false
  verifier complet evenement-inconnu batch/v0.9-audio false

  if [ "$echecs" -ne 0 ]; then
    printf '%s contre-epreuve(s) en echec\n' "$echecs" >&2
    return 1
  fi
  printf 'determiner-profil-ci: contre-epreuves OK\n'
}

if [ "${1:-}" = "--autotest" ]; then
  autotest
else
  profil_ci "${EVENEMENT:-}" "${BASE_REF:-}" "${FORCER_COMPLET:-false}"
fi
