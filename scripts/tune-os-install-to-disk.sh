#!/bin/bash
# Tune OS — installation sur un disque interne (façon ROON ROCK).
#
# À lancer SUR la box Tune OS démarrée depuis la clé USB :
#   curl -sL https://raw.githubusercontent.com/renesenses/tune-server-rust/main/scripts/tune-os-install-to-disk.sh | sudo bash
#
# Le script détecte automatiquement le disque interne (proposé par défaut
# s'il n'y a qu'un candidat), télécharge l'image Tune OS officielle la plus
# récente et l'écrit sur le disque choisi. ⚠️ Le disque cible est EFFACÉ.
# La clé USB (système en cours) ne peut pas être choisie comme cible.
# Installation fraîche : WiFi et dossiers musique seront à reconfigurer.
set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; BLUE='\033[0;34m'; NC='\033[0m'
say() { echo -e "${BLUE}[tune-os]${NC} $*"; }
ok()  { echo -e "${GREEN}[  OK  ]${NC} $*"; }
err() { echo -e "${RED}[ERREUR]${NC} $*" >&2; }

[ "$EUID" -eq 0 ] || { err "Lance-moi avec sudo."; exit 1; }

# Le disque qui porte le système en cours (la clé) — jamais une cible valide.
# lsblk -s remonte la chaîne des ancêtres jusqu'au disque physique : marche
# pour une partition simple (sdb2→sdb) comme pour du LVM (lv→sda3→sda).
BOOT_PART=$(findmnt -n -o SOURCE /)
BOOT_DISK_NAME=$(lsblk -srno NAME "$BOOT_PART" 2>/dev/null | tail -1)
[ -n "$BOOT_DISK_NAME" ] || { err "Disque système introuvable."; exit 1; }

say "Système actuel sur : /dev/${BOOT_DISK_NAME} (la clé USB — elle ne sera pas touchée)"
echo ""
say "Disques candidats pour l'installation :"
CANDIDATES=()
INTERNAL=()
# lsblk -P : paires NAME=".." TRAN=".." — robuste aux modèles avec espaces
# et aux transports vides (virtio en VM).
while IFS= read -r line; do
    NAME=""; TRAN=""; SIZE=""; MODEL=""; TYPE=""
    eval "$line"
    [ "$TYPE" = "disk" ] || continue
    [ "$NAME" = "$BOOT_DISK_NAME" ] && continue
    CANDIDATES+=("$NAME")
    if [ "$TRAN" = "usb" ]; then
        printf '    %-12s %-8s %s  (USB — probablement un disque de musique !)\n' "$NAME" "$SIZE" "$MODEL"
    else
        INTERNAL+=("$NAME")
        printf '    %-12s %-8s %s  (interne)\n' "$NAME" "$SIZE" "$MODEL"
    fi
done < <(lsblk -dP -o NAME,TRAN,SIZE,MODEL,TYPE)

[ "${#CANDIDATES[@]}" -gt 0 ] || { err "Aucun autre disque détecté."; exit 1; }

# Recherche auto : un seul disque INTERNE → proposé par défaut (Entrée pour
# accepter). Les disques USB (musique !) ne sont jamais proposés par défaut.
DEFAULT=""
if [ "${#INTERNAL[@]}" -eq 1 ]; then
    DEFAULT="${INTERNAL[0]}"
    echo ""
    say "Disque interne détecté automatiquement : /dev/${DEFAULT}"
fi

echo ""
read -rp "Disque CIBLE à EFFACER [${DEFAULT:-ex: sda, nvme0n1}] : " TARGET </dev/tty
TARGET="${TARGET:-$DEFAULT}"
TARGET="${TARGET#/dev/}"
TARGET_DEV="/dev/${TARGET}"

[ -n "$TARGET" ] && [ -b "$TARGET_DEV" ] || { err "Disque ${TARGET_DEV} introuvable."; exit 1; }
[ "$TARGET" != "$BOOT_DISK_NAME" ] || { err "C'est la clé USB en cours d'utilisation !"; exit 1; }
lsblk -dno TYPE "$TARGET_DEV" | grep -qx disk || { err "${TARGET_DEV} n'est pas un disque."; exit 1; }

SIZE_BYTES=$(lsblk -bdno SIZE "$TARGET_DEV")
[ "$SIZE_BYTES" -ge 4000000000 ] || { err "Disque trop petit (minimum 4 Go)."; exit 1; }

echo ""
echo -e "${RED}⚠️  TOUT le contenu de ${TARGET_DEV} ($(lsblk -dno SIZE "$TARGET_DEV" | tr -d ' ')) va être DÉFINITIVEMENT EFFACÉ.${NC}"
read -rp "Tape EFFACER (en majuscules) pour confirmer : " CONFIRM </dev/tty
[ "$CONFIRM" = "EFFACER" ] || { say "Annulé — rien n'a été modifié."; exit 0; }

# Dernière version publiée.
say "Recherche de la dernière version de Tune OS…"
VERSION=$(curl -sL "https://api.github.com/repos/renesenses/tune-server-rust/releases/latest" \
    | grep '"tag_name"' | head -1 | sed 's/.*"v\(.*\)".*/\1/')
[ -n "$VERSION" ] || { err "Impossible de déterminer la dernière version."; exit 1; }
IMG_URL="https://github.com/renesenses/tune-server-rust/releases/download/v${VERSION}/tune-os-x86_64-v${VERSION}.img.gz"
say "Image : Tune OS v${VERSION}"

# On fige le système pendant l'écriture.
systemctl stop tune 2>/dev/null || true

say "Téléchargement et écriture sur ${TARGET_DEV} (plusieurs minutes)…"
curl -sL "$IMG_URL" | gunzip | dd of="$TARGET_DEV" bs=4M conv=fsync status=progress
sync
ok "Écriture terminée et synchronisée."

echo ""
ok "Tune OS v${VERSION} est installé sur ${TARGET_DEV}."
echo ""
echo "  Dernières étapes :"
echo "  1. sudo poweroff"
echo "  2. Retire la clé USB"
echo "  3. Rallume la machine : Tune OS démarre depuis le disque interne"
echo "     (la partition s'agrandira automatiquement au premier démarrage)"
echo "  4. Reconfigure le WiFi et tes dossiers musique (installation fraîche)"
echo ""