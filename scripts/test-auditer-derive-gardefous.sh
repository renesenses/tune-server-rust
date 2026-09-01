#!/usr/bin/env bash
# Contre-épreuve de `auditer-derive-gardefous.sh` (#2816).
#
# Un audit jamais vu rougir ne prouve rien : tant qu'on ne l'a pas regardé
# refuser une dérive, « vert » ne signifie pas « conforme », il signifie
# seulement « le script est allé au bout ». Chaque détection est donc jouée
# DEUX fois — sur un état conforme, où elle doit se taire, et sur la dérive
# correspondante, où elle doit rougir avec un motif nommé.
#
# Aucun accès réseau : `AUDIT_FIXTURES` détourne toutes les lectures d'API vers
# des fichiers. Les six dérives rejouées ici sont celles constatées en vrai sur
# `renesenses/tune-server-rust` le 31 août 2026.
#
# Usage : scripts/test-auditer-derive-gardefous.sh

set -uo pipefail

ICI=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
AUDIT="$ICI/auditer-derive-gardefous.sh"
DEPOT="essai/depot"
RACINE=$(mktemp -d "${TMPDIR:-/tmp}/tune-p1-2816-contre-epreuve-XXXXXX")
trap 'rm -rf "$RACINE"' EXIT

rate=0
b64() { printf '%s' "$1" | base64 | tr -d '\n'; }

# ─────────────────────────────────────────────────────────────────────────────
# L'état CONFORME : deux environnements protégés, aucun secret d'environnement
# qui masque le dépôt, gel des tags actif, armement fermé, portes toutes vertes.
# ─────────────────────────────────────────────────────────────────────────────
poser_base() {
  local d="$1" p="repos_essai_depot"
  mkdir -p "$d"

  echo '{"default_branch":"main"}' > "$d/$p.json"

  echo '[{"type":"file","name":"controleur.yml"}]' \
    > "$d/${p}_contents_.github_workflows_ref_main.json"
  printf '{"content":"%s"}' \
    "$(b64 "jobs:
  tag:
    environment: \${{ inputs.dry_run && 'release-dry-run' || 'release' }}
")" > "$d/${p}_contents_.github_workflows_controleur.yml_ref_main.json"

  cat > "$d/${p}_environments.json" <<'JSON'
{"environments":[
 {"name":"release","updated_at":"2026-08-30T18:00:00Z",
  "protection_rules":[{"type":"required_reviewers"}]},
 {"name":"release-dry-run","updated_at":"2026-08-30T18:00:00Z",
  "protection_rules":[{"type":"branch_policy"}]}]}
JSON

  echo '{"secrets":[{"name":"RELEASE_CONTROLLER_TOKEN"},{"name":"DOCKERHUB_TOKEN"}]}' \
    > "$d/${p}_actions_secrets_per_page_100.json"
  echo '{"secrets":[]}' > "$d/${p}_environments_release_secrets_per_page_100.json"
  echo '{"secrets":[]}' > "$d/${p}_environments_release-dry-run_secrets_per_page_100.json"

  echo '[{"id":77,"name":"Gel des tags","target":"tag","enforcement":"active"}]' \
    > "$d/${p}_rulesets_per_page_100.json"
  cat > "$d/${p}_rulesets_77.json" <<'JSON'
{"id":77,"name":"Gel des tags","target":"tag","enforcement":"active",
 "updated_at":"2026-08-31T18:41:31Z",
 "conditions":{"ref_name":{"include":["refs/tags/v*"],"exclude":[]}}}
JSON
  echo '[{"version_id":1},{"version_id":2}]' > "$d/${p}_rulesets_77_history_per_page_100.json"

  echo '{"variables":[{"name":"RELEASE_PROMOTION_ENABLED","value":"false"},
                      {"name":"RELEASE_CONTROLLER_ENABLED","value":"false"}]}' \
    > "$d/${p}_actions_variables_per_page_100.json"

  echo '[{"type":"required_status_checks","parameters":{"required_status_checks":[
       {"context":"release-gate"},{"context":"Test (PostgreSQL)"}]}}]' \
    > "$d/${p}_rules_branches_main.json"
  echo '{"commit":{"sha":"abc12345deadbeef"}}' > "$d/${p}_branches_main.json"
  echo '{"check_runs":[{"name":"release-gate","conclusion":"success"},
                       {"name":"Test (PostgreSQL)","conclusion":"success"}]}' \
    > "$d/${p}_commits_abc12345deadbeef_check-runs_per_page_100.json"
}

