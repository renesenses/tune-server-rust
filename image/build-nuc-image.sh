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

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --- Configuration ---
TUNE_VERSION="${1:---version}"
if [[ "$TUNE_VERSION" == "--version" ]]; then
    TUNE_VERSION="${2:-latest}"
fi

IMAGE_NAME="tune-os-x86_64"
IMAGE_SIZE="3G"
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

# Partition: 1M BIOS boot (Legacy/CSM grub embedding) + 512M EFI + rest ext4.
# The BIOS boot partition is what lets grub-install --target=i386-pc work at
# all on a GPT disk: without a partition flagged bios_grub, grub has nowhere
# to embed its core.img and refuses ("this GPT partition label has no BIOS
# Boot Partition"), which is exactly why a Legacy/CSM-configured machine
# could never boot this image before — there was no fallback for it, only
# UEFI (--target=x86_64-efi).
parted -s "$IMAGE_FILE" mklabel gpt
parted -s "$IMAGE_FILE" mkpart biosboot 1MiB 2MiB
parted -s "$IMAGE_FILE" set 1 bios_grub on
parted -s "$IMAGE_FILE" mkpart ESP fat32 2MiB 514MiB
parted -s "$IMAGE_FILE" set 2 esp on
parted -s "$IMAGE_FILE" mkpart root ext4 514MiB 100%

# Setup loop device
LOOP_DEV=$(losetup --find --show --partscan "$IMAGE_FILE")
PART_EFI="${LOOP_DEV}p2"
PART_ROOT="${LOOP_DEV}p3"

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

log "Bootstrapping Debian ${DEBIAN_RELEASE} (base minimale)..."
# Base minimale seulement : le reste s'installe via apt en chroot, qui sait
# ordonner les dépendances (le configure naïf de debootstrap échoue sur
# polkitd ↔ default-logind).
debootstrap --arch=amd64 --variant=minbase \
    --components=main,contrib,non-free-firmware \
    --include=systemd,systemd-sysv \
    "$DEBIAN_RELEASE" "$ROOTFS" "$DEBIAN_MIRROR" || {
    err "debootstrap failed — dernières lignes de debootstrap.log :"
    tail -n 200 "${ROOTFS}/debootstrap/debootstrap.log" 2>/dev/null || true
    exit 1
}

ok "Debian bootstrap complete"

# --- Mount pseudo-filesystems for chroot ---
mount --bind /dev "${ROOTFS}/dev"
mount --bind /dev/pts "${ROOTFS}/dev/pts"
mount -t proc proc "${ROOTFS}/proc"
mount -t sysfs sys "${ROOTFS}/sys"

# --- Install packages with apt (proper dependency ordering) ---
log "Installing packages via apt..."
cat > "${ROOTFS}/etc/apt/sources.list" <<EOF
deb ${DEBIAN_MIRROR} ${DEBIAN_RELEASE} main contrib non-free-firmware
deb http://security.debian.org/debian-security ${DEBIAN_RELEASE}-security main contrib non-free-firmware
EOF

# Prevent services from starting inside the chroot
printf '#!/bin/sh\nexit 101\n' > "${ROOTFS}/usr/sbin/policy-rc.d"
chmod +x "${ROOTFS}/usr/sbin/policy-rc.d"

# non-free-firmware: WiFi chipsets of consumer PCs (Intel/Realtek/Atheros/Broadcom)
chroot "$ROOTFS" bash -ec "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq \
        dbus udev kmod linux-image-amd64 grub-efi-amd64 grub-pc-bin sudo curl \
        ca-certificates avahi-daemon libnss-mdns alsa-utils wpasupplicant \
        network-manager openssh-server \
        firmware-iwlwifi firmware-realtek firmware-atheros firmware-brcm80211 \
        wireless-regdb cloud-guest-utils cifs-utils smbclient exfatprogs ntfs-3g \
        locales procps iproute2 less nano
"
ok "Packages installed"

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

# Appliance marker: unlocks /api/v1/appliance (WiFi setup from the web UI)
# and the appliance flag in /system/config — see docs/APPLIANCE.md
echo "Tune OS appliance image" > "${ROOTFS}/etc/tune-appliance"

# Une appliance ne dort jamais : sur un portable, logind suspend au rabat
# du capot (retour Stéphane : « la machine ne répond plus » — Asus).
mkdir -p "${ROOTFS}/etc/systemd/logind.conf.d"
cat > "${ROOTFS}/etc/systemd/logind.conf.d/tune-no-sleep.conf" <<EOF
[Login]
HandleLidSwitch=ignore
HandleLidSwitchExternalPower=ignore
HandleLidSwitchDocked=ignore
HandleSuspendKey=ignore
HandleHibernateKey=ignore
IdleAction=ignore
EOF
chroot "$ROOTFS" systemctl mask sleep.target suspend.target hibernate.target hybrid-sleep.target

