#!/usr/bin/env bash
# Verifie qu'une PR qui DIT fermer une issue la ferme reellement.
#
# Pourquoi ce garde-fou existe. Mesure du 20/08/2026 sur les 600 dernieres PR
# fusionnees de ce depot :
#
#   citant au moins une issue dans le corps ......... 390
#   avec un mot-cle de fermeture reconnu par GitHub .. 54
#   => citent sans rien fermer ..................... 336
#
# Consequence : 27 issues corrigees ET livrees sont restees ouvertes, certaines
# depuis la v0.9.63. Le suivi mentait d'un mois, au point de faire classer en
# tete de liste une perte de donnees deja reparee.
#
# Deux fautes distinctes, deux severites distinctes.
#
# 1. ERREUR — le mot-cle est ecrit EN FRANCAIS. « Ferme #1819 », « Corrige
#    #1348 », « Resout #1679 » : l'intention est explicite, et GitHub l'ignore
#    totalement. Il n'accepte que close/closes/closed, fix/fixes/fixed,
#    resolve/resolves/resolved. L'auteur croit avoir ferme, l'issue reste
#    ouverte, et personne ne repasse. 22 PR sont dans ce cas.
#
# 2. AVERTISSEMENT — une reference nue « #1897 » sans mot-cle. C'est le cas
#    MAJORITAIRE et souvent LEGITIME : « suite de #1897 », « cause racine de
#    #1528 ». On ne bloque pas, on affiche la liste dans le resume du job pour
#    que le relecteur voie ce qui restera ouvert apres la fusion.
#
# Autrement dit : on echoue sur une intention trahie, jamais sur une simple
# citation.
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

