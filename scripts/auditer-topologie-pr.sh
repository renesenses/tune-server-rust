#!/usr/bin/env bash
# Garde de topologie des PR et des lots (#2813, parent #2808).
#
# ─── Ce qu'elle répare ────────────────────────────────────────────────────────
# Le 11/09/2026 au matin, un audit a trouvé 73 commits en avance sur `main`
# répartis dans NEUF branches `batch/*` sans AUCUNE PR vers `main`. Après
# vérification par le CONTENU — jamais par le sha — il en restait cinq vrais
# correctifs perdus, tous verts, tous fusionnés dans leur lot, tous absents de
# la production, certains depuis plusieurs semaines. Rien ne surveillait qu'un
# lot fusionné remonte. L'audit l'écrit lui-même : « une vérification
# périodique — toute branche `batch/*` en avance sur `main` sans PR ouverte —
# aurait détecté les cinq en une minute. »
#
# Deuxième famille, le même jour : une PR ouverte vers une base MORTE. #3535
# visait `rc/v0.9.141`, ligne de release abandonnée depuis quatre versions ;
# #3758 visait `batch/bugs-9`, déjà fusionnée. Une PR sur une base morte est un
# piège pour quiconque la lit : elle a l'air vivante, elle est verte, et elle ne
# mènera jamais nulle part.
#
# ─── Ce qu'elle N'EST PAS ─────────────────────────────────────────────────────
# Ce script ne répare RIEN et ne fusionne RIEN. Il n'ouvre pas de PR, ne pousse
# aucune branche, ne ferme aucune issue. Il rend un verdict, nommé, avec le
# nombre de commits en jeu. Remonter un lot engage la ligne de release ; cette
# décision reste humaine.
#
# ─── Le piège qu'elle refuse de reproduire ────────────────────────────────────
# 🔴 `git merge-base --is-ancestor` SEUL donne des faux négatifs. Quatre mesurés
#    le 09/09/2026, et un agent a failli reconstruire une branche entière le
#    11/09 pour rien : `--is-ancestor` répondait « non » alors que les DEUX
#    ARBRES ÉTAIENT IDENTIQUES. Un lot repris par `git commit-tree`, un rebase,
#    une reprise par l'API Git Data suffisent à casser la parenté sans rien
#    changer au contenu.
#
#    Ce script ne juge donc JAMAIS par la parenté. Il juge par le CONTENU :
#      1. arbres identiques             → rien à remonter, verdict immédiat ;
#      2. `git merge-tree --write-tree`  → que changerait la fusion ? Si elle ne
#         change pas l'arbre de `main`, il n'y a rien à remonter. C'est le seul
#         test qui survit à une fusion par ÉCRASEMENT (squash), où plus aucun
#         patch-id ne correspond ;
#      3. repli `git cherry` (patch-id) si `merge-tree --write-tree` manque
#         (git < 2.38) ou si la fusion n'est pas représentable.
#    La garde `la_garde_de_topologie_ne_juge_jamais_par_is_ancestor`
#    (tune-server/tests/workflows_bornes.rs) refuse que `--is-ancestor`
#    réapparaisse ici.
#
# ─── Mode AUDIT, pas mode barrage ─────────────────────────────────────────────
# 🔴 Un garde CI nouveau doit démarrer ÉTROIT. Large d'emblée, il rougit sur du
#    légitime et tout le monde apprend à l'ignorer. Ce script SIGNALE ; c'est le
#    workflow qui décide si le signal bloque, et il ne bloque pas aujourd'hui.
#    Voir `.github/workflows/topologie-pr.yml` et la variable de dépôt
#    `TOPOLOGIE_PR_BLOQUANTE`.
#
# ─── Ce qu'elle laisse passer DÉLIBÉRÉMENT ────────────────────────────────────
#   · les branches `fix/*`, `feat/*`, `docs/*` en avance sur `main` sans PR :
#     il y en a des centaines, abandonnées pour la plupart. Seules les branches
#     `batch/*` — les LOTS, c'est-à-dire du travail déjà revu et fusionné — sont
#     surveillées par la détection A ;
#   · les `rc/*` et `release/*` en avance sur `main` : le train de release a ses
#     propres portes (`promote-release.yml`, `audit-protections.yml`) ;
#   · une PR vers un lot fusionné ce jour même : `batch/bugs-10` a été absorbée
#     dans `main` le 11/09 à 16h01 et portait encore trois PR en vol à 21h30.
#     Ce n'est pas une base morte, c'est un lot qui repart. D'où le délai de
#     dormance (`TOPO_JOURS_DORMANTE`, 7 jours par défaut) ;
#   · les PR en brouillon : elles comptent comme des PR ouvertes pour la
#     détection A. Un brouillon suffit à prouver que quelqu'un regarde.
#
# ─── Contre-épreuve ───────────────────────────────────────────────────────────
# `--autotest` rejoue chaque détection DEUX fois — sur un état conforme, où elle
# doit se taire, et sur la dérive correspondante, où elle doit rougir avec un
# motif nommé. Les scénarios sont de VRAIS dépôts git temporaires, pas des
# simulacres : c'est la seule façon de prouver le point 3 ci-dessus, qui exige
# une histoire réellement divergente à contenu identique.
#
# Usage :
#   scripts/auditer-topologie-pr.sh [dépôt]          — audit complet (planifié)
#   scripts/auditer-topologie-pr.sh --pr <n> [dépôt] — la seule PR <n> (sur PR)
#   scripts/auditer-topologie-pr.sh --autotest       — contre-épreuve, hors ligne
set -uo pipefail

