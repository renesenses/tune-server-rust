#!/usr/bin/env bash
# Détecteur de dérive des garde-fous (#2816, parent #2808).
#
# ─── Ce qu'il N'EST PAS ───────────────────────────────────────────────────────
# Ce script ne répare RIEN. Il ne touche ni ruleset, ni protection, ni variable,
# ni environnement, ni secret, ni tag. Un audit qui répare est un audit dont on
# ne peut plus lire le verdict : le rouge disparaît avec la cause, et personne
# n'apprend jamais que la garde était tombée.
#
# ─── Ce qu'il ajoute à `audit-protections.yml` ────────────────────────────────
# L'audit existant vérifie les RÈGLES DE BRANCHE (contextes exigés, méthode de
# fusion, fils résolus), les variables d'armement, l'absence de tag et le
# manifeste. Il ne regarde ni les environnements, ni les secrets, ni l'état du
# gel des tags, ni la QUALITÉ du passage des portes. Ce script couvre ces
# quatre angles morts — tous constatés en vrai le 31 août 2026 :
#
#   A. Un environnement supprimé emporte ses règles, et GitHub le RECRÉE sans
#      aucune protection dès qu'un workflow le référence. Ce soir, `release`
#      n'existait plus alors que `release-controller.yml` le nomme, et
#      `release-dry-run` avait été recréé avec ZÉRO règle. Une garde peut donc
#      disparaître sans laisser de trace : l'historique d'un environnement
#      supprimé n'est pas récupérable, ce qui rend la détection A *préventive*
#      et non pas médico-légale.
#
#   B. Un secret d'ENVIRONNEMENT masque celui du dépôt pour les jobs qui visent
#      cet environnement. Trois copies de `RELEASE_CONTROLLER_TOKEN` vivaient
#      dans trois environnements ; la promotion a échoué APRÈS l'approbation
#      humaine parce que l'environnement visé n'en avait pas. Le coût d'une
#      résolution asymétrique se paie donc au pire moment.
#
#   C. Le gel des tags s'ouvre et se referme À LA MAIN. Son historique compte
#      13 modifications par paires : rien, dans l'état seul, ne dit qu'il a été
#      refermé après le dernier passage.
#
#   D. Une porte peut être `cancelled`, `skipped` ou `neutral` et la PR rester
#      `CLEAN`. GitHub compte ces conclusions comme non bloquantes ; un nom de
#      porte requis peut donc n'avoir JAMAIS réussi sur la tête auditée.
#
# ─── La règle qui prime sur la lisibilité ─────────────────────────────────────
# Une absence d'accès n'est JAMAIS un succès. Deux fois le 31 août, un droit
# absent s'est présenté comme une absence de données : un 403 muet, et un 404
# sur un brouillon. Toute lecture passe donc par `api()`, qui sépare
# explicitement « lu », « refusé » (403 → ROUGE), « inexistant » (404, dont
# l'interprétation dépend du contexte) et « réponse inconnue » (→ ROUGE).
#
# ─── Contre-épreuve ───────────────────────────────────────────────────────────
# `AUDIT_FIXTURES=<répertoire>` détourne `api()` vers des fichiers au lieu du
# réseau, ce qui permet de simuler une dérive et d'exiger que le script ROUGISSE.
# Voir `scripts/test-auditer-derive-gardefous.sh`.
#
# Usage : scripts/auditer-derive-gardefous.sh [dépôt] [réf...]
#   dépôt : owner/name (défaut : $GITHUB_REPOSITORY, sinon renesenses/tune-server-rust)
#   réf   : branches dont les portes requises doivent avoir un passage RÉUSSI
#           (défaut : la branche par défaut du dépôt)

set -uo pipefail

DEPOT="${1:-${GITHUB_REPOSITORY:-renesenses/tune-server-rust}}"
shift 2>/dev/null || true
REFS_A_VERIFIER=("$@")

echec=0
dire() { printf '  %s\n' "$1"; }
ok()   { printf '  ✅ %s\n' "$1"; }
ko()   { printf '  ❌ %s\n' "$1"; echec=1; }
na()   { printf '  ⚠️  %s\n' "$1"; }

