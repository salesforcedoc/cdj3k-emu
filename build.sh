#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# build.sh
#
# Build all guest artifacts (Linux 6.6 LTS kernel + out-of-tree modules + tools).
# When a development initramfs is present, also install the artifacts into it,
# apply patches, and repack it.  The app bundle does not require this optional
# step: its firmware wizard patches the user-supplied rootfs at install time.
# Run this whenever you change a module, patch script, or kernel config.
#
# Artifacts built (see docker/Dockerfile):
#   Image              - 6.6 LTS kernel (PL011, virtio built-in)
#   subucom_virt.ko    - virtual /dev/subucom_spi1.0
#   virtio_snd.ko      - custom virtio-sound PCM
#   dummy_drv.so       - Xorg dummy video driver (headless mode)
#   ep122_shim.so      - LD_PRELOAD shim for EP122 (+ the cdj3k-mods it links)
#   subucom_forwarder_aarch64 / subucom_live_aarch64 / cfgd_aarch64
#   stemd_client_aarch64
#
# Outputs:
#   build/Image
#   build/docker-out/modules/*.ko
#   guest/out/*
#   build/initramfs-patched.cpio.gz  (only when a source initramfs is present)
#
# Prerequisites:
#   docker with BuildKit support (Docker Desktop / OrbStack)

set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"

# Homebrew's Docker CLI can use Docker Desktop's daemon while omitting the
# app's helper binaries from PATH.  If ~/.docker/config.json names the
# "desktop" credential store, registry pulls then fail before the build starts
# even though Docker Desktop and its helper are installed.  Make the standard
# app-bundle helper location available locally without changing the user's
# shell or Docker configuration.
DOCKER_DESKTOP_BIN="/Applications/Docker.app/Contents/Resources/bin"
if ! command -v docker-credential-desktop >/dev/null 2>&1 \
    && [[ -x "$DOCKER_DESKTOP_BIN/docker-credential-desktop" ]]; then
    export PATH="$DOCKER_DESKTOP_BIN:$PATH"
fi

CLEAN_AFTER_BUILD=0
# SSH access (dropbear on port 2222) is dev-only.  Off by default for shipping
# builds: the SSH-related rootfs patches (02-dropbear-key, 03-dropbear-enable,
# 04-root-password) are no-ops unless this is set.  Enabling SSH leaves a
# **passwordless** root login bound to the QEMU host-forwarded port 2222 -
# fine on a dev box, hostile elsewhere.
ENABLE_SSH=0
for arg in "$@"; do
    case "$arg" in
        --clean)      CLEAN_AFTER_BUILD=1 ;;
        --enable-ssh) ENABLE_SSH=1 ;;
        -h|--help)
            echo "Usage: ./build.sh [--clean] [--enable-ssh]"
            echo "  --clean        remove unpacked rootfs workspace after build"
            echo "  --enable-ssh   enable dropbear + passwordless root (dev only)"
            exit 0
            ;;
        *)
            echo "ERROR: unknown argument: $arg" >&2
            echo "Usage: ./build.sh [--clean] [--enable-ssh]" >&2
            exit 1
            ;;
    esac
done
export ENABLE_SSH

DOCKER_OUT="$REPO_ROOT/build/docker-out"
# Persistent BuildKit cache on disk. OrbStack's default builder cache is too
# small for the kernel compile tree, so GC evicts layers between runs even
# with an unchanged Dockerfile. mode=max keeps intermediate layers, not just
# the tiny scratch export stage. Requires a docker-container buildx builder
# (see docker/ensure-buildx-builder.sh) - the default "docker" driver cannot
# --cache-to type=local.
DOCKER_CACHE="$REPO_ROOT/build/docker-cache"
DOCKER_BUILDER="$("$REPO_ROOT/docker/ensure-buildx-builder.sh")"
# Version stamp the mods report in MOD SETTINGS. .dockerignore keeps .git out
# of the build context, so the values are computed here and passed in; the
# same expressions live in guest/Makefile for a direct `make -C guest docker`.
MOD_BUILD="$(git -C "$REPO_ROOT" describe --tags --always --dirty 2>/dev/null || echo unknown)"
MOD_TAG="$(git -C "$REPO_ROOT" describe --tags --abbrev=0 2>/dev/null || true)"
if [[ -n "$MOD_TAG" ]]; then
    MOD_VERSION="$MOD_TAG"; [[ "$MOD_TAG" == "$MOD_BUILD" ]] || MOD_VERSION="$MOD_TAG+"
else
    MOD_VERSION="unknown"
fi
WORKDIR="$REPO_ROOT/build/work"
ROOTFS_DIR="$WORKDIR/rootfs"
ROOTFS_MODULES="$ROOTFS_DIR/lib/modules"
INITRAMFS_ORIG="$REPO_ROOT/build/initramfs-work/initramfs.cpio.gz"

echo "================================================================"
echo "  CDJ-3000 build (Linux 6.6 LTS)"
echo "================================================================"
echo ""

# [1/5] Build kernel + modules + tools via Docker
echo "[1/5] Building artifacts (docker/Dockerfile - target artifacts)..."
rm -rf "$DOCKER_OUT"
mkdir -p "$DOCKER_CACHE"
docker buildx build \
    --builder "$DOCKER_BUILDER" \
    --platform linux/arm64 \
    --provenance=false \
    --target artifacts \
    --cache-from "type=local,src=$DOCKER_CACHE" \
    --cache-to "type=local,dest=$DOCKER_CACHE,mode=max" \
    --build-arg "MOD_VERSION=$MOD_VERSION" \
    --build-arg "MOD_BUILD=$MOD_BUILD" \
    --output "type=local,dest=$DOCKER_OUT" \
    -f "$REPO_ROOT/docker/Dockerfile" \
    "$REPO_ROOT"
