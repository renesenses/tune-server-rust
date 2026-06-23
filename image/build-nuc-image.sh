#!/usr/bin/env bash
# ============================================================
# Tune OS — NUC/Mini-PC Image Builder
# Builds a bootable x86_64 disk image with Tune Server
# pre-installed on a minimal Debian 12 (bookworm) base.
#
# Must be run on a Linux x86_64 host (e.g. .18) as root.
# Produces: tune-os-x86_64.img (raw disk image, ~1 GB)
#
# Usage:
#   sudo ./build-nuc-image.sh [--version 0.8.157]
# ============================================================
set -euo pipefail

# --- Configuration ---
TUNE_VERSION="${1:---version}"
if [[ "$TUNE_VERSION" == "--version" ]]; then
    TUNE_VERSION="${2:-latest}"
fi

IMAGE_NAME="tune-os-x86_64"
IMAGE_SIZE="2G"
DEBIAN_RELEASE="bookworm"
DEBIAN_MIRROR="http://deb.debian.org/debian"
WORK_DIR="/tmp/tune-os-build"
ROOTFS="${WORK_DIR}/rootfs"
IMAGE_FILE="${WORK_DIR}/${IMAGE_NAME}.img"
LOOP_DEV=""
HOSTNAME="tune"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

log() { echo -e "${BLUE}[tune-os]${NC} $*"; }
ok()  { echo -e "${GREEN}[  OK  ]${NC} $*"; }
err() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

