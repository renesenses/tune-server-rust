#!/usr/bin/env bash
#
# Garde-fou : une version publiee sans note de version sur le forum doit se
# voir.
#
# Pourquoi ce script existe (#2328). Le job `forum` de `release.yml` — « Announce
# on mozaiklabs forum » — est eteint par `if: false` depuis le 03/06/2026
# (496fc446, « manual posts have better formatting »). La publication des notes
# est donc MANUELLE par conception, et ce choix se defend : les notes du forum
# sont ecrites pour des testeurs, pas generees par git-cliff.
#
# Ce qui ne se defend pas, c'est qu'un geste manuel n'ait aucun filet. Rien ne
# signale qu'une version est sortie sans annonce : le job saute, l'interface est
# verte, et le premier a s'en apercevoir est un testeur. Le 22-24/08/2026, quatre
# versions sont sorties en 34 heures pour deux fils de notes, tous deux ecrits
# apres coup — dont un vingt minutes apres la plainte d'un testeur, qui a annonce
# dans la foulee qu'il n'utiliserait plus la mise a jour integree.
#
# Ce script ne poste RIEN sur le forum. Il constate, et il crie. Reactiver le job
# `forum` est un autre sujet, instruit dans #2328 : en l'etat il posterait le
# changelog brut de git-cliff, qui contient un lien vers le depot PRIVE par
# commit (`cliff.toml`), et l'API forum a change depuis (un fil cree par l'API
# nait `moderation_status = 'pending'`, donc invisible, pendant que le POST
# repond 200 — un job vert pour un fil que personne ne voit).
#
# Perimetre : les versions de TUNE, `vX.Y.Z` (#4461). Les releases du
# moissonneur Roon, `moissonneur-vX.Y.Z`, sont ecartees — le motif et le
# pourquoi sont dans le rapprochement, section 3.
#
# Variables d'environnement :
#   FORUM_TOKEN             jeton bearer de l'API forum (obligatoire)
#   GITHUB_REPOSITORY       owner/repo (obligatoire)
#   GH_TOKEN                jeton pour `gh` (obligatoire en CI)
#   API_FORUM               URL de la liste des fils (defaut : mozaiklabs.fr)
#   DELAI_DE_GRACE_MINUTES  age minimal d'une version avant de la signaler (90)
#   FENETRE_HEURES          profondeur d'examen en arriere (72)
#   MAINTENANT_ISO          instant de reference UTC — pour les tests uniquement
#   SANS_ISSUE              a 1, n'ouvre aucune issue : diagnostic seul
#   ITEMS_MINIMUM           items attendus d'une note, forme A (5)
#   FORME_DEPUIS            date a partir de laquelle la forme A est exigee
#                           (2026-09-23 — decision de Bertrand ; les notes
#                           publiees avant ne sont PAS reecrites)
#   FORME_COMBIEN           nombre de notes recentes examinees au plus (5)
#
# Sortie : 0 si tout est annonce (ou si le forum est injoignable), 1 si au moins
# une version publiee n'a pas de fil, ou rend une note illisible par le panneau.
#
# ─────────────────────────────────────────────────────────────────────────────
# Le second controle : la FORME de la note (#4190)
# ─────────────────────────────────────────────────────────────────────────────
#
# Un fil existe ne veut pas dire que le testeur voit quelque chose. Le
# 23/09/2026, le panneau de mise a jour de Tune rendait VINGT entrees sur vingt
# reduites a « Release 0.9.x », sans un seul item — alors que chaque version
# avait son fil, et que cette sonde etait verte.
#
# La cause n'est pas un defaut de code : `parse_release_body` ne retient que
# les PUCES placees sous un titre reconnu (Nouveautes / Ameliorations /
# Corrections), et nos notes sont de la prose sous des titres thematiques
# (« L'egaliseur », « Le tableau de bord »). La regle etait deja ecrite dans
# `docs/RELEASE-WORKFLOW.md` §5 — elle n'etait tenue par rien.
#
# Ce controle la tient. Il ne juge ni le style ni la longueur : il rejoue le
# decoupage du serveur (`.github/scripts/forme-des-notes.awk`, copie conforme
# gardee de `parse_release_body`) et compte ce que le panneau AFFICHERA.
#
# Deux bornes volontaires :
#
#   - il ne regarde que les versions publiees a partir de `FORME_DEPUIS`. Les
#     notes deja publiees ne sont pas reecrites : la forme s'applique aux
#     prochaines, et un controle retroactif ne ferait que crier sur ce qu'on
#     s'est interdit de changer ;
#   - le decompte porte sur le bloc FRANCAIS. Les puces d'un bloc traduit ne
#     doivent pas masquer un francais reste en prose.
#
# Il s'utilise aussi AVANT publication, hors reseau, sur le fichier de notes :
#
#   .github/scripts/notes-de-version-watch.sh --forme notes-v0.9.163.md
#
# C'est la le vrai usage : constater apres coup vaut mieux que rien, refuser
# avant de publier vaut mieux que constater.

