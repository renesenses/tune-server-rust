#!/usr/bin/env bash
# ============================================================
# Tune OS — Allwinner (sunxi) Image Builder
# Builds a bootable aarch64 SD card image with Tune Server for
# Allwinner H6/H616/H618 boards.
#
# Reference board: Orange Pi Zero 2 (H616) — gigabit ethernet via
# RTL8211F, upstream DTB, U-Boot defconfig in Debian. Nothing to
# bring up: it boots on a mainline kernel as-is.
#
# Unlike the Raspberry Pi, sunxi has no FAT firmware partition: the
# SPL+U-Boot blob lives raw at 8 KiB and the kernel is loaded from
# /boot on the ext4 root through extlinux (u-boot-menu).
#
# Must be run on a Linux host as root.
# Cross-builds for aarch64 using qemu-user-static.
#
# Usage:
#   sudo ./build-sunxi-image.sh [--version 0.9.16] [--board orangepi-zero2]
#   sudo ./build-sunxi-image.sh --board custom \
#        --uboot-bin /path/u-boot-sunxi-with-spl.bin \
#        --dtb allwinner/sun50i-h618-my-box.dtb
# ============================================================
set -euo pipefail

TUNE_VERSION="latest"
BOARD="orangepi-zero2"
UBOOT_BIN=""
DTB=""
DEBIAN_RELEASE="bookworm"
# Pilote WiFi Unisoc UWE5622 (module « AW859A » des Orange Pi Zero 2/3).
# Commit epingle : c'est celui qu'Armbian reference, et le seul teste ici.
UWE5622_REPO="https://github.com/armbian/uwe5622.git"
UWE5622_COMMIT="d6bec7538a0b4b67e35715ad71eaa056555524cb"
UWE5622_FW_REPO="https://github.com/armbian/firmware.git"
DEBIAN_MIRROR="http://deb.debian.org/debian"
# Kernel from backports by default: H616/H618 support (EMAC, cpufreq,
# thermal) landed and matured well after the 6.1 in bookworm. Use
# --no-backports if you specifically want the release kernel.
USE_BACKPORTS=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)       TUNE_VERSION="$2"; shift 2 ;;
        --board)         BOARD="$2"; shift 2 ;;
        --uboot-bin)     UBOOT_BIN="$2"; shift 2 ;;
        --dtb)           DTB="$2"; shift 2 ;;
        --no-backports)  USE_BACKPORTS=0; shift ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

# Known boards: U-Boot package directory (the defconfig name) and the DTB
# shipped by Debian's linux-image-arm64. Anything else needs --uboot-bin
# and --dtb — that is the path for an anonymous TV box board, whose DTB is
# not upstream and whose DRAM parameters have to be extracted from the
# stock Android boot0 first.
case "$BOARD" in
    orangepi-zero2)
        UBOOT_DIR="orangepi_zero2"
        BOARD_DTB="allwinner/sun50i-h616-orangepi-zero2.dtb"
        # Bus SDIO qui porte le module WiFi soudé (AW859A). Upstream laisse ce
        # nœud `disabled` — le pinmux et cap-sdio-irq sont pourtant déjà là.
        SDIO_WIFI_NODE="mmc@4021000"
        UWE5622="yes"
        ;;
    orangepi-zero3)
        UBOOT_DIR="orangepi_zero3"
        BOARD_DTB="allwinner/sun50i-h618-orangepi-zero3.dtb"
        ;;
    orangepi-zero2w)
        # H618. Added to U-Boot after bookworm's 2023.01 — expect to need
        # --uboot-bin with a self-built blob here.
        UBOOT_DIR="orangepi_zero2w"
        BOARD_DTB="allwinner/sun50i-h618-orangepi-zero2w.dtb"
        ;;
    custom)
        UBOOT_DIR=""
        BOARD_DTB=""
        SDIO_WIFI_NODE=""
        ;;
    *) echo "Unknown board: $BOARD (orangepi-zero2, orangepi-zero3, orangepi-zero2w, custom)" >&2; exit 1 ;;
esac
[[ -n "$DTB" ]] && BOARD_DTB="$DTB"

if [[ "$BOARD" == "custom" ]]; then
    if [[ -z "$UBOOT_BIN" || -z "$BOARD_DTB" ]]; then
        echo "--board custom requires both --uboot-bin and --dtb" >&2
        exit 1
    fi
fi

