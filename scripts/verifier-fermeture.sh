#!/usr/bin/env bash
# Contrôle la JUSTIFICATION d'une fermeture d'issue.
#
# Le 2026-08-21 a 10:07:12, six issues ont ete fermees en TREIZE SECONDES. Cinq
# portaient une « preuve par contenu » produite par
#
#   git grep -c "<un mot du domaine>" <tag> | head -1
#
# `head -1` rend le premier fichier venu. Les preuves designaient :
#
#   #1657  marqueur `opt-in`      → .github/workflows/ci.yml
#   #1919  marqueur `poller`      → MIGRATION.md
#   #1929  marqueur `precedent`   → tune-core/build.rs
#   #1984  marqueur `dlna`        → docs/ARCHITECTURE-CIBLE-v0.9.md
#
# Aucun de ces fichiers n'a de rapport avec le correctif. Ce n'est pas une
# preuve faible : c'est une preuve INVENTEE, et c'est pire qu'une fermeture
# nue, parce que ca se relit comme une verification. Les six ont du etre
# rouvertes.
#
# Pire encore : #1654 et #1657 citaient un commit dont le CORPS dit, en toutes
# lettres, « Ne ferme aucune des deux issues : les reponses des testeurs
# manquent toujours ». La justification invoquait un commit qui INTERDISAIT la
# fermeture qu'elle operait. Le 2026-08-25, #2230 et #2156 ont reproduit le
# meme defaut ; #2296 a en plus ete fermee avec l'etiquette `keep-open`.
#
# ⚠️ CE QUE CE SCRIPT NE FAIT PAS.
#
# Il ne rouvre rien, et il ne ferme rien. Refermer le ticket d'un testeur est
# une decision ; y ajouter une automatisation qui decide a la place d'un humain
# remplacerait un probleme par un autre. Il PARLE, fort, et laisse trancher.
#
# Il ne fait pas non plus la police de la forme : une fermeture sans preuve du
# tout n'est pas signalee ici. Le defaut vise est la preuve qui MENT, parce que
# c'est elle qui desarme la relecture.
#
# Usage :
#   NUMERO_ISSUE=123 KEEP_OPEN=false verifier-fermeture.sh <commentaire>
#   verifier-fermeture.sh --autotest               joue les cas de preuve
#
# Sortie : 0 si la justification tient (ou s'il n'y a rien a verifier),
#          1 si au moins un signe la contredit.
set -uo pipefail

# Les fichiers qui ne peuvent JAMAIS prouver qu'un correctif est livre.
#
# La documentation decrit une intention, pas un comportement. Deux des cinq
# fausses preuves du 21/08 pointaient un `.md`.
readonly EXT_SANS_PREUVE='\.(md|txt|adoc|rst)$'

# Les refus explicites qu'un message de commit peut porter.
#
# Volontairement litteraux : ce sont des tournures qu'on ecrit exprès, pour
# etre lues. « Ne ferme aucune des deux issues » a ete ecrit, puis ignore.
readonly REFUS='ne ferme (pas|aucune|ni)|ne pr[eé]tend pas|ne r[eé]sout pas|ne corrige (pas|rien)|ne cl[oô]ture pas|reste(nt)? ouverte?s?|non fait|ce que (ça|ca) ne fait pas|cause .*reste inconnue'

# Extrait la valeur d'un champ « clef : valeur » du commentaire, insensible aux
# accents et a la casse. Rend une chaine vide si absent.
champ() {
  local corps="$1" motif="$2"
  printf '%s\n' "$corps" | grep -iEo "$motif" | head -1
}

# Rend les commits a relire avant de fermer :
#
# - TOUS les SHA cites dans le commentaire, pas seulement le premier ;
# - TOUS les commits de l'historique qui mentionnent explicitement l'issue.
#
# Le deuxieme point est ce qui attrape #2230 : la justification citait le
# commit de renommage, tandis que le commit fonctionnel 7c16924a disait
# explicitement « Ce correctif ne resout PAS #2230 ».
commits_candidats() {
  local corps="$1" depot="$2" numero="${NUMERO_ISSUE:-}"

  [ "$depot" != "-" ] || return 0

  {
    printf '%s\n' "$corps" | grep -oE '\b[0-9a-f]{7,40}\b' || true
    if printf '%s\n' "$numero" | grep -qE '^[0-9]+$'; then
      git -C "$depot" log --all --format='%H' --extended-regexp \
        --grep="(^|[^0-9])#${numero}([^0-9]|$)" 2>/dev/null || true
    fi
  } | while IFS= read -r candidat; do
    [ -n "$candidat" ] || continue
    git -C "$depot" rev-parse --verify "$candidat^{commit}" 2>/dev/null || true
  done | sort -u
}