set -u

ICI="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LECTEUR_DE_FORME="$ICI/forme-des-notes.awk"
ITEMS_MINIMUM="${ITEMS_MINIMUM:-5}"
FORME_DEPUIS="${FORME_DEPUIS:-2026-09-23}"

# `verifier_la_forme <etiquette> <fichier>` — 0 si la note rend assez d'items,
# 1 sinon. Ecrit son verdict sur la sortie standard, dans les deux cas : un
# controle qui ne dit rien quand il passe n'apprend a personne ce qu'il mesure.
verifier_la_forme() {
  local etiquette="$1" fichier="$2"
  local sortie f a c total langues
  if ! sortie=$(awk -f "$LECTEUR_DE_FORME" < "$fichier" 2>&1); then
    printf '%s : lecture impossible — %s\n' "$etiquette" "$sortie"
    return 1
  fi
  f=$(printf '%s\n' "$sortie" | awk -F'\t' '$1=="features"{print $2}')
  a=$(printf '%s\n' "$sortie" | awk -F'\t' '$1=="improvements"{print $2}')
  c=$(printf '%s\n' "$sortie" | awk -F'\t' '$1=="fixes"{print $2}')
  langues=$(printf '%s\n' "$sortie" | awk -F'\t' '$1=="langues"{print $2}')
  total=$(( f + a + c ))

  # Le marqueur de langue n'est PAS une condition de succes : une note
  # francaise seule est servie a tout le monde, en le disant (`fallback:true`).
  # Mais l'absence se dit, sinon personne ne se souvient que neuf langues
  # replient.
  if [ "$langues" = "fr" ]; then
    printf '%s : aucun marqueur <!-- lang:xx --> — les neuf autres langues recevront le francais, avec fallback:true (docs/RELEASE-WORKFLOW.md §5).\n' \
      "$etiquette"
  fi

  if [ "$total" -lt "$ITEMS_MINIMUM" ]; then
    printf '%s : %d item(s) pour le panneau (%d nouveautes, %d ameliorations, %d corrections)' \
      "$etiquette" "$total" "$f" "$a" "$c"
    printf ' — il en faut au moins %d.\n' "$ITEMS_MINIMUM"
    return 1
  fi
  printf '%s : %d items (%d nouveautes, %d ameliorations, %d corrections) — langues : %s\n' \
    "$etiquette" "$total" "$f" "$a" "$c" "$langues"
  return 0
}

# Mode hors ligne : `--forme FICHIER…`. Aucun reseau, aucun jeton, aucune
# issue — c'est le contrat qui le rend utilisable dans un pre-commit ou a la
# main juste avant `gh release edit --notes-file`.
if [ "${1:-}" = "--forme" ]; then
  shift
  if [ "$#" -eq 0 ]; then
    echo "usage : $0 --forme FICHIER..." >&2
    exit 2
  fi
  etat_forme=0
  for fichier in "$@"; do
    if [ ! -r "$fichier" ]; then
      echo "fichier illisible : $fichier" >&2
      etat_forme=1
      continue
    fi
    verifier_la_forme "$fichier" "$fichier" || etat_forme=1
  done
  if [ "$etat_forme" -ne 0 ]; then
    cat <<'AIDE'

