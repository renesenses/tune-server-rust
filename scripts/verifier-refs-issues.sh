#!/usr/bin/env bash
# Rend visible, et actionnable, ce qu'une PR declare corriger.
#
# Le probleme mesure le 20/08/2026 : 27 issues corrigees ET LIVREES etaient
# restees ouvertes, la plus ancienne depuis la v0.9.63. Le suivi mentait d'un
# mois, au point de faire classer en tete d'un bilan une perte de donnees
# (#1943) deja reparee.
#
# ⚠️ CE QUI NE MARCHE PAS, ET QU'IL FAUT AVOIR EN TETE AVANT DE LIRE LA SUITE.
#
# Sur ce depot, AUCUN mot-cle de fermeture ne ferme quoi que ce soit — pas meme
# en anglais impeccable. GitHub n'auto-ferme que sur la branche PAR DEFAUT, et
# la doctrine impose que tout correctif cible `release/v0.9`. Mesure du
# 20/08/2026 sur 18 PR portant `Closes`/`Fixes`/`Resolves` et fusionnees sur
# `release/v0.9` :
#
#   fermeture automatique par le merge .... 0
#   fermeture faite a la main plus tard ... 18
#
# Pire, GitHub n'enregistre meme pas le LIEN : `closingIssuesReferences` rend
# une liste vide pour la PR #1749 (« Fixes #1744 ») comme pour la #1838
# (« Ferme #1819 »). Sur cette branche, la ligne de fermeture est du texte
# decoratif, quelle que soit sa langue.
#
# Ce script ne fait donc PAS la police de la langue : ce serait imposer une
# forme sans effet. Il fait la seule chose utile ici — rassembler ce que la PR
# declare corriger, et rendre la commande de fermeture MANUELLE prete a coller,
# puisque c'est le seul mecanisme qui marche.
#
# Il n'echoue jamais. Un controle qui bloque sur une regle inoperante se fait
# desactiver dans la semaine.
#
# Usage :
#   verifier-refs-issues.sh            lit le corps sur STDIN
#   verifier-refs-issues.sh --autotest joue les cas de preuve et sort
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
# Les formes francaises qu'on ecrit spontanement, et qui ne ferment rien.
#
# Volontairement etroite. « traite » et « regle » ont ete essayes puis retires :
# ce sont des verbes de TITRE (« ## Ce que ca regle »), et un titre suivi d'une
# ligne commencant par « #1742 » les declenchait a tort. Un garde-fou qui refuse
# une PR conforme se fait desactiver dans la semaine ; on prefere en laisser
# passer que bloquer a tort.
readonly MOTS_FRANCAIS='ferm(e|ee|ent)|corrig(e|ee|ent)|r(e|é)sou(t|d|dre)|cl(o|ô)t'

# Analyse un corps de PR. Ecrit son rapport sur STDOUT. Sortie toujours 0.
analyser() {
  local corps="$1" resume="${2:-/dev/null}"

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
  propre=$(printf '%s\n' "$corps" | awk '
    /^[[:space:]]*```/ { dans = !dans; next }
    dans { next }
    /^[[:space:]]*>/   { next }
    { gsub(/`[^`]*`/, " "); gsub(/«[^»]*»/, " "); print }
  ')

  # Une declaration d'intention, dans l'une ou l'autre langue. Les deux sont
  # exactement aussi inertes ici : on ne les distingue donc pas.
  local declarees
  declarees=$(printf '%s\n' "$propre" \
    | grep -oiE "(^|[^[:alnum:]_])(${MOTS_ANGLAIS}|${MOTS_FRANCAIS}) +#[0-9]+" \
    | grep -oE '#[0-9]+' | sort -u)

  local nues
  nues=$(printf '%s\n' "$propre" | grep -oE '#[0-9]{2,5}' | sort -u)

  local citees
  citees=$(comm -23 <(printf '%s\n' "$nues" | grep . || true) \
                    <(printf '%s\n' "$declarees" | grep . || true))

  {
    if [ -n "$declarees" ]; then
      echo "### Déclarées corrigées par cette PR"
      printf '%s\n' "$declarees" | sed 's/^/- /'
      echo
      echo "⚠️ **Elles ne se fermeront pas toutes seules.** La base est"
      echo "\`release/v0.9\` ; GitHub n'auto-ferme que sur la branche par défaut."
      echo "Mesuré le 20/08/2026 : sur 18 PR portant \`Closes\`/\`Fixes\` fusionnées"
      echo "sur cette branche, **0 fermeture automatique, 18 faites à la main**."
      echo
      echo "À coller après la fusion :"
      echo
      echo '```bash'
      printf '%s\n' "$declarees" | tr -d '#' | while read -r i; do
        [ -n "$i" ] && echo "gh issue close $i --comment \"Corrigé par cette PR, fusionnée sur release/v0.9.\""
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
    if [ -z "$declarees" ] && [ -z "$citees" ]; then
      echo "Aucune référence d'issue dans cette PR."
    fi
  } | tee -a "$resume"
  return 0
}