cleanup() {
    log "Cleaning up..."
    # Unmount in reverse order
    umount "${ROOTFS}/boot/efi" 2>/dev/null || true
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

# --- Pre-flight checks ---
if [[ $EUID -ne 0 ]]; then
    err "Must be run as root (sudo)"
    exit 1
fi

for tool in debootstrap parted mkfs.ext4 mkfs.vfat losetup grub-install; do
    if ! command -v "$tool" &>/dev/null; then
        err "Missing tool: $tool — install with: apt install debootstrap parted dosfstools grub-efi-amd64-bin"
        exit 1
    fi
done

# --- Resolve Tune version ---
if [[ "$TUNE_VERSION" == "latest" ]]; then
    log "Fetching latest Tune version from GitHub..."
    TUNE_VERSION=$(curl -sL "https://api.github.com/repos/renesenses/tune-server-rust/releases/latest" \
        | grep '"tag_name"' | head -1 | sed 's/.*"v\(.*\)".*/\1/')
    if [[ -z "$TUNE_VERSION" ]]; then
        err "Could not determine latest version"
        exit 1
    fi
fi
log "Building Tune OS with Tune Server v${TUNE_VERSION}"

TUNE_TARBALL_URL="https://github.com/renesenses/tune-server-rust/releases/download/v${TUNE_VERSION}/tune-server-v${TUNE_VERSION}-linux-x86_64.tar.gz"

# --- Create disk image ---
log "Creating ${IMAGE_SIZE} disk image..."
rm -rf "$WORK_DIR"
mkdir -p "$WORK_DIR"
truncate -s "$IMAGE_SIZE" "$IMAGE_FILE"

# Partition: 512M EFI + rest ext4
parted -s "$IMAGE_FILE" mklabel gpt
parted -s "$IMAGE_FILE" mkpart ESP fat32 1MiB 513MiB
parted -s "$IMAGE_FILE" set 1 esp on
parted -s "$IMAGE_FILE" mkpart root ext4 513MiB 100%

# Setup loop device
LOOP_DEV=$(losetup --find --show --partscan "$IMAGE_FILE")
PART_EFI="${LOOP_DEV}p1"
PART_ROOT="${LOOP_DEV}p2"

# Wait for partitions to appear
sleep 1
if [[ ! -b "$PART_ROOT" ]]; then
    partprobe "$LOOP_DEV"
    sleep 1
fi

log "Formatting partitions..."
mkfs.vfat -F 32 -n TUNEEFI "$PART_EFI"
mkfs.ext4 -L tuneroot -q "$PART_ROOT"

# --- Mount and bootstrap ---
mkdir -p "$ROOTFS"
mount "$PART_ROOT" "$ROOTFS"
mkdir -p "${ROOTFS}/boot/efi"
mount "$PART_EFI" "${ROOTFS}/boot/efi"

log "Bootstrapping Debian ${DEBIAN_RELEASE}..."
debootstrap --arch=amd64 --variant=minbase \
    --include=systemd,systemd-sysv,dbus,udev,kmod,linux-image-amd64,\
grub-efi-amd64,sudo,curl,ca-certificates,avahi-daemon,libnss-mdns,\
alsa-utils,libasound2,wpasupplicant,networkmanager,openssh-server,\
locales,procps,iproute2,less,nano \
    "$DEBIAN_RELEASE" "$ROOTFS" "$DEBIAN_MIRROR"

ok "Debian bootstrap complete"

# --- Mount pseudo-filesystems for chroot ---
mount --bind /dev "${ROOTFS}/dev"
mount --bind /dev/pts "${ROOTFS}/dev/pts"
mount -t proc proc "${ROOTFS}/proc"
mount -t sysfs sys "${ROOTFS}/sys"

# --- Configure the system ---
log "Configuring system..."

# Hostname
echo "$HOSTNAME" > "${ROOTFS}/etc/hostname"
cat > "${ROOTFS}/etc/hosts" <<EOF
127.0.0.1   localhost
127.0.1.1   ${HOSTNAME}
EOF

# Locale
chroot "$ROOTFS" bash -c "echo 'en_US.UTF-8 UTF-8' > /etc/locale.gen && locale-gen"

# Timezone
chroot "$ROOTFS" ln -sf /usr/share/zoneinfo/UTC /etc/localtime

# fstab
ROOT_UUID=$(blkid -s UUID -o value "$PART_ROOT")
EFI_UUID=$(blkid -s UUID -o value "$PART_EFI")
cat > "${ROOTFS}/etc/fstab" <<EOF
UUID=${ROOT_UUID}  /          ext4  errors=remount-ro  0 1
UUID=${EFI_UUID}   /boot/efi  vfat  umask=0077         0 1
EOF

# Network: DHCP on all ethernet interfaces
cat > "${ROOTFS}/etc/NetworkManager/conf.d/tune.conf" <<EOF
[main]
plugins=keyfile

[connection]
wifi.powersave=2

[device]
wifi.scan-rand-mac-address=no
EOF

# Enable mDNS (tune.local)
sed -i 's/^hosts:.*/hosts: files mdns4_minimal [NOTFOUND=return] dns/' "${ROOTFS}/etc/nsswitch.conf"

# SSH: enable but disable password auth by default (key only)
mkdir -p "${ROOTFS}/etc/ssh/sshd_config.d"
cat > "${ROOTFS}/etc/ssh/sshd_config.d/tune.conf" <<EOF
PermitRootLogin no
PasswordAuthentication yes
EOF

# Create tune user
chroot "$ROOTFS" bash -c "
    useradd -m -s /bin/bash -G sudo,audio,plugdev tune
    echo 'tune:tune' | chpasswd
    echo 'tune ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/tune
"

# ALSA: set USB audio as default if present
cat > "${ROOTFS}/etc/asound.conf" <<'EOF'
# Tune OS: prefer USB audio device if available
defaults.pcm.card 0
defaults.ctl.card 0
EOF

# Real-time audio priority for tune user
cat > "${ROOTFS}/etc/security/limits.d/tune-audio.conf" <<EOF
tune    -    rtprio    95
tune    -    memlock   unlimited
tune    -    nice      -19
EOF

ok "System configured"

# --- Install Tune Server ---
log "Downloading Tune Server v${TUNE_VERSION}..."
curl -sL "$TUNE_TARBALL_URL" -o "${WORK_DIR}/tune.tar.gz"
if [[ ! -s "${WORK_DIR}/tune.tar.gz" ]]; then
    err "Download failed: ${TUNE_TARBALL_URL}"
    exit 1
fi

mkdir -p "${ROOTFS}/opt/tune"
tar xzf "${WORK_DIR}/tune.tar.gz" -C "${ROOTFS}/opt/tune"
chmod +x "${ROOTFS}/opt/tune/tune-server"

# Verify binary
if [[ ! -f "${ROOTFS}/opt/tune/tune-server" ]]; then
    err "tune-server binary not found in archive"
    exit 1
fi
ok "Tune Server installed to /opt/tune"

# Music directory
mkdir -p "${ROOTFS}/mnt/music"

# Tune configuration
mkdir -p "${ROOTFS}/opt/tune/data"
cat > "${ROOTFS}/opt/tune/tune.toml" <<EOF
# Tune OS default configuration
# Edit via web UI at http://tune.local:8888/settings

[server]
port = 8888
data_dir = "/opt/tune/data"

[library]
music_dirs = ["/mnt/music"]

[audio]
backend = "auto"
EOF

# --- Systemd service ---
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
Environment=TUNE_LOG_LEVEL=info
Environment=RUST_LOG=info
LimitNOFILE=65536
LimitRTPRIO=95
LimitMEMLOCK=infinity

# Hardening
ProtectSystem=strict
ReadWritePaths=/opt/tune/data /mnt/music /tmp
ProtectHome=yes
NoNewPrivileges=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

# Enable services
chroot "$ROOTFS" systemctl enable tune.service
chroot "$ROOTFS" systemctl enable NetworkManager
chroot "$ROOTFS" systemctl enable avahi-daemon
chroot "$ROOTFS" systemctl enable ssh

ok "Tune systemd service installed"

# --- First-boot script ---
cat > "${ROOTFS}/opt/tune/first-boot.sh" <<'FIRSTBOOT'
#!/bin/bash
# Tune OS first-boot setup
# Runs once, then disables itself

MARKER="/opt/tune/data/.first-boot-done"
if [[ -f "$MARKER" ]]; then
    exit 0
fi

# Generate unique machine-id
systemd-machine-id-setup

# Resize root partition to fill disk (if image was flashed to larger drive)
ROOT_PART=$(findmnt -n -o SOURCE /)
ROOT_DISK=$(lsblk -ndo pkname "$ROOT_PART")
PART_NUM=$(echo "$ROOT_PART" | grep -o '[0-9]*$')
if [[ -n "$ROOT_DISK" && -n "$PART_NUM" ]]; then
    growpart "/dev/$ROOT_DISK" "$PART_NUM" 2>/dev/null || true
    resize2fs "$ROOT_PART" 2>/dev/null || true
fi

# Set hostname to tune-XXXX (last 4 of MAC)
MAC=$(ip link show | grep -m1 'link/ether' | awk '{print $2}' | tr -d ':' | tail -c 5)
if [[ -n "$MAC" ]]; then
    hostnamectl set-hostname "tune-${MAC}"
fi

touch "$MARKER"
echo "Tune OS first boot complete."
FIRSTBOOT
chmod +x "${ROOTFS}/opt/tune/first-boot.sh"

cat > "${ROOTFS}/etc/systemd/system/tune-first-boot.service" <<EOF
[Unit]
Description=Tune OS First Boot Setup
After=network-online.target
ConditionPathExists=!/opt/tune/data/.first-boot-done

[Service]
Type=oneshot
ExecStart=/opt/tune/first-boot.sh
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF
chroot "$ROOTFS" systemctl enable tune-first-boot.service

# --- MOTD ---
cat > "${ROOTFS}/etc/motd" <<EOF

  ♫  Tune OS v${TUNE_VERSION}
  ─────────────────────────────
  Web UI:    http://tune.local:8888
  Music:     /mnt/music
  Config:    /opt/tune/tune.toml
  Logs:      journalctl -u tune -f
  User:      tune / tune

  Mount your NAS music share:
    sudo mount -t cifs //nas/music /mnt/music -o guest
    (add to /etc/fstab for permanent mount)

EOF

# --- Install GRUB ---
log "Installing GRUB bootloader..."
chroot "$ROOTFS" grub-install --target=x86_64-efi \
    --efi-directory=/boot/efi \
    --bootloader-id=tune \
    --removable \
    --no-nvram 2>/dev/null || {
    # Fallback: copy EFI binary manually
    mkdir -p "${ROOTFS}/boot/efi/EFI/BOOT"
    cp "${ROOTFS}/usr/lib/grub/x86_64-efi/monolithic/grubx64.efi" \
       "${ROOTFS}/boot/efi/EFI/BOOT/BOOTX64.EFI" 2>/dev/null || true
}

# GRUB config
cat > "${ROOTFS}/etc/default/grub" <<EOF
GRUB_DEFAULT=0
GRUB_TIMEOUT=3
GRUB_CMDLINE_LINUX_DEFAULT="quiet"
GRUB_CMDLINE_LINUX=""
GRUB_DISABLE_OS_PROBER=true
EOF
chroot "$ROOTFS" update-grub 2>/dev/null || true

ok "GRUB installed"

# --- Cleanup ---
log "Cleaning up rootfs..."
chroot "$ROOTFS" apt-get clean
rm -rf "${ROOTFS}/var/cache/apt/archives"/*.deb
rm -rf "${ROOTFS}/var/lib/apt/lists"/*
rm -rf "${ROOTFS}/tmp"/*

# --- Unmount and finalize ---
log "Finalizing image..."
umount "${ROOTFS}/proc"
umount "${ROOTFS}/sys"
umount "${ROOTFS}/dev/pts"
umount "${ROOTFS}/dev"
umount "${ROOTFS}/boot/efi"
umount "${ROOTFS}"

# Copy image to output
OUTPUT_DIR="$(cd "$(dirname "$0")" && pwd)/output"
mkdir -p "$OUTPUT_DIR"
cp "$IMAGE_FILE" "${OUTPUT_DIR}/${IMAGE_NAME}-v${TUNE_VERSION}.img"

# Compress
log "Compressing image..."
gzip -k "${OUTPUT_DIR}/${IMAGE_NAME}-v${TUNE_VERSION}.img"

FINAL_IMG="${OUTPUT_DIR}/${IMAGE_NAME}-v${TUNE_VERSION}.img"
FINAL_GZ="${FINAL_IMG}.gz"

ok "Build complete!"
echo ""
echo "  Raw image:    ${FINAL_IMG} ($(du -h "$FINAL_IMG" | cut -f1))"
echo "  Compressed:   ${FINAL_GZ} ($(du -h "$FINAL_GZ" | cut -f1))"
echo ""
echo "  Flash to NUC: sudo dd if=${FINAL_IMG} of=/dev/sdX bs=4M status=progress"
echo "  Or use:       balenaEtcher / Rufus with the .img file"
echo ""
echo "  Default login: tune / tune"
echo "  Web UI:        http://tune.local:8888"