Forme attendue (docs/RELEASE-WORKFLOW.md §5, arbitrage du 23/09/2026) : la note
OUVRE par les trois rubriques, 5 a 8 puces d'une ligne chacune, sans numero
d'issue — le depot est prive, ces renvois ne menent nulle part pour un testeur.
La prose suit sous « ## Le detail », et n'est jamais lue par le panneau.

    ## Nouveautes
    - …
    ## Ameliorations
    - …
    ## Corrections
    - …

    ## Le detail
    …
AIDE
  fi
  exit "$etat_forme"
fi

# `per_page=100` n'est pas un detail de confort. Sans lui, l'API rend 50 fils —
# dont sept epingles qui remontent a mai — et la page ne redescend qu'a trois
# jours en arriere. Le fil 1533, qui annonce les v0.9.98/99/101, en tombe : la
# sonde accuserait trois versions parfaitement annoncees. Verifie le 27/08/2026.
API_FORUM="${API_FORUM:-https://mozaiklabs.fr/api/v1/forum/threads?per_page=100}"
DELAI_DE_GRACE_MINUTES="${DELAI_DE_GRACE_MINUTES:-90}"
FENETRE_HEURES="${FENETRE_HEURES:-72}"

TRAVAIL="$(mktemp -d)"
trap 'rm -rf "$TRAVAIL"' EXIT

RELEASES="$TRAVAIL/releases.json"
FILS="$TRAVAIL/fils.json"
CORPS="$TRAVAIL/corps.md"

# --- 1. Les versions publiees -------------------------------------------------
#
# La cadence la plus dense observee est de quatre versions de Tune par jour,
# soit douze sur la fenetre de 72 h. Mais depuis le 17/09/2026 le depot publie
# une SECONDE famille de releases — `moissonneur-v0.9.x`, une par tag de Tune —
# qui occupe la liste sans etre examinee (voir le filtre plus bas). `--limit 60`
# garde la meme marge qu'avant pour les versions de Tune une fois l'autre
# famille deduite.
if ! gh release list --repo "$GITHUB_REPOSITORY" --limit 60 \
       --json tagName,isDraft,isPrerelease,publishedAt > "$RELEASES"; then
  echo "::error::impossible de lister les releases GitHub"
  exit 1
fi

# --- 2. Les fils du forum -----------------------------------------------------
#
# Sans filtre `?type=release` : ce parametre fait repondre 302 vers la page
# d'accueil (verifie le 27/08/2026). Le tri par type se fait ici.
#
# Un forum injoignable n'est PAS une version non annoncee. C'est une panne, et
# elle a deja sa sonde (`uptime-watch.yml`). Accuser ici produirait une fausse
# alerte a chaque incident reseau.
CODE=$(curl -s -o "$FILS" -w '%{http_code}' -m 30 \
         -H "Authorization: Bearer ${FORUM_TOKEN:-}" "$API_FORUM" || echo 000)
if [ "$CODE" != "200" ]; then
  echo "::warning::API forum injoignable (HTTP $CODE) — aucune conclusion tiree." \
       "La disponibilite du site est surveillee par uptime-watch."
  exit 0
fi