SCRIPT_MOI="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

MODE=audit
PR_CIBLE=""
ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --autotest) MODE=autotest ;;
    --pr)       PR_CIBLE="${2:-}"; shift ;;
    --pr=*)     PR_CIBLE="${1#--pr=}" ;;
    *)          ARGS+=("$1") ;;
  esac
  shift
done
DEPOT="${ARGS[0]:-${GITHUB_REPOSITORY:-renesenses/tune-server-rust}}"
JOURS_DORMANTE="${TOPO_JOURS_DORMANTE:-7}"
BRANCHE_DEFAUT=main
PR_JSON='[]'

echec=0
# ⚠️ `echec` est un drapeau, pas un compteur : il vaut 1 dès le premier
#    signalement et ne bouge plus. Une section qui se demanderait « ai-je
#    rougi ? » en le comparant à sa valeur d'entrée se déclarerait CONFORME dès
#    qu'une section précédente a rougi — un faux vert imprimé juste sous ses
#    propres croix. Mesuré le 11/09 : la section B annonçait « aucune PR ouverte
#    ne vise une base morte » sous trois lignes ❌. D'où ce compteur.
nb_ko=0
ok() { printf '  ✅ %s\n' "$1"; }
ko() { printf '  ❌ %s\n' "$1"; echec=1; nb_ko=$((nb_ko + 1)); }
na() { printf '  ⚠️  %s\n' "$1"; }

# ─────────────────────────────────────────────────────────────────────────────
# api <chemin>
#
# Même convention que `scripts/auditer-derive-gardefous.sh` : un code de retour
# PARLANT, parce qu'une absence d'accès n'est jamais un succès.
#   0  lu · 3  403 droit manquant · 4  404 · 5  réponse inconnue
#
# `TOPO_FIXTURES` détourne la lecture vers `<dir>/<chemin aplati>.json`
# (`/?=&` → `_`). Un fixture ABSENT vaut 404 : la contre-épreuve ne peut pas
# devenir verte par simple oubli de fichier.
# ─────────────────────────────────────────────────────────────────────────────
api() {
  local chemin="$1" plat corps statut
  if [ -n "${TOPO_FIXTURES:-}" ]; then
    plat=$(printf '%s' "$chemin" | tr '/?=&' '____')
    if [ -f "$TOPO_FIXTURES/$plat.code" ]; then
      statut=$(tr -d '[:space:]' < "$TOPO_FIXTURES/$plat.code")
      case "$statut" in 403) return 3 ;; 404) return 4 ;; 200) : ;; *) return 5 ;; esac
    fi
    [ -f "$TOPO_FIXTURES/$plat.json" ] || return 4
    cat "$TOPO_FIXTURES/$plat.json"
    return 0
  fi
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
expliquer_echec() {
  case "$1" in
    3) ko "$2 : 403 — DROIT MANQUANT. La garde ne peut pas conclure ; ce n'est PAS « rien à signaler »." ;;
    4) ko "$2 : 404 — inexistant ou hors de portée du jeton. La garde ne peut pas conclure." ;;
    *) ko "$2 : réponse d'API inconnue. La garde ne peut pas conclure." ;;
  esac
}

# ─────────────────────────────────────────────────────────────────────────────
# ref <branche> → nom d'une ref locale qui la porte, ou code 1.
#
# Sur un runner, `actions/checkout` ne laisse que la tête de la PR ; les autres
# branches doivent être récupérées. `TOPO_SANS_RESEAU=1` interdit toute
# récupération (contre-épreuve).
# ─────────────────────────────────────────────────────────────────────────────
ref() {
  local b="$1" cand
  for cand in "refs/remotes/origin/$b" "refs/heads/$b"; do
    if git rev-parse -q --verify "$cand^{commit}" >/dev/null 2>&1; then
      printf '%s' "$cand"; return 0
    fi
  done
  if [ -z "${TOPO_SANS_RESEAU:-}" ]; then
    git fetch --no-tags --quiet origin "+refs/heads/$b:refs/remotes/origin/$b" 2>/dev/null
    if git rev-parse -q --verify "refs/remotes/origin/$b^{commit}" >/dev/null 2>&1; then
      printf '%s' "refs/remotes/origin/$b"; return 0
    fi
  fi
  return 1
}

