#!/usr/bin/env bash
# Tests of packaging/docker/entrypoint.sh.
#
#   bash packaging/docker/test-entrypoint.sh              # simulation, no root, no Docker
#   bash packaging/docker/test-entrypoint.sh --conteneur  # + real run in debian:bookworm-slim
#
# The simulation replaces id/getent/groupmod/usermod/chown/setpriv by stubs
# that log their call, and checks what the entrypoint would do. The container
# run executes the real setpriv/usermod/chown as root, in the same base image
# as Dockerfile.dist.
#
# ENTRYPOINT=<file> tests another script (used for the counter-proofs).
set -euo pipefail

ici="$(cd "$(dirname "$0")" && pwd)"
ENTRYPOINT="${ENTRYPOINT:-$ici/entrypoint.sh}"
echecs=0
cas=0

travail="$(mktemp -d)"
trap 'rm -rf "$travail"' EXIT

ok() { cas=$((cas + 1)); echo "ok   - $1"; }
ko() { cas=$((cas + 1)); echecs=$((echecs + 1)); echo "FAIL - $1"; [ -n "${2:-}" ] && printf '%s\n' "$2" | sed 's/^/       /'; }

# Stubs: every call is appended to $JOURNAL; account state lives in $ETAT.
stubs="$travail/stubs"
mkdir -p "$stubs"
cat > "$stubs/id" <<'EOF'
#!/bin/sh
. "$ETAT"
case "$*" in
    "-u") echo "$FAKE_EUID" ;;
    "-u tune") echo "$TUNE_UID" ;;
    "-g tune") echo "$TUNE_GID" ;;
    # `tune` is in `audio` (29) in the image (Dockerfile.dist, #5968).
    "-G tune") echo "$TUNE_GID 29" ;;
    # The current process: root, plus the container's --group-add.
    "-G") echo "$FAKE_EUID $FAKE_GROUP_ADD" ;;
    *) echo "stub id: $*" >&2; exit 2 ;;