# Rend uniquement les refus qui peuvent viser l'issue en cours.
#
# Un commit de garde-fou ou de documentation peut citer textuellement le refus
# porte par un AUTRE correctif. C'est le cas du commit qui corrige #2086 : son
# paragraphe historique cite #1654 et #1657 puis reproduit « Ne ferme aucune des
# deux issues ». Lire ce texte comme un veto sur #2086 rendrait le controle
# impossible a satisfaire par son propre correctif.
#
# Regle conservative :
# - un paragraphe sans numero d'issue reste un refus (cas du SHA explicitement
#   cite dans une justification) ;
# - un paragraphe qui cite l'issue courante reste un refus ;
# - un paragraphe qui ne cite QUE d'autres issues est un recit, pas un veto sur
#   l'issue courante.
refus_pertinents() {
  local message="$1" numero="${2:-}" paragraphe references

  while IFS= read -r -d $'\034' paragraphe; do
    printf '%s\n' "$paragraphe" | grep -qiE "$REFUS" || continue
    references=$(printf '%s\n' "$paragraphe" | grep -oE '#[0-9]+' | sort -u || true)
    if printf '%s\n' "$numero" | grep -qE '^[0-9]+$' \
      && [ -n "$references" ] \
      && ! printf '%s\n' "$references" | grep -qxF "#$numero"
    then
      continue
    fi
    printf '%s\n' "$paragraphe" | grep -iE "$REFUS" | head -2
  done < <(printf '%s\n' "$message" | awk 'BEGIN { RS=""; ORS=sprintf("%c", 28) } { print }')
}

# Analyse un commentaire de fermeture. Ecrit son rapport sur STDOUT.
#
# `depot` permet aux cas de preuve de travailler sur un faux depot ; en usage
# reel c'est le repertoire courant.
analyser() {
  local corps="$1" depot="${2:-.}"
  local problemes=0

  # `keep-open` n'est pas un ornement. Tant qu'elle est posee, elle interdit
  # la fermeture automatique ou distraite ; il faut la retirer par une
  # decision explicite avant de fermer.
  if [ "${KEEP_OPEN:-false}" = "true" ]; then
    printf "FERMETURE INTERDITE  l'issue porte encore l'etiquette \`keep-open\`.\n"
    printf '                    retirez-la explicitement apres arbitrage, ou laissez ouverte.\n'
    problemes=$((problemes + 1))
  fi

  # ── La preuve par contenu : « <marqueur> — <fichier>[:ligne] » ────────────
  #
  # On accepte les deux formes rencontrees : avec et sans numero de ligne, et
  # avec le marqueur entre accents graves ou nu.
  local ligne_preuve fichier marqueur
  ligne_preuve=$(printf '%s\n' "$corps" | grep -iE 'v(é|e)rifi(é|e) par contenu' | head -1)

  if [ -n "$ligne_preuve" ]; then
    marqueur=$(printf '%s\n' "$ligne_preuve" | grep -oE '`[^`]+`' | head -1 | tr -d '`')
    # Le fichier est le dernier chemin cite sur la ligne, eventuellement
    # prefixe d'un tag (« v0.9.94:chemin/fichier.rs:12 »).
    fichier=$(printf '%s\n' "$ligne_preuve" \
      | grep -oE '[A-Za-z0-9_./-]+\.[A-Za-z0-9]+(:[0-9]+)?' | tail -1 \
      | sed -E 's/:[0-9]+$//')

    if [ -n "$fichier" ]; then
      # 1. Un fichier de documentation ne prouve rien.
      if printf '%s\n' "$fichier" | grep -qE "$EXT_SANS_PREUVE"; then
        printf 'PREUVE INVALIDE  le fichier cite est de la documentation : %s\n' "$fichier"
        printf '                 une preuve cite le fichier DU correctif et un marqueur qui lui est propre.\n'
        problemes=$((problemes + 1))
      fi

      # 2. Le marqueur doit ressembler a un symbole, pas a un mot du domaine.
      #
      # Un symbole porte une marque de code : underscore, parentheses, `::`,
      # majuscule interne, ou un mot-cle Rust/SQL. « dlna », « poller »,
      # « opt-in » n'en ont aucune.
      if [ -n "$marqueur" ] \
        && ! printf '%s\n' "$marqueur" | grep -qE '_|\(|\)|::|[a-z][A-Z]|^(fn|const|struct|enum|impl|pub|let|UPDATE|SELECT|ALTER|CREATE) '; then
        printf 'PREUVE FAIBLE    le marqueur « %s » ne ressemble pas a un symbole.\n' "$marqueur"
        printf '                 un mot commun du domaine se trouve partout ; il ne prouve rien.\n'
        problemes=$((problemes + 1))
      fi

      # 3. Le fichier cite doit exister dans le depot.
      if [ "$depot" != "-" ] && [ ! -e "$depot/$fichier" ]; then
        printf 'PREUVE INVALIDE  le fichier cite est introuvable : %s\n' "$fichier"
        problemes=$((problemes + 1))
      fi
    fi
  fi

  # ── Un commit candidat se refusait-il le droit de fermer ? ────────────────
  #
  # C'est le controle le plus rentable : a lui seul il aurait arrete les
  # fermetures de #1654, #1657, #2156, #2230 et #2239.
  local sha message shas extraits empreinte empreintes_vues=""
  shas=$(commits_candidats "$corps" "$depot")
  while IFS= read -r sha; do
    [ -n "$sha" ] || continue
    message=$(git -C "$depot" log -1 --format='%B' "$sha" 2>/dev/null)
    extraits=$(refus_pertinents "$message" "${NUMERO_ISSUE:-}")
    if [ -n "$extraits" ]; then
      # Une PR peut laisser dans l'historique le commit original et sa copie
      # rejouee avec un sujet suffixe par le numero de PR. Les lignes de refus
      # sont identiques : c'est un seul signal, pas deux.
      empreinte=$(printf '%s' "$extraits" | git -C "$depot" hash-object --stdin)
      if printf '%s\n' "$empreintes_vues" | grep -qxF "$empreinte"; then
        continue
      fi
      empreintes_vues="${empreintes_vues}${empreinte}"$'\n'
      printf 'COMMIT REFUSANT  %s dit lui-meme ne pas fermer :\n' "${sha:0:8}"
      printf '%s\n' "$extraits" | sed 's/^/                 > /'
      printf '                 une PR qui se refuse le droit de fermer sait quelque chose que le\n'
      printf '                 verificateur ignore.\n'
      problemes=$((problemes + 1))
    fi
  done <<< "$shas"

  if [ "$problemes" -eq 0 ]; then
    printf 'ok — rien ne contredit cette fermeture.\n'
    return 0
  fi
  printf '\n%d signe(s) contredisent cette fermeture. A relire par un humain.\n' "$problemes"
  return 1
}