# ─────────────────────────────────────────────────────────────────────────────
# api <chemin>
#
# Rend le corps JSON sur la sortie standard et un code de retour PARLANT :
#   0   lu
#   3   403 — droit manquant. L'appelant DOIT rougir : c'est le cas exact où un
#       audit naïf conclurait « rien à signaler ».
#   4   404 — inexistant OU hors de portée du jeton. L'appelant décide, mais ne
#       peut pas confondre avec « lu, et vide ».
#   5   toute autre réponse (5xx, quota, JSON illisible) → l'appelant rougit.
#
# `AUDIT_FIXTURES` : le chemin est aplati (`/`→`_`, `?`→`_`) et cherché comme
# `<dir>/<aplati>.json`. Un fichier `<aplati>.code` contenant `403`, `404`… force
# le code de retour correspondant. Un fixture ABSENT vaut 404 : la contre-épreuve
# ne peut donc pas rendre vert par simple oubli de fichier.
# ─────────────────────────────────────────────────────────────────────────────
api() {
  local chemin="$1" plat corps statut
  if [ -n "${AUDIT_FIXTURES:-}" ]; then
    plat=$(printf '%s' "$chemin" | tr '/?=&' '____')
    if [ -f "$AUDIT_FIXTURES/$plat.code" ]; then
      statut=$(tr -d '[:space:]' < "$AUDIT_FIXTURES/$plat.code")
      case "$statut" in 403) return 3 ;; 404) return 4 ;; 200) : ;; *) return 5 ;; esac
    fi
    [ -f "$AUDIT_FIXTURES/$plat.json" ] || return 4
    cat "$AUDIT_FIXTURES/$plat.json"
    return 0
  fi
  # `--include` puis découpage : `gh api` rend le même code de sortie 1 pour un
  # 403 et pour un 404, et c'est précisément la confusion qui a coûté deux
  # heures. Le statut HTTP est la seule source qui les sépare.
  corps=$(gh api --include "$chemin" 2>/dev/null) || {
    statut=$(printf '%s' "$corps" | sed -n '1s#.*[[:space:]]\([0-9][0-9][0-9]\)[[:space:]].*#\1#p')
    case "$statut" in 403) return 3 ;; 404) return 4 ;; *) return 5 ;; esac
  }
  statut=$(printf '%s' "$corps" | sed -n '1s#.*[[:space:]]\([0-9][0-9][0-9]\)[[:space:]].*#\1#p')
  case "$statut" in
    200|201) printf '%s' "$corps" | sed -n '/^[[:space:]]*[[{]/,$p' ;;
    403) return 3 ;;
    404) return 4 ;;
    *)   return 5 ;;
  esac
}

# Rougit avec un message adapté au code rendu par `api`. Le libellé nomme le
# DROIT manquant plutôt que « erreur », pour que le lecteur sache quoi corriger.
expliquer_echec() {
  local code="$1" quoi="$2"
  case "$code" in
    3) ko "$quoi : 403 — DROIT MANQUANT. L'audit ne peut pas conclure ; ce n'est pas « rien à signaler »." ;;
    4) ko "$quoi : 404 — inexistant ou hors de portée du jeton. L'audit ne peut pas conclure." ;;
    *) ko "$quoi : réponse d'API inconnue. L'audit ne peut pas conclure." ;;
  esac
}

echo "══ Détecteur de dérive des garde-fous — $DEPOT ══"
echo "   horodatage : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo

# La branche par défaut sert de référence : c'est la seule branche depuis
# laquelle GitHub exécute les workflows planifiés et celle qui décide de ce qui
# sera réellement lancé.
if defaut=$(api "repos/$DEPOT" 2>/dev/null); then
  BRANCHE_DEFAUT=$(printf '%s' "$defaut" | jq -r '.default_branch')
else
  expliquer_echec "$?" "$DEPOT : lecture du dépôt"
  BRANCHE_DEFAUT=main
fi
[ "${#REFS_A_VERIFIER[@]}" -eq 0 ] && REFS_A_VERIFIER=("$BRANCHE_DEFAUT")

