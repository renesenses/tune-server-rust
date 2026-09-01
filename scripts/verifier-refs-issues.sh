#!/usr/bin/env bash
# Rend visible, et actionnable, ce qu'une PR declare corriger.
#
# Le probleme mesure le 20/08/2026 : 27 issues corrigees ET LIVREES etaient
# restees ouvertes, la plus ancienne depuis la v0.9.63. Le suivi mentait d'un
# mois, au point de faire classer en tete d'un bilan une perte de donnees
# (#1943) deja reparee.
#
# ⚠️ Une ligne de fermeture reste inerte tant qu'elle ne vit que sur
# `release/v0.9`, mais elle devient active DES QUE son commit rejoint `main`.
# La synchronisation de v0.9.125 vers main l'a prouve : le message du commit
# 7e2aff41 contenait une negation en anglais devant #1897 ; GitHub a ignore la
# negation, reconnu le mot-cle actif, puis ferme l'issue (#2785).
#
# Le controle conserve donc le classement utile et la commande manuelle, mais
# refuse avant fusion les formulations negatives qui contiennent tout de meme
# un mot-cle GitHub. Pour ne pas laisser un commit historique contourner le
# controle, il analyse a la fois le titre/corps filtre de la PR ET les messages
# bruts des commits propres a base..head.
#
# Usage :
#   verifier-refs-issues.sh [--commits FICHIER]  lit titre/corps sur STDIN
#   verifier-refs-issues.sh --autotest           joue les cas de preuve et sort
set -uo pipefail

# Les seules formes que GitHub reconnait. Toute autre orthographe ne ferme rien.
#
# ⚠️ Ecrire `fix(|es|ed)` ici semble naturel et casse tout : `grep -E` refuse
# l'alternative VIDE (« empty (sub)expression »), la recherche des fermetures
# rend une liste vide, et toute issue pourtant fermee par la PR est annoncee
# « restera ouverte ». Le script sort 0 malgre tout — le defaut est donc muet
# pour qui ne regarde que le code de sortie. D'ou les cas de preuve qui lisent
# la SORTIE, plus bas.
readonly MOTS_ANGLAIS='close[sd]?|fix(es|ed)?|resolve[sd]?'
# GitHub ne comprend pas la negation : il cherche son mot-cle actif, meme dans
# « does not close #N ». On reconnait donc la negation anglaise jusqu'a 80
# caracteres avant le mot-cle, sans traverser une phrase ni une autre issue.
# Les contractions couvrent l'apostrophe ASCII et l'apostrophe typographique.
readonly NEGATIONS_ANGLAISES="never|((does|do|did|will|would|should|shall|can|could|must|may|might|need|is|are|was|were|has|have|had)[[:space:]]+not)|((doesn|don|didn|won|wouldn|shouldn|shan|can|couldn|mustn|mightn|needn|isn|aren|wasn|weren|hasn|haven|hadn)['’]t)"
readonly FERMETURE_NEGATIVE="(^|[^[:alnum:]_])(${NEGATIONS_ANGLAISES})[[:space:]]+([^#.!?[:cntrl:]]{0,80}[^[:alnum:]_])?(${MOTS_ANGLAIS})[[:space:]:]+#[0-9]+"
# Les formes francaises qu'on ecrit spontanement, et qui ne ferment rien.
#
# Volontairement etroite. « traite » et « regle » ont ete essayes puis retires :
# ce sont des verbes de TITRE (« ## Ce que ca regle »), et un titre suivi d'une
# ligne commencant par « #1742 » les declenchait a tort. Un garde-fou qui refuse
# une PR conforme se fait desactiver dans la semaine ; on prefere en laisser
# passer que bloquer a tort.
readonly MOTS_FRANCAIS='ferm(e|ee|ent)|corrig(e|ee|ent)|r(e|é)sou(t|d|dre)|cl(o|ô)t'

# Retire uniquement les citations du texte de PR. Les messages de commits ne
# passent JAMAIS ici : une citation dangereuse dans l'historique reste active
# lorsque cet historique rejoint main.
filtrer_texte_pr() {
  awk '
    /^[[:space:]]*```/ { dans = !dans; next }
    dans { next }
    /^[[:space:]]*>/   { next }
    { gsub(/`[^`]*`/, " "); gsub(/«[^»]*»/, " "); print }
  '
}

