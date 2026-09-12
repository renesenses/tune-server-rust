#!/usr/bin/env bash
# REF-0 — Liste NOMINATIVE des tests, par matrice CI et par binaire.
#
# Pourquoi : un compte de tests ne prouve rien (« 2 651 verts » masque un test
# disparu et un test apparu). La règle du chantier est qu'aucune assertion
# n'est supprimée ni affaiblie : il faut donc les NOMS, avant et après.
#
# Les trois matrices sont celles de .github/workflows/ci.yml — recopiées
# telles quelles, pas déduites. Le raccourci `--features tune-server/oaat`
# ne compile pas outputs/local.rs (local-audio absent) : il n'est pas ici.
#
# Usage :   scripts/refonte/tests-nominatifs.sh <dossier_de_sortie> [m1|m2|m3 …]
#   Sans sélection, les trois matrices.
#   Respecte CARGO_TARGET_DIR et CARGO_BUILD_JOBS (sur Shrek : exporter les
#   deux avant, l'export ne survit pas à la session ssh).
#
# Sortie dans <dossier> :
#   commit.txt          — le commit mesuré
#   tests-m<N>.txt      — « binaire<TAB>nom::du::test », trié
#   doctests-m<N>.txt   — doctests par paquet (« paquet<TAB>chemin - nom (line N) »)
#   resume.txt          — comptes par fichier
# Puis `scripts/refonte/comparer.sh <parent> <pr>`.
set -euo pipefail

racine="$(git rev-parse --show-toplevel)"; cd "$racine"
out="${1:?dossier de sortie requis}"; shift || true
mkdir -p "$out"
git rev-parse HEAD > "$out/commit.txt"

# Les matrices, mot pour mot depuis ci.yml (lignes 220, 257, 307 au 03/09/2026).
declare -A MATRICE
MATRICE[m1]='-p tune-core -p tune-http-types -p tune-smart-http -p tune-stream-http -p tune-streaming-http -p tune-server -p tune-bridge --no-default-features --features oaat,cloud-relay,bandcamp'
MATRICE[m2]='-p tune-server --no-default-features --features oaat,local-audio,postgres,dj,karaoke,bandcamp,concerts,plugins-wasm,audio-embedding'
MATRICE[m3]='-p tune-core --features audio-embedding'

choix=("$@"); [ ${#choix[@]} -eq 0 ] && choix=(m1 m2 m3)

# Un binaire de test s'appelle `nom-<hash>` : on retire le hash pour que le
# nom reste stable d'une compilation à l'autre.
nom_stable() { basename "$1" | sed -E 's/-[0-9a-f]{8,}$//'; }

for m in "${choix[@]}"; do
  args="${MATRICE[$m]:?matrice inconnue : $m}"
  echo "== $m : cargo test --no-run $args" >&2
  # shellcheck disable=SC2086
  cargo test --no-run --message-format=json-render-diagnostics $args \
    | jq -r 'select(.reason=="compiler-artifact" and .executable!=null and (.profile.test==true)) | .executable' \
    | sort -u > "$out/.exes-$m"

  : > "$out/tests-$m.txt"
  while IFS= read -r exe; do
    nom="$(nom_stable "$exe")"
    "$exe" --list --format terse 2>/dev/null \
      | sed -nE 's/^(.*): test$/\1/p' \
      | sed "s/^/${nom}\t/" >> "$out/tests-$m.txt"
  done < "$out/.exes-$m"
  sort -o "$out/tests-$m.txt" "$out/tests-$m.txt"
  rm -f "$out/.exes-$m"

  # Doctests : un seul appel avec les arguments exacts de la matrice — les
  # features sont celles de la matrice, elles n'existent pas dans chaque
  # paquet pris isolément (`bandcamp` est un feature de tune-server, pas de
  # tune-core). La ligne listée porte déjà le chemin du fichier, donc le
  # paquet : « tune-core/src/x.rs - module::item (line N) ».
  # shellcheck disable=SC2086
  cargo test --doc $args -- --list 2>/dev/null \
    | sed -nE 's/^(.*): test$/\1/p' \
    | sort > "$out/doctests-$m.txt" \
    || echo "⚠ $m : la liste des doctests a échoué, fichier vide" >&2
done

{
  echo "commit $(cat "$out/commit.txt")"
  for f in "$out"/tests-*.txt "$out"/doctests-*.txt; do
    [ -f "$f" ] && printf '%6d  %s\n' "$(wc -l < "$f")" "$(basename "$f")"
  done
} | tee "$out/resume.txt" >&2
