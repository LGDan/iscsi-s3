# User tutorials

Assumes a running target and open-iscsi. Replace `HOST`, IQNs, and device names for your setup.

---

## Tutorial 1 — Format and mount a new volume

```bash
HOST=127.0.0.1
IQN=iqn.2026-09.local.iscsi-s3:disk0
PORT=3260

sudo iscsiadm -m discovery -t sendtargets -p ${HOST}:${PORT}
sudo iscsiadm -m node -T "$IQN" -p ${HOST}:${PORT} --login

# identify the new disk
lsblk -o NAME,SIZE,MODEL,TRAN
dmesg | tail -20
```

Only on an **empty** volume:

```bash
DEV=/dev/sdX   # your device
sudo mkfs.ext4 "$DEV"
sudo mkdir -p /mnt/iscsi-disk0
sudo mount "$DEV" /mnt/iscsi-disk0
df -h /mnt/iscsi-disk0
```

Quick raw I/O check (destroys data on the device):

```bash
sudo dd if=/dev/zero of="$DEV" bs=1M count=8 oflag=direct
sudo dd if="$DEV" of=/dev/null bs=1M count=8 iflag=direct
```

Logout when finished:

```bash
sudo umount /mnt/iscsi-disk0
sudo iscsiadm -m node -T "$IQN" -p ${HOST}:${PORT} --logout
```

---

## Tutorial 2 — Grow a volume

Capacity is **grow-only**. Geometry (`chunk_size`, `block_size`) cannot change after the first `meta.json` write.

1. Grow live (preferred while the daemon is up):

```bash
iscsi-s3-ctl volume grow disk0 --capacity 20GiB
```

Also raise `capacity` in the TOML so the next restart keeps the new size.

2. On the initiator, rescan and grow the filesystem:

```bash
DEV=/dev/sdX
echo 1 | sudo tee /sys/block/${DEV#/dev/}/device/rescan
blockdev --getsize64 "$DEV"
sudo resize2fs "$DEV"    # ext4 example
```

If rescan is unavailable, logout and login again. Existing chunk objects are left untouched; new capacity is sparse zeros until written.

---

## Tutorial 3 — Use a remote target on the LAN

1. On the target host, set:

```toml
bind = "0.0.0.0:3260"
advertise = "192.168.88.15"   # address initiators will dial
```

2. Publish/firewall TCP `3260` (shared portal for all IQNs).
3. Prefer a trusted network or VPN, or enable [CHAP](configuration.md#chap-authentication).
4. On the initiator:

```bash
sudo iscsiadm -m discovery -t sendtargets -p 192.168.88.15:3260
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 \
  -p 192.168.88.15:3260 --login
```

Autostart (optional):

```bash
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 \
  -p 192.168.88.15:3260 --op update -n node.startup -v automatic
sudo systemctl enable --now iscsid
sudo systemctl enable --now open-iscsi   # where applicable
```

---

## Tutorial 4 — Two volumes

```toml
bind = "0.0.0.0:3260"
advertise = "127.0.0.1"

[[volumes]]
name = "disk0"
iqn = "iqn.2026-09.local.iscsi-s3:disk0"
prefix = "disks/disk0"
capacity = "10GiB"

[[volumes]]
name = "disk1"
iqn = "iqn.2026-09.local.iscsi-s3:disk1"
prefix = "disks/disk1"
capacity = "50GiB"
```

Compose ports:

```yaml
ports:
  - "3260:3260"
```

One discovery lists both IQNs:

```bash
sudo iscsiadm -m discovery -t sendtargets -p 127.0.0.1:3260
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk0 -p 127.0.0.1:3260 --login
sudo iscsiadm -m node -T iqn.2026-09.local.iscsi-s3:disk1 -p 127.0.0.1:3260 --login
```

---

## Tutorial 5 — Firmware iSCSI boot (Intel NIC / NUC)

High-level flow that has been exercised with this project:

1. Target reachable with correct `advertise` (LAN IP of the host publishing the ports).
2. Firmware iSCSI boot: portal, target IQN, **Boot LUN = 0** (this stack exposes one LUN per IQN).
3. Digests: firmware typically wants `HeaderDigest=None` / `DataDigest=None` (negotiated if the initiator offers `None`).
4. Install an OS onto the LUN (live USB, or a helper VM that sees the LUN as a disk). Prefer installing in a way that includes **iSCSI/iBFT** support in the initramfs when the machine will boot via Option ROM.
5. If you land in an initramfs shell after GRUB: the OS likely lacks iBFT/open-iscsi in the initramfs. From a rescue/chroot on the root filesystem (Debian/MX):

```bash
apt-get install -y open-iscsi initramfs-tools
echo 'ISCSI_AUTO=true' > /etc/iscsi/iscsi.initramfs
# add iscsi_ibft / iscsi_tcp modules to /etc/initramfs-tools/modules as needed
update-initramfs -u -k all
update-grub   # ensure ESP is mounted at /boot/efi, not /boot
```

Mount order on UEFI when repairing: root → (optional separate `/boot`) → **ESP at `/boot/efi`**.

Each volume is LUN **0** on its own IQN (same portal). There is no separate “boot LUN” setting in iscsi-s3 config.

---

## Tutorial 6 — Inspect S3 objects (MinIO)

```bash
docker compose run --rm --no-deps --entrypoint /bin/sh createbuckets -c '
  mc alias set local http://minio:9000 minioadmin minioadmin
  mc ls -r local/iscsi/disks/disk0/
  mc cat local/iscsi/disks/disk0/meta.json
'
```

You should see `meta.json` and `chunks/…` after writes.

---

## Tutorial 7 — Dual-path MPIO (multi-instance)

Full procedures live in **[MPIO setup](mpio.md)** — including **Setup A** (one process, dual NIC, **keep the cache**) vs multi-instance (rolling upgrades, cache off).

Quick multi-instance lab start:

```bash
docker compose -f docker-compose.mpio.yml up -d --build
IQN=iqn.2026-09.local.iscsi-s3:disk0
sudo iscsiadm -m discovery -t sendtargets -p 127.0.0.1:3260
sudo iscsiadm -m node -T "$IQN" -p 127.0.0.1:3260 --login
sudo iscsiadm -m node -T "$IQN" -p 127.0.0.1:3261 --login
sudo multipath -ll
```

---

## See also

- [Getting started](getting-started.md)
- [MPIO setup](mpio.md)
- [Examples](../examples.md)
