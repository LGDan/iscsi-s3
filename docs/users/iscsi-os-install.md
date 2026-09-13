# Install a Linux OS onto an iSCSI disk (live ISO + chroot)

This guide installs **Ubuntu**, **Debian**, **Alpine**, or an **Arch-based** distro onto an iscsi-s3 (or any open-iscsi) LUN using a **live ISO**, then fixes GRUB and the initramfs in a **chroot** so the machine can reboot from the network disk.

It complements [firmware iSCSI boot notes](tutorials.md#tutorial-5--firmware-iscsi-boot-intel-nic--nuc) in the tutorials. Here the focus is software initiator install from a live environment (also works when firmware/iBFT will own the session later).

## Overview

```text
1. Boot live ISO (RAM only; do not install onto the live USB)
2. In the live session: install open-iscsi, discover + login to the target
3. Run the distro installer; select the iSCSI block device as the install disk
4. After install finishes, remount the installed root (+ ESP) and chroot
5. Run the fixup script (or manual steps) so initrd/GRUB can attach iSCSI at boot
6. Unmount, logout, reboot into the installed system
```

Helper scripts (run **inside** the chroot, or via `chroot /mnt …`):

| Distro | Script |
|--------|--------|
| Ubuntu / Debian | [`scripts/iscsi-boot-fixup-debian.sh`](../../scripts/iscsi-boot-fixup-debian.sh) |
| Alpine | [`scripts/iscsi-boot-fixup-alpine.sh`](../../scripts/iscsi-boot-fixup-alpine.sh) |
| Arch, EndeavourOS, Manjaro, … | [`scripts/iscsi-boot-fixup-arch.sh`](../../scripts/iscsi-boot-fixup-arch.sh) |

## Prerequisites

- A reachable iscsi-s3 (or other) target with a volume large enough for the OS (recommend ≥ 20 GiB for Ubuntu/Debian; Alpine can be smaller).
- Portal and IQN, for example:
  - Portal: `192.168.88.15:3260`
  - IQN: `iqn.2026-09.local.iscsi-s3:disk0`
- Live ISO media (USB or virtio CD) for the guest/host you are installing.
- Network on the live environment that can reach the portal (DHCP or static).
- If the target uses CHAP, have username/secret ready.

**iscsi-s3 notes**

- Each volume is **LUN 0** on its own IQN.
- Set `advertise` / `portals` so discovery returns a client-reachable address.
- Prefer logging out other initiators from that volume during install.

**Boot modes after install**

| Mode | What brings the disk up |
|------|-------------------------|
| **Firmware / iBFT** | NIC Option ROM or UEFI iSCSI attaches the LUN; OS initrd uses iBFT |
| **Software initiator** | initrd runs `iscsistart` / open-iscsi using node records + kernel cmdline |

The fixup scripts enable **both** where possible (iBFT modules + open-iscsi auto-login).

---

## Shared live-session steps (all distros)

Boot the live ISO, open a root shell (`sudo -i` on Ubuntu/Debian live).

Set variables (adjust for your lab):

```bash
export PORTAL=192.168.88.15:3260
export IQN=iqn.2026-09.local.iscsi-s3:disk0
# Optional CHAP:
# export ISCSI_USER=iscsiuser
# export ISCSI_PASS='change-me'
```

### Discover and login

Exact package install differs per distro (see sections below). After `open-iscsi` / `iscsid` is available:

```bash
iscsiadm -m discovery -t sendtargets -p "$PORTAL"

if [ -n "${ISCSI_USER:-}" ]; then
  iscsiadm -m node -T "$IQN" -p "$PORTAL" \
    --op update -n node.session.auth.authmethod -v CHAP
  iscsiadm -m node -T "$IQN" -p "$PORTAL" \
    --op update -n node.session.auth.username -v "$ISCSI_USER"
  iscsiadm -m node -T "$IQN" -p "$PORTAL" \
    --op update -n node.session.auth.password -v "$ISCSI_PASS"
fi

# Keep the session across the installer if possible
iscsiadm -m node -T "$IQN" -p "$PORTAL" \
  --op update -n node.startup -v automatic

iscsiadm -m node -T "$IQN" -p "$PORTAL" --login
iscsiadm -m session -P 3
```

Find the block device:

```bash
# Typical: /dev/sdX or /dev/disk/by-path/*-iscsi-*-lun-0
ls -l /dev/disk/by-path/*iscsi* 2>/dev/null
lsblk -o NAME,SIZE,TYPE,TRAN,MODEL
```

Remember the device (example: `/dev/sdb`). **Do not** confuse it with the live USB.

### Installer tips (Ubuntu / Debian)

- Choose **Something else** / manual partitioning when in doubt.
- Put the root filesystem on the iSCSI disk (`/`).
- On UEFI systems, create a small **EFI System Partition** (≈ 512 MiB, FAT32, flags `esp`) on the **same** iSCSI disk and mount it at `/boot/efi`.
- Install the bootloader to that disk (not to the live USB).
- If the installer offers “iSCSI” target configuration itself, you may use it **or** the pre-login method above — do not double-login to conflicting portals.

### After the installer exits

Stay in the live environment. Remount the installed system (adjust device and mountpoints):

```bash
export DISK=/dev/sdb          # whole disk that received the install
export ROOT_PART=/dev/sdb2    # root partition (example)
export ESP_PART=/dev/sdb1     # EFI partition if UEFI (example)

mkdir -p /mnt
mount "$ROOT_PART" /mnt
# Optional separate /boot:
# mkdir -p /mnt/boot && mount /dev/sdbX /mnt/boot
if [ -n "${ESP_PART:-}" ] && [ -b "$ESP_PART" ]; then
  mkdir -p /mnt/boot/efi
  mount "$ESP_PART" /mnt/boot/efi
fi

mount --bind /dev /mnt/dev
mount --bind /proc /mnt/proc
mount --bind /sys /mnt/sys
mount --bind /run /mnt/run
# DNS for apt/apk/pacman inside chroot:
cp -L /etc/resolv.conf /mnt/etc/resolv.conf
```

Copy the fixup script into the target (from this repo, a USB, or curl), then chroot — or pipe it:

```bash
# Example: script already on the live system at /tmp/iscsi-boot-fixup-debian.sh
cp /tmp/iscsi-boot-fixup-debian.sh /mnt/tmp/
chmod +x /mnt/tmp/iscsi-boot-fixup-debian.sh
chroot /mnt /tmp/iscsi-boot-fixup-debian.sh \
  --portal "$PORTAL" --iqn "$IQN"
# add --username / --password if using CHAP
```

Cleanup before reboot:

```bash
umount -R /mnt
iscsiadm -m node -T "$IQN" -p "$PORTAL" --logout
reboot
```

Configure firmware or PXE as needed so the next boot uses iSCSI (iBFT) or a local bootloader that can reach the portal.

---

## Ubuntu (live Desktop or Server ISO)

Tested pattern: Ubuntu 22.04 / 24.04 live.

### 1. Packages in the live session

```bash
apt-get update
apt-get install -y open-iscsi
systemctl start iscsid || service open-iscsi start
```

Then run the [shared discover/login](#discover-and-login) steps.

### 2. Installer

- Desktop: run **Install Ubuntu**.
- Server: follow Subiquity; if it can attach iSCSI natively, either use that **or** your pre-attached `/dev/sdX`.
- Target disk = the iSCSI LUN; include ESP on UEFI.

When the installer offers to reboot, choose **Continue testing** / stay in the live session so you can chroot.

### 3. Chroot fixup

```bash
# mount as in shared steps, then:
chroot /mnt /tmp/iscsi-boot-fixup-debian.sh \
  --portal "$PORTAL" --iqn "$IQN"
```

The Debian-family script installs `open-iscsi`, writes `ISCSI_AUTO=true`, adds `iscsi_tcp` / `iscsi_ibft` to initramfs modules, regenerates initramfs, and updates GRUB with `GRUB_CMDLINE_LINUX` hints when needed.

### 4. Reboot

Logout iSCSI from the live host, reboot. For firmware boot, ensure the NIC iSCSI attempt points at the same portal/IQN/LUN 0.

---

## Debian (live ISO)

Same flow as Ubuntu (`open-iscsi` + installer + chroot). Use a Debian live image that includes (or can install) a graphical/calamares or debian-installer path you are comfortable with.

### 1. Packages in the live session

```bash
apt-get update
apt-get install -y open-iscsi
systemctl start iscsid || true
```

Login as above, note `/dev/sdX`.

### 2. Installer

Install to the iSCSI disk. Prefer a GPT + ESP layout on UEFI. Leave the live session running after install.

### 3. Chroot fixup

Use the same script as Ubuntu:

```bash
chroot /mnt /tmp/iscsi-boot-fixup-debian.sh \
  --portal "$PORTAL" --iqn "$IQN"
```

On pure BIOS/legacy installs (no ESP), the script still updates initramfs and GRUB; ensure `grub-install` was pointed at the iSCSI disk during install.

---

## Alpine (live ISO)

Alpine’s installer (`setup-alpine`) is text-based. Typical approach: attach iSCSI in the live environment, then install with `setup-disk` to that device (or use `setup-alpine` and select the iSCSI disk when asked).

### 1. Packages in the live session

```bash
# Live typically has networking via setup-interfaces / DHCP already
apk update
apk add open-iscsi lsblk sgdisk e2fsprogs dosfstools grub grub-efi
rc-service iscsid start || true
```

Discover/login (same `iscsiadm` commands as above). Confirm the device with `lsblk`.

### 2. Install onto the iSCSI disk

Example using `setup-disk` (destructive — wipes `$DISK`):

```bash
export DISK=/dev/sdb
# Answer setup-alpine networking/apk questions first if you have not.
# Then install to the iSCSI disk:
export BOOTLOADER=grub
setup-disk -m sys "$DISK"
```

Or complete `setup-alpine` and choose `$DISK` when prompted for the install disk.

UEFI: ensure an ESP exists and GRUB-EFI was installed to it. If `setup-disk` did not create one the way you need, partition manually first (sgdisk + mkfs.vfat + mkfs.ext4), mount under `/mnt`, then run `setup-disk` in mounted mode — see Alpine wiki “setup-disk”.

### 3. Remount and chroot

```bash
# After setup-disk, root is often left unmounted; remount:
mount "${DISK}2" /mnt        # adjust: Alpine often uses p2=root, p1=ESP or BIOS boot
mkdir -p /mnt/boot/efi
mount "${DISK}1" /mnt/boot/efi 2>/dev/null || true

mount --bind /dev /mnt/dev
mount --bind /proc /mnt/proc
mount --bind /sys /mnt/sys
cp -L /etc/resolv.conf /mnt/etc/resolv.conf

cp /tmp/iscsi-boot-fixup-alpine.sh /mnt/tmp/
chmod +x /mnt/tmp/iscsi-boot-fixup-alpine.sh
chroot /mnt /tmp/iscsi-boot-fixup-alpine.sh \
  --portal "$PORTAL" --iqn "$IQN"
```

### 4. Reboot

```bash
umount -R /mnt
iscsiadm -m node -T "$IQN" -p "$PORTAL" --logout
reboot
```

---

## Arch-based (live ISO)

Tested pattern: Arch, and other pacman + mkinitcpio systems (EndeavourOS, Manjaro). The official Arch ISO is already a root shell.

### 1. Packages in the live session

```bash
# Wired DHCP is usually up already. Wireless: iwctl, then:
pacman -Sy --noconfirm open-iscsi
systemctl start iscsid
```

Then run the [shared discover/login](#discover-and-login) steps. Confirm the device with `lsblk`.

### 2. Install onto the iSCSI disk

Use `archinstall` and pick the iSCSI disk, or install by hand (`pacstrap` / `genfstab` / bootloader). Include a bootloader (GRUB or systemd-boot).

Do **not** reboot when the installer finishes. If it already rebooted, boot the ISO again, log in, and remount the installed system.

UEFI mount points differ from Debian:

- **systemd-boot** (common on Arch): the ESP is usually mounted at `/boot`.
- **GRUB**: kernels often stay on the root filesystem, and the ESP is `/boot/efi`.

The ESP that holds the initramfs and bootloader config must be mounted at that same path **inside the chroot** before the fixup script runs. Otherwise `mkinitcpio` writes an image the firmware will not load.

### 3. Remount and chroot

Prefer `arch-chroot` (it bind-mounts `/dev`, `/proc`, `/sys` and copies resolv.conf):

```bash
# systemd-boot example: ESP at /boot
mount /dev/sdb2 /mnt          # root
mount /dev/sdb1 /mnt/boot     # ESP — adjust partition numbers

# GRUB example instead: ESP at /boot/efi
# mount /dev/sdb2 /mnt
# mkdir -p /mnt/boot/efi && mount /dev/sdb1 /mnt/boot/efi

cp /tmp/iscsi-boot-fixup-arch.sh /mnt/tmp/
chmod +x /mnt/tmp/iscsi-boot-fixup-arch.sh
arch-chroot /mnt /tmp/iscsi-boot-fixup-arch.sh \
  --portal "$PORTAL" --iqn "$IQN"
# add --username / --password if using CHAP
# add --iface eth0 if DHCP must use a specific NIC (predictable names)
```

If `arch-chroot` is not available, use the [shared remount steps](#after-the-installer-exits) and `chroot /mnt …` instead.

The script installs `open-iscsi` and `mkinitcpio-nfs-utils`, writes a node record, and adds an `iscsi` mkinitcpio hook that runs `iscsistart` before the root mount (iBFT if the firmware attached the LUN, otherwise the portal/IQN you passed). On a busybox initramfs it also inserts the `net` hook. On a systemd initramfs (runtime hooks are skipped) it installs `initrd-iscsi.service` instead. It adds `ip=dhcp` and `rd.neednet=1` to the bootloader cmdline, then runs `mkinitcpio -P`.

### 4. Reboot

```bash
umount -R /mnt
iscsiadm -m node -T "$IQN" -p "$PORTAL" --logout
reboot
```

---

## What the fixup scripts do

### Debian / Ubuntu (`iscsi-boot-fixup-debian.sh`)

1. `apt-get install -y open-iscsi initramfs-tools` (and `grub-efi-amd64` or similar if missing on UEFI).
2. Write initiator node record for `PORTAL`/`IQN` (optional CHAP).
3. Set `node.startup = automatic` and create `/etc/iscsi/iscsi.initramfs` with `ISCSI_AUTO=true`.
4. Append `iscsi_tcp` and `iscsi_ibft` to `/etc/initramfs-tools/modules`.
5. Optionally extend `GRUB_CMDLINE_LINUX` with `RD.ISCSI.IBFT=1` / `ip=::::…` style hints when `--iface` / `--initiator-iqn` are passed.
6. `update-initramfs -u -k all` and `update-grub`.

### Alpine (`iscsi-boot-fixup-alpine.sh`)

1. `apk add open-iscsi grub` (and EFI package when needed).
2. Configure `/etc/iscsi/iscsid.conf` / node under `/etc/iscsi/nodes/…`.
3. Enable `iscsid` and iscsi initiator features used at early boot (`/etc/conf.d`, mkinitfs features).
4. Ensure `iscsi` feature is listed for mkinitfs; rebuild initramfs (`mkinitfs`).
5. `grub-mkconfig -o /boot/grub/grub.cfg` (and note EFI path when applicable).

### Arch-based (`iscsi-boot-fixup-arch.sh`)

1. `pacman -Sy --needed --noconfirm open-iscsi mkinitcpio mkinitcpio-nfs-utils` (and `grub` / `efibootmgr` when missing).
2. Write `/etc/iscsi/initiatorname.iscsi` and an open-iscsi node for `PORTAL`/`IQN` (optional CHAP). `node.startup = automatic`. Session timeouts are relaxed so a brief network blip does not fail the root disk.
3. Enable `iscsid.service` and `iscsi.service`.
4. Install an mkinitcpio `iscsi` hook that runs `iscsistart` (iBFT first, then the given portal/IQN). Busybox configs also get the `net` hook before `block`. Systemd configs get `initrd-iscsi.service` instead, because runtime hooks do not run when the `systemd` hook is in `HOOKS`.
5. Add `ip=dhcp` (or `ip=:::::IFACE:dhcp` if `--iface` is set) and `rd.neednet=1` to GRUB, `/etc/kernel/cmdline`, and systemd-boot entries when those files exist.
6. Append `_netdev,x-systemd.requires=iscsid.service` to the `/` line in `/etc/fstab`.
7. `mkinitcpio -P`, and `grub-mkconfig` when `/boot/grub` exists.

On Arch, systemd-boot usually mounts the ESP at `/boot`, not `/boot/efi`. Mount the ESP at the same path the install used before running the script.

Run `script --help` inside each file for flags.

---

## Verification checklist

After reboot into the installed OS:

```bash
# Session present?
iscsiadm -m session
# Root on iSCSI?
findmnt /
lsblk -o NAME,SIZE,TYPE,TRAN
# initrd hooks present (Debian/Ubuntu)?
lsinitramfs /boot/initrd.img-$(uname -r) | grep -i iscsi | head
# Arch?
lsinitcpio /boot/initramfs-linux.img | grep -i iscsi | head
```

If you drop to an initramfs shell:

- No route to portal → fix live network / firmware iSCSI / cmdline `ip=`.
- No iSCSI modules → re-run fixup; confirm `iscsi_tcp` in initramfs.
- Wrong portal in node DB → update node records and rebuild initramfs.
- UEFI: ESP not mounted at `/boot/efi` during `update-grub` → remount and regenerate.

---

## Related

- [Tutorials — firmware iSCSI boot](tutorials.md#tutorial-5--firmware-iscsi-boot-intel-nic--nuc)
- [Getting started](getting-started.md) — open-iscsi login basics
- [Configuration](configuration.md) — `advertise` / portals / CHAP
- [MPIO](mpio.md) — multipath is usually **not** used for the OS boot LUN in v1 labs; keep a single path for simplicity