# Autologin console : l'utilisateur ne tape jamais de mot de passe sur la box
# (retour Gil : « le mot de passe, je vois pas trop pourquoi »). Le mot de
# passe reste requis pour SSH et sudo.
mkdir -p "${ROOTFS}/etc/systemd/system/getty@tty1.service.d"
cat > "${ROOTFS}/etc/systemd/system/getty@tty1.service.d/autologin.conf" <<'EOF'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin tune --noclear %I $TERM
EOF

# USB storage: auto-mount partitions under /media/<kernel> (headless, no udisks
# session). exFAT/NTFS/FAT mount root-owned world-readable — enough for scanning.
mkdir -p "${ROOTFS}/media"
cat > "${ROOTFS}/etc/udev/rules.d/99-tune-usb-mount.rules" <<'EOF'
ACTION=="add", SUBSYSTEMS=="usb", SUBSYSTEM=="block", ENV{ID_FS_USAGE}=="filesystem", RUN+="/usr/bin/systemd-mount --no-block --automount=yes --collect $devnode /media/%k"
ACTION=="remove", SUBSYSTEMS=="usb", SUBSYSTEM=="block", ENV{ID_FS_USAGE}=="filesystem", RUN+="/usr/bin/systemd-umount /media/%k"
EOF

# SSH password login stays available for a headless appliance, but the account
# starts locked. first-boot generates a unique temporary password and expires
# it immediately; there is no public credential window before that service.
mkdir -p "${ROOTFS}/etc/ssh/sshd_config.d"
cat > "${ROOTFS}/etc/ssh/sshd_config.d/tune.conf" <<EOF
PermitRootLogin no
PasswordAuthentication yes
EOF

# Create the tune user locked. Never bake a reusable password into an image.
chroot "$ROOTFS" bash -c "
    useradd -m -s /bin/bash -G sudo,audio,plugdev tune
    echo 'tune ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/tune
"

install -D -m 0755 "${SCRIPT_DIR}/tune-os-password.sh" \
    "${ROOTFS}/usr/local/sbin/tune-os-password"
cat > "${ROOTFS}/etc/profile.d/tune-password-notice.sh" <<'EOF'
# Remove the physical-console notice only after shadow confirms that the
# forced password change has happened.
if [ "${USER:-}" = tune ] && command -v sudo >/dev/null 2>&1; then
    sudo -n /usr/local/sbin/tune-os-password --acknowledge >/dev/null 2>&1 || true
fi
EOF
chmod 0644 "${ROOTFS}/etc/profile.d/tune-password-notice.sh"

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
# TUNE_TARBALL_PATH : archive déjà récupérée par l'appelant (la CI la télécharge
# avec le jeton, seule façon de lire les assets d'une release en BROUILLON —
# c'est l'état dans lequel naît toute release depuis #1588). Sans lui, on
# retombe sur l'URL publique, qui ne marche que sur une release publiée.
if [[ -n "${TUNE_TARBALL_PATH:-}" && -f "${TUNE_TARBALL_PATH}" ]]; then
    log "Using pre-fetched Tune Server tarball: ${TUNE_TARBALL_PATH}"
    cp "${TUNE_TARBALL_PATH}" "${WORK_DIR}/tune.tar.gz"
else
    log "Downloading Tune Server v${TUNE_VERSION}..."
    # -f : un 404 doit échouer ici, et non se transformer en page HTML écrite
    # dans tune.tar.gz — non vide, donc indétectable par un simple test -s.
    curl -fsSL "$TUNE_TARBALL_URL" -o "${WORK_DIR}/tune.tar.gz" || {
        err "Download failed: ${TUNE_TARBALL_URL}"
        exit 1
    }
fi

if [[ ! -s "${WORK_DIR}/tune.tar.gz" ]]; then
    err "Empty tarball: ${TUNE_TARBALL_URL}"
    exit 1
