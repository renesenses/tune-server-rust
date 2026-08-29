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

The kernel comes from **bookworm-backports** (6.12) by default: H616/H618
support — EMAC, cpufreq, thermal — matured well after the 6.1 in bookworm.
`--no-backports` falls back to the release kernel.

#### Building U-Boot for an H616/H618 board

`--uboot-bin` is not optional in practice: Debian's `u-boot-sunxi` packages
only 13 sunxi boards and **none on H616/H618** (for Orange Pi it carries
`orangepi_one_plus` and `orangepi_zero_plus2` only). Build the blob
yourself — on an aarch64 Linux host this needs no cross toolchain:

```bash
apt install build-essential git bc bison flex libssl-dev python3-dev             python3-setuptools python3-pyelftools swig             device-tree-compiler libgnutls28-dev uuid-dev

# BL31 — Trusted Firmware-A for the H616 platform (also used by H618)
git clone https://github.com/ARM-software/arm-trusted-firmware
cd arm-trusted-firmware && git checkout lts-v2.14.6
make CROSS_COMPILE= PLAT=sun50i_h616 DEBUG=0 bl31 -j"$(nproc)"

# U-Boot, with BL31 folded into the FIT
cd .. && git clone https://github.com/u-boot/u-boot
cd u-boot && git checkout v2026.07
make orangepi_zero2_defconfig
make BL31=../arm-trusted-firmware/build/sun50i_h616/release/bl31.bin -j"$(nproc)"
# -> u-boot-sunxi-with-spl.bin
```

#### On-board WiFi: Unisoc UWE5622

The build enables the SDIO bus carrying the soldered WiFi module and wires its
power sequence. Verified on hardware, the module now enumerates:

    mmc1: new high speed SDIO card at address 8800

Upstream leaves all of this out, and each piece was needed — found by comparing
with the H616/H618 boards that *do* declare WiFi in mainline (BigTreeTech CB1,
Transpeed 8K618-T), and confirmed one at a time:

| Missing piece | Symptom without it |
|---|---|
| `status = "okay"` on mmc1 | no controller at all |
| `mmc-pwrseq` releasing WL-REG-ON (PG18, active low) + PG10 muxed as the 32 kHz clock output | `Failed to initialize a non-removable card` |
| `vmmc-supply` (same rail as mmc0) | `error -22 whilst initialising SDIO card` — empty voltage window |

The chip is a **Unisoc UWE5622** — what the "AW859A" can actually contains, per
Armbian's own board config (`orangepizero2.csc` enables the `uwe5622-allwinner`
extension). No in-tree driver matches it, because the card reports
`vendor=0x0000 device=0x0000`: that is not a fault, it is this part's signature,
and the out-of-tree driver is written for exactly it — its SDIO table is
`{SDIO_DEVICE(0, 0)}` and it probes `/sys/bus/sdio/devices/mmc1:8800:1`, the
very path the DTB patch produces.

The build therefore compiles that driver from `armbian/uwe5622` at a pinned
commit, inside the chroot against the shipped kernel, and installs
`uwe5622_bsp_sdio.ko`, `sprdwl_ng.ko` and `sprdbt_tty.ko` into
`/lib/modules/<kver>/updates` with the firmware from `armbian/firmware`. The
toolchain is purged afterwards. `sprdwl_ng` is loaded at boot via
`/etc/modules-load.d`; depmod pulls the BSP module in as a dependency.

**WPA3 is not supported.** The driver advertises WEP/TKIP/CCMP/CMAC and no SAE,
so NetworkManager rejects a WPA3 AP with the misleading `The Wi-Fi network could
not be found` — it means "no compatible AP", not "not visible". Use a WPA2 SSID;
most routers broadcast one alongside their WPA3 network. Connecting also emits a
`field-spanning write` warning from the driver's `add_key` — noisy but harmless,
the link comes up and stays up.

A USB dongle also works and needs nothing extra: `wpasupplicant`,
`wireless-regdb` and the Realtek/Atheros/Broadcom firmware packages are already
in the image — which matters, since the board has no network of its own to fetch
anything with.

#### Where the console actually is

On the Orange Pi Zero 2 the kernel console is **UART0 on PH0 (TX) / PH1 (RX)**,
115200n8 — `serial@5000000`, the only UART left `okay` in the board DTB, with
`chosen/stdout-path = "serial0:115200n8"`.

It is **not on the 26-pin GPIO header**, which carries UART5 on physical pins 8
and 10 (`TXD.5` / `RXD.5` in wiringOP's `physNames_ZERO_2`) — and UART5 is
disabled in the DTB, so wiring there shows nothing. Use the board's dedicated
debug connector.

Two hardware traps that cost a full debugging session:

- **Never connect the adapter's VCC wire.** With it attached the board does not
  boot at all: 3.3 V is pushed back into a rail through the SoC's ESD diodes and
  the AXP305 power sequencing never completes. TX, RX and GND only.
- **A USB-PD-only charger may deliver nothing.** The board presents no CC
  resistors, so a strict PD source (an Apple laptop brick) stays at 0 V. Use a
  USB-A charger with an A-to-C cable, or a plain 5 V/3 A supply.

Verify the result landed where the BROM looks — offset 8 KiB must carry the
sunxi SPL magic:

```bash
dd if=tune-os-orangepi-zero2-vX.Y.Z.img bs=1024 skip=8 count=1 | strings | head -1
# eGON.BT0
```

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
| Orange Pi Zero 2 (H616) | aarch64 | **Validated on hardware — boots, WiFi works** |
| Orange Pi Zero 3 / Zero 2W (H618) | aarch64 | Untested |
| Allwinner TV box (custom DTB) | aarch64 | Bring-up required |
