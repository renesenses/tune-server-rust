#!/bin/sh
# Entrypoint of the renesenses/tune image (Dockerfile.dist, Dockerfile).
#
# Without PUID/PGID nothing changes: tune-server runs as `tune` (uid/gid 1000),
# exactly as with the former `USER tune`. The image starts as root only so
# that this script can, when asked, adopt the owner of the host folder mounted
# on /data, as linuxserver.io images do:
#
#   docker run -e PUID=99 -e PGID=100 -v /mnt/user/appdata/tune:/data ...
#
# With PUID and/or PGID set, it re-numbers the `tune` account, gives /data to
# it (only the entries that are not already its own — /music is read-only and
# never touched), then drops root for good before starting the server.
#
# Started as non-root (`docker run --user ...`), it changes nothing and starts
# the server directly.
#
# Local audio output (#5968): a USB DAC is reached through /dev/snd, passed
# with `--device /dev/snd` (compose: `devices: [/dev/snd:/dev/snd]`). Its
# nodes keep the HOST group id (29 on Debian/Ubuntu, 63 on Fedora...), so the
# server keeps, besides the groups of `tune` in the image (`audio` among
# them), the group owning /dev/snd and every `--group-add` of the container.
# `setpriv --init-groups` alone would drop the last two.
set -eu

TUNE_BIN="${TUNE_ENTRYPOINT_BIN:-/app/tune-server}"
TUNE_USER=tune
DATA_DIR="${TUNE_ENTRYPOINT_DATA_DIR:-/data}"
SND_DIR="${TUNE_ENTRYPOINT_SND_DIR:-/dev/snd}"

die() {
    echo "FATAL: $*" >&2
    exit 78 # EX_CONFIG
}

is_uint() {
    case "$1" in
        '' | *[!0-9]*) return 1 ;;
        *) return 0 ;;
    esac
}

if [ "$(id -u)" != 0 ]; then
    if [ -n "${PUID:-}${PGID:-}" ]; then
        echo "warning: PUID/PGID ignored: the container was started as uid $(id -u), not root (drop --user to use them)." >&2
    fi
    exec "$TUNE_BIN" "$@"
fi

# Started as root: the account the server will run as.
cur_uid="$(id -u "$TUNE_USER")"
cur_gid="$(id -g "$TUNE_USER")"

if [ -n "${PUID:-}${PGID:-}" ]; then
    want_uid="${PUID:-$cur_uid}"
    want_gid="${PGID:-$cur_gid}"
    is_uint "$want_uid" || die "PUID must be a number, got '$want_uid'."
    is_uint "$want_gid" || die "PGID must be a number, got '$want_gid'."
    [ "$want_uid" != 0 ] || die "PUID=0 would run Tune as root; use the uid that owns your data folder."

    # Their chatter ("usermod: no changes"...) goes to stderr, never stdout.
    if [ "$want_gid" != "$cur_gid" ]; then
        groupmod -o -g "$want_gid" "$TUNE_USER" >&2
    fi
    if [ "$want_uid" != "$cur_uid" ]; then
        usermod -o -u "$want_uid" "$TUNE_USER" >&2
    fi
    # groupmod normally moves the primary group too; only fix it if not.
    if [ "$(id -g "$TUNE_USER")" != "$want_gid" ]; then
        usermod -g "$want_gid" "$TUNE_USER" >&2
    fi

    if [ -d "$DATA_DIR" ]; then
        # Only what is not already ours: a restart costs one walk, no write.
        find "$DATA_DIR" \( ! -user "$want_uid" -o ! -group "$want_gid" \) \
            -exec chown -h "$want_uid:$want_gid" {} +
    fi
    echo "tune entrypoint: running as uid=$want_uid gid=$want_gid (PUID/PGID)." >&2
fi

# Same environment as the former `USER tune`: HOME is the account's home
# (the server keeps its log under $HOME/.local/state).
HOME="$(getent passwd "$TUNE_USER" | cut -d: -f6)"
export HOME

# Supplementary groups (#5968): those of `tune`, then the owner of the sound
# devices and the container's `--group-add` (root's own 0 and the unmapped
# 65534 of a rootless user namespace excluded: neither can be granted).
groups=""
add_group() {
    case "$1" in '' | 0 | 65534 | *[!0-9]*) return 0 ;; esac
    case ",$groups," in *",$1,"*) return 0 ;; esac
    groups="${groups:+$groups,}$1"
}
for g in $(id -G "$TUNE_USER"); do
    case ",$groups," in *",$g,"*) ;; *) groups="${groups:+$groups,}$g" ;; esac
done
for node in "$SND_DIR"/*; do
    if [ -c "$node" ]; then
        add_group "$(stat -L -c %g "$node")"
    fi
done
for g in $(id -G); do
    add_group "$g"
done

exec setpriv --reuid="$TUNE_USER" --regid="$TUNE_USER" --groups="$groups" \
    "$TUNE_BIN" "$@"