# ═════════════════════════════════════════════════════════════════════════════
# A. Environnements : référencés par les workflows vs réellement protégés
#
# Le danger n'est pas qu'un environnement manque — c'est que GitHub le RECRÉE
# tout seul, vide de toute règle, au premier job qui le nomme. Un environnement
# référencé mais absent est donc une garde DÉJÀ tombée, pas un risque futur.
# ═════════════════════════════════════════════════════════════════════════════
echo "=== A. Environnements référencés par les workflows de $BRANCHE_DEFAUT ==="

# Les workflows sont lus sur la BRANCHE PAR DÉFAUT et non dans la copie locale :
# c'est ce que GitHub exécutera, indépendamment de la branche auditée.
# `base64 -d` sous GNU, `-D` sous BSD/macOS : le script doit tourner aussi bien
# sur le runner Ubuntu que sur le poste qui joue la contre-épreuve.
decoder64() { base64 -d 2>/dev/null || base64 -D 2>/dev/null; }

noms_references=""
if listing=$(api "repos/$DEPOT/contents/.github/workflows?ref=$BRANCHE_DEFAUT"); then
  while IFS= read -r fichier; do
    [ -z "$fichier" ] && continue
    contenu=$(api "repos/$DEPOT/contents/.github/workflows/$fichier?ref=$BRANCHE_DEFAUT") || continue
    brut=$(printf '%s' "$contenu" | jq -r '.content // ""' | tr -d '\n' | decoder64) || continue
    # Deux formes coexistent : `environment: nom` et le ternaire
    # `environment: ${{ … && 'a' || 'b' }}`. Les DEUX branches du ternaire sont
    # atteignables ; n'en extraire qu'une laisserait la moitié des
    # environnements hors de l'audit.
    noms=$(printf '%s\n' "$brut" \
      | grep -E "^[[:space:]]*environment:" \
      | sed -E "s/^[[:space:]]*environment:[[:space:]]*//" \
      | grep -oE "'[^']+'|\"[^\"]+\"|^[A-Za-z0-9_.-]+" \
      | tr -d "'\"")
    for n in $noms; do
      case "$n" in ""|\$*) continue ;; esac
      noms_references="$noms_references$n
"
    done
  done < <(printf '%s' "$listing" | jq -r '.[] | select(.type=="file") | .name')
else
  expliquer_echec "$?" "workflows de $BRANCHE_DEFAUT"
fi

noms_references=$(printf '%s' "$noms_references" | sort -u | grep -v '^$' || true)
liste_existants=""

if existants=$(api "repos/$DEPOT/environments"); then
  liste_existants=$(printf '%s' "$existants" | jq -r '.environments[]?.name' | sort -u)
  if [ -z "$noms_references" ]; then
    na "aucune clef « environment: » trouvée dans les workflows — rien à confronter"
  fi
  for env in $noms_references; do
    if ! printf '%s\n' "$liste_existants" | grep -Fxq "$env"; then
      ko "environnement « $env » RÉFÉRENCÉ mais INEXISTANT — GitHub le recréera SANS AUCUNE protection au premier job qui le nomme ; si des approbateurs y étaient définis, ils sont perdus sans trace"
      continue
    fi
    regles=$(printf '%s' "$existants" \
      | jq -r --arg e "$env" '.environments[] | select(.name==$e) | [.protection_rules[]?.type] | join(",")')
    maj=$(printf '%s' "$existants" | jq -r --arg e "$env" '.environments[] | select(.name==$e) | .updated_at')
    if [ -z "$regles" ]; then
      ko "environnement « $env » existe avec ZÉRO règle de protection (modifié $maj) — il ne bloque rien ; c'est la signature d'une recréation automatique après suppression"
    else
      ok "environnement « $env » : $regles (modifié $maj)"
    fi
  done
  # Un environnement que plus aucun workflow ne nomme n'est pas une faute, mais
  # il porte peut-être encore des secrets : il est signalé, pas compté rouge.
  for env in $liste_existants; do
    printf '%s\n' "$noms_references" | grep -Fxq "$env" \
      || na "environnement « $env » existe mais AUCUN workflow de $BRANCHE_DEFAUT ne le référence — vérifier qu'il ne porte pas de secret orphelin"
  done
else
  expliquer_echec "$?" "$DEPOT : liste des environnements"
fi
echo