# Analyse le titre/corps de PR et les messages propres a base..head. Ecrit son
# rapport sur STDOUT. Sort 1 uniquement sur une fermeture negative dangereuse.
analyser() {
  local corps="$1" resume="${2:-/dev/null}" commits="${3:-/dev/null}"

  if [ ! -r "$commits" ]; then
    echo "Messages de commits illisibles : $commits" >&2
    return 2
  fi

  # Une reference citee n'engage a rien : on retire les blocs ```, les lignes
  # commencant par « > », et le code EN LIGNE entre accents graves. Sans ca,
  # coller un extrait de journal contenant « Fixes #123 » suffirait a faire
  # passer — ou echouer — une PR sur du texte cite.
  #
  # ⚠️ Ce script s'est declenche DEUX FOIS sur sa propre PR, qui explique la
  # regle et doit donc citer les formes fautives :
  #   1. accents graves oublies — la citation en code en ligne comptait ;
  #   2. GUILLEMETS FRANCAIS oublies — en francais on cite entre « … », et le
  #      corps corrige de la PR #2010 disait « Ferme #1819 » sans accents
  #      graves. Le script proposait de fermer #1819 et #1744, que cette PR ne
  #      corrige pas : une commande destructrice prete a coller, sur les
  #      mauvaises issues.
  # C'est le defaut le plus vicieux d'un controle : il frappe precisement ceux
  # qui l'expliquent. Quatrieme occurrence de cette famille dans le depot cette
  # semaine — le garde-fou apt decoupait deja sur un libelle present dans son
  # propre commentaire.
  local propre
  propre=$(printf '%s\n' "$corps" | filtrer_texte_pr)

  # Les messages de commit sont ajoutes BRUTS. Ne pas leur appliquer les
  # filtres de citation : l'incident #1897 vivait dans un message de commit,
  # pas dans le corps visible de la PR de synchronisation.
  local ensemble
  ensemble=$(printf '%s\n' "$propre"; cat -- "$commits")

  local dangereuses
  dangereuses=$(printf '%s\n' "$ensemble" \
    | grep -oiE "$FERMETURE_NEGATIVE" \
    | grep -oE '#[0-9]+' | sort -u)

  # Une declaration d'intention, dans l'une ou l'autre langue. Les deux sont
  # exactement aussi inertes ici : on ne les distingue donc pas.
  local declarees
  local declarees_brutes
  declarees_brutes=$(printf '%s\n' "$ensemble" \
    | grep -oiE "(^|[^[:alnum:]_])(${MOTS_ANGLAIS}|${MOTS_FRANCAIS}) +#[0-9]+" \
    | grep -oE '#[0-9]+' | sort -u)
  declarees=$(comm -23 <(printf '%s\n' "$declarees_brutes" | grep . || true) \
                       <(printf '%s\n' "$dangereuses" | grep . || true))

  local nues
  nues=$(printf '%s\n' "$ensemble" | grep -oE '#[0-9]{2,5}' | sort -u)

  local actionnables
  actionnables=$(comm -23 <(printf '%s\n' "$nues" | grep . || true) \
                        <(printf '%s\n' "$dangereuses" | grep . || true))

  local citees
  citees=$(comm -23 <(printf '%s\n' "$actionnables" | grep . || true) \
                    <(printf '%s\n' "$declarees" | grep . || true))

  {
    if [ -n "$dangereuses" ]; then
      echo "### ⛔ Formulation de fermeture négative refusée"
      printf '%s\n' "$dangereuses" | sed 's/^/- /'
      echo
      echo "GitHub ignore la négation et active quand même \`close\`/\`fix\`/\`resolve\`"
      echo "quand le commit rejoint \`main\`. Remplacer la formulation par \`Refs #N\`."
      echo
    fi
    if [ -n "$declarees" ]; then
      echo "### Déclarées corrigées par cette PR"
      printf '%s\n' "$declarees" | sed 's/^/- /'
      echo
      echo "GitHub active les mots-clés sur \`main\`. Sur \`release/v0.9\`, l'effet"
      echo "peut être différé jusqu'à la synchronisation du tag publié vers \`main\`."
      echo "Ne jamais employer une négation avec ces mots-clés : écrire \`Refs #N\`."
      echo
      echo "À coller seulement après preuve dans la release publiée, si l'issue reste ouverte :"
      echo
      echo '```bash'
      printf '%s\n' "$declarees" | tr -d '#' | while read -r i; do
        [ -n "$i" ] && echo "gh issue close $i --comment \"Corrigé et vérifié dans la release publiée.\""
      done
      echo '```'
      echo
      echo "Puis **vérifier**, ne pas supposer : \`gh issue view <n> --json state\`."
      echo
    fi
    if [ -n "$citees" ]; then
      echo "### Simplement citées"
      printf '%s\n' "$citees" | sed 's/^/- /'
      echo
      echo "Aucune action : « suite de #N », « cause racine de #N » sont légitimes"
      echo "et majoritaires — 336 des 390 PR qui citent une issue sont dans ce cas."
      echo
    fi
    if [ -z "$dangereuses" ] && [ -z "$declarees" ] && [ -z "$citees" ]; then
      echo "Aucune référence d'issue dans cette PR."
    fi
  } | tee -a "$resume"

  [ -z "$dangereuses" ]
}

