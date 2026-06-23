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

### Flash to disk

```bash
sudo dd if=output/tune-os-x86_64-v0.8.157.img of=/dev/sdX bs=4M status=progress
```

Or use [balenaEtcher](https://etcher.balena.io/) / [Rufus](https://rufus.ie/).

## What's inside

- **Debian 12 (bookworm)** minimal headless
- **Tune Server** with web client at `http://tune.local:8888`
- **ALSA** with USB audio support (auto-detected)
- **avahi-daemon** for mDNS (`.local` discovery)
- **NetworkManager** (DHCP auto on all interfaces)
- **SSH** enabled (user: `tune`, password: `tune`)
- **Auto-resize** root partition on first boot
- **systemd hardened** service with audio RT priority

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
