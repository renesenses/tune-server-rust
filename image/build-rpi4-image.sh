#!/usr/bin/env bash
# ============================================================
# Tune OS — Raspberry Pi 4/5 Image Builder
# Builds a bootable aarch64 SD card image with Tune Server.
#
# Must be run on a Linux host as root.
# Cross-builds for aarch64 using qemu-user-static.
#
# Usage:
#   sudo ./build-rpi4-image.sh [--version 0.8.157]
# ============================================================
set -euo pipefail

TUNE_VERSION="${1:---version}"
if [[ "$TUNE_VERSION" == "--version" ]]; then
    TUNE_VERSION="${2:-latest}"
fi

IMAGE_NAME="tune-os-rpi4"
IMAGE_SIZE="2G"
WORK_DIR="/tmp/tune-os-build-rpi"
ROOTFS="${WORK_DIR}/rootfs"
IMAGE_FILE="${WORK_DIR}/${IMAGE_NAME}.img"
LOOP_DEV=""
HOSTNAME="tune"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

log() { echo -e "${BLUE}[tune-os]${NC} $*"; }
ok()  { echo -e "${GREEN}[  OK  ]${NC} $*"; }
err() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

cleanup() {
    log "Cleaning up..."
    umount "${ROOTFS}/boot/firmware" 2>/dev/null || true
    umount "${ROOTFS}/proc" 2>/dev/null || true
    umount "${ROOTFS}/sys" 2>/dev/null || true
    umount "${ROOTFS}/dev/pts" 2>/dev/null || true
    umount "${ROOTFS}/dev" 2>/dev/null || true
    umount "${ROOTFS}" 2>/dev/null || true
    if [[ -n "$LOOP_DEV" ]]; then
        losetup -d "$LOOP_DEV" 2>/dev/null || true
    fi
}
trap cleanup EXIT

if [[ $EUID -ne 0 ]]; then
    err "Must be run as root"
    exit 1
fi

# Check for qemu-user-static (needed for cross-arch debootstrap)
if [[ "$(uname -m)" != "aarch64" ]] && ! command -v qemu-aarch64-static &>/dev/null; then
    err "Cross-build requires qemu-user-static: apt install qemu-user-static binfmt-support"
    exit 1
fi

# --- Resolve version ---
if [[ "$TUNE_VERSION" == "latest" ]]; then
    TUNE_VERSION=$(curl -sL "https://api.github.com/repos/renesenses/tune-server-rust/releases/latest" \
        | grep '"tag_name"' | head -1 | sed 's/.*"v\(.*\)".*/\1/')
fi
log "Building Tune OS RPi4 with Tune Server v${TUNE_VERSION}"

TUNE_TARBALL_URL="https://github.com/renesenses/tune-server-rust/releases/download/v${TUNE_VERSION}/tune-server-v${TUNE_VERSION}-linux-aarch64.tar.gz"

# --- Create image ---
log "Creating ${IMAGE_SIZE} image..."
rm -rf "$WORK_DIR"
mkdir -p "$WORK_DIR"
truncate -s "$IMAGE_SIZE" "$IMAGE_FILE"

# RPi uses MBR: 256M boot (fat32) + rest root (ext4)
parted -s "$IMAGE_FILE" mklabel msdos
parted -s "$IMAGE_FILE" mkpart primary fat32 4MiB 260MiB
parted -s "$IMAGE_FILE" set 1 lba on
parted -s "$IMAGE_FILE" mkpart primary ext4 260MiB 100%

LOOP_DEV=$(losetup --find --show --partscan "$IMAGE_FILE")
PART_BOOT="${LOOP_DEV}p1"
PART_ROOT="${LOOP_DEV}p2"
sleep 1
partprobe "$LOOP_DEV" 2>/dev/null || true
sleep 1

mkfs.vfat -F 32 -n TUNEBOOT "$PART_BOOT"
mkfs.ext4 -L tuneroot -q "$PART_ROOT"

mkdir -p "$ROOTFS"
mount "$PART_ROOT" "$ROOTFS"
mkdir -p "${ROOTFS}/boot/firmware"
mount "$PART_BOOT" "${ROOTFS}/boot/firmware"

# --- Bootstrap ---
log "Bootstrapping Debian ${DEBIAN_RELEASE:-bookworm} for aarch64..."
debootstrap --arch=arm64 --variant=minbase \
    --foreign \
    --include=systemd,systemd-sysv,dbus,udev,kmod,\
sudo,curl,ca-certificates,avahi-daemon,libnss-mdns,\
alsa-utils,libasound2,networkmanager,openssh-server,\
locales,procps,iproute2,less,nano,\
linux-image-arm64,raspi-firmware \
    bookworm "$ROOTFS" http://deb.debian.org/debian

# Complete second stage with qemu
if [[ "$(uname -m)" != "aarch64" ]]; then
    cp /usr/bin/qemu-aarch64-static "${ROOTFS}/usr/bin/"
fi
chroot "$ROOTFS" /debootstrap/debootstrap --second-stage

ok "Debian aarch64 bootstrap complete"

# --- Mount pseudo-fs ---
mount --bind /dev "${ROOTFS}/dev"
mount --bind /dev/pts "${ROOTFS}/dev/pts"
mount -t proc proc "${ROOTFS}/proc"
mount -t sysfs sys "${ROOTFS}/sys"