# ═════════════════════════════════════════════════════════════════════════════
# B. Secrets d'environnement qui masquent (ou ne remplacent pas) ceux du dépôt
#
# Règle de résolution GitHub : pour un job visant l'environnement E, un secret
# défini dans E l'emporte sur celui du dépôt. Un nom présent dans CERTAINS
# environnements seulement et absent du dépôt ne se résout donc que pour une
# partie des jobs — et l'échec tombe après l'approbation humaine.
#
# L'invariant exigé : tout nom de secret défini dans au moins un environnement
# doit se résoudre pour TOUS les environnements référencés — soit parce qu'il
# existe au niveau du dépôt (repli universel), soit parce qu'il est défini dans
# chacun d'eux.
# ═════════════════════════════════════════════════════════════════════════════
echo "=== B. Résolution des secrets entre dépôt et environnements ==="

secrets_depot=""
if s=$(api "repos/$DEPOT/actions/secrets?per_page=100"); then
  secrets_depot=$(printf '%s' "$s" | jq -r '.secrets[]?.name' | sort -u)
  dire "$(printf '%s' "$s" | jq -r '.secrets | length') secret(s) au niveau du dépôt"
else
  # Le GITHUB_TOKEN d'Actions ne peut PAS lister les secrets : aucune permission
  # de workflow ne l'autorise. C'est un 403 attendu, et il doit rougir quand
  # même — sinon cette section entière deviendrait un vert creux permanent.
  expliquer_echec "$?" "$DEPOT : liste des secrets de dépôt (un jeton portant le droit secrets:read est requis ; github.token ne l'a jamais)"
fi

env_avec_secret=""
tous_noms_env=""
for env in $noms_references; do
  printf '%s\n' "$liste_existants" | grep -Fxq "$env" || continue
  if es=$(api "repos/$DEPOT/environments/$env/secrets?per_page=100"); then
    noms=$(printf '%s' "$es" | jq -r '.secrets[]?.name' | sort -u)
    env_avec_secret="$env_avec_secret$env
"
    for n in $noms; do tous_noms_env="$tous_noms_env$n
"; done
    [ -z "$noms" ] && dire "« $env » : aucun secret propre — les secrets du dépôt s'appliquent" \
                   || dire "« $env » : $(printf '%s\n' "$noms" | tr '\n' ' ')"
    eval "secrets_de_${env//[^A-Za-z0-9]/_}=\$noms"
  else
    expliquer_echec "$?" "« $env » : liste des secrets d'environnement"
  fi
done

for nom in $(printf '%s' "$tous_noms_env" | sort -u | grep -v '^$' || true); do
  if printf '%s\n' "$secrets_depot" | grep -Fxq "$nom"; then
    na "« $nom » est défini au dépôt ET dans un environnement : la copie d'environnement MASQUE celle du dépôt pour les jobs qui le visent — deux valeurs à faire tourner, une seule visible"
  fi
  manquants=""
  for env in $env_avec_secret; do
    var="secrets_de_${env//[^A-Za-z0-9]/_}"
    printf '%s\n' "${!var}" | grep -Fxq "$nom" || manquants="$manquants $env"
  done
  if [ -n "$manquants" ] && ! printf '%s\n' "$secrets_depot" | grep -Fxq "$nom"; then
    ko "« $nom » est défini dans certains environnements mais NI au niveau du dépôt NI dans :$manquants — un job visant l'un d'eux échouera APRÈS l'approbation humaine"
  fi
done
echo

