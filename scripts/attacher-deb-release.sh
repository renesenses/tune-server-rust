#!/usr/bin/env bash
# Attacher les paquets Debian a une release : un fichier a la fois, les
# empreintes en DERNIER, et le verdict rendu par l'INVENTAIRE, pas par le code
# de sortie.
#
# ─── Ce qui n'allait pas ───────────────────────────────────────────────────
#
# `deb.yml` envoyait les trois fichiers d'un seul geste :
#
#     gh release upload "$TAG" dist/*.deb dist/SHA256SUMS.deb --repo … --clobber
#
# Or `dist/*.deb` couvre DEJA `dist/SHA256SUMS.deb` : l'etape precedente ecrit
# le fichier d'empreintes dans le meme dossier, et son nom finit par `.deb`.
# Le meme actif etait donc nomme DEUX FOIS dans un seul appel `--clobber`, qui
# supprime puis reteleverse chaque actif : le second passage portait sur un
# actif qui venait d'etre remplace. D'ou, run 33536592140, deux fois de suite,
# le meme message sur le meme fichier :
#
#     HTTP 404: Not Found
#     (…/releases/380652812/assets?label=&name=SHA256SUMS.deb)
#
# Et l'ordre alphabetique du glob mettait `SHA256SUMS.deb` EN PREMIER : la
# promesse partait avant ce qu'elle promet.
#
# ─── Ce que l'erreur laissait derriere elle ────────────────────────────────
#
# Le 404 arrivait APRES un envoi reussi — les empreintes verifiees a la main
# correspondaient aux paquets publies. Le danger n'etait donc pas l'erreur,
# c'etait son sillage : au premier passage, `tune-server_0.9.130_amd64.deb`
# MANQUAIT alors que `SHA256SUMS.deb`, lui, etait bien publie et l'annoncait.
# Un fichier d'empreintes qui promet un artefact absent. Quelqu'un sur PC
# aurait vu un `.deb` publie sans trouver le sien.
#
# ─── Les trois regles d'ici ────────────────────────────────────────────────
#
#   1. UN FICHIER PAR APPEL. Un echec ne peut plus laisser un lot a moitie
#      attache, et le fichier fautif est nomme.
#   2. LES EMPREINTES EN DERNIER, jamais avant les paquets qu'elles decrivent.
#      Au pire, un `.deb` sans ses empreintes — jamais des empreintes sans le
#      `.deb`. Le premier cas est inoffensif, le second ment.
#   3. L'INVENTAIRE TRANCHE. `gh release upload` peut rendre non-zero apres
#      avoir reussi (c'est le 404 ci-dessus) et zero sans que tout soit la. Le
#      seul fait qui compte est la liste des actifs de la release.
#
# S'eprouve seul : `bash scripts/attacher-deb-release.sh --autotest`.

set -euo pipefail

# Combien de fois retenter l'envoi d'UN fichier avant de le declarer perdu.
DEB_ESSAIS="${DEB_ESSAIS:-2}"

# Coutures : l'autotest y branche des commandes factices. En production, ce
# sont les deux valeurs par defaut ci-dessous qui servent.
_televerser() { # <tag> <fichier>
  if [ -n "${DEB_TELEVERSEMENT_CMD:-}" ]; then
    "$DEB_TELEVERSEMENT_CMD" "$1" "$2"
  else
    gh release upload "$1" "$2" --repo "$GITHUB_REPOSITORY" --clobber
  fi
}

_inventaire() { # <tag> -> un nom d'actif par ligne
  if [ -n "${DEB_INVENTAIRE_CMD:-}" ]; then
    "$DEB_INVENTAIRE_CMD" "$1"
  else
    gh release view "$1" --repo "$GITHUB_REPOSITORY" --json assets --jq '.assets[].name'
  fi
}

# ⚠️ Surtout PAS `_inventaire … | grep -q` : `grep -q` ferme le tube des la
# premiere correspondance, la lecture en amont prend un SIGPIPE, et `pipefail`
# transforme un actif PRESENT en absence. Meme piege que le `dpkg-deb
# --contents | grep -q` deja corrige dans deb.yml. On lit, puis on cherche.
_present() { # <tag> <nom>
  local liste
  liste="$(_inventaire "$1")" || return 1
  grep -qxF -- "$2" <<<"$liste"
}