# --- Same config as NUC (hostname, locale, user, tune service) ---
echo "$HOSTNAME" > "${ROOTFS}/etc/hostname"
cat > "${ROOTFS}/etc/hosts" <<EOF
127.0.0.1   localhost
127.0.1.1   ${HOSTNAME}
EOF

chroot "$ROOTFS" bash -c "echo 'en_US.UTF-8 UTF-8' > /etc/locale.gen && locale-gen"
chroot "$ROOTFS" ln -sf /usr/share/zoneinfo/UTC /etc/localtime

ROOT_UUID=$(blkid -s UUID -o value "$PART_ROOT")
BOOT_UUID=$(blkid -s UUID -o value "$PART_BOOT")
cat > "${ROOTFS}/etc/fstab" <<EOF
UUID=${ROOT_UUID}  /               ext4  errors=remount-ro  0 1
UUID=${BOOT_UUID}  /boot/firmware  vfat  defaults           0 2
EOF

sed -i 's/^hosts:.*/hosts: files mdns4_minimal [NOTFOUND=return] dns/' "${ROOTFS}/etc/nsswitch.conf"

chroot "$ROOTFS" bash -c "
    useradd -m -s /bin/bash -G sudo,audio,plugdev tune
    echo 'tune:tune' | chpasswd
    echo 'tune ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/tune
"

# Audio priority
cat > "${ROOTFS}/etc/security/limits.d/tune-audio.conf" <<EOF
tune    -    rtprio    95
tune    -    memlock   unlimited
tune    -    nice      -19
EOF

# --- Install Tune ---
log "Downloading Tune Server v${TUNE_VERSION} (aarch64)..."
curl -sL "$TUNE_TARBALL_URL" -o "${WORK_DIR}/tune.tar.gz"
mkdir -p "${ROOTFS}/opt/tune"
tar xzf "${WORK_DIR}/tune.tar.gz" -C "${ROOTFS}/opt/tune"
chmod +x "${ROOTFS}/opt/tune/tune-server"
mkdir -p "${ROOTFS}/opt/tune/data" "${ROOTFS}/mnt/music"

cat > "${ROOTFS}/opt/tune/tune.toml" <<EOF
[server]
port = 8888
data_dir = "/opt/tune/data"

[library]
music_dirs = ["/mnt/music"]

[audio]
backend = "auto"
EOF

# Systemd service (same as NUC)
cat > "${ROOTFS}/etc/systemd/system/tune.service" <<EOF
[Unit]
Description=Tune Music Server
After=network-online.target avahi-daemon.service
Wants=network-online.target

[Service]
Type=simple
User=tune
Group=audio
WorkingDirectory=/opt/tune
ExecStart=/opt/tune/tune-server
Restart=always
RestartSec=3
Environment=TUNE_DATA_DIR=/opt/tune/data
Environment=TUNE_PORT=8888
Environment=RUST_LOG=info
LimitNOFILE=65536
LimitRTPRIO=95
LimitMEMLOCK=infinity
ProtectSystem=strict
ReadWritePaths=/opt/tune/data /mnt/music /tmp
ProtectHome=yes
NoNewPrivileges=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

chroot "$ROOTFS" systemctl enable tune.service
chroot "$ROOTFS" systemctl enable NetworkManager
chroot "$ROOTFS" systemctl enable avahi-daemon
chroot "$ROOTFS" systemctl enable ssh

cat > "${ROOTFS}/etc/motd" <<EOF

  ♫  Tune OS v${TUNE_VERSION} (Raspberry Pi)
  ─────────────────────────────────────
  Web UI:    http://tune.local:8888
  Music:     /mnt/music
  Config:    /opt/tune/tune.toml
  Logs:      journalctl -u tune -f
  User:      tune / tune

EOF

ok "Tune installed on RPi4 image"

# --- Cleanup ---
chroot "$ROOTFS" apt-get clean
rm -rf "${ROOTFS}/var/cache/apt/archives"/*.deb
rm -rf "${ROOTFS}/var/lib/apt/lists"/*
rm -f "${ROOTFS}/usr/bin/qemu-aarch64-static"

umount "${ROOTFS}/proc"
umount "${ROOTFS}/sys"
umount "${ROOTFS}/dev/pts"
umount "${ROOTFS}/dev"
umount "${ROOTFS}/boot/firmware"
umount "${ROOTFS}"

# --- Output ---
OUTPUT_DIR="$(cd "$(dirname "$0")" && pwd)/output"
mkdir -p "$OUTPUT_DIR"
cp "$IMAGE_FILE" "${OUTPUT_DIR}/${IMAGE_NAME}-v${TUNE_VERSION}.img"
gzip -k "${OUTPUT_DIR}/${IMAGE_NAME}-v${TUNE_VERSION}.img"

FINAL_IMG="${OUTPUT_DIR}/${IMAGE_NAME}-v${TUNE_VERSION}.img"
ok "Build complete!"
echo ""
echo "  Image:  ${FINAL_IMG} ($(du -h "$FINAL_IMG" | cut -f1))"
echo "  GZ:     ${FINAL_IMG}.gz ($(du -h "${FINAL_IMG}.gz" | cut -f1))"
echo ""
echo "  Flash:  sudo dd if=${FINAL_IMG} of=/dev/sdX bs=4M status=progress"
echo "  Login:  tune / tune"
echo "  Web:    http://tune.local:8888"