# ---------------------------------------------------------------------------
# Cas de preuve. Un garde-fou sans contre-epreuve ne prouve rien : deux fois
# cette semaine, un garde-fou de ce depot repondait vert sur le defaut qu'il
# etait cense attraper. Chaque cas porte donc son inverse.
# ---------------------------------------------------------------------------
autotest() {
  local echecs=0
  attendu() {
    local libelle="$1" code_attendu="$2" corps="$3"
    analyser "$corps" >/dev/null 2>&1
    local code=$?
    if [ "$code" -eq "$code_attendu" ]; then
      printf '  ok    %s\n' "$libelle"
    else
      printf '  ECHEC %s (attendu %s, obtenu %s)\n' "$libelle" "$code_attendu" "$code"
      echecs=$((echecs + 1))
    fi
  }

  # Le script n'echoue plus jamais : la langue du mot-cle n'a aucun effet ici,
  # et bloquer sur une regle inoperante ne ferait que le faire desactiver.
  attendu "« Ferme #1819 » ne bloque plus"        0 'Ferme #1819'
  attendu "« Closes #1819 » ne bloque pas non plus" 0 'Closes #1819'
  attendu "reference nue"                         0 'Cette PR est la suite de #1897.'
  attendu "corps vide"                            0 ''

  # ------------------------------------------------------------------
  # Le code de sortie ne prouve plus rien puisqu'il vaut toujours 0. Tout
  # se joue desormais dans le CLASSEMENT, donc dans la SORTIE. C'est deja
  # ce qui avait sauve la mise avec `fix(|es|ed)` : l'alternative vide que
  # grep -E refuse vidait la liste des declarations, et le script sortait
  # 0 en annoncant le contraire de la verite.
  # ------------------------------------------------------------------
  classe() {
    local libelle="$1" corps="$2" section="$3" numero="$4"
    local sortie
    sortie=$(analyser "$corps" 2>/dev/null)
    if printf '%s\n' "$sortie" | sed -n "/$section/,\$p" | grep -qx -- "- $numero"; then
      printf '  ok    %s\n' "$libelle"
    else
      printf '  ECHEC %s — %s absent de « %s »\n' "$libelle" "$numero" "$section"
      echecs=$((echecs + 1))
    fi
  }
  absent_de() {
    local libelle="$1" corps="$2" motif="$3"
    if analyser "$corps" 2>/dev/null | grep -q -- "$motif"; then
      printf '  ECHEC %s — « %s » ne devrait pas apparaitre\n' "$libelle" "$motif"
      echecs=$((echecs + 1))
    else
      printf '  ok    %s\n' "$libelle"
    fi
  }

  # Les deux langues sont desormais traitees a l'identique.
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

  contient() {
    local libelle="$1" corps="$2" motif="$3"
    if analyser "$corps" 2>/dev/null | grep -qF -- "$motif"; then
      printf '  ok    %s\n' "$libelle"
    else
      printf '  ECHEC %s — « %s » absent de la sortie\n' "$libelle" "$motif"
      echecs=$((echecs + 1))
    fi
  }

  # Le seul mecanisme qui marche ici, c'est la fermeture manuelle : la commande
  # doit sortir prete a coller, sinon ce script ne sert a rien.
  contient "la commande gh est emise"   'Closes #1993' 'gh issue close 1993'
  contient "l'avertissement est present" 'Closes #1993' "ne se fermeront pas toutes seules"
  contient "les deux langues emettent la commande" 'Ferme #1993' 'gh issue close 1993'

  # Le texte cite ne doit rien declencher — le piege qui a fait echouer ce
  # garde-fou sur sa PROPRE PR, qui explique la regle en la citant.
  absent_de "code EN LIGNE ignore"  'On ecrit parfois `Ferme #1819`.'    '- #1819'
  absent_de "bloc de code ignore"   '```\nFerme #1819\n```'              '- #1819'
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

analyser "$(cat)" "${GITHUB_STEP_SUMMARY:-/dev/null}"