# Pose UN fichier et ne rend la main que lorsque l'inventaire le confirme.
_poser() { # <tag> <fichier>
  local tag="$1" fichier="$2" nom essai=1 etat
  nom="$(basename "$fichier")"

  while :; do
    etat=0
    _televerser "$tag" "$fichier" || etat=$?

    # L'inventaire d'abord, le code de sortie ensuite : un 404 rendu apres un
    # envoi reussi ne doit pas faire echouer une release complete.
    if _present "$tag" "$nom"; then
      if [ "$etat" -ne 0 ]; then
        echo "note: $nom est bien attache alors que l'envoi a rendu $etat — on s'en tient a l'inventaire" >&2
      fi
      echo "attache: $nom"
      return 0
    fi

    if [ "$essai" -ge "$DEB_ESSAIS" ]; then
      echo "::error::$nom absent de la release $tag apres $essai tentative(s) (dernier code d'envoi: $etat)" >&2
      return 1
    fi
    echo "note: $nom encore absent (code $etat), tentative $((essai + 1))/$DEB_ESSAIS" >&2
    essai=$((essai + 1))
  done
}

# attacher_deb <tag> <dossier>
attacher_deb() {
  local tag="$1" dossier="$2"
  local sommes="$dossier/SHA256SUMS.deb"
  local paquets=() fichier nom

  # Les paquets, et EUX SEULS : `SHA256SUMS.deb` finit par `.deb` mais n'en
  # est pas un. C'est cette confusion qui le faisait nommer deux fois.
  for fichier in "$dossier"/*.deb; do
    [ -e "$fichier" ] || continue
    [ "$(basename "$fichier")" != SHA256SUMS.deb ] || continue
    paquets+=("$fichier")
  done

  # Un envoi qui ne trouve rien doit ECHOUER, pas se declarer vert : c'est
  # exactement la panne qu'on corrige — une release qui parait complete.
  if [ "${#paquets[@]}" -eq 0 ]; then
    echo "::error::aucun paquet .deb dans $dossier — il n'y a rien a attacher" >&2
    return 1
  fi
  if [ ! -f "$sommes" ]; then
    echo "::error::$sommes absent — les paquets ne partiront pas sans leurs empreintes" >&2
    return 1
  fi

  # ORDRE : les paquets, puis les empreintes. Jamais l'inverse.
  for fichier in "${paquets[@]}"; do
    _poser "$tag" "$fichier" || return 1
  done
  _poser "$tag" "$sommes" || return 1

  # Verdict final sur l'inventaire complet, relu une derniere fois.
  local publies attendus=() manquants=()
  publies="$(_inventaire "$tag")"
  for fichier in "${paquets[@]}" "$sommes"; do
    nom="$(basename "$fichier")"
    attendus+=("$nom")
    grep -qxF "$nom" <<<"$publies" || manquants+=("$nom")
  done
  if [ "${#manquants[@]}" -ne 0 ]; then
    echo "::error::la release $tag n'expose pas ${manquants[*]} — SHA256SUMS.deb promettrait un artefact absent" >&2
    return 1
  fi

  echo "release $tag : ${attendus[*]}"
  return 0
}

# --------------------------------------------------------------------------
# Contre-epreuves. Chacune imprime une ligne « ok: … » ; le nombre de lignes
# est compte par `tune-server/tests/workflows_bornes.rs`, pour qu'un autotest
# devenu muet ne puisse pas passer pour vert.
# --------------------------------------------------------------------------

_garanties=0
_ok() {
  _garanties=$((_garanties + 1))
  echo "ok: $1"
}
_echouer() {
  echo "ECHEC: $1" >&2
  exit 1
}

autotest() {
  local atelier
  atelier="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$atelier'" EXIT

  local dist="$atelier/dist"
  export _JOURNAL="$atelier/journal"        # un envoi par ligne : « <tag> <chemin> »
  export _PUBLIES="$atelier/publies"        # la release vue de l'exterieur
  export _REGLES="$atelier/regles"          # « <nom> <code> <attache_des_l_essai> »
  export _INV_COMPTEUR="$atelier/inv"       # nombre de lectures de l'inventaire
  export _OUBLI="" _OUBLI_DES=0             # actif qui disparait de l'inventaire

  export DEB_TELEVERSEMENT_CMD="$atelier/faux-upload"
  export DEB_INVENTAIRE_CMD="$atelier/faux-inventaire"

  # Faux `gh release upload` : journalise l'appel, rend le code demande, et
  # n'attache l'actif qu'a partir du numero d'essai demande (0 = jamais).
  cat > "$DEB_TELEVERSEMENT_CMD" <<'FIN'
#!/usr/bin/env bash
nom="$(basename "$2")"
printf '%s %s\n' "$1" "$2" >> "$_JOURNAL"
code=0
seuil=1
while read -r cible c s; do
  [ "$cible" = "$nom" ] || continue
  code="$c"
  seuil="$s"
done < "$_REGLES"
n="$(grep -c -- "/$nom\$" "$_JOURNAL" || true)"
if [ "$seuil" -ne 0 ] && [ "$n" -ge "$seuil" ]; then
  grep -qxF "$nom" "$_PUBLIES" || printf '%s\n' "$nom" >> "$_PUBLIES"
fi
exit "$code"
FIN

  # Faux `gh release view --json assets` : peut « oublier » un actif a partir
  # d'une certaine lecture, pour eprouver le verdict FINAL.
  cat > "$DEB_INVENTAIRE_CMD" <<'FIN'
#!/usr/bin/env bash
n=$(($(cat "$_INV_COMPTEUR") + 1))
printf '%s' "$n" > "$_INV_COMPTEUR"
if [ -n "${_OUBLI:-}" ] && [ "$n" -ge "${_OUBLI_DES:-1}" ]; then
  grep -vxF -- "$_OUBLI" "$_PUBLIES" || true
else
  cat "$_PUBLIES"
fi
FIN
  chmod +x "$DEB_TELEVERSEMENT_CMD" "$DEB_INVENTAIRE_CMD"

  local amd64=tune-server_0.9.130_amd64.deb
  local arm64=tune-server_0.9.130_arm64.deb

  _preparer() { # [regle...]
    rm -rf "$dist"
    mkdir -p "$dist"
    : > "$_JOURNAL"
    : > "$_PUBLIES"
    : > "$_REGLES"
    printf '0' > "$_INV_COMPTEUR"
    _OUBLI=""
    _OUBLI_DES=0
    printf 'paquet amd64\n' > "$dist/$amd64"
    printf 'paquet arm64\n' > "$dist/$arm64"
    printf 'empreintes\n' > "$dist/SHA256SUMS.deb"
    [ "$#" -eq 0 ] || printf '%s\n' "$@" > "$_REGLES"
  }
  _envois() { # <nom> -> nombre d'appels portant ce fichier
    grep -c -- "/$1\$" "$_JOURNAL" || true
  }

  local etat

  # 1. Nominal : un fichier par appel. C'est la correction de l'envoi en lot.
  _preparer
  attacher_deb v0.9.130 "$dist" >/dev/null && etat=0 || etat=$?
  [ "$etat" -eq 0 ] || _echouer "le cas nominal doit reussir (etat=$etat)"
  [ "$(wc -l < "$_JOURNAL")" -eq 3 ] || _echouer "$(wc -l < "$_JOURNAL") envois au lieu de 3"
  [ "$(awk '{print NF}' "$_JOURNAL" | sort -u | tr -d '\n')" = 2 ] \
    || _echouer "un envoi porte plusieurs fichiers a la fois"
  _ok "chaque fichier part dans son propre appel, jamais en lot"

  # 2. Le doublon qui provoquait le 404 n'existe plus.
  [ "$(_envois SHA256SUMS.deb)" -eq 1 ] \
    || _echouer "SHA256SUMS.deb envoye $(_envois SHA256SUMS.deb) fois — le doublon du glob est de retour"
  _ok "SHA256SUMS.deb n'est plus nomme deux fois (cause du HTTP 404 du run 33536592140)"

  # 3. L'ordre : les empreintes ferment la marche.
  [ "$(basename "$(tail -n 1 "$_JOURNAL" | awk '{print $2}')")" = SHA256SUMS.deb ] \
    || _echouer "les empreintes ne sont pas parties en dernier"
  _ok "les empreintes partent APRES les paquets qu'elles annoncent"

  # 4. Le cas mesure : l'envoi rend une erreur mais l'actif EST attache.
  _preparer "SHA256SUMS.deb 1 1"
  attacher_deb v0.9.130 "$dist" >/dev/null && etat=0 || etat=$?
  [ "$etat" -eq 0 ] || _echouer "un 404 apres un envoi reussi ne doit pas faire echouer (etat=$etat)"
  [ "$(_envois SHA256SUMS.deb)" -eq 1 ] || _echouer "un envoi deja abouti a ete refait inutilement"
  _ok "envoi en erreur mais actif present : l'inventaire tranche, succes"

  # 5. Le danger reel : tout rend 0, mais amd64 n'est pas la. C'est l'etat
  #    exact laisse par le premier passage de la v0.9.130.
  _preparer "$amd64 0 0"
  attacher_deb v0.9.130 "$dist" >/dev/null && etat=0 || etat=$?
  [ "$etat" -ne 0 ] || _echouer "un paquet manquant malgre un code 0 doit ECHOUER"
  _ok "code de sortie 0 mais paquet absent : echec (l'etat laisse par la v0.9.130)"

  # 6. Un paquet definitivement perdu : les empreintes ne partent PAS.
  _preparer "$arm64 1 0"
  attacher_deb v0.9.130 "$dist" >/dev/null && etat=0 || etat=$?
  [ "$etat" -ne 0 ] || _echouer "un paquet jamais attache doit faire echouer"
  [ "$(_envois SHA256SUMS.deb)" -eq 0 ] \
    || _echouer "SHA256SUMS.deb a ete envoye alors qu'un paquet manque"
  ! grep -qxF SHA256SUMS.deb "$_PUBLIES" \
    || _echouer "la release annonce des empreintes sans le paquet qu'elles decrivent"
  _ok "paquet perdu : les empreintes ne sont jamais posees, la release ne ment pas"

  # 7. Reprise : un actif encore invisible au premier essai est retente.
  _preparer "$arm64 0 2"
  attacher_deb v0.9.130 "$dist" >/dev/null && etat=0 || etat=$?
  [ "$etat" -eq 0 ] || _echouer "une absence passagere doit etre rattrapee (etat=$etat)"
  [ "$(_envois "$arm64")" -eq 2 ] || _echouer "$arm64 devait etre retente une fois exactement"
  _ok "actif encore invisible au premier essai : seconde tentative, puis succes"

  # 8. Le verdict FINAL porte sur l'inventaire complet : un actif qui
  #    disparait apres sa propre verification est encore rattrape.
  _preparer
  _OUBLI="$amd64"
  _OUBLI_DES=4          # les 3 verifications par fichier passent, la 4e tranche
  attacher_deb v0.9.130 "$dist" >/dev/null && etat=0 || etat=$?
  [ "$etat" -ne 0 ] || _echouer "l'inventaire final doit refuser une release incomplete"
  _OUBLI=""
  _OUBLI_DES=0
  _ok "inventaire final relu : une release amputee apres coup est refusee"

  # 9. Un dossier sans paquet ne doit pas passer pour un succes.
  _preparer
  rm -f "$dist/$amd64" "$dist/$arm64"
  attacher_deb v0.9.130 "$dist" >/dev/null && etat=0 || etat=$?
  [ "$etat" -ne 0 ] || _echouer "un dossier sans paquet doit echouer"
  [ "$(wc -l < "$_JOURNAL")" -eq 0 ] || _echouer "des empreintes seules ont ete envoyees"
  _ok "aucun paquet a attacher : echec, et rien n'est publie"

  # 10. Des paquets sans leur fichier d'empreintes : refus, rien ne part.
  _preparer
  rm -f "$dist/SHA256SUMS.deb"
  attacher_deb v0.9.130 "$dist" >/dev/null && etat=0 || etat=$?
  [ "$etat" -ne 0 ] || _echouer "des paquets sans SHA256SUMS.deb doivent echouer"
  [ "$(wc -l < "$_JOURNAL")" -eq 0 ] || _echouer "un paquet est parti sans que ses empreintes existent"
  _ok "SHA256SUMS.deb absent du dossier : rien ne part"

  echo "$_garanties garanties verifiees"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  case "${1:-}" in
    --autotest) autotest ;;
    "")
      echo "usage: $0 <tag> <dossier>   |   $0 --autotest" >&2
      exit 64
      ;;
    *)
      [ "$#" -eq 2 ] || { echo "usage: $0 <tag> <dossier>" >&2; exit 64; }
      attacher_deb "$1" "$2"
      ;;
  esac
fi