# ---------------------------------------------------------------------------
# Cas de preuve. Un garde-fou sans contre-epreuve ne prouve rien : deux fois
# cette semaine, un garde-fou de ce depot repondait vert sur le defaut qu'il
# etait cense attraper. Chaque cas porte donc son inverse.
# ---------------------------------------------------------------------------
autotest() {
  local echecs=0 bac fichier_commits
  bac=$(mktemp -d)
  fichier_commits="$bac/commits"
  trap 'rm -rf -- "$bac"' RETURN

  attendu() {
    local libelle="$1" code_attendu="$2" corps="$3" commits="${4:-}"
    printf '%s\n' "$commits" > "$fichier_commits"
    analyser "$corps" /dev/null "$fichier_commits" >/dev/null 2>&1
    local code=$?
    if [ "$code" -eq "$code_attendu" ]; then
      printf '  ok    %s\n' "$libelle"
    else
      printf '  ECHEC %s (attendu %s, obtenu %s)\n' "$libelle" "$code_attendu" "$code"
      echecs=$((echecs + 1))
    fi
  }

  attendu "incident 7e2aff41 refuse" 1 '' 'This does not close #1897'
  attendu "negation du corps PR refusee" 1 'This does not close #1897'
  attendu "do not fix refuse"         1 '' 'Do not fix #1897 from this commit.'
  attendu "contraction refusee"       1 '' "This won't resolve #1897"
  attendu "not only n'est pas une negation" 0 '' 'This not only fixes #2495, it documents it.'
  attendu "Refs dans un commit reste vert" 0 '' 'Refs #1897'
  attendu "vrai Closes reste vert"    0 '' 'Closes #2495'
  attendu "reference nue"             0 'Cette PR est la suite de #1897.'
  attendu "corps vide"                0 ''

  # Une citation du corps de PR est documentaire et filtree. La meme suite
  # d'octets dans un message de commit n'est jamais masquee : elle rejoindra
  # main avec l'historique et GitHub la lira.
  attendu "citation documentaire ignoree" 0 \
    $'Incident historique :\n```text\nThis does not close #1897\n```'
  attendu "citation dans commit jamais masquee" 1 '' \
    $'Incident historique :\n```text\nThis does not close #1897\n```'

  # ------------------------------------------------------------------
  # Le code de sortie ne prouve plus rien puisqu'il vaut toujours 0. Tout
  # se joue desormais dans le CLASSEMENT, donc dans la SORTIE. C'est deja
  # ce qui avait sauve la mise avec `fix(|es|ed)` : l'alternative vide que
  # grep -E refuse vidait la liste des declarations, et le script sortait
  # 0 en annoncant le contraire de la verite.
  # ------------------------------------------------------------------
  classe() {
    local libelle="$1" corps="$2" section="$3" numero="$4" commits="${5:-}"
    local sortie
    printf '%s\n' "$commits" > "$fichier_commits"
    sortie=$(analyser "$corps" /dev/null "$fichier_commits" 2>/dev/null)
    if printf '%s\n' "$sortie" | sed -n "/$section/,\$p" | grep -qx -- "- $numero"; then
      printf '  ok    %s\n' "$libelle"
    else
      printf '  ECHEC %s — %s absent de « %s »\n' "$libelle" "$numero" "$section"
      echecs=$((echecs + 1))
    fi
  }
  absent_de() {
    local libelle="$1" corps="$2" motif="$3" commits="${4:-}"
    printf '%s\n' "$commits" > "$fichier_commits"
    if analyser "$corps" /dev/null "$fichier_commits" 2>/dev/null | grep -q -- "$motif"; then
      printf '  ECHEC %s — « %s » ne devrait pas apparaitre\n' "$libelle" "$motif"
      echecs=$((echecs + 1))
    else
      printf '  ok    %s\n' "$libelle"
    fi
  }

  # Le classement positif existant ne doit pas regresser.
  classe "Closes  -> declaree"  'Closes #1993'   'Déclarées corrigées' '#1993'
  classe "Fixes   -> declaree"  'Fixes #1993'    'Déclarées corrigées' '#1993'
  classe "Fix     -> declaree"  'Fix #1993'      'Déclarées corrigées' '#1993'
  classe "Ferme   -> declaree"  'Ferme #1993'    'Déclarées corrigées' '#1993'
  classe "Corrige -> declaree"  'Corrige #1993'  'Déclarées corrigées' '#1993'
  classe "Résout  -> declaree"  'Résout #1993'   'Déclarées corrigées' '#1993'
  classe "reference nue -> citee" 'Suite de #1897.' 'Simplement citées' '#1897'
  # Cas mixte : l'une declaree, l'autre citee, dans le meme corps.
  classe "mixte — la declaree" 'Closes #1993, suite de #1897.' 'Déclarées corrigées' '#1993'
  classe "mixte — la citee"    'Closes #1993, suite de #1897.' 'Simplement citées'   '#1897'
  classe "Closes commit -> declaree" '' 'Déclarées corrigées' '#2495' 'Closes #2495'
  classe "Refs commit -> citee" '' 'Simplement citées' '#1897' 'Refs #1897'

  contient() {
    local libelle="$1" corps="$2" motif="$3" commits="${4:-}"
    printf '%s\n' "$commits" > "$fichier_commits"
    if analyser "$corps" /dev/null "$fichier_commits" 2>/dev/null | grep -qF -- "$motif"; then
      printf '  ok    %s\n' "$libelle"
    else
      printf '  ECHEC %s — « %s » absent de la sortie\n' "$libelle" "$motif"
      echecs=$((echecs + 1))
    fi
  }

  # Le seul mecanisme qui marche ici, c'est la fermeture manuelle : la commande
  # doit sortir prete a coller, sinon ce script ne sert a rien.
  contient "la commande gh est emise"   'Closes #1993' 'gh issue close 1993'
  contient "l'avertissement differe est present" 'Closes #1993' "l'effet"
  contient "les deux langues emettent la commande" 'Ferme #1993' 'gh issue close 1993'

  # Le texte cite ne doit rien declencher — le piege qui a fait echouer ce
  # garde-fou sur sa PROPRE PR, qui explique la regle en la citant.
  absent_de "code EN LIGNE ignore"  'On ecrit parfois `Ferme #1819`.'    '- #1819'
  absent_de "bloc de code ignore"   $'```\nFerme #1819\n```'                '- #1819'
  absent_de "citation ignoree"      '> Ferme #1819'                      '- #1819'
  # En francais on cite entre guillemets, pas entre accents graves. Sans ce
  # filtre, le corps corrige de la PR #2010 faisait emettre « gh issue close
  # 1819 » et « ... 1744 » — des commandes pretes a coller, sur des issues que
  # la PR ne corrige pas.
  absent_de "guillemets francais ignores" 'On ecrit « Ferme #1819 », ce qui ne ferme rien.' '- #1819'
  absent_de "guillemets — cas reel PR #2010" 'la #1838 (« Ferme #1819 ») ne cree aucun lien' '- #1819'
  # L'inverse : hors guillemets, toujours attrape.
  classe "hors guillemets, attrape" 'Voir « le guide ». Ferme #1819' 'Déclarées corrigées' '#1819'
  # Et l'inverse, sans quoi le filtre pourrait tout avaler.
  classe "hors accents graves, attrape" 'Voir `le guide`. Ferme #1819' 'Déclarées corrigées' '#1819'
  # « fermeture » contient « ferme » mais n'est pas suivi d'un numero.
  absent_de "« Fermeture du chantier » n'est pas un mot-cle" 'Fermeture du chantier, voir #1897.' 'Déclarées corrigées'

  echo
  if [ "$echecs" -eq 0 ]; then
    echo "autotest : tous les cas passent"
    return 0
  fi
  echo "autotest : $echecs cas en echec"
  return 1
}

if [ "${1:-}" = "--autotest" ]; then
  autotest
  exit $?
fi

commits=/dev/null
if [ "${1:-}" = "--commits" ] && [ "$#" -eq 2 ]; then
  commits="$2"
elif [ "$#" -ne 0 ]; then
  echo "Usage : $0 [--commits FICHIER] | $0 --autotest" >&2
  exit 2
fi

analyser "$(cat)" "${GITHUB_STEP_SUMMARY:-/dev/null}" "$commits"
