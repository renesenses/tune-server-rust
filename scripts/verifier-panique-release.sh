#!/usr/bin/env bash
# Contre-épreuve #2305 / #2306 — sur le VÉRITABLE binaire release.
#
# ⚠️ Pourquoi un script et pas un `cargo test` : Cargo IGNORE le réglage `panic`
# du profil pour les cibles de test et force toujours l'unwind. Un test resterait
# donc vert avec `panic = "abort"` dans Cargo.toml — c'est-à-dire exactement dans
# la configuration qui tue le serveur en production. Le seul témoin valable est
# un binaire construit en release et lancé comme processus enfant (JP Robbe).
#
# Quatre vérifications :
#   1. le processus SURVIT au décodage d'un fichier malformé ;
#   2. le décodage rend une ERREUR pour ce fichier ;
#   3. `symphonia_decoder_panic` est journalisé ;
#   4. une panique volontaire nomme le VRAI symbole Rust, pas un `onig_*`.
set -uo pipefail

racine="$(cd "$(dirname "$0")/.." && pwd)"
cd "$racine"

echec=0
verifier() { # <libellé> <condition déjà évaluée : 0 = ok>
  if [ "$2" -eq 0 ]; then printf '  ✅ %s\n' "$1"
  else printf '  ❌ %s\n' "$1"; echec=1; fi
}

echo "== Profil release effectif =="
sed -n '/^\[profile\.release\]/,/^\[/p' Cargo.toml | grep -E '^(panic|strip)' | sed 's/^/  /'
grep -q '^panic = "unwind"' Cargo.toml; verifier 'panic = "unwind"' $?
grep -q '^strip = "debuginfo"' Cargo.toml; verifier 'strip = "debuginfo"' $?

echo
echo "== Construction du témoin en release =="
cargo build --release --example temoin_panique_release -p tune-core >/dev/null 2>&1
verifier "le témoin compile en release" $?
temoin="target/release/examples/temoin_panique_release"
[ -x "$temoin" ]; verifier "binaire présent : $temoin" $?
[ -x "$temoin" ] || { echo; echo "ABANDON"; exit 1; }

echo
echo "== 1-3. Un fichier malformé ne doit pas tuer le processus =="
travail="$(mktemp -d)"
trap 'rm -rf "$travail"' EXIT
malforme="$travail/malforme.m4a"
# Un en-tête m4a plausible suivi d'octets aléatoires : le conteneur est reconnu,
# le flux ne l'est pas. C'est le cas de #2305.
head -c 2048 tune-core/tests/fixtures/test.m4a > "$malforme" 2>/dev/null \
  || printf '\x00\x00\x00\x20ftypM4A ' > "$malforme"
head -c 8192 /dev/urandom >> "$malforme"

sortie="$travail/sortie.txt"
RUST_LOG=error "$temoin" decodage "$malforme" > "$sortie" 2>&1
code=$?

printf '  code de sortie : %s\n' "$code"
[ "$code" -eq 0 ]; verifier "le processus se termine normalement (pas d'abort)" $?
grep -q "PROCESSUS_VIVANT" "$sortie"; verifier "le processus a survécu au décodage" $?
grep -q "DECODAGE_ERREUR" "$sortie"; verifier "le décodage rend une erreur pour ce fichier" $?

# La panique interceptée n'est journalisée que si symphonia panique vraiment.
# Un fichier simplement refusé sans panique est un succès plus faible mais
# légitime : on le DIT plutôt que de le faire passer pour la preuve attendue.
if grep -q "symphonia_decoder_panic" "$sortie"; then
  printf '  ✅ %s\n' "symphonia_decoder_panic journalisé — le catch_unwind a bien repris la main"
else
  printf '  ⚠️  %s\n' "pas de symphonia_decoder_panic : ce fichier a été REFUSÉ sans paniquer."
  printf '     %s\n' "La survie est prouvée, l'interception ne l'est pas. Fournir un fixture"
  printf '     %s\n' "qui fait réellement paniquer symphonia pour compléter cette épreuve."
fi

echo
echo "== 3 bis. catch_unwind doit être OPÉRANT dans le binaire release =="
# C'est la propriété du PROFIL, et le vrai sujet de #2305 : `decode.rs` porte
# déjà ses `catch_unwind` (lignes 439 et 485), l'installateur aussi, et le hook
# `tune-crash.log` également. Sous `panic = "abort"` ils sont tous décoratifs et
# ce mode tue le processus.
interception="$travail/interception.txt"
"$temoin" interception > "$interception" 2>&1
code_int=$?
[ "$code_int" -eq 0 ]; verifier "le processus survit à une panique interceptée" $?
grep -q "INTERCEPTION_OK" "$interception"; verifier "catch_unwind a repris la main" $?

echo
echo "== 4. Le backtrace doit nommer le vrai symbole =="
trace="$travail/trace.txt"
RUST_BACKTRACE=1 "$temoin" panique > "$trace" 2>&1
code_panique=$?
[ "$code_panique" -ne 0 ]; verifier "une panique volontaire fait bien échouer le processus" $?
grep -q "fonction_temoin_de_crash" "$trace"; verifier "le backtrace nomme fonction_temoin_de_crash" $?
if grep -qi "onig_" "$trace"; then
  printf '  ❌ %s\n' "le backtrace contient des symboles onig_* : la table est encore amputée (#2306)"
  echec=1
else
  printf '  ✅ %s\n' "aucun symbole onig_* parasite"
fi

echo
if [ "$echec" -eq 0 ]; then echo "RÉSULTAT : contre-épreuve PASSÉE"; else
  echo "RÉSULTAT : contre-épreuve ÉCHOUÉE"
  echo "--- sortie du décodage ---"; cat "$sortie"
  echo "--- backtrace ---"; head -30 "$trace"
fi
exit "$echec"
