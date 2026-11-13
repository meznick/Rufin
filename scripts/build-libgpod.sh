#!/usr/bin/env bash
# scripts/build-libgpod.sh — build the pinned libgpod install for the iPod
# integration (B-007 recipe, itl-player docs/LIBGPOD.md).
#
# Rebuilds libgpod 0.8.3 with the Arch 0.8.3-20 patch set into a prefix,
# verifies every input checksum, and writes the `rufin-libgpod.abi` marker
# next to the library. The marker's LAYOUTS line is taken from the crate
# constant `crates/ipod/src/ffi/layouts_sha.rs`, so the script can never
# drift from what the wrapper pins.
#
# Usage:
#
#   scripts/build-libgpod.sh [--prefix DIR]
#
# Defaults (matching the documented dev setup):
#   prefix     /tmp/opencode/libgpod-install
#   work dir   /tmp/opencode/libgpod-src   (created, reusable, NOT cleaned)
#
# After a successful build:
#
#   export RUFIN_LIBGPOD_DIR=/tmp/opencode/libgpod-install/lib
#   cargo test -p ipod --features sync-writes --test integration
#
# The tarball and the 8 patches are pinned by sha256 (itl-player
# docs/LIBGPOD.md, Arch packaging commit a74dc08b0f070f03ecc5b7ed12cd895ab0d64dee).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${PREFIX:-/tmp/opencode/libgpod-install}"
WORK="${WORK:-/tmp/opencode/libgpod-src}"

usage() {
  cat <<'EOF'
usage: scripts/build-libgpod.sh [--prefix DIR]

Builds the pinned libgpod 0.8.3 (Arch 0.8.3-20 patch set) into --prefix
(default /tmp/opencode/libgpod-install), verifies every input checksum,
and writes the ABI marker next to the library.

After the build:
  export RUFIN_LIBGPOD_DIR=/tmp/opencode/libgpod-install/lib
  cargo test -p ipod --features sync-writes --test integration
EOF
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --prefix=*) PREFIX="${1#*=}"; shift ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
  esac
done
VERSION=0.8.3
TARBALL=libgpod-${VERSION}.tar.bz2
TARBALL_SHA256=638a7959d04e95f1e62abad02bd33702e4e8dfef98485ac7d9d50395c37e955d
TARBALL_URL="https://downloads.sourceforge.net/sourceforge/gtkpod/${TARBALL}"

# patch name -> sha256, all applied -p1 (Arch libgpod 0.8.3-20)
declare -A PATCHES=(
  [libgpod-0.8.2-pkgconfig_overlinking.patch]=02daa2b46d0cfdee14c9d96eabf903ba4638694e669c5cb0ec462076837ac2ff
  [libgpod-0.8.3-pkgconfig_sharp.patch]=9a5e649c79667b75a60adfe2d2a9e353c63d161e9a422e0d69c15bd124f9af63
  [libgpod-playcounts.patch]=51aa60113936688631d2f9489b7dd97727eb6412f30aeeff0bee6398c9f6b3e9
  [libgpod-udev.patch]=90112731daca580f12f96e9d62fe2174f0fb29dcfba5f5d2cb86eb4d4d7e88fa
  [libgpod-0.8.3-libplist-2.2.patch]=2fa96505e467632c24ffbfe02a5b4811c0ec162ea16ce1922ca78ac90ff7bd90
  [libgpod-0.8.3-libplist-2.3.patch]=8e7099240387552d7a962754c51ca600a8210e2e3bbe6d0067afebfc40a1cba6
  [libgpod-0.8.3-implicit-int.patch]=6bd0d2d20490c9130cb1b33097bef46ab0eb3c314ed4e18b8b4fdfa918e9af88
  [libgpod-0.8.3-pointer-types.patch]=ea5a7ad6f12ba6d70de6be72f02337f21a0662ff4631e8c1a3afc1312804251e
)
PATCH_API="https://gitlab.archlinux.org/api/v4/projects/archlinux%2Fpackaging%2Fpackages%2Flibgpod/repository/files"

fail() { echo "build-libgpod: $*" >&2; exit 1; }

for tool in curl tar bzip2 patch autoreconf make pkg-config; do
  command -v "$tool" >/dev/null || fail "missing required tool: $tool (pacman -S ... ; intltool is also required)"
done

mkdir -p "$WORK" "$PREFIX/lib" "$PREFIX/lib/udev"
cd "$WORK"

# --- tarball ------------------------------------------------------------
if [[ ! -f "$TARBALL" ]]; then
  echo "build-libgpod: downloading $TARBALL_URL"
  curl -fL --retry 3 -o "$TARBALL" "$TARBALL_URL"
fi
echo "$TARBALL_SHA256  $TARBALL" | sha256sum -c - \
  || fail "tarball checksum mismatch; delete $WORK/$TARBALL and retry"

# --- patches ------------------------------------------------------------
for name in "${!PATCHES[@]}"; do
  expected="${PATCHES[$name]}"
  if [[ ! -f "$name" ]] || ! echo "$expected  $name" | sha256sum -c - >/dev/null 2>&1; then
    echo "build-libgpod: downloading patch $name"
    # Patch names are URL-safe ([-_.a-z0-9]), no encoding needed.
    curl -fL --retry 3 -G "$PATCH_API/$name/raw" \
      --data-urlencode "ref=main" -o "$name"
    echo "$expected  $name" | sha256sum -c - || fail "patch $name checksum mismatch"
  fi
done

# --- unpack + patch -----------------------------------------------------
SRC="$WORK/libgpod-$VERSION"
if [[ ! -d "$SRC" ]]; then
  tar xf "$TARBALL"
fi
cd "$SRC"
echo "build-libgpod: applying patches"
for name in "${!PATCHES[@]}"; do
  patch -p1 -N -i "$WORK/$name" >/dev/null || fail "patch $name failed to apply"
done

# --- build --------------------------------------------------------------
echo "build-libgpod: autoreconf + configure + make (prefix $PREFIX)"
autoreconf -fvi >/dev/null
CFLAGS="-O2 -Wno-incompatible-pointer-types -Wno-implicit-int" \
  ./configure \
    --prefix="$PREFIX" \
    --enable-udev --with-mono=no --with-python=no \
    --with-udev-dir="$PREFIX/lib/udev" \
    --without-libimobiledevice >/dev/null
make -j"$(nproc)" >/dev/null
make install >/dev/null

# --- ABI marker ---------------------------------------------------------
# The LAYOUTS value is the crate's pinned constant — read it from the source
# so the marker can never disagree with the wrapper (known debt: it is not a
# plain sha256 of the current layouts.rs file; see ffi/layouts_sha.rs).
LAYOUTS=$(sed -n 's/.*"\(sha256:[0-9a-f]*\)".*/\1/p' \
  "$REPO_ROOT/crates/ipod/src/ffi/layouts_sha.rs" | head -n1)
[[ -n "$LAYOUTS" ]] || fail "could not read LAYOUTS_SHA256 from crates/ipod/src/ffi/layouts_sha.rs"
MARKER="$PREFIX/lib/rufin-libgpod.abi"
printf 'VERSION=%s\nLAYOUTS=%s\n' "$VERSION" "$LAYOUTS" > "$MARKER"

echo "build-libgpod: done"
echo "  library: $PREFIX/lib/libgpod.so.4"
echo "  marker:  $MARKER"
echo
echo "Next:"
echo "  export RUFIN_LIBGPOD_DIR=$PREFIX/lib"
echo "  cargo test -p ipod --features sync-writes --test integration"