# --- 3. Le rapprochement ------------------------------------------------------
#
# En python3 plutot qu'en bash : la comparaison de versions demande des bornes
# de mot, et un `grep` naif ferait passer la v0.9.10 pour annoncee des qu'un fil
# parle de la v0.9.101.
MANQUANTES=$(
  MANQ_RELEASES="$RELEASES" MANQ_FILS="$FILS" \
  MANQ_GRACE="$DELAI_DE_GRACE_MINUTES" MANQ_FENETRE="$FENETRE_HEURES" \
  python3 <<'PY'
import datetime as dt
import json
import os
import re
import sys

maintenant = os.environ.get("MAINTENANT_ISO", "")
if maintenant:
    reference = dt.datetime.fromisoformat(maintenant.replace("Z", "+00:00"))
else:
    reference = dt.datetime.now(dt.timezone.utc)

grace = dt.timedelta(minutes=float(os.environ["MANQ_GRACE"]))
fenetre = dt.timedelta(hours=float(os.environ["MANQ_FENETRE"]))

with open(os.environ["MANQ_RELEASES"], encoding="utf-8") as f:
    releases = json.load(f)
with open(os.environ["MANQ_FILS"], encoding="utf-8") as f:
    fils = json.load(f)

tous = fils.get("threads", [])
titres = [t.get("title") or "" for t in tous if t.get("type") == "release"]

# Symetrique du filtre pose sur les tags (#4461) : un fil du MOISSONNEUR
# n'annonce pas une version de Tune, et doit etre retire d'ici avant tout
# rapprochement.
#
# Sans ce retrait, le fil groupe « Moissonneur Roon v0.9.155 a v0.9.159 — Notes
# de version » vaudrait annonce pour les versions v0.9.155 ET v0.9.159 DU
# SERVEUR : il contient « 0.9.155 » et « 0.9.159 », precedes d'un « v » qui
# n'est ni un chiffre ni un point, donc avec les bornes que `annoncee()`
# exige. La sonde se tairait sur deux versions de Tune reellement non
# annoncees — un vert qui ne garde rien, et le pire des deux erreurs
# possibles ici.
titres = [t for t in titres if not re.search(r"moissonneur", t, re.IGNORECASE)]

# Jusqu'ou cette page voit-elle ?
#
# L'API rend une page, pas l'histoire. Au-dela de son fil non epingle le plus
# ancien, l'absence d'annonce ne prouve rien : elle peut n'etre que l'absence
# de la page. Les fils EPINGLES sont exclus du calcul — ils remontent a mai et
# donneraient une couverture imaginaire de trois mois.
dates = [
    dt.datetime.fromisoformat(t["created_at"])
    for t in tous
    if t.get("created_at") and not t.get("is_pinned")
]
plancher = min(dates).astimezone(dt.timezone.utc) if dates else None
if plancher is not None and plancher > reference - fenetre:
    sys.stderr.write(
        f"::warning::la page de fils ne redescend qu'au {plancher.isoformat()} ; "
        f"les versions publiees avant ne sont pas examinees.\n"
    )


# Quelles releases cette sonde surveille-t-elle ? (#4461)
#
# Le depot publie DEUX familles de releases sous le meme toit :
#
#   `v0.9.x`             Tune, le serveur. C'est lui qui se met a jour tout
#                        seul chez le testeur, et c'est pour lui que les notes
#                        de version existent : savoir ce qu'on installe.
#
#   `moissonneur-v0.9.x` le moissonneur Roon, un outil en ligne de commande que
#                        le testeur telecharge a la main. Publie a part depuis
#                        le 17/09/2026 (3e0af030) PARCE QUE ses archives, posees
#                        sur la release de Tune, etaient prises pour l'archive
#                        du serveur par la mise a jour automatique.
#
# Seule la premiere famille a des notes de version, et ce n'est pas un oubli.
# Le moissonneur n'a pas de journal des changements : son propre numero est
# 0.1.0, le numero du tag est emprunte au tag de Tune qui a declenche sa
# construction, et son code n'a pas bouge entre `moissonneur-v0.9.155` et
# `moissonneur-v0.9.159` (`git diff v0.9.155 v0.9.159 -- tools/` : vide).
# Exiger un fil par tag reviendrait a demander cinq annonces pour zero
# changement — et a faire figurer le moissonneur dans la liste des « versions
# de Tune », la confusion meme que la separation des releases a supprimee.
#
# Le filtre est POSITIF : on nomme ce qu'on surveille, on ne liste pas ce qu'on
# ecarte. Une troisieme famille de tags apparaitra un jour ; elle sera ignoree,
# mais pas en silence — les tags ecartes sont dits sur la sortie d'erreur, pour
# qu'un nouveau venu se remarque au lieu de disparaitre.
TAG_DE_TUNE = re.compile(r"^v[0-9]+\.[0-9]+\.[0-9]+$")


def annoncee(tag):
    """Un fil parle-t-il de CETTE version, et pas d'une voisine ?

    Les bornes sont indispensables : « Tune v0.9.101 — Notes de version »
    ne doit pas passer pour l'annonce de la v0.9.10. Elles doivent en
    revanche laisser passer les fils groupes, qui sont la regle des jours
    denses : « Tune v0.9.103 et v0.9.104 — Notes de version ».
    """
    numero = tag[1:] if tag.startswith("v") else tag
    motif = re.compile(r"(?<![0-9.])" + re.escape(numero) + r"(?![0-9.])")
    return any(motif.search(titre) for titre in titres)


manquantes = []
ecartees = []
for r in releases:
    if r.get("isDraft") or r.get("isPrerelease"):
        continue
    tag = r.get("tagName") or ""
    if not TAG_DE_TUNE.match(tag):
        ecartees.append(tag)
        continue
    publiee_le = r.get("publishedAt") or ""
    if not publiee_le or publiee_le.startswith("0001-"):
        continue
    quand = dt.datetime.fromisoformat(publiee_le.replace("Z", "+00:00"))
    age = reference - quand
    if age > fenetre:
        continue
    # Hors de ce que la page couvre : on se tait. Une absence n'est une preuve
    # que si on a regarde.
    if plancher is not None and quand < plancher:
        continue
    # Le delai de grace est ce qui distingue « pas encore ecrit » de « oublie ».
    # Les huit dernieres versions ont ete annoncees en moins de 22 minutes ; la
    # plainte du testeur portait sur plus de huit heures.
    if age < grace:
        continue
    if not annoncee(tag):
        heures = age.total_seconds() / 3600.0
        manquantes.append((tag, publiee_le, heures))

if ecartees:
    sys.stderr.write(
        "Releases ecartees — pas des versions de Tune, pas de notes attendues : "
        + ", ".join(sorted(ecartees))
        + "\n"
    )

for tag, publiee_le, heures in manquantes:
    print(f"{tag}\t{publiee_le}\t{heures:.1f}")

if not manquantes:
    sys.stderr.write("Toutes les versions publiees de la fenetre ont leur fil.\n")
PY
)
ETAT_PYTHON=$?

