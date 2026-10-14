#!/usr/bin/env bash
# Boot a guest kernel under Firecracker with a busybox initramfs and check the
# GitHub-runner parity features from configs/preloop-x86_64-<series>.fragment.
# Needs /dev/kvm access, a static busybox, gzip, and mkfs.{minix,vfat,exfat,
# ntfs,btrfs}. Exits non-zero when any required check fails.
#
# Usage: kernel/smoke-test.sh <vmlinux.bin> <firecracker binary>
set -euo pipefail

KERNEL=$(realpath "$1")
FIRECRACKER=$(realpath "$2")
BUSYBOX=${BUSYBOX:-$(command -v busybox)}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

root=$WORK/root
mkdir -p "$root"/{bin,proc,sys,dev,tmp,img,mnt,etc}
cp "$BUSYBOX" "$root/bin/busybox"
for applet in sh mount umount uname cat echo mkdir grep sysctl unshare su id reboot; do
	ln -s busybox "$root/bin/$applet"
done
printf 'root:x:0:0::/root:/bin/sh\nrunner:x:1001:1001::/tmp:/bin/sh\n' > "$root/etc/passwd"
printf 'root:x:0:\nrunner:x:1001:\n' > "$root/etc/group"
for fs in minix vfat exfat btrfs; do truncate -s 128M "$root/img/$fs.img"; done
mkfs.minix "$root/img/minix.img" > /dev/null
mkfs.vfat "$root/img/vfat.img" > /dev/null
mkfs.exfat "$root/img/exfat.img" > /dev/null
mkfs.btrfs -q "$root/img/btrfs.img" > /dev/null

knob=/proc/sys/kernel/apparmor_restrict_unprivileged_userns
cat > "$root/init" <<EOF
#!/bin/sh
mount -t proc proc /proc; mount -t sysfs sysfs /sys
mount -t devtmpfs dev /dev; mount -t tmpfs tmp /tmp
mount -t securityfs securityfs /sys/kernel/security
check() { if eval "\$2" > /tmp/out 2>&1; then echo "SMOKE PASS \$1"; else echo "SMOKE FAIL \$1: \$(cat /tmp/out)"; fi; }
echo "SMOKE INFO release=\$(uname -r) lsm=\$(cat /sys/kernel/security/lsm)"
check userns_knob_default_0 'test "\$(cat $knob)" = 0'
check userns_knob_accepts_1 'echo 1 > $knob && test "\$(cat $knob)" = 1'
check userns_knob_prettier  'echo 0 > $knob && test "\$(sysctl -n kernel.apparmor_restrict_unprivileged_userns)" = 0'
check userns_knob_rejects_2 '! sh -c "echo 2 > $knob" && test "\$(cat $knob)" = 0'
check yama_ptrace_scope_1   'test "\$(cat /proc/sys/kernel/yama/ptrace_scope)" = 1'
check unprivileged_userns   'test "\$(su runner -c "unshare -U -r id -u")" = 0'
for fs in minix vfat exfat btrfs; do
	check "loop_mount_\$fs" "mkdir -p /mnt/\$fs && mount -o loop -t \$fs /img/\$fs.img /mnt/\$fs && echo hi > /mnt/\$fs/f && test \"\\\$(cat /mnt/\$fs/f)\" = hi && umount /mnt/\$fs"
done
for fs in ntfs3 udf erofs f2fs hfs hfsplus jfs nilfs2 zonefs 9p nfsd overlay fuse; do
	check "registered_\$fs" "grep -qw \$fs /proc/filesystems"
done
check unix_diag 'test -e /proc/net/unix'
echo "SMOKE INFO damon_reclaim=\$(cat /sys/module/damon_reclaim/parameters/enabled)"
reboot -f
EOF
chmod +x "$root/init"
(cd "$root" && find . -print0 | "$BUSYBOX" cpio --null -o -H newc 2> /dev/null) | gzip -1 > "$WORK/initramfs.cpio.gz"

cat > "$WORK/vm.json" <<EOF
{
  "boot-source": {
    "kernel_image_path": "$KERNEL",
    "initrd_path": "$WORK/initramfs.cpio.gz",
    "boot_args": "console=ttyS0 reboot=k panic=1 pci=off rdinit=/init damon_reclaim.enabled=Y"
  },
  "drives": [],
  "machine-config": { "vcpu_count": 2, "mem_size_mib": 2048 }
}
EOF
timeout 120 "$FIRECRACKER" --no-api --config-file "$WORK/vm.json" --api-sock "$WORK/fc.sock" > "$WORK/boot.log" 2>&1 || true
grep -a 'SMOKE ' "$WORK/boot.log" | tr -d '\r'
grep -aq 'SMOKE PASS' "$WORK/boot.log" || { echo "guest produced no results; boot log:" >&2; tail -30 "$WORK/boot.log" >&2; exit 1; }
! grep -aq 'SMOKE FAIL' "$WORK/boot.log"
