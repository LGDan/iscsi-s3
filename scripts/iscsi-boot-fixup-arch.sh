#!/bin/sh
# Configure an installed Arch-based root so it can boot from iSCSI.
# Intended to run inside a chroot (after live-ISO install onto an iSCSI LUN).
# Covers Arch, EndeavourOS, Manjaro, and other pacman + mkinitcpio systems.
#
# Usage (from live environment):
#   arch-chroot /mnt /tmp/iscsi-boot-fixup-arch.sh \
#     --portal 192.168.88.15:3260 \
#     --iqn iqn.2026-09.local.iscsi-s3:disk0 \
#     [--username USER --password SECRET] \
#     [--initiator-iqn iqn.2026-09.local:client] \
#     [--iface eth0]
#
# See docs/users/iscsi-os-install.md

set -eu

PORTAL=""
IQN=""
USERNAME=""
PASSWORD=""
INITIATOR_IQN=""
IFACE=""

usage() {
  sed -n '2,15p' "$0" | tr -d '#'
  exit "${1:-0}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --portal) PORTAL="${2:-}"; shift 2 ;;
    --iqn) IQN="${2:-}"; shift 2 ;;
    --username) USERNAME="${2:-}"; shift 2 ;;
    --password) PASSWORD="${2:-}"; shift 2 ;;
    --initiator-iqn) INITIATOR_IQN="${2:-}"; shift 2 ;;
    --iface) IFACE="${2:-}"; shift 2 ;;
    -h|--help) usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage 1 ;;
  esac
done

if [ -z "$PORTAL" ] || [ -z "$IQN" ]; then
  echo "error: --portal and --iqn are required" >&2
  usage 1
fi

if [ -n "$USERNAME" ] && [ -z "$PASSWORD" ]; then
  echo "error: --password is required with --username" >&2
  exit 1
fi

if [ "$(id -u)" -ne 0 ]; then
  echo "error: must run as root (inside chroot)" >&2
  exit 1
fi

sq() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

parse_portal() {
  p=$1
  case "$p" in
    \[*\])
      PORTAL_HOST=${p#\[}
      PORTAL_HOST=${PORTAL_HOST%\]}
      PORTAL_PORT=3260
      ;;
    \[*\]:*)
      PORTAL_HOST=${p#\[}
      PORTAL_HOST=${PORTAL_HOST%%\]*}
      PORTAL_PORT=${p##*\]:}
      ;;
    *)
      colons=$(printf '%s' "$p" | tr -cd ':' | wc -c | tr -d ' ')
      if [ "$colons" -eq 1 ]; then
        PORTAL_HOST=${p%:*}
        PORTAL_PORT=${p##*:}
      else
        PORTAL_HOST=$p
        PORTAL_PORT=3260
      fi
      ;;
  esac
}

