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
# fermeture qu'elle operait.
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
#   verifier-fermeture.sh <corps-du-commentaire>   analyse, ecrit sur STDOUT
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
readonly REFUS='ne ferme (pas|aucune|ni)|ne pretend (pas|PAS)|ne resout pas|reste ouverte|restent ouvertes|ne cloture pas'

# Extrait la valeur d'un champ « clef : valeur » du commentaire, insensible aux
# accents et a la casse. Rend une chaine vide si absent.
champ() {
  local corps="$1" motif="$2"
  printf '%s\n' "$corps" | grep -iEo "$motif" | head -1
}

# Analyse un commentaire de fermeture. Ecrit son rapport sur STDOUT.
#
# `depot` permet aux cas de preuve de travailler sur un faux depot ; en usage
# reel c'est le repertoire courant.
analyser() {
  local corps="$1" depot="${2:-.}"
  local problemes=0

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

  # ── Le commit invoque se refusait-il le droit de fermer ? ─────────────────
  #
  # C'est le controle le plus rentable : a lui seul il aurait arrete les
  # fermetures de #1654 et #1657.
  local sha
  sha=$(printf '%s\n' "$corps" | grep -oE '\b[0-9a-f]{7,40}\b' | head -1)
  if [ -n "$sha" ] && [ "$depot" != "-" ] && git -C "$depot" cat-file -e "$sha^{commit}" 2>/dev/null; then
    local message
    message=$(git -C "$depot" log -1 --format='%B' "$sha" 2>/dev/null)
    if printf '%s\n' "$message" | grep -qiE "$REFUS"; then
      printf 'COMMIT REFUSANT  %s dit lui-meme ne pas fermer :\n' "${sha:0:8}"
      printf '%s\n' "$message" | grep -iE "$REFUS" | head -2 | sed 's/^/                 > /'
      printf '                 une PR qui se refuse le droit de fermer sait quelque chose que le\n'
      printf '                 verificateur ignore.\n'
      problemes=$((problemes + 1))
    fi
  fi

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

  # ⚠️ Le cas qui compte le plus : sans depot, le controle du commit refusant
  # ne peut PAS s'exercer. Ce cas le dit, pour qu'on ne croie pas le
  # contre-epreuve plus large qu'elle n'est.
  printf '  note  le controle « commit refusant » exige un depot ; il est verifie en CI, pas ici.\n'

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
