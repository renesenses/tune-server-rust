#!/usr/bin/env bash
# Tune OS SSH credential lifecycle.
#
# New images create `tune` with a locked password.  On the first boot this
# script generates a per-machine credential, expires it immediately and shows
# it on the *physical console* through /etc/issue.d.  It is deliberately not an
# SSH Banner: sending the credential to every remote client would defeat the
# whole protection.
#
# Released servers also embed this file and run --migrate-legacy once on Tune
# OS.  That mode rotates the password only when the shadow hash still verifies
# against the historical public password "tune".  A password already changed
# by an administrator is never overwritten.
set -euo pipefail

readonly TUNE_USER="tune"
readonly STATE_DIR="/var/lib/tune-os"
readonly MIGRATION_MARKER="${STATE_DIR}/ssh-password-v2"
readonly INITIAL_SECRET="${STATE_DIR}/initial-ssh-password"
readonly ISSUE_NOTICE="/etc/issue.d/90-tune-initial-password.issue"

generate_password() {
    # 96 random bits, rendered as 24 shell/keyboard-friendly hexadecimal
    # characters. openssl is present with openssh-server on every Tune image.
    openssl rand -hex 12
}

password_matches_legacy() {
    local shadow_hash="$1"
    [[ -n "$shadow_hash" && "$shadow_hash" != "!" && "$shadow_hash" != "*" ]] || return 1
    command -v perl >/dev/null 2>&1 || return 2
    TUNE_SHADOW_HASH="$shadow_hash" perl -e '
        my $hash = $ENV{"TUNE_SHADOW_HASH"};
        exit(crypt("tune", $hash) eq $hash ? 0 : 1);
    '
}

write_notice() {
    local password="$1"
    install -d -m 0700 "$STATE_DIR"
    printf '%s\n' "$password" > "$INITIAL_SECRET"
    chmod 0600 "$INITIAL_SECRET"
    install -d -m 0755 "$(dirname "$ISSUE_NOTICE")"
    cat > "$ISSUE_NOTICE" <<EOF

  Tune OS — premier accès SSH
  Utilisateur : tune
  Mot de passe temporaire : ${password}
  Le changement de ce mot de passe est obligatoire à la première connexion.

EOF
    chmod 0644 "$ISSUE_NOTICE"
}

rotate_and_expire() {
    local reason="$1" password
    password="$(generate_password)"
    [[ ${#password} -eq 24 ]] || {
        echo "Tune OS: impossible de générer un mot de passe temporaire" >&2
        return 1
    }

    # Do not publish the notice until both account operations have succeeded.
    printf '%s:%s\n' "$TUNE_USER" "$password" | chpasswd
    chage -d 0 "$TUNE_USER"
    write_notice "$password"
    printf '%s\n' "$reason" > "$MIGRATION_MARKER"
    chmod 0600 "$MIGRATION_MARKER"
    printf '%s\n' \
        "Tune OS: mot de passe SSH temporaire généré; consulter la console locale." >&2
}

first_boot() {
    [[ -e "$MIGRATION_MARKER" ]] && return 0
    rotate_and_expire "generated-on-first-boot"
}

migrate_legacy() {
    [[ -e "$MIGRATION_MARKER" ]] && return 0

    local shadow_hash match_status
    shadow_hash="$(getent shadow "$TUNE_USER" | cut -d: -f2)"
    match_status=0
    password_matches_legacy "$shadow_hash" || match_status=$?
    case "$match_status" in
        0)
            rotate_and_expire "rotated-legacy-default"
            ;;
        1)
            # The administrator has already changed or locked the password.
            # Record the decision so every server restart does not re-read it.
            install -d -m 0700 "$STATE_DIR"
            printf '%s\n' "custom-password-preserved" > "$MIGRATION_MARKER"
            chmod 0600 "$MIGRATION_MARKER"
            ;;
        *)
            echo "Tune OS: mot de passe historique non vérifiable; migration différée" >&2
            return 1
            ;;
    esac
}

acknowledge_change() {
    # chage -d 0 writes 0 in shadow's last-change field.  Never erase the only
    # copy of the temporary password while that field still says it is due.
    local last_change
    last_change="$(getent shadow "$TUNE_USER" | cut -d: -f3)"
    [[ -n "$last_change" && "$last_change" != "0" ]] || return 0
    rm -f "$INITIAL_SECRET" "$ISSUE_NOTICE"
}

main() {
    [[ ${EUID:-$(id -u)} -eq 0 ]] || {
        echo "Tune OS: cette opération doit être exécutée par root" >&2
        return 1
    }
    case "${1:-}" in
        --first-boot) first_boot ;;
        --migrate-legacy) migrate_legacy ;;
        --acknowledge) acknowledge_change ;;
        *) echo "usage: $0 --first-boot|--migrate-legacy|--acknowledge" >&2; return 2 ;;
    esac
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