# Rebuild HOOKS=(...) so iscsi (and net, on busybox init) run before block.
rewrite_hooks_line() {
  line=$1
  want_net=$2
  prefix=${line%%(*}
  rest=${line#*)}
  inside=${line#*(}
  inside=${inside%%)*}
  # shellcheck disable=SC2086
  set -- $inside
  orig="$*"
  cleaned=""
  for tok in $orig; do
    if [ "$tok" = "iscsi" ]; then
      continue
    fi
    if [ "$want_net" = 1 ] && [ "$tok" = "net" ]; then
      continue
    fi
    cleaned="$cleaned $tok"
  done
  out=""
  inserted=0
  for tok in $cleaned; do
    if [ "$tok" = "block" ]; then
      if [ "$want_net" = 1 ]; then
        out="$out net"
      fi
      out="$out iscsi"
      inserted=1
    fi
    out="$out $tok"
  done
  if [ "$inserted" = 0 ]; then
    if [ "$want_net" = 1 ]; then
      out="$out net"
    fi
    out="$out iscsi"
    echo "warning: HOOKS has no block hook; appended iscsi at the end" >&2
  fi
  out=${out# }
  printf '%s(%s)%s\n' "$prefix" "$out" "$rest"
}

hooks_contain() {
  line=$1
  tok=$2
  printf '%s\n' "$line" | tr '()' '  ' | tr ' ' '\n' | grep -qx "$tok"
}

update_mkinitcpio_hooks() {
  conf=/etc/mkinitcpio.conf
  if [ ! -f "$conf" ]; then
    echo "warning: $conf missing; writing a minimal HOOKS line" >&2
    mkdir -p /etc
    echo 'HOOKS=(base udev autodetect microcode modconf keyboard keymap block filesystems fsck)' > "$conf"
  fi
  line=$(grep -E '^HOOKS=\(' "$conf" | head -1 || true)
  if [ -z "$line" ]; then
    line='HOOKS=(base udev autodetect microcode modconf keyboard keymap block filesystems fsck)'
    printf '\n%s\n' "$line" >> "$conf"
    echo "warning: no active HOOKS= line; appended a default" >&2
  fi
  want_net=1
  if hooks_contain "$line" systemd; then
    want_net=0
    echo "==> systemd mkinitcpio hook detected; initrd unit will start iSCSI (runtime hooks are skipped)"
  else
    echo "==> busybox mkinitcpio; adding net + iscsi hooks before block"
  fi
  new=$(rewrite_hooks_line "$line" "$want_net")
  tmp=$(mktemp)
  awk -v old="$line" -v new="$new" '
    !done && $0 == old { print new; done = 1; next }
    { print }
  ' "$conf" > "$tmp"
  mv "$tmp" "$conf"
  if [ "$want_net" = 0 ]; then
    USE_SYSTEMD_INITRD=1
  else
    USE_SYSTEMD_INITRD=0
  fi
}

set_node() {
  iscsiadm -m node -T "$IQN" -p "$PORTAL" --op update -n "$1" -v "$2"
}

append_cmdline_token() {
  file=$1
  token=$2
  [ -f "$file" ] || return 0
  if grep -q "$token" "$file"; then
    return 0
  fi
  if [ ! -s "$file" ]; then
    printf '%s\n' "$token" > "$file"
    return 0
  fi
  tmp=$(mktemp)
  awk -v token="$token" '
    NF && !done { print $0 " " token; done = 1; next }
    { print }
  ' "$file" > "$tmp"
  mv "$tmp" "$file"
}

ensure_grub_token() {
  file=/etc/default/grub
  token=$1
  [ -f "$file" ] || return 0
  if grep -q "$token" "$file"; then
    return 0
  fi
  if grep -q '^GRUB_CMDLINE_LINUX=' "$file"; then
    sed -i "s|^GRUB_CMDLINE_LINUX=\"\\(.*\\)\"|GRUB_CMDLINE_LINUX=\"\\1 ${token}\"|" "$file"
  else
    echo "GRUB_CMDLINE_LINUX=\"${token}\"" >> "$file"
  fi
}

patch_loader_entries() {
  token=$1
  for file in /boot/loader/entries/*.conf /efi/loader/entries/*.conf; do
    [ -f "$file" ] || continue
    if grep -q "$token" "$file"; then
      continue
    fi
    case "$token" in
      ip=*)
        if grep -q 'ip=' "$file"; then
          continue
        fi
        ;;
    esac
    if grep -q '^options ' "$file"; then
      sed -i "s|^options |options ${token} |" "$file"
    fi
  done
}

update_fstab() {
  file=/etc/fstab
  if [ ! -f "$file" ]; then
    echo "warning: no $file; add _netdev,x-systemd.requires=iscsid.service to the root mount" >&2
    return 0
  fi
  if ! awk '$0 !~ /^[[:space:]]*#/ && NF >= 2 && $2 == "/" { found = 1 } END { exit !found }' "$file"; then
    echo "warning: no root (/) line in $file" >&2
    return 0
  fi
  tmp=$(mktemp)
  awk '
    $0 ~ /^[[:space:]]*#/ || NF < 4 { print; next }
    $2 == "/" {
      opts = $4
      if (opts !~ /(^|,)_netdev(,|$)/) opts = opts ",_netdev"
      if (opts !~ /x-systemd\.requires=iscsid\.service/) opts = opts ",x-systemd.requires=iscsid.service"
      $4 = opts
    }
    { print }
  ' "$file" > "$tmp"
  mv "$tmp" "$file"
}

echo "==> Installing open-iscsi and initramfs tooling"
pacman -Sy --needed --noconfirm open-iscsi mkinitcpio mkinitcpio-nfs-utils

if [ -d /sys/firmware/efi ]; then
  pacman -S --needed --noconfirm grub efibootmgr || true
else
  pacman -S --needed --noconfirm grub || true
fi

if [ -z "$INITIATOR_IQN" ] && [ -f /etc/iscsi/initiatorname.iscsi ]; then
  INITIATOR_IQN=$(sed -n 's/^InitiatorName=//p' /etc/iscsi/initiatorname.iscsi | head -1)
fi
if [ -z "$INITIATOR_IQN" ] && command -v iscsi-iname >/dev/null 2>&1; then
  INITIATOR_IQN=$(iscsi-iname)
fi
if [ -z "$INITIATOR_IQN" ]; then
  echo "error: no initiator IQN; pass --initiator-iqn" >&2
  exit 1
fi

echo "==> Setting initiator IQN"
mkdir -p /etc/iscsi
printf 'InitiatorName=%s\n' "$INITIATOR_IQN" > /etc/iscsi/initiatorname.iscsi

echo "==> Ensuring iscsid defaults"
if [ -f /etc/iscsi/iscsid.conf ]; then
  sed -i 's/^node.startup.*/node.startup = manual/' /etc/iscsi/iscsid.conf || true
fi

parse_portal "$PORTAL"

echo "==> Writing open-iscsi node for $IQN @ $PORTAL"
if iscsiadm -m discovery -t sendtargets -p "$PORTAL" >/tmp/iscsi-discovery.out 2>/dev/null; then
  cat /tmp/iscsi-discovery.out || true
else
  echo "warning: discovery failed (offline chroot is OK); configuring node record anyway" >&2
  iscsiadm -m node -T "$IQN" -p "$PORTAL" -o new 2>/dev/null || true
fi

set_node node.startup automatic
set_node 'node.conn[0].startup' automatic || true
# Root-on-iSCSI: do not fail commands at the first network blip (Arch wiki).
set_node node.session.timeo.replacement_timeout 86400 || true
set_node 'node.conn[0].timeo.noop_out_interval' 0 || true
set_node 'node.conn[0].timeo.noop_out_timeout' 0 || true

if [ -n "$USERNAME" ]; then
  set_node node.session.auth.authmethod CHAP
  set_node node.session.auth.username "$USERNAME"
  set_node node.session.auth.password "$PASSWORD"
fi

echo "==> Enabling iscsid and automatic login"
systemctl enable iscsid.service 2>/dev/null || true
systemctl enable iscsi.service 2>/dev/null || true

echo "==> Writing initramfs iSCSI hook"
mkdir -p /usr/lib/iscsi-boot /etc/initcpio/install /etc/initcpio/hooks /etc/systemd/system

cat > /usr/lib/iscsi-boot/connect.sh <<'EOF'
#!/bin/sh
# Start the boot LUN. Used by the mkinitcpio iscsi hook and initrd-iscsi.service.
# iscsistart must not run at the same time as iscsid; this only runs in the initramfs.
set -u

ENV=/etc/iscsi/iscsi-boot.env
if [ -f "$ENV" ]; then
  # shellcheck disable=SC1090
  . "$ENV"
fi

modprobe iscsi_tcp 2>/dev/null || true
modprobe iscsi_ibft 2>/dev/null || true
modprobe libiscsi 2>/dev/null || true
modprobe libiscsi_tcp 2>/dev/null || true
modprobe scsi_transport_iscsi 2>/dev/null || true
modprobe crc32c 2>/dev/null || true

if iscsistart -N >/dev/null 2>&1; then
  echo "iscsi-boot: network configured from iBFT"
fi
if iscsistart -b >/dev/null 2>&1; then
  echo "iscsi-boot: session started from iBFT"
  exit 0
fi

if [ -z "${ISCSI_INITIATOR:-}" ] || [ -z "${ISCSI_TARGET_IQN:-}" ] || [ -z "${ISCSI_HOST:-}" ]; then
  echo "iscsi-boot: missing portal/iqn in $ENV" >&2
  exit 1
fi

p_noop_interval='node.conn[0].timeo.noop_out_interval=0'
p_noop_timeout='node.conn[0].timeo.noop_out_timeout=0'
p_replace='node.session.timeo.replacement_timeout=86400'

attempt=0
while [ "$attempt" -lt 30 ]; do
  if [ -n "${ISCSI_USERNAME:-}" ]; then
    if iscsistart -i "$ISCSI_INITIATOR" -t "$ISCSI_TARGET_IQN" -g 1 \
        -a "$ISCSI_HOST" -p "${ISCSI_PORT:-3260}" \
        -P "$p_noop_interval" -P "$p_noop_timeout" -P "$p_replace" \
        -u "$ISCSI_USERNAME" -w "${ISCSI_PASSWORD:-}"; then
      echo "iscsi-boot: session started"
      exit 0
    fi
  else
    if iscsistart -i "$ISCSI_INITIATOR" -t "$ISCSI_TARGET_IQN" -g 1 \
        -a "$ISCSI_HOST" -p "${ISCSI_PORT:-3260}" \
        -P "$p_noop_interval" -P "$p_noop_timeout" -P "$p_replace"; then
      echo "iscsi-boot: session started"
      exit 0
    fi
  fi
  attempt=$((attempt + 1))
  sleep 1
done
echo "iscsi-boot: failed to start session to ${ISCSI_HOST}:${ISCSI_PORT:-3260} ${ISCSI_TARGET_IQN}" >&2
exit 1
EOF
chmod 755 /usr/lib/iscsi-boot/connect.sh

cat > /etc/iscsi/iscsi-boot.env <<EOF
# Generated by iscsi-boot-fixup-arch.sh
ISCSI_PORTAL=$(sq "$PORTAL")
ISCSI_HOST=$(sq "$PORTAL_HOST")
ISCSI_PORT=$(sq "$PORTAL_PORT")
ISCSI_TARGET_IQN=$(sq "$IQN")
ISCSI_INITIATOR=$(sq "$INITIATOR_IQN")
ISCSI_USERNAME=$(sq "$USERNAME")
ISCSI_PASSWORD=$(sq "$PASSWORD")
EOF
chmod 600 /etc/iscsi/iscsi-boot.env

cat > /etc/initcpio/hooks/iscsi <<'EOF'
#!/usr/bin/ash
run_hook() {
  msg "Connecting iSCSI target"
  /usr/lib/iscsi-boot/connect.sh
}
EOF

cat > /etc/initcpio/install/iscsi <<'EOF'
#!/bin/bash
build() {
  map add_module iscsi_tcp iscsi_ibft libiscsi libiscsi_tcp scsi_transport_iscsi crc32c
  add_checked_modules '/drivers/net'
  add_binary iscsistart
  add_file /usr/lib/iscsi-boot/connect.sh
  add_file /etc/iscsi/iscsi-boot.env
  add_runscript

  if grep -E '^HOOKS=\([^)]*[[:space:](]systemd[[:space:])]' /etc/mkinitcpio.conf >/dev/null 2>&1; then
    if declare -F add_systemd_unit >/dev/null; then
      add_systemd_unit initrd-iscsi.service || true
    fi
    add_file /etc/systemd/system/initrd-iscsi.service
    add_dir /etc/systemd/system/initrd-root-device.target.wants
    add_symlink /etc/systemd/system/initrd-root-device.target.wants/initrd-iscsi.service \
      /etc/systemd/system/initrd-iscsi.service
    add_dir /etc/systemd/system/sysroot.mount.requires
    add_symlink /etc/systemd/system/sysroot.mount.requires/initrd-iscsi.service \
      /etc/systemd/system/initrd-iscsi.service
  fi
}

help() {
  cat <<HELPEOF
This hook logs into the iSCSI boot LUN before the root filesystem is mounted.
HELPEOF
}
EOF
chmod 755 /etc/initcpio/install/iscsi /etc/initcpio/hooks/iscsi

cat > /etc/systemd/system/initrd-iscsi.service <<'EOF'
[Unit]
Description=Connect iSCSI boot LUN
DefaultDependencies=no
After=systemd-networkd.service
Before=sysroot.mount
Wants=systemd-networkd.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/lib/iscsi-boot/connect.sh

[Install]
WantedBy=initrd-root-device.target
RequiredBy=sysroot.mount
EOF

echo "==> Updating mkinitcpio HOOKS"
update_mkinitcpio_hooks
grep -E '^HOOKS=\(' /etc/mkinitcpio.conf | head -1 | sed 's/^/    /'

if [ -n "$IFACE" ]; then
  IP_TOKEN="ip=:::::${IFACE}:dhcp"
else
  IP_TOKEN="ip=dhcp"
fi

echo "==> Updating bootloader cmdline hints (ip= / rd.neednet=1)"
if [ -f /etc/default/grub ] && ! grep -q 'ip=' /etc/default/grub; then
  ensure_grub_token "$IP_TOKEN"
fi
if [ -f /etc/default/grub ] && ! grep -q 'rd.neednet=' /etc/default/grub; then
  ensure_grub_token "rd.neednet=1"
fi
if [ -f /etc/kernel/cmdline ] && ! grep -q 'ip=' /etc/kernel/cmdline; then
  append_cmdline_token /etc/kernel/cmdline "$IP_TOKEN"
fi
if [ -f /etc/kernel/cmdline ] && ! grep -q 'rd.neednet=' /etc/kernel/cmdline; then
  append_cmdline_token /etc/kernel/cmdline "rd.neednet=1"
fi
patch_loader_entries "$IP_TOKEN"
patch_loader_entries "rd.neednet=1"

echo "==> Marking root as a network mount"
update_fstab

if [ -d /sys/firmware/efi ]; then
  if [ ! -d /boot/EFI ] && [ ! -d /boot/efi/EFI ] && [ ! -d /efi/EFI ] && [ ! -d /boot/loader ]; then
    echo "warning: UEFI detected but no ESP looks mounted under /boot, /boot/efi, or /efi" >&2
    echo "warning: mount the ESP before relying on mkinitcpio / the bootloader config" >&2
  fi
fi

echo "==> Rebuilding initramfs"
if ! command -v mkinitcpio >/dev/null 2>&1; then
  echo "error: mkinitcpio not found" >&2
  exit 1
fi
mkinitcpio -P

echo "==> Refreshing GRUB config"
if command -v grub-mkconfig >/dev/null 2>&1 && [ -d /boot/grub ]; then
  grub-mkconfig -o /boot/grub/grub.cfg || true
fi

echo "==> Done."
echo "    Portal=$PORTAL IQN=$IQN initiator=$INITIATOR_IQN"
if [ "$USE_SYSTEMD_INITRD" -eq 1 ]; then
  echo "    Initramfs: systemd (initrd-iscsi.service). ip= on the kernel cmdline brings the NIC up."
else
  echo "    Initramfs: busybox (net + iscsi hooks). ip= on the kernel cmdline is required by the net hook."
fi
echo "    CHAP secrets in the initramfs live in /etc/iscsi/iscsi-boot.env."
echo "    Arch systemd-boot usually mounts the ESP at /boot (not /boot/efi)."
echo "    Exit chroot, umount -R /mnt, iscsi logout, then reboot."
