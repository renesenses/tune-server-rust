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
# Variables d'environnement :
#   FORUM_TOKEN             jeton bearer de l'API forum (obligatoire)
#   GITHUB_REPOSITORY       owner/repo (obligatoire)
#   GH_TOKEN                jeton pour `gh` (obligatoire en CI)
#   API_FORUM               URL de la liste des fils (defaut : mozaiklabs.fr)
#   DELAI_DE_GRACE_MINUTES  age minimal d'une version avant de la signaler (90)
#   FENETRE_HEURES          profondeur d'examen en arriere (72)
#   MAINTENANT_ISO          instant de reference UTC — pour les tests uniquement
#   SANS_ISSUE              a 1, n'ouvre aucune issue : diagnostic seul
#
# Sortie : 0 si tout est annonce (ou si le forum est injoignable), 1 si au moins
# une version publiee n'a pas de fil.

set -u

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
# `--limit 40` couvre tres largement la fenetre de 72 h : la cadence la plus
# dense observee est de quatre versions par jour.
if ! gh release list --repo "$GITHUB_REPOSITORY" --limit 40 \
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
for r in releases:
    if r.get("isDraft") or r.get("isPrerelease"):
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
    if not annoncee(r["tagName"]):
        heures = age.total_seconds() / 3600.0
        manquantes.append((r["tagName"], publiee_le, heures))

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

if [ -z "$MANQUANTES" ]; then
  echo "OK — aucune version publiee sans note de version."
  exit 0
fi

echo "Versions publiees sans fil de notes :"
echo "$MANQUANTES"

# --- 4. Le cri ----------------------------------------------------------------
{
  printf 'Detecte par `notes-de-version-watch` le %s.\n\n' \
    "$(date -u '+%Y-%m-%d a %H:%M UTC')"
  printf 'Ces versions sont **publiees sur GitHub** et **sans fil de notes sur le forum** :\n\n'
  printf '| Version | Publiee le | Depuis |\n|---|---|---|\n'
  printf '%s\n' "$MANQUANTES" | while IFS=$'\t' read -r tag quand heures; do
    printf '| `%s` | %s | %s h |\n' "$tag" "$quand" "$heures"
  done
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