# verifier <attendu:vert|rouge> <intitulé> <motif grep|-> <mutation…>
verifier() {
  local attendu="$1" titre="$2" motif="$3"; shift 3
  local d="$RACINE/$(printf '%s' "$titre" | tr -c 'A-Za-z0-9' '_')"
  poser_base "$d"
  ( P="repos_essai_depot"; cd "$d" && "$@" )
  local sortie code
  # `ENV_SUP` permet de rejouer les chemins de REPLI (jeton bridé + variables
  # injectées par le contexte `vars`), qui ne se traversent que si l'API refuse.
  sortie=$(env ${ENV_SUP:-} AUDIT_FIXTURES="$d" bash "$AUDIT" "$DEPOT" main 2>&1); code=$?
  local obtenu=vert; [ "$code" -ne 0 ] && obtenu=rouge

  if [ "$obtenu" != "$attendu" ]; then
    printf '❌ %-52s attendu %s, obtenu %s\n' "$titre" "$attendu" "$obtenu"
    printf '%s\n' "$sortie" | sed 's/^/     │ /'
    rate=1
    return
  fi
  # Rougir ne suffit pas : une dérive doit rougir POUR SA RAISON. Sans ce
  # contrôle, une erreur de fixture ferait passer le test pour la mauvaise cause.
  if [ "$motif" != "-" ] && ! printf '%s' "$sortie" | grep -q "$motif"; then
    printf '❌ %-52s %s, mais le motif « %s » est absent\n' "$titre" "$obtenu" "$motif"
    printf '%s\n' "$sortie" | sed 's/^/     │ /'
    rate=1
    return
  fi
  printf '✅ %-52s %s\n' "$titre" "$obtenu"
}

echo "══ Contre-épreuve du détecteur de dérive (#2816) ══"
echo

echo "── L'état conforme doit rester vert ──"
verifier vert "état conforme" - true

echo
echo "── A. Environnements (dérive constatée le 31/08) ──"
verifier rouge "A1 environnement référencé mais supprimé" "RÉFÉRENCÉ mais INEXISTANT" \
  bash -c 'jq "{environments:[.environments[]|select(.name!=\"release\")]}" \
             repos_essai_depot_environments.json > t && mv t repos_essai_depot_environments.json'
verifier rouge "A2 environnement recréé sans aucune règle" "ZÉRO règle de protection" \
  bash -c 'jq "{environments:[.environments[]|if .name==\"release\" then .protection_rules=[] else . end]}" \
             repos_essai_depot_environments.json > t && mv t repos_essai_depot_environments.json'
verifier rouge "A3 liste des environnements refusée (403)" "DROIT MANQUANT" \
  bash -c 'echo 403 > repos_essai_depot_environments.code'

echo
echo "── B. Secrets : masquage et résolution asymétrique ──"
verifier rouge "B1 secret dans un env, absent du dépôt et de l'autre" "échouera APRÈS l'approbation humaine" \
  bash -c 'echo "{\"secrets\":[{\"name\":\"JETON_PROMO\"}]}" > repos_essai_depot_environments_release_secrets_per_page_100.json'
verifier vert "B2 même secret partout : résolution garantie" "-" \
  bash -c 'echo "{\"secrets\":[{\"name\":\"JETON_PROMO\"}]}" > repos_essai_depot_environments_release_secrets_per_page_100.json;
           echo "{\"secrets\":[{\"name\":\"JETON_PROMO\"}]}" > repos_essai_depot_environments_release-dry-run_secrets_per_page_100.json'
verifier vert "B3 copie d'env sur un secret du dépôt : signalée" "MASQUE celle du dépôt" \
  bash -c 'echo "{\"secrets\":[{\"name\":\"RELEASE_CONTROLLER_TOKEN\"}]}" > repos_essai_depot_environments_release_secrets_per_page_100.json'
verifier rouge "B4 secrets du dépôt illisibles (403)" "DROIT MANQUANT" \
  bash -c 'echo 403 > repos_essai_depot_actions_secrets_per_page_100.code'

echo
echo "── C. Gel des tags ──"
verifier rouge "C1 gel rouvert et jamais refermé (disabled)" "le gel des tags est OUVERT" \
  bash -c 'sed "s/\"enforcement\":\"active\"/\"enforcement\":\"disabled\"/" repos_essai_depot_rulesets_77.json > t && mv t repos_essai_depot_rulesets_77.json'
verifier rouge "C2 gel en mode evaluate (journalise, ne refuse pas)" "le gel des tags est OUVERT" \
  bash -c 'sed "s/\"enforcement\":\"active\"/\"enforcement\":\"evaluate\"/" repos_essai_depot_rulesets_77.json > t && mv t repos_essai_depot_rulesets_77.json'
verifier rouge "C3 plus aucun ruleset de tag" "AUCUN ruleset de tag" \
  bash -c 'echo "[]" > repos_essai_depot_rulesets_per_page_100.json'
verifier rouge "C4 ruleset de tag qui ne couvre plus v*" "AUCUN ne couvre refs/tags/v" \
  bash -c 'sed "s#refs/tags/v\*#refs/tags/nightly-*#" repos_essai_depot_rulesets_77.json > t && mv t repos_essai_depot_rulesets_77.json'