if [ "$ETAT_PYTHON" -ne 0 ]; then
  echo "::error::le rapprochement versions/fils a echoue"
  exit 1
fi

# --- 3 bis. La forme des notes (#4190) ----------------------------------------
#
# Un fil existe ne prouve pas qu'on y lit quelque chose. On rejoue ici, sur le
# corps de chaque release RECENTE de Tune, le decoupage exact du serveur, et on
# compte les items que le panneau affichera.
#
# `FORME_DEPUIS` borne le controle aux notes ecrites sous la regle : les notes
# anterieures ne sont pas reecrites (decision du 23/09/2026), donc crier
# dessus n'apprendrait rien et rendrait la sonde rouge a perpetuite.
FORME_KO=""
CANDIDATES=$(jq -r --arg depuis "$FORME_DEPUIS" '
  .[]
  | select((.isDraft | not) and (.isPrerelease | not))
  | select(.tagName | test("^v[0-9]+\\.[0-9]+\\.[0-9]+$"))
  | select((.publishedAt // "") >= $depuis)
  | .tagName
' "$RELEASES" 2>/dev/null | head -n "${FORME_COMBIEN:-5}")

if [ -n "$CANDIDATES" ]; then
  echo "Forme des notes (items que le panneau affichera) :"
  while IFS= read -r tag; do
    [ -n "$tag" ] || continue
    if ! gh release view "$tag" --repo "$GITHUB_REPOSITORY" --json body -q .body \
           > "$TRAVAIL/corps-$tag.md" 2>/dev/null; then
      echo "  $tag : corps illisible — non juge."
      continue
    fi
    if ! verifier_la_forme "  $tag" "$TRAVAIL/corps-$tag.md"; then
      FORME_KO="${FORME_KO}${tag}"$'\n'
    fi
  done <<< "$CANDIDATES"
fi
FORME_KO=$(printf '%s' "$FORME_KO" | sed '/^$/d')

if [ -z "$MANQUANTES" ] && [ -z "$FORME_KO" ]; then
  echo "OK — aucune version publiee sans note de version."
  exit 0
fi

[ -n "$MANQUANTES" ] && { echo "Versions publiees sans fil de notes :"; echo "$MANQUANTES"; }
[ -n "$FORME_KO" ] && { echo "Notes illisibles par le panneau :"; echo "$FORME_KO"; }

# --- 4. Le cri ----------------------------------------------------------------
{
  printf 'Detecte par `notes-de-version-watch` le %s.\n\n' \
    "$(date -u '+%Y-%m-%d a %H:%M UTC')"
  if [ -n "$MANQUANTES" ]; then
    printf 'Ces versions sont **publiees sur GitHub** et **sans fil de notes sur le forum** :\n\n'
    printf '| Version | Publiee le | Depuis |\n|---|---|---|\n'
    printf '%s\n' "$MANQUANTES" | while IFS=$'\t' read -r tag quand heures; do
      printf '| `%s` | %s | %s h |\n' "$tag" "$quand" "$heures"
    done
    printf '\n'
  fi
  if [ -n "$FORME_KO" ]; then
    printf 'Ces notes existent mais le **panneau de mise a jour n en tire rien** :\n'
    printf 'moins de %s items apres le decoupage de `parse_release_body`, donc\n' "$ITEMS_MINIMUM"
    printf 'une entree reduite a « Release x.y.z » chez le testeur (#4190).\n\n'
    printf '%s\n' "$FORME_KO" | while IFS= read -r tag; do
      [ -n "$tag" ] && printf -- '- `%s`\n' "$tag"
    done
    printf '\nLa note doit OUVRIR par `## Nouveautes`, `## Ameliorations` et\n'
    printf '`## Corrections`, 5 a 8 puces d une ligne chacune, sans numero d issue.\n'
    printf 'La prose suit sous `## Le detail` — voir `docs/RELEASE-WORKFLOW.md` §5.\n'
    printf 'Le corps se corrige sans retaguer : `gh release edit <tag> --notes-file`.\n\n'
    printf 'Se verifier avant publication, hors reseau :\n'
    printf '`.github/scripts/notes-de-version-watch.sh --forme notes.md`\n\n'
  fi
  printf '\n## Quoi faire\n\n'
  printf 'Ecrire le fil, a la main, comme d habitude : `type=release`, titre\n'
  printf '`Tune <version> — Notes de version`, `user_id=18` (Bertrand — le compte 1\n'
  printf 's affiche « Admin »), puis `moderation_status = approved`, sans quoi\n'
  printf 'PERSONNE ne voit le fil. Un fil groupe couvrant plusieurs versions\n'
  printf 'convient : cette sonde le reconnait.\n\n'
  printf 'Ce n est pas une panne de CI. Le job `Announce on mozaiklabs forum` de\n'
  printf '`release.yml` est eteint **volontairement** depuis le 03/06/2026 — voir\n'
  printf '#2328 avant d envisager de le rallumer.\n\n'
  printf 'Cette issue se referme a la main une fois les notes publiees.\n'
} > "$CORPS"

if [ "${SANS_ISSUE:-0}" = "1" ]; then
  cat "$CORPS"
  exit 1
fi

# Une seule issue ouverte a la fois : la sonde tourne toutes les heures, une
# lacune qui dure ne doit pas en produire une par heure.
EXISTANTE=$(gh issue list --repo "$GITHUB_REPOSITORY" --state open \
              --search 'in:title Version publiee sans note de version' \
              --json number -q '.[0].number' 2>/dev/null || echo "")
if [ -n "$EXISTANTE" ]; then
  gh issue comment "$EXISTANTE" --repo "$GITHUB_REPOSITORY" --body-file "$CORPS"
  echo "issue #$EXISTANTE mise a jour"
else
  gh issue create --repo "$GITHUB_REPOSITORY" \
    --title "🔴 Version publiee sans note de version sur le forum" \
    --body-file "$CORPS"
  echo "issue creee"
fi

exit 1