# Analyse un corps de PR. Ecrit son verdict sur STDOUT.
# Sortie : 0 = conforme, 1 = intention de fermeture en francais.
analyser() {
  local corps="$1" resume="${2:-/dev/null}"

  # Une reference dans un bloc de code ou une citation n'engage a rien : on
  # retire les blocs ``` et les lignes commencant par « > » avant d'analyser.
  # Sans ca, coller un extrait de journal contenant « Fixes #123 » suffirait a
  # faire passer — ou echouer — une PR sur du texte cite.
  local propre
  propre=$(printf '%s\n' "$corps" | awk '
    /^[[:space:]]*```/ { dans = !dans; next }
    dans { next }
    /^[[:space:]]*>/   { next }
    { print }
  ')

  local francais
  francais=$(printf '%s\n' "$propre" \
    | grep -oiE "(^|[^[:alnum:]_])(${MOTS_FRANCAIS}) +#[0-9]+" \
    | sed -E 's/^[^[:alnum:]]*//' | sort -u)

  if [ -n "$francais" ]; then
    {
      echo "## Mot-cle de fermeture en francais — GitHub ne le lit pas"
      echo
      echo "Ces mentions n'auront **aucun effet** a la fusion :"
      echo
      printf '%s\n' "$francais" | sed 's/^/- `/; s/$/`/'
      echo
      echo "GitHub n'accepte que \`Closes #N\`, \`Fixes #N\` ou \`Resolves #N\`."
      echo "Remplacez, ou dites explicitement que c'est une simple reference."
    } | tee -a "$resume"
    return 1
  fi

  local nues
  nues=$(printf '%s\n' "$propre" | grep -oE '#[0-9]{2,5}' | sort -u)
  local fermees
  fermees=$(printf '%s\n' "$propre" \
    | grep -oiE "(${MOTS_ANGLAIS}) +#[0-9]+" | grep -oE '#[0-9]+' | sort -u)

  local restantes
  restantes=$(comm -23 <(printf '%s\n' "$nues" | grep . || true) \
                       <(printf '%s\n' "$fermees" | grep . || true))

  {
    if [ -n "$fermees" ]; then
      echo "### Sera ferme a la fusion"
      printf '%s\n' "$fermees" | sed 's/^/- /'
      echo
    fi
    if [ -n "$restantes" ]; then
      echo "### Cite, mais restera ouvert"
      printf '%s\n' "$restantes" | sed 's/^/- /'
      echo
      echo "Si l'une de ces issues est reellement traitee par cette PR, ecrivez"
      echo "\`Closes #N\`. Sinon il n'y a rien a faire : une reference est legitime."
    fi
    if [ -z "$fermees" ] && [ -z "$restantes" ]; then
      echo "Aucune reference d'issue dans cette PR."
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

  attendu "« Ferme #1819 » est refuse"            1 'Ferme #1819'
  attendu "« Corrige #1348 » est refuse"          1 'Corrige #1348'
  attendu "« Résout #1679 » est refuse"           1 'Résout #1679'
  attendu "« Clôt #12 » est refuse"               1 'Clôt #12'
  # L'inverse : la bonne forme doit passer, sinon le garde-fou bloque tout.
  attendu "« Closes #1819 » passe"                0 'Closes #1819'
  attendu "« Fixes #1819 » passe"                 0 'Fixes #1819'
  attendu "« Resolves #1819 » passe"              0 'Resolves #1819'
  attendu "casse indifferente"                    0 'closes #1819'
  # Une reference nue est legitime : 336 PR sur 390 sont dans ce cas.
  attendu "reference nue « suite de #1897 »"      0 'Cette PR est la suite de #1897.'
  attendu "corps vide"                            0 ''
  # « fermeture » contient « ferme » mais n'est pas suivi d'un numero.
  attendu "« fermeture du chantier » n'est pas un mot-cle" 0 'Fermeture du chantier, voir #1897.'
  # Le piege qui a fait echouer deux garde-fous cette semaine : le texte cite.
  attendu "« Ferme #1 » dans un bloc de code est ignore" 0 '```
Ferme #1819
```'
  attendu "« Ferme #1 » dans une citation est ignore" 0 '> Ferme #1819'
  # Et son inverse : hors bloc, il doit toujours etre attrape.
  attendu "hors bloc, toujours attrape"           1 '```
du code
```
Ferme #1819'

  # ------------------------------------------------------------------
  # Les cas ci-dessus ne lisent que le CODE DE SORTIE. Ca ne suffit pas :
  # un classement faux (une issue fermee annoncee « restera ouverte »)
  # sort 0 lui aussi, donc reste invisible. C'est exactement ce qui est
  # arrive avec `fix(|es|ed)`. Les cas suivants lisent la SORTIE.
  # ------------------------------------------------------------------
  classe() {
    local libelle="$1" corps="$2" section="$3" numero="$4"
    local sortie
    sortie=$(analyser "$corps" 2>/dev/null)
    # La ligne « - #N » doit apparaitre APRES le titre de section attendu.
    if printf '%s\n' "$sortie" | sed -n "/$section/,\$p" | grep -qx -- "- $numero"; then
      printf '  ok    %s\n' "$libelle"
    else
      printf '  ECHEC %s — %s absent de « %s »\n' "$libelle" "$numero" "$section"
      echecs=$((echecs + 1))
    fi
  }

  classe "Closes #1993 est classe FERME"  'Closes #1993'            'Sera ferme'   '#1993'
  classe "Fixes #1993 est classe FERME"   'Fixes #1993'             'Sera ferme'   '#1993'
  classe "Fix #1993 est classe FERME"     'Fix #1993'               'Sera ferme'   '#1993'
  classe "Resolved #1993 est classe FERME" 'Resolved #1993'         'Sera ferme'   '#1993'
  classe "une reference nue reste OUVERTE" 'Suite de #1897.'        'restera ouvert' '#1897'
  # Le cas mixte, le plus proche du reel : une PR ferme l'une et cite l'autre.
  classe "cas mixte — la fermee"  'Closes #1993, suite de #1897.'  'Sera ferme'   '#1993'
  classe "cas mixte — la citee"   'Closes #1993, suite de #1897.'  'restera ouvert' '#1897'

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