echo ""

# Copy kernel image to canonical path
cp "$DOCKER_OUT/Image" "$REPO_ROOT/build/Image"
echo "  ✓  Image → build/Image"

# Save tools for bundle.sh.  These are build products and do not depend on a
# Pioneer rootfs; the app's firmware wizard installs them into the rootfs at
# first launch.
mkdir -p "$REPO_ROOT/guest/out"
for bin in ep122_shim.so subucom_forwarder_aarch64 subucom_live_aarch64 cfgd_aarch64 \
           stemd_client_aarch64; do
    cp "$DOCKER_OUT/$bin" "$REPO_ROOT/guest/out/$bin"
done
echo "  ✓  guest tools → guest/out/"
echo ""

# A source initramfs is optional for normal app builds.  It is useful only for
# the legacy developer workflow that produces a pre-patched standalone image;
# distributed app bundles patch the user's firmware at install time instead.
if [[ ! -f "$INITRAMFS_ORIG" ]]; then
    echo "Source initramfs not found; skipping optional development rootfs repack."
    echo "The app's Install Firmware wizard will extract and patch the user-supplied firmware."
    echo ""
    echo "================================================================"
    echo "  Build complete (bundle assets)."
    echo ""
    echo "  Kernel:    build/Image"
    echo "  Modules:   build/docker-out/modules/"
    echo "  Tools:     guest/out/"
    echo "================================================================"
    exit 0
fi

# [2/5] Restore Pioneer rootfs
echo "[2/5] Restoring rootfs from: $INITRAMFS_ORIG"
rm -rf "$ROOTFS_DIR"
mkdir -p "$ROOTFS_DIR"
(
    cd "$ROOTFS_DIR"
    { gzip -dc "$INITRAMFS_ORIG" 2>/dev/null || true; } | cpio -i --make-directories 2>/dev/null
)
mkdir -p "$ROOTFS_MODULES"
echo "      rootfs restored at $ROOTFS_DIR"
echo ""

# [3/5] Stage modules where patch script 22 can find them
echo "[3/5] Staging out-of-tree modules..."
VANILLA_MODS_STAGE="$REPO_ROOT/initramfs-patch/vanilla-modules"
rm -rf "$VANILLA_MODS_STAGE"
mkdir -p "$VANILLA_MODS_STAGE"
for ko in subucom_virt.ko virtio_snd.ko udev_usb1.ko; do
    cp "$DOCKER_OUT/modules/$ko" "$VANILLA_MODS_STAGE/"
    echo "  ✓  staged $ko"
done
cp "$DOCKER_OUT/dummy_drv.so" "$REPO_ROOT/initramfs-patch/dummy_drv.so"
echo "  ✓  staged dummy_drv.so"

# Install shared tools into rootfs
mkdir -p "$ROOTFS_DIR/usr/bin"
for tool in \
    "subucom_live_aarch64:usr/bin" \
    "subucom_forwarder_aarch64:usr/bin" \
; do
    src="${tool%%:*}"
    dst="${tool##*:}"
    cp "$DOCKER_OUT/$src" "$ROOTFS_DIR/$dst"
    chmod 755 "$ROOTFS_DIR/$dst"
done

# The STEMS sidecar, at the path 30-stemd-client.sh's unit execs (the .app
# provisioner installs it from Contents/Resources/tools/ under the same name).
cp "$DOCKER_OUT/stemd_client_aarch64" "$ROOTFS_DIR/usr/bin/stemd_client"
chmod 755 "$ROOTFS_DIR/usr/bin/stemd_client"

mkdir -p "$ROOTFS_DIR/home/root"
cp "$DOCKER_OUT/ep122_shim.so" "$ROOTFS_DIR/home/root/ep122_shim.so"
chmod 755 "$ROOTFS_DIR/home/root/ep122_shim.so"

USB_IMG_SRC="$REPO_ROOT/build/usb.img"
if [[ -f "$USB_IMG_SRC" ]]; then
    mkdir -p "$ROOTFS_DIR/opt"
    cp "$USB_IMG_SRC" "$ROOTFS_DIR/opt/usb.img"
    echo "  ✓  usb.img → /opt/usb.img"
fi
echo ""

# [4/5] Apply rootfs patches
echo "[4/5] Applying rootfs patches..."
"$REPO_ROOT/initramfs-patch/patch-rootfs.sh" "$ROOTFS_DIR"
rm -rf "$VANILLA_MODS_STAGE"
rm -f "$REPO_ROOT/initramfs-patch/dummy_drv.so"
echo ""

# [5/5] Repack initramfs
echo "[5/5] Repacking initramfs via Docker..."
docker run --rm \
    -v "$WORKDIR":/work \
    -v "$REPO_ROOT/initramfs-patch/repack.sh":/work/repack.sh:ro \
    arm64v8/alpine:3.19 \
    sh /work/repack.sh
mv "$WORKDIR/initramfs-patched.cpio.gz" "$REPO_ROOT/build/initramfs-patched.cpio.gz"

echo ""
echo "================================================================"
echo "  Build complete."
echo ""
echo "  Kernel:    build/Image"
echo "  Initramfs: build/initramfs-patched.cpio.gz"
echo "================================================================"

if [[ "$CLEAN_AFTER_BUILD" -eq 1 ]]; then
    rm -rf "$WORKDIR/rootfs"
    echo "  Workspace cleaned."
fi