IMAGE_NAME="tune-os-${BOARD}"
IMAGE_SIZE="4G"   # 790 Mo utiles, + la chaine de build temporaire du pilote WiFi
WORK_DIR="/tmp/tune-os-build-sunxi"
ROOTFS="${WORK_DIR}/rootfs"
IMAGE_FILE="${WORK_DIR}/${IMAGE_NAME}.img"
LOOP_DEV=""
HOSTNAME="tune"
# sunxi BROM reads the SPL at 8 KiB; the SPL+ATF+U-Boot FIT that follows
# stays well under 1 MiB. Start the root partition at 8 MiB so a rebuilt,
# larger U-Boot never overwrites the filesystem.
UBOOT_SEEK_KIB=8
PART_START="8MiB"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

log() { echo -e "${BLUE}[tune-os]${NC} $*"; }
ok()  { echo -e "${GREEN}[  OK  ]${NC} $*"; }
err() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

cleanup() {
    log "Cleaning up..."
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

if [[ "$(uname -m)" != "aarch64" ]] && ! command -v qemu-aarch64-static &>/dev/null; then
    err "Cross-build requires qemu-user-static: apt install qemu-user-static binfmt-support"
    exit 1
fi

for tool in debootstrap parted mkfs.ext4 losetup blkid dtc git; do
    command -v "$tool" &>/dev/null || { err "Missing tool: $tool"; exit 1; }
done

# --- Resolve version ---
if [[ "$TUNE_VERSION" == "latest" ]]; then
    TUNE_VERSION=$(curl -sL "https://api.github.com/repos/renesenses/tune-server-rust/releases/latest" \
        | grep '"tag_name"' | head -1 | sed 's/.*"v\(.*\)".*/\1/')
fi
log "Building Tune OS for ${BOARD} with Tune Server v${TUNE_VERSION}"

# linux-aarch64 = the glibc build, which is what a Debian rootfs wants.
# Its release features are `oaat,postgres` — no `local-audio`, so the
# binary links no ALSA at all. Correct for a board used as a network
# source: playback goes out to DLNA/Chromecast/OpenHome/Squeezebox
# renderers, never to a local card.
TUNE_TARBALL_URL="https://github.com/renesenses/tune-server-rust/releases/download/v${TUNE_VERSION}/tune-server-v${TUNE_VERSION}-linux-aarch64.tar.gz"

# --- Create image ---
log "Creating ${IMAGE_SIZE} image..."
rm -rf "$WORK_DIR"
mkdir -p "$WORK_DIR"
truncate -s "$IMAGE_SIZE" "$IMAGE_FILE"

# Single ext4 partition: U-Boot reads the kernel from /boot over ext4, so
# there is no boot partition to keep in sync.
parted -s "$IMAGE_FILE" mklabel msdos
parted -s "$IMAGE_FILE" mkpart primary ext4 "$PART_START" 100%

LOOP_DEV=$(losetup --find --show --partscan "$IMAGE_FILE")
PART_ROOT="${LOOP_DEV}p1"
sleep 1
partprobe "$LOOP_DEV" 2>/dev/null || true
sleep 1

mkfs.ext4 -L tuneroot -q "$PART_ROOT"
mkdir -p "$ROOTFS"
mount "$PART_ROOT" "$ROOTFS"

# --- Bootstrap ---
log "Bootstrapping Debian ${DEBIAN_RELEASE} for aarch64..."
debootstrap --arch=arm64 --variant=minbase --foreign \
    --components=main,contrib,non-free-firmware \
    --include=systemd,systemd-sysv \
    "$DEBIAN_RELEASE" "$ROOTFS" "$DEBIAN_MIRROR" || {
    err "debootstrap failed — last lines of debootstrap.log:"
    tail -n 200 "${ROOTFS}/debootstrap/debootstrap.log" 2>/dev/null || true
    exit 1
}

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

# --- Packages via apt (proper dependency ordering, as on the NUC image) ---
cat > "${ROOTFS}/etc/apt/sources.list" <<EOF
deb ${DEBIAN_MIRROR} ${DEBIAN_RELEASE} main contrib non-free-firmware
deb http://security.debian.org/debian-security ${DEBIAN_RELEASE}-security main contrib non-free-firmware
EOF
if [[ "$USE_BACKPORTS" -eq 1 ]]; then
    echo "deb ${DEBIAN_MIRROR} ${DEBIAN_RELEASE}-backports main contrib non-free-firmware" \
        >> "${ROOTFS}/etc/apt/sources.list"
    KERNEL_PKG="linux-image-arm64/${DEBIAN_RELEASE}-backports"
else
    KERNEL_PKG="linux-image-arm64"
fi

# Le blob U-Boot de Debian ne sert que si --uboot-bin n'est pas fourni.
# bookworm ne package que 13 cartes sunxi, aucune en H616/H618 : pour ces
# cartes le blob se construit depuis les sources (cf. image/README.md).
if [[ -n "$UBOOT_BIN" ]]; then
    UBOOT_PKG=""
else
    UBOOT_PKG="u-boot-sunxi"
fi

printf '#!/bin/sh\nexit 101\n' > "${ROOTFS}/usr/sbin/policy-rc.d"
chmod +x "${ROOTFS}/usr/sbin/policy-rc.d"

# libstdc++6: the release binary's only non-base shared dependency. Verified
# on the shipped tarball — `objdump -p tune-server` lists libstdc++.so.6,
# libgcc_s, libdl, libpthread, libm, libc. Nothing else, in particular no
# libasound (built without `local-audio`) and no libopus (linked static).
# Do not rely on it arriving transitively: a minbase rootfs may not have it,
# and the failure is the server refusing to start at all.
# u-boot-menu generates /boot/extlinux/extlinux.conf on every kernel
# upgrade; u-boot-sunxi ships the board's SPL+U-Boot blob.
# cifs-utils + nfs-common: the library lives on a NAS.
log "Installing packages via apt..."
chroot "$ROOTFS" bash -ec "
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
        dbus udev kmod ${KERNEL_PKG} u-boot-menu ${UBOOT_PKG} \
        libstdc++6 \
        sudo curl ca-certificates avahi-daemon libnss-mdns \
        network-manager wpasupplicant wireless-regdb openssh-server \
        firmware-realtek firmware-brcm80211 firmware-atheros \
        cifs-utils smbclient nfs-common exfatprogs ntfs-3g \
        cloud-guest-utils e2fsprogs iputils-ping sqlite3 \
        systemd-timesyncd \
        device-tree-compiler usbutils iw rfkill wireless-tools \
        locales procps iproute2 less nano
"
ok "Packages installed"

# --- Enable the SDIO bus carrying the on-board WiFi module ---
# Upstream ships this board's mmc1 as `status = "disabled"`, so the soldered
# WiFi module never appears — no interface, whatever the antenna is doing. The
# node is otherwise complete (mmc1 pinmux on PG0-PG5, cap-sdio-irq,
# mmc-ddr-3_3v), so flipping the status is enough for the SDIO device to
# enumerate. Whether a *driver* then binds depends on the chip; the enumeration
# itself is what identifies it (`dmesg | grep mmc1`).
if [[ -n "${SDIO_WIFI_NODE:-}" ]]; then
    KVER=$(basename "$(ls -1 "${ROOTFS}"/boot/vmlinuz-* | sort -V | tail -1)" | sed 's/^vmlinuz-//')
    DTB_FILE="${ROOTFS}/usr/lib/linux-image-${KVER}/${BOARD_DTB}"
    if [[ ! -f "$DTB_FILE" ]]; then
        err "DTB introuvable pour le patch SDIO : ${DTB_FILE}"; exit 1
    fi
    log "Enabling ${SDIO_WIFI_NODE} (on-board WiFi SDIO bus) in the board DTB..."
    dtc -I dtb -O dts "$DTB_FILE" > "${WORK_DIR}/board.dts" 2>/dev/null
    SDIO_WIFI_NODE="$SDIO_WIFI_NODE" python3 - "${WORK_DIR}/board.dts" <<'PATCH'
import os, re, sys, pathlib

# Upstream ships mmc1 disabled on these boards, so the soldered WiFi module
# never appears. Three things are needed, each found by comparing with the
# H616/H618 boards that *do* declare WiFi in mainline (BigTreeTech CB1,
# Transpeed 8K618-T) and verified on hardware one at a time:
#   1. status = "okay" on the bus            -> controller probes
#   2. mmc-pwrseq releasing WL-REG-ON (PG18) -> chip answers, but -EINVAL
#      + PG10 muxed as the 32 kHz clock out     (its external clock)
#   3. vmmc-supply, copied from mmc0         -> voltage window valid, card
#                                               enumerates
# Phandles differ per DTB, so they are resolved from the tree, never hardcoded.

node_name = os.environ["SDIO_WIFI_NODE"]
p = pathlib.Path(sys.argv[1])
t = p.read_text()

def find(name):
    m = re.search(r"(\t\t" + re.escape(name) + r" \{.*?\n\t\t\};\n)", t, re.S)
    if not m:
        sys.exit("noeud %s introuvable" % name)
    return m

def phandle_of(name):
    inner = find(name).group(1)
    ph = re.findall(r"^\t\t\tphandle = <(0x[0-9a-f]+)>;", inner, re.M)
    if len(ph) != 1:
        sys.exit("%s : %d phandle(s) a la racine, 1 attendu" % (name, len(ph)))
    return ph[0]

pio = phandle_of("pinctrl@300b000")
rtc = phandle_of("rtc@7000000")
used = {int(x, 16) for x in re.findall(r"phandle = <(0x[0-9a-f]+)>", t)}
ph_pwrseq = "0x%x" % (max(used) + 1)
ph_clkpin = "0x%x" % (max(used) + 2)

# vmmc : le meme rail que la carte SD (vcc-eth-mmc sur la Zero 2). Sans lui la
# fenetre de tension du controleur est vide et l init SDIO rend -EINVAL.
sd = re.search(r"vmmc-supply = <(0x[0-9a-f]+)>;", find("mmc@4020000").group(1))
if not sd:
    sys.exit("mmc0 n a pas de vmmc-supply : rien a copier pour mmc1")
vmmc = sd.group(1)

# --- le bus lui-meme ---
m = find(node_name)
node = m.group(1)
needle = 'status = "disabled";'
if node.count(needle) != 1:
    sys.exit("%s : un seul status=disabled attendu, %d trouve(s)"
             % (node_name, node.count(needle)))
patched = node.replace(
    "\t\t\t" + needle,
    '\t\t\tstatus = "okay";\n'
    "\t\t\tvmmc-supply = <%s>;\n" % vmmc +
    "\t\t\tbus-width = <0x04>;\n"
    "\t\t\tnon-removable;\n"
    "\t\t\tkeep-power-in-suspend;\n"
    "\t\t\tmmc-pwrseq = <%s>;" % ph_pwrseq)
if patched == node:
    sys.exit("%s : le remplacement du status n a rien change" % node_name)
patched = patched[:-len("\t\t};\n")] + \
    "\n\t\t\twifi@1 {\n\t\t\t\treg = <0x01>;\n\t\t\t};\n\t\t};\n"
t = t[:m.start(1)] + patched + t[m.end(1):]

# --- PG10 en sortie d horloge 32 kHz ---
mp = find("pinctrl@300b000")
grp = ('\t\t\tx32clk-fanout-pin {\n\t\t\t\tpins = "PG10";\n'
       '\t\t\t\tfunction = "clock";\n\t\t\t\tphandle = <%s>;\n\t\t\t};\n\n'
       % ph_clkpin)
inner = mp.group(1)
t = t[:mp.start(1)] + inner[:-len("\t\t};\n")] + grp + "\t\t};\n" + t[mp.end(1):]

# --- la sequence de mise sous tension, a la racine ---
anchor = "\tleds {"
if anchor not in t:
    sys.exit("ancre 'leds' introuvable a la racine du DTS")
pwrseq = ("\twifi-pwrseq {\n"
          '\t\tcompatible = "mmc-pwrseq-simple";\n'
          "\t\tclocks = <%s 0x01>;\n" % rtc +
          '\t\tclock-names = "ext_clock";\n'
          "\t\tpinctrl-0 = <%s>;\n" % ph_clkpin +
          '\t\tpinctrl-names = "default";\n'
          "\t\treset-gpios = <%s 0x06 0x12 0x01>;\n" % pio +
          "\t\tpost-power-on-delay-ms = <0xc8>;\n"
          "\t\tphandle = <%s>;\n" % ph_pwrseq +
          "\t};\n\n")
t = t.replace(anchor, pwrseq + anchor, 1)
p.write_text(t)
PATCH
    dtc -I dts -O dtb -o "$DTB_FILE" "${WORK_DIR}/board.dts" 2>/dev/null
    # -A24 : `status` se trouve une dizaine de lignes après l'ouverture du
    # nœud ; une fenêtre trop courte fait échouer la vérification, pas le patch.
    dtc -I dtb -O dts "$DTB_FILE" 2>/dev/null \
        | grep -A24 "${SDIO_WIFI_NODE} {" | grep -q 'status = "okay"' \
        || { err "le patch SDIO n'a pas pris dans ${DTB_FILE}"; exit 1; }
    ok "${SDIO_WIFI_NODE} enabled"
fi

# --- Unisoc UWE5622 WiFi driver (out-of-tree) ---
# The soldered "AW859A" module is a Unisoc UWE5622. No in-tree driver matches
# it — the chip reports vendor=0x0000/device=0x0000, and the out-of-tree driver
# is written for exactly that: its SDIO table is `{SDIO_DEVICE(0, 0)}` and it
# probes `/sys/bus/sdio/devices/mmc1:8800:1`, which is what the DTB patch above
# produces. Source and firmware come from Armbian, at a pinned commit.
#
# Built inside the chroot so the image is self-contained and the build fails
# loudly if the driver ever stops compiling against the shipped kernel. The
# toolchain is purged afterwards.
if [[ "${UWE5622:-}" == "yes" ]]; then
    KVER=$(basename "$(ls -1 "${ROOTFS}"/boot/vmlinuz-* | sort -V | tail -1)" | sed 's/^vmlinuz-//')
    log "Building the Unisoc UWE5622 WiFi driver (${UWE5622_COMMIT:0:12}) for ${KVER}..."
    git clone -q "$UWE5622_REPO" "${WORK_DIR}/uwe5622" \
        || { err "clone du pilote uwe5622 impossible"; exit 1; }
    git -C "${WORK_DIR}/uwe5622" checkout -q "$UWE5622_COMMIT" \
        || { err "commit ${UWE5622_COMMIT} introuvable"; exit 1; }
    # tty-sdio hardcodes an in-tree include path; make it relative so the
    # module can be built out of tree.
    sed -i 's|-I$(srctree)/drivers/net/wireless/uwe5622/unisocwcn/include|-I$(src)/../unisocwcn/include|' \
        "${WORK_DIR}/uwe5622/tty-sdio/Makefile"
    mkdir -p "${ROOTFS}/usr/src"
    cp -R "${WORK_DIR}/uwe5622" "${ROOTFS}/usr/src/uwe5622"

    chroot "$ROOTFS" bash -ec "
        export DEBIAN_FRONTEND=noninteractive
        apt-get install -y -qq --no-install-recommends \
            build-essential bc linux-headers-${KVER} >/dev/null
        make -C /lib/modules/${KVER}/build M=/usr/src/uwe5622 modules \
            CONFIG_AW_WIFI_DEVICE_UWE5622=y \
            CONFIG_WLAN_UWE5622=m \
            CONFIG_TTY_OVERY_SDIO=m \
            CONFIG_AW_BOARD=y -j\$(nproc)
        mkdir -p /lib/modules/${KVER}/updates
        for ko in \$(find /usr/src/uwe5622 -name '*.ko'); do
            strip --strip-debug \"\$ko\"
            cp \"\$ko\" /lib/modules/${KVER}/updates/
        done
        depmod -a ${KVER}
    " || { err "compilation du pilote uwe5622 echouee"; exit 1; }

    for m in uwe5622_bsp_sdio sprdwl_ng sprdbt_tty; do
        [[ -f "${ROOTFS}/lib/modules/${KVER}/updates/${m}.ko" ]] \
            || { err "module ${m}.ko manquant apres compilation"; exit 1; }
    done

    log "Fetching UWE5622 firmware..."
    git clone -q --depth 1 --filter=blob:none --sparse "$UWE5622_FW_REPO" \
        "${WORK_DIR}/uwe5622-fw" || { err "clone du firmware impossible"; exit 1; }
    git -C "${WORK_DIR}/uwe5622-fw" sparse-checkout set uwe5622 >/dev/null 2>&1
    [[ -f "${WORK_DIR}/uwe5622-fw/uwe5622/wcnmodem.bin" ]] \
        || { err "wcnmodem.bin introuvable dans le depot firmware"; exit 1; }
    mkdir -p "${ROOTFS}/lib/firmware/uwe5622"
    cp "${WORK_DIR}/uwe5622-fw/uwe5622/"* "${ROOTFS}/lib/firmware/uwe5622/"
    # The per-board RF calibration .ini must also sit directly in /lib/firmware:
    # the driver builds its path from WIFI_BOARD_CFG_PATH, which falls back to
    # "/lib/firmware" unless UNISOC_WIFI_CUS_CONFIG was defined at build time.
    # Without this the chip boots and runs, then aborts with
    # `[CMD] WIFI_CMD_DOWNLOAD_INI, [REASON] LOAD_INI_DATA_FAILED` and no
    # interface appears. Verified on hardware.
    cp "${WORK_DIR}/uwe5622-fw/uwe5622/"*.ini "${ROOTFS}/lib/firmware/"

    # The BSP module auto-loads on the SDIO alias; the WiFi one does not.
    echo sprdwl_ng > "${ROOTFS}/etc/modules-load.d/uwe5622.conf"

    # Drop the toolchain: it exists only to build the module above.
    chroot "$ROOTFS" bash -ec "
        export DEBIAN_FRONTEND=noninteractive
        apt-get purge -y -qq build-essential bc linux-headers-${KVER} \
            gcc g++ cpp make libc6-dev >/dev/null 2>&1 || true
        apt-get autoremove --purge -y -qq >/dev/null 2>&1 || true
    "
    rm -rf "${ROOTFS}/usr/src/uwe5622"
    ok "UWE5622 driver + firmware installed"
fi

# --- Bootloader ---
if [[ -z "$UBOOT_BIN" ]]; then
    UBOOT_BIN="${ROOTFS}/usr/lib/u-boot/${UBOOT_DIR}/u-boot-sunxi-with-spl.bin"
fi
if [[ ! -f "$UBOOT_BIN" ]]; then
    err "U-Boot blob not found: ${UBOOT_BIN}"
    err "Debian's u-boot-sunxi may not carry this board — build U-Boot"
    err "(${UBOOT_DIR}_defconfig) and pass it with --uboot-bin."
    exit 1
fi
log "Writing U-Boot at ${UBOOT_SEEK_KIB} KiB ($(basename "$UBOOT_BIN"))..."
dd if="$UBOOT_BIN" of="$LOOP_DEV" bs=1024 seek="$UBOOT_SEEK_KIB" conv=notrunc,fsync status=none
ok "U-Boot written"

# --- System configuration ---
echo "$HOSTNAME" > "${ROOTFS}/etc/hostname"
cat > "${ROOTFS}/etc/hosts" <<EOF
127.0.0.1   localhost
127.0.1.1   ${HOSTNAME}
EOF

chroot "$ROOTFS" bash -c "echo 'en_US.UTF-8 UTF-8' > /etc/locale.gen && locale-gen"
chroot "$ROOTFS" ln -sf /usr/share/zoneinfo/UTC /etc/localtime

ROOT_UUID=$(blkid -s UUID -o value "$PART_ROOT")
cat > "${ROOTFS}/etc/fstab" <<EOF
UUID=${ROOT_UUID}  /  ext4  errors=remount-ro  0 1
EOF

# extlinux entries for U-Boot's sysboot. The DTB is pinned explicitly
# rather than left to fdtdir guessing: on a board brought up by hand, a
# silently wrong DTB is the hardest failure to diagnose.
# console=ttyS0,115200 is the sunxi debug UART — the 13-pin header on the
# Orange Pi Zero 2, and the first thing you need when a boot goes wrong.
cat > "${ROOTFS}/etc/default/u-boot" <<EOF
U_BOOT_UPDATE="true"
U_BOOT_ROOT="root=UUID=${ROOT_UUID}"
U_BOOT_PARAMETERS="rootwait rw console=ttyS0,115200 console=tty1 consoleblank=0"
U_BOOT_FDT="${BOARD_DTB}"
U_BOOT_FDT_DIR="/usr/lib/linux-image-"
U_BOOT_TIMEOUT="3"
U_BOOT_MENU_LABEL="Tune OS"
EOF

EXTLINUX="${ROOTFS}/boot/extlinux/extlinux.conf"
chroot "$ROOTFS" u-boot-update || err "u-boot-update failed"
if ! grep -q "$BOARD_DTB" "$EXTLINUX" 2>/dev/null; then
    log "extlinux.conf missing or does not pin ${BOARD_DTB} — writing it by hand"
    KVER=$(basename "$(ls -1 "${ROOTFS}"/boot/vmlinuz-* | sort -V | tail -1)" | sed 's/^vmlinuz-//')
    mkdir -p "${ROOTFS}/boot/extlinux"
    cat > "${ROOTFS}/boot/extlinux/extlinux.conf" <<EOF
default l0
timeout 30
menu title Tune OS

label l0
    menu label Tune OS, kernel ${KVER}
    linux /boot/vmlinuz-${KVER}
    initrd /boot/initrd.img-${KVER}
    fdt /usr/lib/linux-image-${KVER}/${BOARD_DTB}
    append root=UUID=${ROOT_UUID} rootwait rw console=ttyS0,115200 console=tty1
EOF
fi
grep -q "$BOARD_DTB" "$EXTLINUX" \
    || { err "extlinux.conf still does not reference ${BOARD_DTB}"; exit 1; }
ok "Boot configuration written ($(basename "$BOARD_DTB"))"

sed -i 's/^hosts:.*/hosts: files mdns4_minimal [NOTFOUND=return] dns/' "${ROOTFS}/etc/nsswitch.conf"

cat > "${ROOTFS}/etc/NetworkManager/conf.d/tune.conf" <<EOF
[main]
plugins=keyfile

[connection]
wifi.powersave=2

[device]
wifi.scan-rand-mac-address=no
EOF

# Kernel log readable by the tune user: hardware diagnosis on an appliance
# happens over the serial console, and `dmesg` denied is a dead end.
echo 'kernel.dmesg_restrict = 0' > "${ROOTFS}/etc/sysctl.d/10-tune-dmesg.conf"

# Appliance marker: unlocks /api/v1/appliance (network setup from the web
# UI) and the appliance flag in /system/config — see docs/APPLIANCE.md
echo "Tune OS appliance image (${BOARD})" > "${ROOTFS}/etc/tune-appliance"

# USB storage: auto-mount partitions under /media/<kernel> (headless, no
# udisks session). Same rule as the NUC image.
mkdir -p "${ROOTFS}/media"
cat > "${ROOTFS}/etc/udev/rules.d/99-tune-usb-mount.rules" <<'EOF'
ACTION=="add", SUBSYSTEMS=="usb", SUBSYSTEM=="block", ENV{ID_FS_USAGE}=="filesystem", RUN+="/usr/bin/systemd-mount --no-block --automount=yes --collect $devnode /media/%k"
ACTION=="remove", SUBSYSTEMS=="usb", SUBSYSTEM=="block", ENV{ID_FS_USAGE}=="filesystem", RUN+="/usr/bin/systemd-umount /media/%k"
EOF

mkdir -p "${ROOTFS}/etc/ssh/sshd_config.d"
cat > "${ROOTFS}/etc/ssh/sshd_config.d/tune.conf" <<EOF
PermitRootLogin no
PasswordAuthentication yes
EOF

chroot "$ROOTFS" bash -c "
    useradd -m -s /bin/bash -G sudo,plugdev tune
    echo 'tune:tune' | chpasswd
    echo 'tune ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/tune
"

# --- Install Tune ---
# TUNE_TARBALL_PATH : archive déjà récupérée par l'appelant (la CI la télécharge
# avec le jeton, seule façon de lire les assets d'une release en BROUILLON —
# c'est l'état dans lequel naît toute release depuis #1588). Sans lui, on
# retombe sur l'URL publique, qui ne marche que sur une release publiée.
if [[ -n "${TUNE_TARBALL_PATH:-}" && -f "${TUNE_TARBALL_PATH}" ]]; then
    log "Using pre-fetched Tune Server tarball: ${TUNE_TARBALL_PATH}"
    cp "${TUNE_TARBALL_PATH}" "${WORK_DIR}/tune.tar.gz"
else
    log "Downloading Tune Server v${TUNE_VERSION} (aarch64)..."
    # -f : un 404 doit échouer ici, et non se transformer en page HTML écrite
    # dans tune.tar.gz — non vide, donc indétectable par un simple test -s.
    curl -fsSL "$TUNE_TARBALL_URL" -o "${WORK_DIR}/tune.tar.gz" \
        || { err "Download failed: ${TUNE_TARBALL_URL}"; exit 1; }
fi

if [[ ! -s "${WORK_DIR}/tune.tar.gz" ]]; then
    err "Empty tarball: ${TUNE_TARBALL_URL}"; exit 1
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
mkdir -p "${ROOTFS}/opt/tune/data" "${ROOTFS}/mnt/music"

# Flat keys, no TOML sections: TuneConfig deserializes every field at the top
# level, and serde silently drops unknown tables — a `[server]` / `[library]`
# layout parses "fine" and yields nothing but defaults. That bug leaves
# music_dirs empty, which startup.rs then persists as "[]" in the database,
# where the first-run guard treats it as deliberately set: the image never
# scans anything and no amount of editing the file afterwards helps.
# /media covers auto-mounted USB drives, /mnt/music the NAS mounts.
cat > "${ROOTFS}/opt/tune/tune.toml" <<EOF
# Tune OS default configuration
# Edit via web UI at http://tune.local:8888/settings

port = 8888
db_path = "/opt/tune/data/tune.db"
web_dir = "/opt/tune/web"
artwork_dir = "/opt/tune/data/artwork_cache"
auto_scan = true
log_level = "info"

music_dirs = ["/mnt/music", "/media"]
EOF

# Root, like the NUC appliance image: the server drives nmcli (network
# setup) and mount.cifs (SMB shares) itself — cf. /etc/tune-appliance.
cat > "${ROOTFS}/etc/systemd/system/tune.service" <<EOF
[Unit]
Description=Tune Music Server
After=network-online.target avahi-daemon.service
Wants=network-online.target

[Service]
Type=simple
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

# Hardening (root, mais système en lecture seule hors chemins listés).
# /opt/tune entier : l'auto-update remplace le binaire et web/ in place.
# /mnt entier : les partages SMB montés depuis l'UI apparaissent sous /mnt.
# Pas de NoNewPrivileges : mount.cifs est un helper setuid.
ProtectSystem=strict
ReadWritePaths=/opt/tune /mnt /media /tmp
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

# Port 80 -> 8888 proxy: browsers silently upgrade http:// links to https://
# and the TLS attempt on :8888 is refused. Port 80 lets the printed URL
# drop the port. See build-nuc-image.sh for details.
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

# --- First boot: unique machine-id, grow the filesystem, unique hostname ---
cat > "${ROOTFS}/opt/tune/first-boot.sh" <<'FIRSTBOOT'
#!/bin/bash
# Tune OS first-boot setup. Runs once, then disables itself.

MARKER="/opt/tune/data/.first-boot-done"
if [[ -f "$MARKER" ]]; then
    exit 0
fi

systemd-machine-id-setup

# Grow the root partition to fill the card (the image is 2G; cards are not)
ROOT_PART=$(findmnt -n -o SOURCE /)
ROOT_DISK=$(lsblk -ndo pkname "$ROOT_PART")
PART_NUM=$(echo "$ROOT_PART" | grep -o '[0-9]*$')
if [[ -n "$ROOT_DISK" && -n "$PART_NUM" ]]; then
    growpart "/dev/$ROOT_DISK" "$PART_NUM" 2>/dev/null || true
    resize2fs "$ROOT_PART" 2>/dev/null || true
fi

# Set hostname to tune-XXXX (last 4 of MAC) so several boxes coexist
MAC=$(ip link show | grep -m1 'link/ether' | awk '{print $2}' | tr -d ':' | tail -c 5)
if [[ -n "$MAC" ]]; then
    hostnamectl set-hostname "tune-${MAC}"
    # /etc/hosts doit suivre, sinon chaque sudo perd deux secondes sur
    # « unable to resolve host » (la résolution du nom court échoue).
    sed -i "s/^127\.0\.1\.1.*/127.0.1.1\ttune-${MAC}/" /etc/hosts
    # Keep the printed URL truthful: tune.local dies with the rename
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

# Ces cartes n'ont pas de RTC : sans NTP l'horloge demarre en 1970 puis se cale
# sur un horodatage de build (constate : 116 jours dans le passe). apt refuse
# alors les depots (« Release file is not valid yet »), la validation TLS
# echoue, et les comparaisons de mtime du scanner deviennent douteuses.
# systemd-timesyncd n'est qu'une *recommandation* de systemd, donc absent avec
# --no-install-recommends : il faut le demander explicitement.
chroot "$ROOTFS" systemctl enable systemd-timesyncd

chroot "$ROOTFS" systemctl enable tune.service
chroot "$ROOTFS" systemctl enable tune-first-boot.service
chroot "$ROOTFS" systemctl enable tune-web80.socket
chroot "$ROOTFS" systemctl enable NetworkManager
chroot "$ROOTFS" systemctl enable avahi-daemon
chroot "$ROOTFS" systemctl enable ssh
chroot "$ROOTFS" systemctl enable serial-getty@ttyS0.service

cat > "${ROOTFS}/etc/motd" <<EOF

  ♫  Tune OS v${TUNE_VERSION} (${BOARD})
  ─────────────────────────────────────
  Web UI:    http://tune.local   (ou http://tune.local:8888)
  Music:     NAS/SMB shares: web UI → Settings → Network
             USB drives auto-mount under /media
             (SMB à la main : identifiants dans un fichier 0600, jamais
              dans fstab ; options _netdev,nofail,iocharset=utf8)
  Renderers: réseau seulement (pas de sortie audio locale sur cette image)
  Config:    /opt/tune/tune.toml
  Logs:      journalctl -u tune -f
  Console:   série ttyS0 @ 115200
  User:      tune / tune

EOF

ok "Tune installed on ${BOARD} image"

# --- Cleanup ---
rm -f "${ROOTFS}/usr/sbin/policy-rc.d"
chroot "$ROOTFS" apt-get clean
rm -rf "${ROOTFS}/var/cache/apt/archives"/*.deb
rm -rf "${ROOTFS}/var/lib/apt/lists"/*
rm -f "${ROOTFS}/usr/bin/qemu-aarch64-static"

umount "${ROOTFS}/proc"
umount "${ROOTFS}/sys"
umount "${ROOTFS}/dev/pts"
umount "${ROOTFS}/dev"
umount "${ROOTFS}"

# --- Output ---
OUTPUT_DIR="$(cd "$(dirname "$0")" && pwd)/output"
mkdir -p "$OUTPUT_DIR"
FINAL_IMG="${OUTPUT_DIR}/${IMAGE_NAME}-v${TUNE_VERSION}.img"
cp "$IMAGE_FILE" "$FINAL_IMG"
gzip -kf "$FINAL_IMG"

ok "Build complete!"
echo ""
echo "  Image:  ${FINAL_IMG} ($(du -h "$FINAL_IMG" | cut -f1))"
echo "  GZ:     ${FINAL_IMG}.gz ($(du -h "${FINAL_IMG}.gz" | cut -f1))"
echo ""
echo "  Flash:  sudo dd if=${FINAL_IMG} of=/dev/sdX bs=4M status=progress"
echo "  Login:  tune / tune  (console série ttyS0 @ 115200)"
echo "  Web:    http://tune.local"
