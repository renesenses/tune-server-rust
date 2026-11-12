#!/usr/bin/env bash
# Contournement de la panne DNS de security.ubuntu.com sur les runners GitHub.
#
# 11/11/2026, vers 14 h 30 UTC : les runners Ubuntu (image « resolute ») ne
# resolvent plus security.ubuntu.com. Leurs sources passent par
# `mirror+file:/etc/apt/apt-mirrors.txt`, ou security.ubuntu.com sert de
# dernier repli : quand le miroir Azure ne repond pas, apt y tombe, echoue
# (« Temporary failure resolving 'security.ubuntu.com' ») et TOUTES les
# etapes apt de la CI rougissent (Clippy, FFI mobile, Test, Tests PostgreSQL).
#
# A appeler AVANT `apt-get update`, dans chaque etape apt des workflows :
#
#     bash scripts/ci/apt-robuste.sh
#
# DNS normal (security.ubuntu.com se resout) : le script ne touche a RIEN et
# rend 0 — le comportement habituel est inchange.
#
# DNS en panne : il retire security.ubuntu.com (et tout miroir qui ne se
# resout pas ou ne repond pas) de apt-mirrors.txt, redirige les sources qui
# le nomment directement vers archive.ubuntu.com (qui porte aussi la poche
# -security), et pose `Acquire::Retries "5"` pour les apt-get qui suivent.
# Idempotent : le rappeler a chaque essai d'une boucle de reprise est sans
# effet de bord. Il n'echoue jamais : au pire il laisse les sources telles
# quelles et l'etape apt dit elle-meme ce qui ne va pas.

set -u

HOTE_SECURITE="security.ubuntu.com"
REPLI="http://archive.ubuntu.com/ubuntu"

if [ "$(uname -s)" != "Linux" ] || ! command -v apt-get >/dev/null 2>&1; then
  exit 0
fi

if getent hosts "$HOTE_SECURITE" >/dev/null 2>&1; then
  exit 0
fi

echo "::warning::$HOTE_SECURITE ne se resout pas — apt passe par le miroir principal (scripts/ci/apt-robuste.sh)"

SUDO=""
if [ "$(id -u)" != "0" ]; then
  SUDO="sudo"
fi

# Nom de code de la distribution, pour sonder les miroirs.
CODE=""
if [ -r /etc/os-release ]; then
  CODE="$(. /etc/os-release && echo "${VERSION_CODENAME:-}")"
fi

hote_de() {
  # http://azure.archive.ubuntu.com/ubuntu/ -> azure.archive.ubuntu.com
  printf '%s\n' "$1" | sed -E 's#^[a-z+]+://##; s#[/:].*$##'
}

miroir_repond() {
  local url="$1" hote
  hote="$(hote_de "$url")"
  getent hosts "$hote" >/dev/null 2>&1 || return 1
  [ -n "$CODE" ] || return 0
  command -v curl >/dev/null 2>&1 || return 0
  curl -fsS -o /dev/null --max-time 8 "${url%/}/dists/$CODE/InRelease"
}

# 1. La liste de miroirs (format : « URL<TAB>priority:N », une par ligne).
LISTE=/etc/apt/apt-mirrors.txt
if [ -f "$LISTE" ]; then
  echo "--- $LISTE avant :"
  cat "$LISTE"
  gardes="$(mktemp)"
  resolus="$(mktemp)"
  while IFS= read -r ligne || [ -n "$ligne" ]; do
    url="${ligne%%[[:space:]]*}"
    [ -n "$url" ] || continue
    hote="$(hote_de "$url")"
    [ "$hote" = "$HOTE_SECURITE" ] && continue
    getent hosts "$hote" >/dev/null 2>&1 || continue
    printf '%s\n' "$ligne" >> "$resolus"
    if miroir_repond "$url"; then
      printf '%s\n' "$ligne" >> "$gardes"
    else
      echo "::warning::miroir $url ne repond pas — ecarte"
    fi
  done < "$LISTE"
  # Aucun miroir ne repond : on garde ceux qui se resolvent ; aucun ne se
  # resout : le repli archive.ubuntu.com (amd64 ; arm64 passe par ports).
  if [ ! -s "$gardes" ]; then
    cp "$resolus" "$gardes"
  fi
  if [ ! -s "$gardes" ] && [ "$(dpkg --print-architecture 2>/dev/null)" = "amd64" ]; then
    printf '%s/\tpriority:1\n' "$REPLI" >> "$gardes"
  fi
  if [ -s "$gardes" ]; then
    $SUDO cp "$gardes" "$LISTE"
    $SUDO chmod 644 "$LISTE"
  fi
  rm -f "$gardes" "$resolus"
  echo "--- $LISTE apres :"
  cat "$LISTE"
fi

# 2. Les sources qui nomment security.ubuntu.com en direct (sources.list
#    classique ou fichiers deb822 *.sources).
for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.list /etc/apt/sources.list.d/*.sources; do
  [ -f "$f" ] || continue
  if grep -q "$HOTE_SECURITE" "$f"; then
    echo "source redirigee vers $REPLI : $f"
    $SUDO sed -i -E "s#https?://$HOTE_SECURITE/ubuntu/?#$REPLI/#g" "$f"
  fi
done

# 3. Reprises reseau pour les apt-get qui suivent.
echo 'Acquire::Retries "5";' | $SUDO tee /etc/apt/apt.conf.d/80-tune-apt-robuste >/dev/null

# Les index deja telecharges depuis security peuvent pointer vers des .deb
# injoignables : on les jette, l'update qui suit les reprend du miroir.
$SUDO rm -rf /var/lib/apt/lists/*

exit 0
