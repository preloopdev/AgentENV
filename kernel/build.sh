#!/usr/bin/env bash
# Build the Firecracker guest kernel (x86_64) used for GitHub Actions runner
# parity. Inputs live next to this script:
#   configs/preloop-x86_64-<version>.defconfig   full config (savedefconfig)
#   configs/preloop-x86_64-<series>.fragment     runner-parity delta, checked below
#   patches/<series>/*.patch                     applied in name order
#
# Prerequisites: curl, xz, gcc, make, flex, bison, bc, libelf-dev, libssl-dev,
# pahole >= 1.22 (BTF). Output: $OUT/vmlinux.bin (debug info stripped, .BTF
# kept) and $OUT/vmlinux.bin.sha256.
#
# Usage: kernel/build.sh [version]   (default 6.1.175)
set -euo pipefail

VERSION=${1:-6.1.175}
SERIES=${VERSION%.*}
MAJOR=${VERSION%%.*}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=${WORK:-$PWD/kernel-build}
OUT=${OUT:-$WORK/out}
JOBS=${JOBS:-$(nproc)}

defconfig=$HERE/configs/preloop-x86_64-$VERSION.defconfig
fragment=$HERE/configs/preloop-x86_64-$SERIES.fragment
for f in "$defconfig" "$fragment"; do
	[ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
done

mkdir -p "$WORK" "$OUT"
cd "$WORK"
tarball=linux-$VERSION.tar.xz
base=https://cdn.kernel.org/pub/linux/kernel/v$MAJOR.x
[ -f "$tarball" ] || curl -fsSLO "$base/$tarball"
curl -fsSL "$base/sha256sums.asc" | grep " $tarball\$" | sha256sum -c -

rm -rf "linux-$VERSION"
tar -xJf "$tarball"
cd "linux-$VERSION"
for p in "$HERE/patches/$SERIES"/*.patch; do
	[ -e "$p" ] || continue
	patch -p1 --forward < "$p"
done

cp "$defconfig" .config
make -s olddefconfig

# Kconfig silently drops options whose dependencies are unmet, so check that
# every value the parity fragment asks for made it into the final config.
missing=0
while IFS= read -r line; do
	case $line in
	CONFIG_*=* | "# CONFIG_"*" is not set")
		grep -qxF "$line" .config || { echo "not honoured: $line" >&2; missing=1; } ;;
	esac
done < "$fragment"
[ "$missing" -eq 0 ] || exit 1

# GCC 15 rejects a const discard in the libbpf bundled with 6.1 (built for
# resolve_btfids); downgrade that one diagnostic for host tools only.
make -j"$JOBS" HOSTCFLAGS=-Wno-error=discarded-qualifiers vmlinux

objcopy --strip-debug vmlinux "$OUT/vmlinux.bin"
readelf -S "$OUT/vmlinux.bin" | grep -q '\.BTF' || { echo "vmlinux.bin has no .BTF section" >&2; exit 1; }
(cd "$OUT" && sha256sum vmlinux.bin > vmlinux.bin.sha256)
echo "built $(cat include/config/kernel.release): $OUT/vmlinux.bin"
