#!/usr/bin/env bash
# REF-0 — Inventaire des gardes qui relisent un fichier source par son chemin.
#
# Trois formes existent dans ce dépôt, et une découpe de fichier les casse
# même sans changer une ligne de production :
#   - `include_str!("../orchestrator.rs")` — le test embarque le source ;
#   - `read_to_string("src/outputs/local.rs")` — lecture à l'exécution ;
#   - `#[path = "…"] mod …;` — déclaration de module par chemin.
#
# Le piège : la garde vit souvent dans un fichier VOISIN de celui qu'on
# découpe (resample.rs lit local.rs, zone_manager.rs lit zones.rs). Un grep
# dans le fichier découpé ne la voit pas. Ce script les liste toutes, par
# LECTEUR et par fichier LU, en chemins résolus depuis la racine du dépôt.
#
# Usage :   scripts/refonte/gardes.sh [ref] [fichier_de_sortie]
#   ref     : commit à inspecter (défaut HEAD) — permet de mesurer le PARENT
#             d'une PR sans changer de branche.
#   sortie  : défaut stdout.
#
# Sortie, une ligne par garde, triée, tabulée :
#   <lecteur>:<ligne>  <forme>  <fichier lu, chemin dépôt>
# Rejouer sur le parent et sur la PR, puis `scripts/refonte/comparer.sh`.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
ref="${1:-HEAD}"
out="${2:-/dev/stdout}"

# Normalise `a/b/../c` en `a/c` sans toucher au disque (le fichier peut ne pas
# exister sur la ref inspectée, et realpath -m n'existe pas sur macOS).
normaliser() {
  local IFS='/' seg; local -a pile=()
  for seg in $1; do
    case "$seg" in
      ''|'.') ;;
      '..') [ ${#pile[@]} -gt 0 ] && unset 'pile[${#pile[@]}-1]' ;;
      *) pile+=("$seg") ;;
    esac
  done
  (IFS='/'; echo "${pile[*]}")
}

# Le dossier de base d'un chemin selon la forme :
#   include_str! et #[path] sont relatifs au fichier lecteur ;
#   read_to_string est relatif au répertoire de la caisse (cargo test y place
#   le processus), c'est-à-dire au premier segment du chemin du lecteur.
git grep -n -E \
  'include_str!\([[:space:]]*"[^"]*\.rs"[[:space:]]*\)|read_to_string\([[:space:]]*([A-Za-z_:]*Path::new\()?[[:space:]]*"[^"]*\.rs"|#\[path[[:space:]]*=[[:space:]]*"[^"]+"[[:space:]]*\]' \
  "$ref" -- '*.rs' \
| sed -E "s#^${ref}:##" \
| while IFS= read -r ligne; do
    lecteur="${ligne%%:*}"; reste="${ligne#*:}"
    numero="${reste%%:*}"; contenu="${reste#*:}"
    if [[ "$contenu" =~ include_str!\(\ *\"([^\"]*\.rs)\" ]]; then
      forme="include_str"; cible="${BASH_REMATCH[1]}"
      base="$(dirname "$lecteur")"
    elif [[ "$contenu" =~ read_to_string\(.*\"([^\"]*\.rs)\" ]]; then
      forme="read_to_string"; cible="${BASH_REMATCH[1]}"
      base="${lecteur%%/*}"
    elif [[ "$contenu" =~ \#\[path\ *=\ *\"([^\"]+)\" ]]; then
      forme="mod_path"; cible="${BASH_REMATCH[1]}"
      base="$(dirname "$lecteur")"
    else
      continue
    fi
    printf '%s:%s\t%s\t%s\n' "$lecteur" "$numero" "$forme" "$(normaliser "$base/$cible")"
  done \
| sort > "$out"