esac
EOF
cat > "$stubs/getent" <<'EOF'
#!/bin/sh
. "$ETAT"
[ "$*" = "passwd tune" ] || { echo "stub getent: $*" >&2; exit 2; }
echo "tune:x:$TUNE_UID:$TUNE_GID::/home/tune:/bin/false"
EOF
cat > "$stubs/groupmod" <<'EOF'
#!/bin/sh
echo "groupmod $*" >> "$JOURNAL"
[ "$1 $2 $4" = "-o -g tune" ] && echo "TUNE_GID=$3" >> "$ETAT"
EOF
cat > "$stubs/usermod" <<'EOF'
#!/bin/sh
echo "usermod $*" >> "$JOURNAL"
# Like the real one, it chats on stdout (seen in CI: "usermod: no changes").
echo "usermod: no changes"
[ "$1 $2 $4" = "-o -u tune" ] && echo "TUNE_UID=$3" >> "$ETAT"
exit 0
EOF
cat > "$stubs/chown" <<'EOF'
#!/bin/sh
echo "chown $*" >> "$JOURNAL"
EOF
cat > "$stubs/setpriv" <<'EOF'
#!/bin/sh
echo "setpriv $* HOME=$HOME" >> "$JOURNAL"
EOF
# Group owning the fake sound nodes (63 = `audio` on a Fedora host).
cat > "$stubs/stat" <<'EOF'
#!/bin/sh
. "$ETAT"
[ "$1 $2 $3" = "-L -c %g" ] || { echo "stub stat: $*" >&2; exit 2; }
echo "$FAKE_SND_GID"
EOF
cat > "$stubs/serveur" <<'EOF'
#!/bin/sh
echo "serveur $*" >> "$JOURNAL"
EOF
chmod +x "$stubs"/*

# One run. $1 = effective uid; the rest is the environment (VAR=value).
lancer() {
    local euid="$1"
    shift
    export ETAT="$travail/etat" JOURNAL="$travail/journal"
    printf 'FAKE_EUID=%s\nTUNE_UID=1000\nTUNE_GID=1000\nFAKE_GROUP_ADD="%s"\nFAKE_SND_GID=%s\n' \
        "$euid" "${GROUP_ADD:-}" "${SND_GID:-29}" > "$ETAT"
    : > "$JOURNAL"
    rm -rf "$travail/data" "$travail/music" "$travail/snd"
    mkdir -p "$travail/data/artwork_cache" "$travail/music" "$travail/snd"
    # SND=1: /dev/snd passed to the container (a char device stands in).
    if [ -n "${SND:-}" ]; then ln -s /dev/null "$travail/snd/controlC0"; fi
    touch "$travail/data/tune.db" "$travail/music/piste.flac"
    sortie=0
    erreur="$(env -i PATH="$stubs:/usr/bin:/bin" HOME=/root ETAT="$ETAT" JOURNAL="$JOURNAL" \
        TUNE_ENTRYPOINT_BIN="$stubs/serveur" TUNE_ENTRYPOINT_DATA_DIR="$travail/data" \
        TUNE_ENTRYPOINT_SND_DIR="$travail/snd" \
        "$@" sh "$ENTRYPOINT" --un "deux mots" 2>&1 >"$travail/stdout")" || sortie=$?
    stdout="$(cat "$travail/stdout")"
    journal="$(cat "$JOURNAL")"
}

a() { grep -qF -- "$1" <<<"$journal"; }

# 1. Non-root, no PUID/PGID: server started directly, nothing else.
lancer 1000
if [ "$sortie" = 0 ] && [ "$journal" = "serveur --un deux mots" ]; then
    ok "non-root sans PUID/PGID : le serveur part tel quel"
else ko "non-root sans PUID/PGID" "sortie=$sortie
$journal"; fi

# 2. Non-root with PUID: warned, still started directly.
lancer 1000 PUID=99
if [ "$sortie" = 0 ] && [ "$journal" = "serveur --un deux mots" ] && grep -q "PUID/PGID ignored" <<<"$erreur"; then
    ok "non-root avec PUID : avertissement, aucun changement"
else ko "non-root avec PUID" "sortie=$sortie err=$erreur
$journal"; fi

# 3. Root, no PUID/PGID: straight drop to tune, same HOME as USER tune.
lancer 0
if [ "$sortie" = 0 ] && [ "$journal" = "setpriv --reuid=tune --regid=tune --groups=1000,29 $stubs/serveur --un deux mots HOME=/home/tune" ]; then
    ok "root sans PUID/PGID : bascule directe vers tune, rien d'autre"
else ko "root sans PUID/PGID" "sortie=$sortie err=$erreur
$journal"; fi

# 4. Root, PUID=99 PGID=100 (unRAID): re-number, chown /data only, drop.
lancer 0 PUID=99 PGID=100
if [ "$sortie" = 0 ] && a "groupmod -o -g 100 tune" && a "usermod -o -u 99 tune" \
    && ! a "usermod -g" && a "chown -h 99:100 $travail/data" && [ -z "$stdout" ] \
    && ! a "music" \
    && [ "$(tail -n1 <<<"$journal")" = "setpriv --reuid=tune --regid=tune --groups=100,29 $stubs/serveur --un deux mots HOME=/home/tune" ]; then
    ok "root PUID=99 PGID=100 : compte renuméroté, /data rendu, /music intact, bascule"
else ko "root PUID=99 PGID=100" "sortie=$sortie err=$erreur stdout=$stdout
$journal"; fi

# 5. Root, PUID/PGID = owner of the files already: no chown at all.
moi_u="$(/usr/bin/id -u)"; moi_g="$(/usr/bin/id -g)"
lancer 0 PUID="$moi_u" PGID="$moi_g"
if [ "$sortie" = 0 ] && ! a "chown" && a "setpriv"; then
    ok "root, /data déjà au bon propriétaire : aucun chown"
else ko "root, /data déjà au bon propriétaire" "sortie=$sortie
$journal"; fi

# 6. Only PGID: the uid is kept.
lancer 0 PGID=100
if [ "$sortie" = 0 ] && a "groupmod -o -g 100 tune" && ! a "usermod -o -u" && a "chown -h 1000:100"; then
    ok "PGID seul : uid conservé"
else ko "PGID seul" "sortie=$sortie
$journal"; fi

# 7. Invalid values: EX_CONFIG, nothing modified, server not started.
for v in "PUID=abc" "PGID=-5" "PUID=0"; do
    lancer 0 "$v"
    if [ "$sortie" = 78 ] && [ -z "$journal" ] && grep -q "FATAL" <<<"$erreur"; then
        ok "$v refusé (78), rien touché"
    else ko "$v refusé" "sortie=$sortie err=$erreur
$journal"; fi
done

# 8. #5968 — /dev/snd passed from a Fedora host (audio = 63) plus
#    `--group-add 44`: both kept next to tune's own groups, no duplicate.
SND=1 SND_GID=63 GROUP_ADD="44 29" lancer 0
if [ "$sortie" = 0 ] && a "setpriv --reuid=tune --regid=tune --groups=1000,29,63,44 $stubs/serveur"; then
    ok "/dev/snd (gid 63) et --group-add 44 : groupes conservés"
else ko "/dev/snd et --group-add" "sortie=$sortie err=$erreur
$journal"; fi

# 9. Rootless user namespace: /dev/snd shows up as 65534 (unmapped), which
#    cannot be granted — left out instead of failing setgroups.
SND=1 SND_GID=65534 lancer 0
if [ "$sortie" = 0 ] && a "setpriv --reuid=tune --regid=tune --groups=1000,29 $stubs/serveur"; then
    ok "/dev/snd non mappé (65534) : écarté"
else ko "/dev/snd non mappé" "sortie=$sortie err=$erreur
$journal"; fi

if [ "${1:-}" = "--conteneur" ]; then
    if ! docker info >/dev/null 2>&1; then
        ko "conteneur : Docker indisponible"
    else
        racine="$(cd "$ici/../.." && pwd)"
        # shellcheck disable=SC2016  # expanded inside the container
        script='set -eu
groupadd -g 1000 tune && useradd -u 1000 -g tune -m -s /bin/false tune
printf "#!/bin/sh\necho \"\$(id -u) \$(id -g) \$HOME \$*\"\n" > /srv.sh && chmod 755 /srv.sh
mkdir -p /data/artwork_cache && touch /data/tune.db && chown -R root:root /data
export TUNE_ENTRYPOINT_BIN=/srv.sh
echo "SANS=$(sh /repo/packaging/docker/entrypoint.sh a b)"
echo "AVEC=$(PUID=99 PGID=100 sh /repo/packaging/docker/entrypoint.sh a b 2>/dev/null)"
echo "DATA=$(stat -c "%u:%g" /data /data/tune.db /data/artwork_cache | tr "\n" " ")"
echo "MUSIC=$(stat -c "%u:%g" /music/piste.flac)"
touch /music/x 2>/dev/null && echo "MUSIC_RW" || true
mkdir /snd && mknod /snd/controlC0 c 1 3 && chgrp 63 /snd/controlC0
printf "#!/bin/sh\necho \"\$(id -G)\"\n" > /grp.sh && chmod 755 /grp.sh
echo "SND=$(TUNE_ENTRYPOINT_BIN=/grp.sh TUNE_ENTRYPOINT_SND_DIR=/snd sh /repo/packaging/docker/entrypoint.sh)"'
        res="$(docker run --rm -v "$racine:/repo:ro" -v "$travail/music:/music:ro" \
            debian:bookworm-slim sh -c "$script" 2>&1)" || true
        mu="$(stat -c '%u:%g' "$travail/music/piste.flac" 2>/dev/null || echo '?')"
        if grep -qx "SANS=1000 1000 /home/tune a b" <<<"$res" \
            && grep -qx "AVEC=99 100 /home/tune a b" <<<"$res" \
            && grep -qx "DATA=99:100 99:100 99:100 " <<<"$res" \
            && grep -qx "MUSIC=$mu" <<<"$res" && ! grep -q MUSIC_RW <<<"$res" \
            && grep -qx "SND=100 63" <<<"$res"; then
            ok "conteneur bookworm-slim : vrais setpriv/usermod/chown"
        else ko "conteneur bookworm-slim" "$res"; fi
    fi
fi

echo "$cas cas, $echecs échec(s)"
[ "$echecs" = 0 ]
