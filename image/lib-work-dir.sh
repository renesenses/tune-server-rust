#!/usr/bin/env bash
# #4770 — le dossier de travail des scripts d'image (build-*-image.sh).
#
# Il portait un nom FIXE (/tmp/tune-os-build, -rpi, -sunxi), vidé par
# `rm -rf` au démarrage : deux fabrications simultanées sur un même hôte
# s'effaçaient l'une l'autre. Désormais :
#   - TUNE_OS_WORK_DIR, s'il est posé, fixe le dossier (la CI y lit
#     debootstrap.log après un échec) ; il n'est alors jamais supprimé à la
#     sortie ;
#   - sinon, chaque fabrication a son dossier neuf (mktemp -d), supprimé à la
#     sortie une fois tout démonté.

# Rend le dossier de travail. $1 : étiquette (tune-os-build, tune-os-build-rpi…).
tune_os_work_dir() {
    local etiquette="${1:?étiquette manquante}"
    if [[ -n "${TUNE_OS_WORK_DIR:-}" ]]; then
        local d="${TUNE_OS_WORK_DIR%/}"
        case "$d" in
            "" | /tmp | /var/tmp | /root | /home)
                echo "TUNE_OS_WORK_DIR refusé : « ${TUNE_OS_WORK_DIR} » (il est vidé au démarrage)" >&2
                return 1
                ;;
            /*) ;;
            *)
                echo "TUNE_OS_WORK_DIR doit être un chemin absolu : « ${TUNE_OS_WORK_DIR} »" >&2
                return 1
                ;;
        esac
        printf '%s\n' "$d"
    else
        mktemp -d "${TMPDIR:-/tmp}/${etiquette}.XXXXXX"
    fi
}

# Supprime le dossier de travail s'il a été créé par tune_os_work_dir (pas de
# TUNE_OS_WORK_DIR) et si plus rien n'y est monté. Un montage restant (umount
# refusé) laisse le dossier en place : jamais de rm -rf à travers /dev ou /proc.
tune_os_work_dir_nettoyer() {
    local d="${1:-}"
    [[ -z "${TUNE_OS_WORK_DIR:-}" && -n "$d" && -d "$d" ]] || return 0
    if [[ -r /proc/mounts ]] && awk -v d="$d" '$2 == d || index($2, d "/") == 1 { t = 1 } END { exit !t }' /proc/mounts; then
        echo "Dossier de travail laissé en place, un montage y subsiste : $d" >&2
        return 0
    fi
    rm -rf --one-file-system "$d" 2>/dev/null || rm -rf "$d"
}