# ── Contre-epreuve ─────────────────────────────────────────────────────────
#
# Jouee A CHAQUE FOIS avant l'analyse reelle : un garde-fou dont la
# contre-epreuve ne tourne pas finit par repondre vert sur le defaut qu'il
# devait attraper. C'est arrive deux fois dans ce depot la meme semaine.
autotest() {
  local echecs=0
  attendu() {
    local libelle="$1" code_attendu="$2" corps="$3"
    analyser "$corps" - >/dev/null 2>&1
    local code=$?
    if [ "$code" -eq "$code_attendu" ]; then
      printf '  ok    %s\n' "$libelle"
    else
      printf '  ECHEC %s (attendu %d, obtenu %d)\n' "$libelle" "$code_attendu" "$code"
      echecs=$((echecs + 1))
    fi
  }

  # Les cinq fausses preuves reelles du 2026-08-21. Chacune DOIT etre refusee.
  attendu 'fausse preuve #1984 (doc)' 1 \
    'Vérifié par contenu dans le tag : `dlna` — v0.9.94:docs/ARCHITECTURE-CIBLE-v0.9.md:5.'
  attendu 'fausse preuve #1919 (doc)' 1 \
    'Vérifié par contenu dans le tag : `poller` — v0.9.94:MIGRATION.md:1.'
  attendu 'fausse preuve #1657 (mot commun)' 1 \
    'Vérifié par contenu dans le tag : `opt-in` — v0.9.94:.github/workflows/ci.yml:1.'
  attendu 'fausse preuve #1953 (mot commun)' 1 \
    'Vérifié par contenu dans le tag : `chromecast` — v0.9.94:tune-core/src/audio/formats.rs:10.'
  attendu 'fausse preuve #1929 (mot commun)' 1 \
    'Vérifié par contenu dans le tag : `precedent` — v0.9.94:tune-core/build.rs:1.'

  # Une VRAIE preuve doit passer : symbole, fichier de code.
  attendu 'preuve valable (fn)' 0 \
    'Vérifié par contenu dans le tag : `fn is_dop_pcm` — v0.9.92:tune-core/src/outputs/local.rs:1455.'
  attendu 'preuve valable (const)' 0 \
    'Vérifié par contenu : `PART_MAX_PURGE` — tune-server/src/routes/system/scan.rs:830.'
  attendu 'preuve valable (chemin qualifie)' 0 \
    'Vérifié par contenu : `dsd_dop_not_requested` — tune-core/src/orchestrator.rs:2860.'

  # Une fermeture sans preuve n'est PAS le defaut vise : elle ne ment pas.
  attendu 'fermeture sans preuve' 0 'Corrigé par la PR #2033, livré en v0.9.94.'

  # Un commentaire vide ne doit rien declencher.
  attendu 'commentaire vide' 0 ''

  # Les refus de fermeture doivent etre testes sur un VRAI depot Git. Le
  # commentaire ne cite volontairement aucun SHA : le garde-fou doit retrouver
  # le commit par le numero de l'issue, sinon #2230 lui echappe encore.
  local depot_test
  depot_test=$(mktemp -d "${TMPDIR:-/tmp}/verifier-fermeture.XXXXXX")
  git -C "$depot_test" init -q
  git -C "$depot_test" config user.name 'Contre-epreuve'
  git -C "$depot_test" config user.email 'contre-epreuve@example.invalid'
  git -C "$depot_test" commit -q --allow-empty \
    -m 'fix: adaptation partielle (#9991)' \
    -m 'Ce correctif ne resout PAS #9991.'

  NUMERO_ISSUE=9991 analyser 'Corrigé par une PR.' "$depot_test" >/dev/null 2>&1
  local code=$?
  if [ "$code" -eq 1 ]; then
    printf '  ok    commit refusant retrouve par numero d issue\n'
  else
    printf '  ECHEC commit refusant retrouve par numero d issue (attendu 1, obtenu %d)\n' "$code"
    echecs=$((echecs + 1))
  fi

  KEEP_OPEN=true analyser 'Correction livrée.' - >/dev/null 2>&1
  code=$?
  if [ "$code" -eq 1 ]; then
    printf '  ok    etiquette keep-open encore posee\n'
  else
    printf '  ECHEC etiquette keep-open encore posee (attendu 1, obtenu %d)\n' "$code"
    echecs=$((echecs + 1))
  fi

  # Un message sans refus ne doit pas devenir suspect parce qu'il est lie a
  # une issue : la recherche large sert a LIRE, pas a condamner par defaut.
  git -C "$depot_test" commit -q --allow-empty \
    -m 'fix: correction complete avec contre-epreuve (#9992)'
  NUMERO_ISSUE=9992 analyser 'Corrigé et vérifié.' "$depot_test" >/dev/null 2>&1
  code=$?
  if [ "$code" -eq 0 ]; then
    printf '  ok    commit complet lie a l issue\n'
  else
    printf '  ECHEC commit complet lie a l issue (attendu 0, obtenu %d)\n' "$code"
    echecs=$((echecs + 1))
  fi

  # Le correctif de #2086 cite les refus historiques de #1654/#1657 dans son
  # propre message. Ce recit ne doit pas se transformer en veto sur #2086.
  git -C "$depot_test" commit -q --allow-empty \
    -m 'fix: garde des fermetures (#9993)' \
    -m 'Incident : #1111 et #2222 citaient « Ne ferme aucune des deux issues ».'
  NUMERO_ISSUE=9993 analyser 'Corrigé et vérifié.' "$depot_test" >/dev/null 2>&1
  code=$?
  if [ "$code" -eq 0 ]; then
    printf '  ok    refus historique visant d autres issues ignore\n'
  else
    printf '  ECHEC refus historique visant d autres issues ignore (attendu 0, obtenu %d)\n' "$code"
    echecs=$((echecs + 1))
  fi

  # Si l'issue courante figure dans le paragraphe, les autres numeros ne
  # diluent pas le refus : le veto reste applicable.
  git -C "$depot_test" commit -q --allow-empty \
    -m 'fix: traitement groupe (#9994)' \
    -m 'Ne ferme pas #9994 ; #1111 est corrige mais ce chantier reste ouvert.'
  NUMERO_ISSUE=9994 analyser 'Corrigé et vérifié.' "$depot_test" >/dev/null 2>&1
  code=$?
  if [ "$code" -eq 1 ]; then
    printf '  ok    refus visant l issue courante parmi plusieurs numeros\n'
  else
    printf '  ECHEC refus visant l issue courante parmi plusieurs numeros (attendu 1, obtenu %d)\n' "$code"
    echecs=$((echecs + 1))
  fi

  rm -rf "$depot_test"

  if [ "$echecs" -gt 0 ]; then
    printf '\n%d cas de preuve en echec.\n' "$echecs"
    return 1
  fi
  printf '\nTous les cas de preuve passent.\n'
  return 0
}

if [ "${1:-}" = "--autotest" ]; then
  autotest
  exit $?
fi

analyser "${1:-$(cat)}" "${DEPOT:-.}"