# ─────────────────────────────────────────────────────────────────────────────
# apporte_du_contenu <branche>
#   0 = elle apporte quelque chose que `main` n'a pas
#   1 = elle n'apporte RIEN (contenu déjà absorbé, quelle que soit l'histoire)
#   2 = indécidable (branche absente du dépôt local)
#
# 🔴 Aucune des trois voies n'est `--is-ancestor`. Voir l'en-tête.
# ─────────────────────────────────────────────────────────────────────────────
apporte_du_contenu() {
  local r rmain t_branche t_main brut etat t_fusion
  r=$(ref "$1") || return 2
  rmain=$(ref "$BRANCHE_DEFAUT") || return 2
  t_branche=$(git rev-parse -q --verify "$r^{tree}") || return 2
  t_main=$(git rev-parse -q --verify "$rmain^{tree}") || return 2
  # 1. Arbres identiques : rien à remonter, même si l'histoire diverge. C'est
  #    EXACTEMENT le cas où `--is-ancestor` répond « non » à tort.
  [ "$t_branche" = "$t_main" ] && return 1
  # 2. Verdict décisif : que changerait la fusion ?
  #
  #    ⚠️ Uniquement si la fusion est PROPRE. En conflit, `merge-tree` rend
  #    quand même un arbre — celui des marqueurs de conflit — et il ne
  #    ressemble évidemment pas à celui de `main`. Le prendre pour un verdict
  #    est un faux positif MESURÉ : `batch/vague-14` sortait ainsi « en avance »
  #    avec... ZÉRO commit en avance, le 11/09/2026. Un conflit dit que les deux
  #    côtés ont bougé, pas que la branche apporte quelque chose.
  brut=$(git merge-tree --write-tree "$rmain" "$r" 2>/dev/null); etat=$?
  if [ "$etat" -eq 0 ]; then
    t_fusion=$(printf '%s\n' "$brut" | head -1)
    if [ -n "$t_fusion" ] && git rev-parse -q --verify "$t_fusion^{tree}" >/dev/null 2>&1; then
      [ "$t_fusion" = "$t_main" ] && return 1
      return 0
    fi
  fi
  # 3. Repli patch-id : fusion en conflit, ou `merge-tree --write-tree` absent
  #    (git < 2.38). `git cherry` compare les PATCHS, pas les sha.
  [ "$(git cherry "$rmain" "$r" 2>/dev/null | grep -c '^+')" -gt 0 ] && return 0
  return 1
}

# Nombre de commits dont le patch n'est pas dans `main` — pour le MESSAGE, pas
# pour le verdict : un lot fusionné par écrasement en compte beaucoup alors
# qu'il n'apporte rien.
commits_en_avance() {
  local r rmain
  r=$(ref "$1")              || { printf '?'; return; }
  rmain=$(ref "$BRANCHE_DEFAUT") || { printf '?'; return; }
  git cherry "$rmain" "$r" 2>/dev/null | grep -c '^+'
}

jours_depuis() {
  local r quand
  r=$(ref "$1") || { printf '999999'; return; }
  quand=$(git log -1 --format=%ct "$r" 2>/dev/null) || { printf '999999'; return; }
  [ -n "$quand" ] || { printf '999999'; return; }
  printf '%s' $(( ( $(date +%s) - quand ) / 86400 ))
}

# La RC vivante = la plus haute AU SENS DES VERSIONS. `sort -V` range
# `rc/v0.9.9` avant `rc/v0.9.141` ; un tri lexical ferait l'inverse et
# désignerait une RC morte comme vivante.
rc_vivante() {
  git for-each-ref --format='%(refname:strip=3)' 'refs/remotes/origin/rc/*' 2>/dev/null \
    | sort -V | tail -1
}