echo
echo "── D. Armement laissé ouvert ──"
verifier rouge "D1 RELEASE_PROMOTION_ENABLED resté à true" "RELEASE_PROMOTION_ENABLED = true — ARMÉ" \
  bash -c 'sed "s/\"RELEASE_PROMOTION_ENABLED\",\"value\":\"false\"/\"RELEASE_PROMOTION_ENABLED\",\"value\":\"true\"/" repos_essai_depot_actions_variables_per_page_100.json > t && mv t repos_essai_depot_actions_variables_per_page_100.json'
verifier rouge "D2 variables illisibles (403)" "DROIT MANQUANT" \
  bash -c 'echo 403 > repos_essai_depot_actions_variables_per_page_100.code'
# Repli : le jeton du workflow ne peut pas énumérer les variables, mais le
# contexte `vars` en injecte deux. L'armement doit rester détecté — et le fait
# que la liste soit PARTIELLE doit être dit, pas tu.
ENV_SUP="AUDIT_VARS_INJECTEES=RELEASE_PROMOTION_ENABLED=true" \
  verifier rouge "D3 repli sur vars : armement détecté quand même" "RELEASE_PROMOTION_ENABLED = true — ARMÉ" \
  bash -c 'echo 403 > repos_essai_depot_actions_variables_per_page_100.code'
ENV_SUP="AUDIT_VARS_INJECTEES=RELEASE_PROMOTION_ENABLED=false" \
  verifier vert "D4 repli sur vars : la liste partielle est signalée" "ne couvre QUE les noms déjà connus" \
  bash -c 'echo 403 > repos_essai_depot_actions_variables_per_page_100.code'
verifier vert "D5 aucune variable *_ENABLED : dit, pas supposé" "aucune variable \*_ENABLED" \
  bash -c 'echo "{\"variables\":[]}" > repos_essai_depot_actions_variables_per_page_100.json'

echo
echo "── E. Portes requises : présence n'est pas passage ──"
verifier rouge "E1 porte requise CANCELLED (PR pourtant CLEAN)" "n'a JAMAIS réussi" \
  bash -c 'sed "s/\"release-gate\",\"conclusion\":\"success\"/\"release-gate\",\"conclusion\":\"cancelled\"/" repos_essai_depot_commits_abc12345deadbeef_check-runs_per_page_100.json > t && mv t repos_essai_depot_commits_abc12345deadbeef_check-runs_per_page_100.json'
verifier rouge "E2 porte requise SKIPPED" "n'a JAMAIS réussi" \
  bash -c 'sed "s/\"release-gate\",\"conclusion\":\"success\"/\"release-gate\",\"conclusion\":\"skipped\"/" repos_essai_depot_commits_abc12345deadbeef_check-runs_per_page_100.json > t && mv t repos_essai_depot_commits_abc12345deadbeef_check-runs_per_page_100.json'
verifier vert "E3 une seule réussite parmi plusieurs suffit" "-" \
  bash -c 'echo "{\"check_runs\":[{\"name\":\"release-gate\",\"conclusion\":\"cancelled\"},{\"name\":\"release-gate\",\"conclusion\":\"success\"},{\"name\":\"Test (PostgreSQL)\",\"conclusion\":\"success\"}]}" > repos_essai_depot_commits_abc12345deadbeef_check-runs_per_page_100.json'
verifier rouge "E4 porte requise sans aucune exécution" "AUCUNE exécution" \
  bash -c 'echo "{\"check_runs\":[{\"name\":\"release-gate\",\"conclusion\":\"success\"}]}" > repos_essai_depot_commits_abc12345deadbeef_check-runs_per_page_100.json'
verifier rouge "E5 plus aucun contexte requis sur la branche" "AUCUN contexte requis" \
  bash -c 'echo "[]" > repos_essai_depot_rules_branches_main.json'

echo
echo "── F. Un droit absent ne doit jamais se lire « rien à signaler » ──"
verifier rouge "F1 règles de branche refusées (403)" "DROIT MANQUANT" \
  bash -c 'echo 403 > repos_essai_depot_rules_branches_main.code'
verifier rouge "F2 workflows introuvables (404)" "404" \
  bash -c 'echo 404 > repos_essai_depot_contents_.github_workflows_ref_main.code'
verifier rouge "F3 réponse d'API inconnue (500)" "réponse d'API inconnue" \
  bash -c 'echo 500 > repos_essai_depot_rulesets_per_page_100.code'

echo
if [ "$rate" -eq 0 ]; then
  echo "══ Contre-épreuve VERTE : chaque détection a été vue rougir sur sa dérive"
  echo "   et rester silencieuse sur l'état conforme."
else
  echo "══ Contre-épreuve ROUGE : au moins une détection ne se déclenche pas."
fi
exit "$rate"