# ═════════════════════════════════════════════════════════════════════════════
# C. Gel des tags : est-il REFERMÉ maintenant ?
#
# Le gel s'ouvre pour taguer et se referme après. Son état courant est la seule
# donnée qui compte ; son historique dit seulement à quelle fréquence on y
# touche. Un gel `disabled` ou `evaluate` ne bloque rien : `evaluate` journalise
# sans refuser, ce qui ressemble à une garde active dans l'interface.
# ═════════════════════════════════════════════════════════════════════════════
echo "=== C. Gel des tags v* ==="
if rs=$(api "repos/$DEPOT/rulesets?per_page=100"); then
  gels=$(printf '%s' "$rs" | jq -c '[.[] | select(.target=="tag")] | .[]')
  if [ -z "$gels" ]; then
    ko "AUCUN ruleset de tag — les tags v* ne sont protégés par rien"
  else
    trouve_v=0
    while IFS= read -r g; do
      [ -z "$g" ] && continue
      id=$(printf '%s' "$g" | jq -r '.id')
      detail=$(api "repos/$DEPOT/rulesets/$id") || { expliquer_echec "$?" "ruleset $id"; continue; }
      # Nom, portée ET enforcement viennent tous de la lecture DÉTAILLÉE : la
      # liste ne porte pas les conditions, et mélanger les deux sources laisse
      # croire qu'on a vérifié la portée d'un gel dont on a lu l'état ailleurs.
      nom=$(printf '%s' "$detail" | jq -r '.name')
      enf=$(printf '%s' "$detail" | jq -r '.enforcement')
      inclus=$(printf '%s' "$detail" | jq -r '.conditions.ref_name.include[]?' | tr '\n' ' ')
      case "$inclus" in *'refs/tags/v'*) trouve_v=1 ;; *) continue ;; esac
      maj=$(printf '%s' "$detail" | jq -r '.updated_at')
      if [ "$enf" = "active" ]; then
        ok "« $nom » : gel ACTIF sur $inclus (dernière modification $maj)"
      else
        ko "« $nom » : enforcement = $enf — le gel des tags est OUVERT ; evaluate journalise sans refuser et disabled ne fait rien (dernière modification $maj)"
      fi
      if h=$(api "repos/$DEPOT/rulesets/$id/history?per_page=100"); then
        n=$(printf '%s' "$h" | jq 'length')
        dire "« $nom » : $n version(s) dans l'historique — le gel est manœuvré à la main, chaque ouverture doit être suivie d'une fermeture"
      else
        # L'historique est un confort, pas une garde : son absence ne rougit pas,
        # mais elle est dite pour que personne ne croie l'avoir consulté.
        na "« $nom » : historique du ruleset NON LU — l'audit ne peut pas dire combien de fois il a été manœuvré"
      fi
    done <<< "$gels"
    [ "$trouve_v" -eq 0 ] && ko "des rulesets de tag existent mais AUCUN ne couvre refs/tags/v*"
  fi
else
  expliquer_echec "$?" "$DEPOT : liste des rulesets"
fi
echo

# ═════════════════════════════════════════════════════════════════════════════
# D. Variables d'armement laissées ouvertes
#
# Rien ne remet `RELEASE_PROMOTION_ENABLED` à `false` après une promotion : la
# remise à zéro est manuelle, donc oubliable. Une variable armée en dehors d'une
# fenêtre de publication est une porte laissée ouverte.
#
# Deux sources, dans cet ordre : les variables injectées par le workflow appelant
# (contexte `vars`, seule voie ouverte à `github.token`), sinon l'API — qui exige
# un jeton plus large et rougit explicitement si elle est refusée.
# ═════════════════════════════════════════════════════════════════════════════
echo "=== D. Variables d'armement ==="
verifier_armement() {
  local nom="$1" val="$2" source="$3"
  case "$val" in
    ""|false|FALSE|False) ok "$nom = ${val:-absente} — désarmé ($source)" ;;
    *) ko "$nom = $val — ARMÉ ($source) ; rien ne le remet à false après une promotion, il doit être refermé à la main" ;;
  esac
}
# L'API d'abord : elle ÉNUMÈRE toutes les variables `*_ENABLED`, y compris
# celles qui n'existaient pas quand ce script a été écrit. La liste injectée est
# un repli — elle ne voit que ce qu'on a pensé à y mettre, et une porte
# d'armement ajoutée demain lui échapperait.
v=$(api "repos/$DEPOT/actions/variables?per_page=100"); code_vars=$?
if [ "$code_vars" -eq 0 ]; then
  n=0
  while IFS=$'\t' read -r nom val; do
    [ -z "$nom" ] && continue
    n=$((n + 1))
    verifier_armement "$nom" "$val" "API"
  done < <(printf '%s' "$v" | jq -r '.variables[]? | select(.name | test("_ENABLED$")) | [.name,.value] | @tsv')
  [ "$n" -eq 0 ] && na "aucune variable *_ENABLED au dépôt — rien à désarmer, ou la convention de nommage a changé"