charger_etat() {
  local corps code nb
  if corps=$(api "repos/$DEPOT"); then
    BRANCHE_DEFAUT=$(printf '%s' "$corps" | jq -r '.default_branch // "main"')
  else
    code=$?; expliquer_echec "$code" "$DEPOT : lecture du dépôt"
  fi
  if corps=$(api "repos/$DEPOT/pulls?state=open&per_page=100"); then
    PR_JSON="$corps"
    nb=$(printf '%s' "$PR_JSON" | jq 'length')
    # ⚠️ Une page pleine est une BORNE atteinte, pas un inventaire. Le dépôt a
    #    déjà payé cette confusion : une ronde de tri ne hachait que les 40 fils
    #    les plus récents et laissait 47 messages sur 112 invisibles. Une garde
    #    qui ne voit que les 100 premières PR rendrait un vert qui ne couvre pas
    #    ce qu'on croit.
    if [ "$nb" -ge 100 ]; then
      ko "inventaire des PR tronqué à $nb : la page est PLEINE, la garde ne voit pas tout. Paginer avant de croire ce verdict."
    fi
  else
    code=$?; expliquer_echec "$code" "$DEPOT : liste des PR ouvertes"
  fi
}

# ═════════════════════════════════════════════════════════════════════════════
# A. Lot orphelin — branche `batch/*` qui apporte du contenu à `main` sans PR
#    ouverte pour l'y porter. Le cas des 73 commits.
# ═════════════════════════════════════════════════════════════════════════════
detection_lots_orphelins() {
  echo "── A. Lots en avance sur $BRANCHE_DEFAUT sans PR pour les remonter ──"
  local lots lot n bases b vivante remonte trouve=0
  lots=$(git for-each-ref --format='%(refname:strip=3)' 'refs/remotes/origin/batch/*' 2>/dev/null | sort)
  if [ -z "$lots" ]; then
    ko "aucune branche batch/* visible en local : la garde n'a RIEN mesuré. Récupérer les refs avant de conclure (git fetch origin '+refs/heads/batch/*:refs/remotes/origin/batch/*')."
    echo
    return
  fi
  vivante=$(rc_vivante)
  while IFS= read -r lot; do
    [ -n "$lot" ] || continue
    apporte_du_contenu "$lot"
    case $? in
      1) continue ;;                                        # rien à remonter
      2) na "$lot : absente du dépôt local, non mesurée."; continue ;;
    esac
    bases=$(printf '%s' "$PR_JSON" | jq -r --arg lot "$lot" '.[] | select(.head.ref == $lot) | .base.ref')
    remonte=0
    while IFS= read -r b; do
      [ -n "$b" ] || continue
      if [ "$b" = "$BRANCHE_DEFAUT" ] || { [ -n "$vivante" ] && [ "$b" = "$vivante" ]; }; then
        remonte=1
      fi
    done <<< "$bases"
    [ "$remonte" = 1 ] && continue
    n=$(commits_en_avance "$lot")
    if [ -n "$bases" ]; then
      ko "$lot : $n commit(s) absents de $BRANCHE_DEFAUT, et sa seule PR ouverte vise $(printf '%s' "$bases" | tr '\n' ' ')— une base qui ne remonte pas."
    else
      ko "$lot : $n commit(s) absents de $BRANCHE_DEFAUT et AUCUNE PR ouverte pour les y porter. C'est du travail fusionné, vert, et invisible en production."
    fi
    trouve=$((trouve + 1))
  done <<< "$lots"
  [ "$trouve" -eq 0 ] && ok "tout lot qui apporte du contenu a une PR ouverte vers $BRANCHE_DEFAUT."
  echo
}

