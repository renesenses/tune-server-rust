#!/usr/bin/env bash
# Decide si une modification peut affecter le binaire Rust.
#
# Entree : chemins Git separes par NUL sur STDIN
#          (`git diff --no-renames --name-only -z`).
# Sortie : 0 seulement si TOUS les chemins appartiennent a la petite liste
#          explicitement non-Rust ; 1 au moindre doute, y compris un diff vide.
#
# Cette liste est volontairement fermee. Ajouter un type de fichier ici exige
# une contre-epreuve : une erreur coute une compilation inutile ; une erreur
# dans l'autre sens laisse passer du code non teste.
set -uo pipefail

impact_non_rust() {
  local chemin nombre=0

  while IFS= read -r -d '' chemin; do
    [ -n "$chemin" ] || continue
    nombre=$((nombre + 1))
    case "$chemin" in
      .github/workflows/fermeture-issues.yml \
        | .github/workflows/refs-issues.yml \
        | docs/* | *.md | LICENSE | LICENSE.* \
        | scripts/verifier-fermeture.sh \
        | scripts/verifier-refs-issues.sh)
        ;;
      *)
        printf 'impact Rust : %s\n' "$chemin"
        return 1
        ;;
    esac
  done

  if [ "$nombre" -eq 0 ]; then
    printf 'impact Rust par securite : diff vide\n'
    return 1
  fi

  printf 'impact non-Rust : %d fichier(s), voie rapide autorisee\n' "$nombre"
  return 0
}

autotest() {
  local echecs=0

  attendu() {
    local libelle="$1" code_attendu="$2"
    shift 2

    if [ "$#" -eq 0 ]; then
      impact_non_rust </dev/null >/dev/null
    else
      impact_non_rust < <(printf '%s\0' "$@") >/dev/null
    fi
    local code=$?

    if [ "$code" -eq "$code_attendu" ]; then
      printf '  ok    %s\n' "$libelle"
    else
      printf '  ECHEC %s (attendu %d, obtenu %d)\n' "$libelle" "$code_attendu" "$code"
      echecs=$((echecs + 1))
    fi
  }

  # Cas reel de #2428 : aucune compilation Rust ne pouvait verifier ces deux
  # fichiers ; elle a pourtant coute 29 min 44 de runners et 9 min 38 de mur.
  attendu 'workflow + garde-fou shell' 0 \
    '.github/workflows/fermeture-issues.yml' \
    'scripts/verifier-fermeture.sh'
  attendu 'documentation seule' 0 'docs/ARCHITECTURE.md' 'README.md'
  # Chaque chemin potentiellement embarque ou compile doit garder la CI Rust.
  attendu 'source Rust' 1 'tune-core/src/audio/decode.rs'
  attendu 'dependances verrouillees' 1 'Cargo.lock'
  attendu 'migration SQL' 1 'tune-core/migrations/0042_audio.sql'
  attendu 'script de livraison inconnu' 1 'scripts/bump-all.sh'
  attendu 'la CI elle-meme exige une passe complete' 1 '.github/workflows/ci.yml'
  attendu 'le workflow de release exige une passe complete' 1 '.github/workflows/release.yml'
  attendu 'un script GitHub inconnu exige une passe complete' 1 '.github/scripts/forum-watch.py'
  attendu 'le classifieur exige sa propre passe complete' 1 'scripts/detecter-impact-ci.sh'
  attendu 'melange doc et code' 1 'README.md' 'tune-server/src/main.rs'
  attendu 'renommage Rust vers documentation' 1 'tune-core/src/ancien.rs' 'docs/ancien.md'
  attendu 'diff vide fail-closed' 1

  if [ "$echecs" -gt 0 ]; then
    printf '\n%d contre-epreuve(s) en echec.\n' "$echecs"
    return 1
  fi
  printf '\nToutes les contre-epreuves d impact passent.\n'
  return 0
}

if [ "${1:-}" = "--autotest" ]; then
  autotest
  exit $?
fi

impact_non_rust