elif [ -n "${AUDIT_VARS_INJECTEES:-}" ]; then
  na "variables non énumérables par l'API : repli sur la liste injectée, qui ne couvre QUE les noms déjà connus. Une porte d'armement créée depuis ne serait pas vue."
  for couple in $AUDIT_VARS_INJECTEES; do
    case "$couple" in *=*) verifier_armement "${couple%%=*}" "${couple#*=}" "contexte vars" ;; esac
  done
else
  expliquer_echec "$code_vars" "$DEPOT : variables d'armement (fournir AUDIT_VARS_INJECTEES depuis le contexte vars si le jeton n'a pas le droit)"
fi
echo

# ═════════════════════════════════════════════════════════════════════════════
# E. Chaque porte REQUISE a-t-elle au moins un passage RÉUSSI ?
#
# GitHub considère `skipped`, `cancelled` et `neutral` comme non bloquants : une
# porte requise peut porter ces conclusions et la PR rester CLEAN. Le seul
# témoin d'une porte qui a vraiment tourné est une conclusion `success` sur la
# tête auditée. Sur la tête de `main` du 31 août, `release-gate` était `skipped`
# et deux autres portes requises n'avaient produit aucun run.
# ═════════════════════════════════════════════════════════════════════════════
echo "=== E. Passage effectif des portes requises ==="
for ref in "${REFS_A_VERIFIER[@]}"; do
  enc=$(printf '%s' "$ref" | sed 's#/#%2F#g')
  echo "· $ref"
  # `regles=$(api …)` puis test séparé, jamais `if ! regles=$(api …)` : après
  # une condition NIÉE, `$?` vaut le résultat de la négation et non le code
  # rendu par `api`. Le 403 se serait affiché comme une erreur générique — soit
  # exactement la confusion « droit manquant / pas de données » que cet audit
  # existe pour interdire.
  regles=$(api "repos/$DEPOT/rules/branches/$enc"); code=$?
  if [ "$code" -ne 0 ]; then
    expliquer_echec "$code" "$ref : règles effectives"
    continue
  fi
  exiges=$(printf '%s' "$regles" \
    | jq -r '[.[] | select(.type=="required_status_checks")
             | .parameters.required_status_checks[]?.context] | .[]' 2>/dev/null)
  if [ -z "$exiges" ]; then
    ko "$ref : AUCUN contexte requis — il n'y a aucune porte à franchir"
    continue
  fi
  b=$(api "repos/$DEPOT/branches/$enc"); code=$?
  if [ "$code" -ne 0 ]; then
    expliquer_echec "$code" "$ref : tête de branche"
    continue
  fi
  tete=$(printf '%s' "$b" | jq -r '.commit.sha')
  cr=$(api "repos/$DEPOT/commits/$tete/check-runs?per_page=100"); code=$?
  if [ "$code" -ne 0 ]; then
    expliquer_echec "$code" "$ref : exécutions de contrôle sur $tete"
    continue
  fi
  dire "tête = ${tete:0:8}"
  while IFS= read -r porte; do
    [ -z "$porte" ] && continue
    concl=$(printf '%s' "$cr" | jq -r --arg n "$porte" \
      '[.check_runs[] | select(.name==$n) | (.conclusion // .status)] | join(",")')
    if [ -z "$concl" ]; then
      ko "$ref : porte requise « $porte » — AUCUNE exécution sur ${tete:0:8} ; la branche ne prouve rien"
    elif printf '%s' "$concl" | tr ',' '\n' | grep -Fxq success; then
      ok "$ref : « $porte » a au moins un passage réussi ($concl)"
    else
      ko "$ref : porte requise « $porte » n'a JAMAIS réussi sur ${tete:0:8} — conclusions : $concl ; GitHub ne bloque pas sur skipped/cancelled/neutral"
    fi
  done <<< "$exiges"
done
echo

echo "══════════════════════════════════════════════════════════════════════════"
if [ "$echec" -eq 0 ]; then
  echo "VERT — pour ce que l'audit a PU lire. Les ⚠️ ci-dessus ne sont pas des"
  echo "succès : ce sont des points qu'aucune API accessible ne permet de trancher."
else
  echo "ROUGE — voir les ❌ ci-dessus. Aucune correction n'a été appliquée :"
  echo "ce script détecte, il ne répare pas."
fi
exit "$echec"
