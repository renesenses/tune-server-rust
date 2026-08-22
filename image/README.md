# Tune OS — Installable Images

Bootable disk images with Tune Server pre-installed on minimal Debian 12.
Flash to a NUC, mini-PC, or Raspberry Pi — boot — Tune runs.

## Quick Start

### Build NUC/x86_64 image (on a Linux x86_64 host)

```bash
sudo apt install debootstrap parted dosfstools grub-efi-amd64-bin
sudo ./build-nuc-image.sh --version 0.8.157
```

### Build RPi4/aarch64 image (on any Linux host)

```bash
sudo apt install debootstrap parted dosfstools qemu-user-static binfmt-support
sudo ./build-rpi4-image.sh --version 0.8.157
```

### Build Allwinner/sunxi image (on any Linux host)

For H616/H618 boards used as a **network source**: the aarch64 release
binary is built without `local-audio`, so this image has no local audio
output at all — playback goes to DLNA/Chromecast/OpenHome/Squeezebox
renderers on the network.

```bash
sudo apt install debootstrap parted qemu-user-static binfmt-support
sudo ./build-sunxi-image.sh --version 0.9.16 --board orangepi-zero2
```

Known boards: `orangepi-zero2` (H616, gigabit), `orangepi-zero3` (H618),
`orangepi-zero2w` (H618). For a board with no upstream DTB — a TV box —
use `--board custom` with an explicit blob and device tree:

```bash
sudo ./build-sunxi-image.sh --board custom \
     --uboot-bin ./u-boot-sunxi-with-spl.bin \
     --dtb allwinner/sun50i-h618-my-box.dtb
```

Unlike the Raspberry Pi, sunxi has no FAT firmware partition: the script
writes the SPL+U-Boot blob raw at 8 KiB and U-Boot loads the kernel from
`/boot` over ext4 via extlinux. The serial console (`ttyS0` @ 115200) is
enabled — it is the first thing you need if a boot goes wrong.

### Flash to disk

```bash
sudo dd if=output/tune-os-x86_64-v0.8.157.img of=/dev/sdX bs=4M status=progress
```

Or use [balenaEtcher](https://etcher.balena.io/) / [Rufus](https://rufus.ie/).

## What's inside

- **Debian 12 (bookworm)** minimal headless
- **Tune Server** with web client at `http://tune.local:8888`
- **Appliance mode** (`/etc/tune-appliance`): WiFi setup from the web UI
  (Settings → Network) after a first boot on ethernet — see `docs/APPLIANCE.md`
- **WiFi firmware** (non-free): Intel, Realtek, Atheros, Broadcom
- **USB storage auto-mount** under `/media` (exFAT/NTFS/FAT/ext4), indexed
  automatically as a music folder
- **SMB/CIFS tools** for NAS shares mounted from the web UI
- **ALSA** with USB audio support (auto-detected)
- **avahi-daemon** for mDNS (`.local` discovery)
- **NetworkManager** (DHCP auto on all interfaces)
- **SSH** enabled (user: `tune`, password: `tune`)
- **Auto-resize** root partition on first boot
- **systemd** service with audio RT priority

## CI build

The `Tune OS Image` GitHub Actions workflow (`.github/workflows/tune-os.yml`,
manual dispatch) builds the x86_64 image for an existing release and attaches
`tune-os-x86_64-vX.Y.Z.img.gz` (+ `.sha256`) to it.

## Default credentials

- **User:** `tune`
- **Password:** `tune`
- **Web UI:** `http://tune.local:8888`

## Mount music storage

```bash
# NAS via SMB/CIFS
sudo mount -t cifs //nas-ip/music /mnt/music -o guest

# NAS via NFS
sudo mount -t nfs nas-ip:/music /mnt/music

# USB drive (auto-mounted if labeled)
# Plug in → appears at /media/tune/LABEL
```

Add to `/etc/fstab` for permanent mount.

## Supported hardware

| Platform | Architecture | Status |
|----------|-------------|--------|
| Intel NUC (Gen 7-13) | x86_64 | Supported |
| Mini-PC (Beelink, MeLe, etc.) | x86_64 | Supported |
| Raspberry Pi 4 | aarch64 | Supported |
| Raspberry Pi 5 | aarch64 | Supported |
| Generic x86_64 PC | x86_64 | Supported |
| Odroid / Rock Pi | aarch64 | Untested |
| Orange Pi Zero 2 (H616) | aarch64 | Builds, first boot not yet validated |
| Orange Pi Zero 3 / Zero 2W (H618) | aarch64 | Untested |
| Allwinner TV box (custom DTB) | aarch64 | Bring-up required |