fi
# Le test qui manquait : une page d'erreur passe le test de taille, pas celui-ci.
if ! gzip -t "${WORK_DIR}/tune.tar.gz" 2>/dev/null; then
    err "Not a gzip archive (page d'erreur ?): ${TUNE_TARBALL_URL}"
    head -c 200 "${WORK_DIR}/tune.tar.gz" >&2 || true
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
# Format PLAT (cf. tune.toml.example) — les sections [server]/[library]
# ne sont pas lues et le serveur retombait sur db_path relatif ("tune.db")
# dans /opt/tune, en lecture seule (ProtectSystem=strict) → crash-loop.
cat > "${ROOTFS}/opt/tune/tune.toml" <<EOF
# Tune OS default configuration
# Edit via web UI at http://tune.local:8888/settings

port = 8888
db_path = "/opt/tune/data/tune.db"
web_dir = "/opt/tune/web"
artwork_dir = "/opt/tune/data/artwork_cache"
auto_scan = true
log_level = "info"

# /media : disques USB auto-montés ; /mnt/music : montages NAS manuels
music_dirs = ["/mnt/music", "/media"]
EOF

# --- Systemd service ---
cat > "${ROOTFS}/etc/systemd/system/tune.service" <<EOF
[Unit]
Description=Tune Music Server
After=network-online.target avahi-daemon.service
Wants=network-online.target

[Service]
Type=simple
# Root sur l'image appliance : le serveur pilote nmcli (config WiFi) et
# mount.cifs (partages SMB) directement — cf. /etc/tune-appliance.
User=root
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

# Hardening (root, mais système en lecture seule hors chemins listés)
ProtectSystem=strict
# /opt/tune entier : l'auto-update remplace le binaire et web/ in place
# (install_unix sur current_exe) — /opt/tune/data seul bloquait la MAJ.
ReadWritePaths=/opt/tune /mnt /media /tmp
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

# --- Port 80 → 8888 proxy ---
# Browsers (Chrome HTTPS-First, Safari) silently upgrade http:// links to
# https://; with only :8888 open the TLS attempt is refused and the user sees
# a dead link unless they hand-edit the URL back to http (retour Bertrand,
# tune-e32f.local). Serving port 80 lets the URL drop scheme AND port
# (tune-xxxx.local), and the browsers' https→http fallback then works.
# systemd-socket-proxyd ships with systemd — no extra package, no firewall.
cat > "${ROOTFS}/etc/systemd/system/tune-web80.socket" <<EOF
[Unit]
Description=Tune OS web UI on port 80 (proxied to :8888)

[Socket]
ListenStream=80

[Install]
WantedBy=sockets.target
EOF

cat > "${ROOTFS}/etc/systemd/system/tune-web80.service" <<EOF
[Unit]
Description=Proxy port 80 to the Tune web UI on :8888
Requires=tune-web80.socket
After=tune.service

[Service]
ExecStart=/usr/lib/systemd/systemd-socket-proxyd 127.0.0.1:8888
PrivateTmp=yes
PrivateNetwork=no
EOF

# Password initialization is a separate fail-closed boot unit. sshd requires
# its success, rather than merely being ordered after it.
cat > "${ROOTFS}/etc/systemd/system/tune-first-boot-password.service" <<EOF
[Unit]
Description=Tune OS First Boot SSH Password
After=local-fs.target
Before=ssh.service getty@tty1.service
ConditionPathExists=!/var/lib/tune-os/ssh-password-v2

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/tune-os-password --first-boot

[Install]
WantedBy=multi-user.target
EOF
mkdir -p "${ROOTFS}/etc/systemd/system/ssh.service.d"
cat > "${ROOTFS}/etc/systemd/system/ssh.service.d/tune-password.conf" <<EOF
[Unit]
Requires=tune-first-boot-password.service
After=tune-first-boot-password.service
EOF

# Enable services
chroot "$ROOTFS" systemctl enable tune.service
chroot "$ROOTFS" systemctl enable NetworkManager
chroot "$ROOTFS" systemctl enable avahi-daemon
chroot "$ROOTFS" systemctl enable ssh
chroot "$ROOTFS" systemctl enable tune-web80.socket
chroot "$ROOTFS" systemctl enable tune-first-boot-password.service

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
    # Keep the printed URL truthful: tune.local dies with the rename
    # (retour Stéphane : « machine injoignable après redémarrage »)
    sed -i "s|http://tune\.local|http://tune-${MAC}.local|" /etc/motd
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
  Web UI:    http://tune.local   (ou http://tune.local:8888)
  Music:     USB drives auto-mount under /media
             NAS/SMB shares: web UI → Settings → Network
  WiFi:      web UI → Settings → Network (first boot: ethernet)
  Config:    /opt/tune/tune.toml
  Logs:      journalctl -u tune -f
  SSH user:  tune (temporary password shown on the physical console at first boot)