# ═════════════════════════════════════════════════════════════════════════════
# B. Base morte — une PR ouverte qui ne mène nulle part.
# ═════════════════════════════════════════════════════════════════════════════
juger_une_pr() {
  local num="$1" tete="$2" base="$3" vivante jours n etat
  # B3 — la base a disparu.
  if ! ref "$base" >/dev/null; then
    ko "#$num ($tete → $base) : la base N'EXISTE PLUS. Cette PR ne peut pas être fusionnée."
    return
  fi
  # B4 — topologie hors doctrine : un correctif va vers un lot ou une RC, jamais
  #      droit sur la branche par défaut (critère d'acceptation n°1 de #2813).
  if [ "$base" = "$BRANCHE_DEFAUT" ]; then
    case "$tete" in
      batch/*|rc/*|release/*) : ;;
      *) ko "#$num ($tete → $base) : topologie hors doctrine. Un correctif va vers un lot batch/* ou vers une RC ; seule une intégration (batch/*, rc/*, release/*) vise $BRANCHE_DEFAUT." ;;
    esac
    return
  fi
  # B1 — base RC dépassée. #3535 visait rc/v0.9.141, quatre versions derrière.
  case "$base" in
    rc/*)
      vivante=$(rc_vivante)
      if [ -n "$vivante" ] && [ "$base" != "$vivante" ]; then
        ko "#$num ($tete → $base) : base RC DÉPASSÉE, la ligne active est $vivante. Cette PR ne sera jamais publiée."
      fi
      return ;;
  esac
  # B2 — lot absorbé ET dormant. Un lot fusionné ce jour même repart souvent ;
  #      un lot fusionné il y a plus de TOPO_JOURS_DORMANTE jours, non.
  case "$base" in
    batch/*)
      apporte_du_contenu "$base"; etat=$?
      if [ "$etat" -eq 1 ]; then
        jours=$(jours_depuis "$base")
        if [ "$jours" -gt "$JOURS_DORMANTE" ]; then
          ko "#$num ($tete → $base) : base ABSORBÉE dans $BRANCHE_DEFAUT et sans révision depuis $jours jours. Le lot est fini ; rebaser sur le lot courant."
        fi
      fi
      return ;;
  esac
  # B5 — pile cassée : la PR est empilée sur une branche de travail qui n'a
  #      elle-même aucune PR ouverte et ne bouge plus. Rien ne la portera.
  if ! printf '%s' "$PR_JSON" | jq -e --arg b "$base" 'any(.[]; .head.ref == $b)' >/dev/null 2>&1; then
    jours=$(jours_depuis "$base")
    if [ "$jours" -gt "$JOURS_DORMANTE" ]; then
      n=$(commits_en_avance "$base")
      ko "#$num ($tete → $base) : pile cassée. La base porte $n commit(s) hors de $BRANCHE_DEFAUT, n'a AUCUNE PR ouverte et n'a pas bougé depuis $jours jours."
    fi
  fi
}

detection_bases_mortes() {
  echo "── B. PR ouvertes dont la base ne mène nulle part ──"
  local avant="$nb_ko" lignes num tete base
  lignes=$(printf '%s' "$PR_JSON" | jq -r '.[] | "\(.number)\t\(.head.ref)\t\(.base.ref)"')
  if [ -n "$PR_CIBLE" ]; then
    lignes=$(printf '%s\n' "$lignes" | awk -F'\t' -v n="$PR_CIBLE" '$1 == n')
    if [ -z "$lignes" ]; then
      ko "PR #$PR_CIBLE introuvable parmi les PR ouvertes de $DEPOT : la garde n'a RIEN jugé."
      echo
      return
    fi
  fi
  while IFS=$'\t' read -r num tete base; do
    [ -n "${num:-}" ] || continue
    juger_une_pr "$num" "$tete" "$base"
  done <<< "$lignes"
  [ "$nb_ko" = "$avant" ] && ok "aucune PR ouverte ne vise une base morte."
  echo
}

# ═════════════════════════════════════════════════════════════════════════════
# Contre-épreuve : de VRAIS dépôts git, pas des simulacres.
# ═════════════════════════════════════════════════════════════════════════════
autotest() {
  local D rate=0 sortie
  RACINE=$(mktemp -d "${TMPDIR:-/tmp}/tune-2813-contre-epreuve-XXXXXX") || return 1
  # RACINE reste GLOBALE : un `local` la ferait disparaître avant que le trap
  # de sortie ne s'exécute, et `set -u` transformerait le nettoyage en erreur.
  trap 'rm -rf "${RACINE:-}"' EXIT
  export TOPO_SANS_RESEAU=1
  export TOPO_FIXTURES="$RACINE/fixtures"
  mkdir -p "$TOPO_FIXTURES"
  echo '{"default_branch":"main"}' > "$TOPO_FIXTURES/repos_essai_depot.json"
  local PULLS="$TOPO_FIXTURES/repos_essai_depot_pulls_state_open_per_page_100.json"

  D="$RACINE/depot"
  git init -q -b main "$D" || return 1
  (
    set -e
    cd "$D"
    git config user.email c@e; git config user.name c
    export GIT_AUTHOR_DATE="2020-01-01T00:00:00" GIT_COMMITTER_DATE="2020-01-01T00:00:00"
    echo un > a.txt; git add -A; git commit -qm base

    # Un lot absorbé et DORMANT : fusionné dans main, dernière révision 2020.
    git checkout -q -b batch/absorbe-dormant
    echo deux > b.txt; git add -A; git commit -qm "absorbé, dormant"
    git checkout -q main
    unset GIT_AUTHOR_DATE GIT_COMMITTER_DATE
    git merge -q --no-ff -m "remontée du lot dormant" batch/absorbe-dormant

    # Un lot absorbé mais ACTIF : fusionné dans main aujourd'hui même. C'est le
    # cas mesuré de `batch/bugs-10` le 11/09 — trois PR en vol vers lui alors
    # qu'il venait d'être remonté. Le signaler serait un faux positif.
    git checkout -q -b batch/absorbe-actif
    echo trois > c.txt; git add -A; git commit -qm "absorbé, actif"
    git checkout -q main
    git merge -q --no-ff -m "remontée du lot actif" batch/absorbe-actif

    # Un lot ORPHELIN : du contenu que main n'a pas, et personne pour le porter.
    git checkout -q -b batch/orphelin main
    echo perdu > correctif.txt; git add -A; git commit -qm "correctif perdu"
    echo encore >> correctif.txt; git commit -qam "second correctif perdu"

    # Un lot dont TOUS les patchs sont déjà dans main, mais dont la fusion
    # CONFLITERAIT parce que main a continué sur les mêmes lignes. Faux positif
    # mesuré le 11/09/2026 sur `batch/vague-14` : « en avance » avec zéro commit
    # en avance. `merge-tree` rend l'arbre des marqueurs de conflit ; le prendre
    # pour un verdict désignerait un lot fini comme du travail perdu.
    git checkout -q main
    echo v0 > conflit.txt; git add -A; git commit -qm "conflit : état initial"
    git checkout -q -b batch/conflit-sans-apport main
    echo v1 > conflit.txt; git commit -qam "conflit : la branche pose v1"
    git checkout -q main
    # `-x` est OBLIGATOIRE ici, et ce n'est pas de la cosmétique : sans lui, le
    # commit repris a le même arbre, le même message, le même parent et le même
    # auteur que l'original. Créés dans la MÊME SECONDE, les deux commits ont
    # alors le même sha — main contient littéralement la tête de la branche, la
    # fusion devient triviale et le décor n'est plus un conflit. Mesuré : la
    # contre-épreuve passait sous `bash -x` (plus lent, secondes différentes) et
    # rougissait sans. `-x` ajoute une ligne au message : sha différent, patch-id
    # identique, ce qui est exactement le scénario voulu.
    git cherry-pick -x batch/conflit-sans-apport >/dev/null 2>&1 || exit 1
    echo v2 > conflit.txt; git commit -qam "conflit : main continue en v2"

    # Un lot à ARBRE IDENTIQUE à main, sans parenté : le piège de
    # `--is-ancestor`. Reconstruit par `commit-tree`, exactement comme le fait
    # une reprise par l'API Git Data.
    git branch batch/repris "$(git commit-tree "$(git rev-parse main^{tree})" -m "lot repris hors de l'histoire")"

    # Trois RC, dont deux dépassées.
    git branch rc/v0.9.9 main
    git branch rc/v0.9.141 main
    git branch rc/v0.9.146 main

    # Une branche de travail dormante sans PR, et une qui a la sienne.
    GIT_AUTHOR_DATE="2020-01-01T00:00:00" GIT_COMMITTER_DATE="2020-01-01T00:00:00" \
      git checkout -q -b fix/dormante main
    echo x > d.txt; git add -A
    GIT_AUTHOR_DATE="2020-01-01T00:00:00" GIT_COMMITTER_DATE="2020-01-01T00:00:00" \
      git commit -qm "branche dormante"
    git checkout -q -b fix/portee main
    echo y > e.txt; git add -A; git commit -qm "branche portée par une PR"
    git checkout -q main

    # Les refs sont cherchées sous `refs/remotes/origin/*` : on les y copie,
    # comme après `git fetch origin '+refs/heads/*:refs/remotes/origin/*'`.
    for b in $(git for-each-ref --format='%(refname:strip=2)' refs/heads); do
      git update-ref "refs/remotes/origin/$b" "refs/heads/$b"
    done
  ) || { echo "RATÉ: construction du dépôt de contre-épreuve impossible"; return 1; }

  joue() {
    printf '%s' "$1" > "$PULLS"
    sortie=$( cd "$D" && TOPO_FIXTURES="$TOPO_FIXTURES" TOPO_SANS_RESEAU=1 \
              TOPO_JOURS_DORMANTE="${2:-7}" bash "$SCRIPT_MOI" essai/depot 2>&1 )
  }
  exige()    { if printf '%s' "$sortie" | grep -q -- "$1"; then echo "ok: $2"
               else echo "RATÉ: $2"; printf '%s\n' "$sortie" | sed 's/^/    | /'; rate=1; fi; }
  interdit() { if printf '%s' "$sortie" | grep -q -- "$1"; then echo "RATÉ: $2"
               printf '%s\n' "$sortie" | sed 's/^/    | /'; rate=1; else echo "ok: $2"; fi; }

  # ═══ 0. LE DÉCOR SE VÉRIFIE LUI-MÊME ═══════════════════════════════════════
  # ⚠️ La construction tourne dans un `( … ) || { … }` : bash y DÉSACTIVE `set -e`,
  #    parce que le statut du sous-shell est testé. Une commande de décor peut
  #    donc échouer SANS un mot — c'est arrivé ici avec `git cherry-pick -q`, qui
  #    n'existe pas : le scénario du conflit se construisait à l'envers et la
  #    garantie correspondante rougissait pour la mauvaise raison. Chaque
  #    scénario est donc mesuré AVANT d'être utilisé comme preuve.

  # Le piège `--is-ancestor` existe-t-il vraiment dans ce décor ?
  if ( cd "$D" && git merge-base --is-ancestor refs/remotes/origin/batch/repris refs/remotes/origin/main ); then
    echo "RATÉ: le scénario du piège --is-ancestor n'en est pas un (il répond « oui »)"; rate=1
  else
    echo "ok: le scénario reproduit le piège — --is-ancestor dit « non » sur batch/repris alors que son ARBRE est celui de main"
  fi
  # Le scénario du conflit est-il bien « zéro patch en avance ET fusion en
  # conflit » ? Sans les DEUX, la garantie qui suit ne prouverait rien.
  local n_conflit etat_conflit
  n_conflit=$( cd "$D" && git cherry refs/remotes/origin/main refs/remotes/origin/batch/conflit-sans-apport | grep -c '^+' )
  ( cd "$D" && git merge-tree --write-tree refs/remotes/origin/main refs/remotes/origin/batch/conflit-sans-apport >/dev/null 2>&1 )
  etat_conflit=$?
  if [ "$n_conflit" -eq 0 ] && [ "$etat_conflit" -ne 0 ]; then
    echo "ok: le scénario du conflit en est un — zéro patch en avance, et merge-tree sort en $etat_conflit (fusion conflictuelle)"
  else
    echo "RATÉ: le scénario du conflit n'en est pas un (patchs en avance = $n_conflit, merge-tree = $etat_conflit)"; rate=1
  fi

  # ── A. lots orphelins ───────────────────────────────────────────────────────
  joue '[]'
  exige    'batch/orphelin.*AUCUNE PR ouverte' "un lot en avance sans PR est SIGNALÉ, nommé, avec son nombre de commits"
  interdit 'batch/absorbe-dormant :' "un lot absorbé dans main n'est pas signalé comme orphelin"
  interdit 'batch/repris'            "⭐ un lot à ARBRE IDENTIQUE à main est silencieux — le faux négatif de --is-ancestor ne devient pas un faux POSITIF ici"
  interdit 'batch/conflit-sans-apport' "⭐ un lot dont la fusion CONFLITERAIT mais dont tous les patchs sont dans main est silencieux — l'arbre des marqueurs de conflit n'est pas un verdict (batch/vague-14, 11/09)"
  interdit 'fix/dormante'            "une branche fix/* en avance sans PR est ignorée : la détection A ne surveille QUE les lots"

  joue '[{"number":1,"head":{"ref":"batch/orphelin"},"base":{"ref":"main"}}]'
  interdit 'batch/orphelin' "le même lot, une PR ouverte vers main, est silencieux"

  joue '[{"number":2,"head":{"ref":"batch/orphelin"},"base":{"ref":"rc/v0.9.9"}}]'
  exige 'batch/orphelin.*une base qui ne remonte pas' "une PR de lot vers une RC morte ne vaut PAS remontée"

  # ── B. bases mortes ─────────────────────────────────────────────────────────
  joue '[{"number":10,"head":{"ref":"fix/x"},"base":{"ref":"rc/v0.9.141"}}]'
  exige '#10.*base RC DÉPASSÉE.*rc/v0.9.146' "une PR vers une RC dépassée est signalée, en nommant la RC vivante"

  joue '[{"number":11,"head":{"ref":"fix/x"},"base":{"ref":"rc/v0.9.146"}}]'
  interdit '#11' "une PR vers la RC la plus récente est silencieuse"

  joue '[{"number":12,"head":{"ref":"fix/x"},"base":{"ref":"batch/disparue"}}]'
  exige "#12.*N'EXISTE PLUS" "une PR dont la base a disparu est signalée"

  joue '[{"number":13,"head":{"ref":"fix/x"},"base":{"ref":"main"}}]'
  exige '#13.*topologie hors doctrine' "une PR fix/* → main est signalée"

  joue '[{"number":14,"head":{"ref":"batch/orphelin"},"base":{"ref":"main"}}]'
  interdit '#14' "une PR batch/* → main est silencieuse : c'est la topologie voulue"

  joue '[{"number":15,"head":{"ref":"fix/x"},"base":{"ref":"batch/absorbe-dormant"}}]'
  exige '#15.*base ABSORBÉE' "une PR vers un lot absorbé et dormant est signalée"

  joue '[{"number":16,"head":{"ref":"fix/x"},"base":{"ref":"batch/absorbe-actif"}}]'
  interdit '#16' "⭐ une PR vers un lot absorbé mais ACTIF est silencieuse — batch/bugs-10 portait trois PR le jour de sa remontée"

  joue '[{"number":17,"head":{"ref":"feat/y"},"base":{"ref":"fix/dormante"}}]'
  exige '#17.*pile cassée' "une PR empilée sur une branche dormante sans PR est signalée"

  joue '[{"number":18,"head":{"ref":"feat/y"},"base":{"ref":"fix/portee"}},{"number":19,"head":{"ref":"fix/portee"},"base":{"ref":"batch/orphelin"}}]'
  interdit '#18' "une PR empilée sur une branche qui a sa propre PR ouverte est silencieuse"

  # ⭐ Le faux vert le plus insidieux : une section qui se déclare conforme sous
  #    ses propres croix. Ici la section A rougit (lot orphelin) ET la section B
  #    rougit (#13 fix/* → main) : aucune des deux ne doit imprimer de ✅.
  joue '[{"number":13,"head":{"ref":"fix/x"},"base":{"ref":"main"}}]'
  interdit 'aucune PR ouverte ne vise une base morte' "⭐ une section qui a rougi ne se déclare JAMAIS conforme, même si une autre section avait déjà rougi avant elle"

  # ── Le mode PR ne juge QUE la PR demandée ───────────────────────────────────
  printf '%s' '[{"number":20,"head":{"ref":"fix/x"},"base":{"ref":"rc/v0.9.141"}},{"number":21,"head":{"ref":"fix/y"},"base":{"ref":"rc/v0.9.146"}}]' > "$PULLS"
  sortie=$( cd "$D" && bash "$SCRIPT_MOI" --pr 21 essai/depot 2>&1 )
  interdit '#20' "le mode --pr ne juge que la PR demandée : le voisin rouge ne la contamine pas"
  sortie=$( cd "$D" && bash "$SCRIPT_MOI" --pr 20 essai/depot 2>&1 )
  exige '#20.*base RC DÉPASSÉE' "le mode --pr rend bien le verdict de la PR demandée"
  sortie=$( cd "$D" && bash "$SCRIPT_MOI" --pr 999 essai/depot 2>&1 )
  exige 'introuvable' "une PR absente de l'inventaire fait ROUGIR : la garde ne rend pas vert sur ce qu'elle n'a pas vu"

  # ── Une lecture refusée n'est jamais un silence ─────────────────────────────
  rm -f "$PULLS"; echo 403 > "${PULLS%.json}.code"
  sortie=$( cd "$D" && bash "$SCRIPT_MOI" essai/depot 2>&1 )
  exige 'DROIT MANQUANT' "une lecture d'API refusée fait ROUGIR — ce n'est pas « rien à signaler »"
  rm -f "${PULLS%.json}.code"

  # ── Une page pleine est une borne, pas un inventaire ────────────────────────
  { printf '['; for i in $(seq 1 100); do
      [ "$i" -gt 1 ] && printf ','
      printf '{"number":%d,"head":{"ref":"fix/x"},"base":{"ref":"main"}}' "$i"
    done; printf ']'; } > "$PULLS"
  sortie=$( cd "$D" && bash "$SCRIPT_MOI" essai/depot 2>&1 )
  exige 'inventaire des PR tronqué à 100' "une page de 100 PR est déclarée TRONQUÉE, pas prise pour un inventaire"

  # ── Un dépôt sans aucune ref de lot ne rend pas vert ────────────────────────
  git init -q -b main "$RACINE/vide"
  ( cd "$RACINE/vide" && git config user.email c@e && git config user.name c \
    && echo z > z.txt && git add -A && git commit -qm z \
    && git update-ref refs/remotes/origin/main refs/heads/main ) >/dev/null
  printf '%s' '[]' > "$PULLS"
  sortie=$( cd "$RACINE/vide" && bash "$SCRIPT_MOI" essai/depot 2>&1 )
  exige "n'a RIEN mesuré" "un dépôt où aucune branche batch/* n'a été récupérée fait ROUGIR au lieu de conclure « conforme »"

  return "$rate"
}

if [ "$MODE" = autotest ]; then
  echo "══ Contre-épreuve de la garde de topologie (#2813) ══"
  autotest
  exit $?
fi

echo "══ Garde de topologie des PR et des lots — $DEPOT ══"
echo "   horodatage : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "   mode AUDIT : cette garde SIGNALE, elle ne bloque pas."
echo
charger_etat
if [ -n "$PR_CIBLE" ]; then
  detection_bases_mortes
else
  detection_lots_orphelins
  detection_bases_mortes
fi
if [ "$echec" -ne 0 ]; then
  echo "VERDICT : anomalies de topologie signalées ci-dessus."
else
  echo "VERDICT : topologie conforme."
fi
exit "$echec"
