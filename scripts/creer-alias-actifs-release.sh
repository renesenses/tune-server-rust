#!/usr/bin/env bash
# Cree les noms stables utilises par /releases/latest/download sans remplacer
# les actifs versionnes, qui restent le contrat de l'auto-update.

set -euo pipefail

creer_aliases() {
  local racine="$1"
  local tag="$2"
  local version="${tag#v}"
  local destination="$racine/aliases-stables"
  local specification plateforme extension source nom_source nom_alias
  local -a trouves

  mkdir -p "$destination"

  for specification in \
    "linux-x86_64:tar.gz" \
    "linux-aarch64:tar.gz" \
    "linux-aarch64-musl:tar.gz"
  do
    plateforme="${specification%%:*}"
    extension="${specification#*:}"
    nom_source="tune-server-v${version}-${plateforme}.${extension}"
    nom_alias="tune-server-${plateforme}.${extension}"

    trouves=()
    while IFS= read -r -d '' source; do
      trouves+=("$source")
    done < <(find "$racine" -type f -name "$nom_source" -print0)

    if [ "${#trouves[@]}" -eq 0 ]; then
      # Un re-run partiel peut ne telecharger qu'une partie de la matrice. La
      # release conserve alors l'alias deja publie au lieu d'en fabriquer un
      # a partir d'un autre binaire.
      printf 'avertissement: %s absent de ce run, alias conserve tel quel\n' \
        "$nom_source" >&2
      continue
    fi
    if [ "${#trouves[@]}" -ne 1 ]; then
      printf 'erreur: %s existe %d fois dans les artefacts\n' \
        "$nom_source" "${#trouves[@]}" >&2
      return 1
    fi

    cp "${trouves[0]}" "$destination/$nom_alias"
    cmp -s "${trouves[0]}" "$destination/$nom_alias" || {
      printf 'erreur: alias %s different de sa source\n' "$nom_alias" >&2
      return 1
    }
    printf '%s -> %s\n' "$nom_source" "$nom_alias"
  done
}

autotest() {
  TEMPORAIRE_AUTOTEST="$(mktemp -d)"
  trap 'rm -rf -- "$TEMPORAIRE_AUTOTEST"' EXIT

  mkdir -p "$TEMPORAIRE_AUTOTEST/linux-x86" "$TEMPORAIRE_AUTOTEST/linux-musl"
  printf 'gnu-x86\n' > \
    "$TEMPORAIRE_AUTOTEST/linux-x86/tune-server-v0.9.999-linux-x86_64.tar.gz"
  printf 'musl-arm\n' > \
    "$TEMPORAIRE_AUTOTEST/linux-musl/tune-server-v0.9.999-linux-aarch64-musl.tar.gz"

  creer_aliases "$TEMPORAIRE_AUTOTEST" v0.9.999

  cmp -s \
    "$TEMPORAIRE_AUTOTEST/linux-x86/tune-server-v0.9.999-linux-x86_64.tar.gz" \
    "$TEMPORAIRE_AUTOTEST/aliases-stables/tune-server-linux-x86_64.tar.gz"
  cmp -s \
    "$TEMPORAIRE_AUTOTEST/linux-musl/tune-server-v0.9.999-linux-aarch64-musl.tar.gz" \
    "$TEMPORAIRE_AUTOTEST/aliases-stables/tune-server-linux-aarch64-musl.tar.gz"
  test ! -e "$TEMPORAIRE_AUTOTEST/aliases-stables/tune-server-linux-aarch64.tar.gz"

  printf 'creer-alias-actifs-release: contre-epreuves OK\n'
}

if [ "${1:-}" = "--autotest" ]; then
  autotest
else
  if [ "$#" -ne 2 ]; then
    printf 'usage: %s <repertoire-artefacts> <tag-vX.Y.Z>\n' "$0" >&2
    exit 2
  fi
  creer_aliases "$1" "$2"
fi