EOF

# Login screen (before login): live hostname via agetty (\n). L'IP n'utilise
# PAS \4 : agetty rend /etc/issue avant l'acquisition DHCP → champ vide
# (retour Stéphane). Un dispatcher NetworkManager réécrit le fichier dès que
# le réseau monte ; agetty relit /etc/issue à chaque affichage du prompt.
cat > "${ROOTFS}/etc/issue" <<'EOF'

  Tune OS — Web UI : http://\n.local

EOF

cat > "${ROOTFS}/etc/NetworkManager/dispatcher.d/50-tune-issue" <<'EOF'
#!/bin/bash
# Tune OS : affiche l'URL et l'IP réelles sur l'écran de login.
case "$2" in
    up|dhcp4-change|hostname) ;;
    *) exit 0 ;;
esac
IP=$(hostname -I 2>/dev/null | awk '{print $1}')
HN=$(hostname)
{
    echo ""
    if [ -n "$IP" ]; then
        echo "  Tune OS — Web UI : http://${HN}.local   (IP : http://${IP})"
    else
        echo "  Tune OS — Web UI : http://${HN}.local"
    fi
    echo ""
} > /etc/issue
exit 0
EOF
chmod +x "${ROOTFS}/etc/NetworkManager/dispatcher.d/50-tune-issue"

# --- Install GRUB (UEFI, required + BIOS/Legacy, best-effort) ---
#
# Previously UEFI-only, and every failure (grub-install itself, the manual
# EFI-binary fallback, update-grub) was swallowed with `|| true` with no
# check afterward that a bootloader actually ended up on disk — an image
# with NO bootloader at all could reach "ok GRUB installed" and get
# published as a working build. Two independent testers reported images
# that don't boot; this is very likely why, and would have been invisible
# without actually mounting a published image to look (see PR discussion).
#
# Now: UEFI failure is FATAL (that's the regression — this used to work),
# real errors are shown instead of hidden, and a post-install check
# confirms EFI/BOOT/BOOTX64.EFI actually exists before declaring success.
# BIOS/Legacy (--target=i386-pc, into the new bios_grub partition above) is
# best-effort on top: a failure there is a warning, not a build-breaker,
# since it's a net-new capability that didn't exist before this fix either.
log "Installing GRUB bootloader (UEFI + BIOS/Legacy)..."

UEFI_OK=1
if ! chroot "$ROOTFS" grub-install --target=x86_64-efi \
        --efi-directory=/boot/efi \
        --bootloader-id=tune \
        --removable \
        --no-nvram; then
    err "grub-install --target=x86_64-efi failed — trying the manual EFI binary fallback"
    mkdir -p "${ROOTFS}/boot/efi/EFI/BOOT"
    cp "${ROOTFS}/usr/lib/grub/x86_64-efi/monolithic/grubx64.efi" \
       "${ROOTFS}/boot/efi/EFI/BOOT/BOOTX64.EFI" || UEFI_OK=0
fi
[[ -f "${ROOTFS}/boot/efi/EFI/BOOT/BOOTX64.EFI" ]] || UEFI_OK=0

if [[ "$UEFI_OK" -ne 1 ]]; then
    err "EFI/BOOT/BOOTX64.EFI is absent after install + fallback — this image would NOT boot on real UEFI hardware. Aborting instead of publishing it."
    exit 1
fi
ok "UEFI bootloader confirmed present (EFI/BOOT/BOOTX64.EFI)"

if chroot "$ROOTFS" grub-install --target=i386-pc "$LOOP_DEV"; then
    ok "BIOS/Legacy bootloader installed"
else
    err "grub-install --target=i386-pc failed — this image won't boot on a machine set to Legacy/CSM (UEFI boot still works)"
fi

# GRUB config — single grub.cfg under /boot/grub, used by both the UEFI and
# the BIOS/Legacy core.img (each just locates it differently at early boot).
cat > "${ROOTFS}/etc/default/grub" <<EOF
GRUB_DEFAULT=0
GRUB_TIMEOUT=3
GRUB_CMDLINE_LINUX_DEFAULT="quiet"
GRUB_CMDLINE_LINUX=""
GRUB_DISABLE_OS_PROBER=true
EOF
chroot "$ROOTFS" update-grub

ok "GRUB installed"

# --- Cleanup ---
log "Cleaning up rootfs..."
rm -f "${ROOTFS}/usr/sbin/policy-rc.d"
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
echo "  SSH login: tune / temporary password generated on first boot (physical console)"
echo "  Web UI:        http://tune.local:8888"
