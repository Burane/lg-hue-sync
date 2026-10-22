#!/usr/bin/env bash
set -euo pipefail
SDK="${WEBOS_SDK:-$HOME/.local/share/webosbrew-native-toolchain/arm-webos-linux-gnueabi_sdk-buildroot}"
DEST="${WEBOS_NATIVE_HOME:-$HOME/.local/share/webosbrew-rust}"
IMAGE="${CROSS_IMAGE:-lg-hue-sync-cross:rust-stable}"
SCRIPTS="$(cd "$(dirname "$0")" && pwd)"
test -x "$SDK/bin/arm-webos-linux-gnueabi-gcc" || { echo "Install and relocate the macOS webOS Brew native SDK first; see docs/operations.md." >&2; exit 1; }
rustup target add --toolchain stable armv7-unknown-linux-gnueabi
mkdir -p "$DEST"
if [ ! -d "$DEST/sysroot" ]; then
    # Bootstrap from the already cached, canonical builder; never pull or rebuild.
    IMAGE_ID="$(docker image inspect "$IMAGE" --format '{{.Id}}')"
    STAGING="$(mktemp -d "$DEST/.sysroot.XXXXXX")"
    CONTAINER=""
    trap 'if [ -n "$CONTAINER" ]; then docker rm "$CONTAINER" >/dev/null || true; fi; rm -rf "$STAGING"' EXIT
    CONTAINER="$(docker create --pull=never "$IMAGE")"
    mkdir -p "$STAGING/usr"
    docker cp "$CONTAINER:/usr/arm-linux-gnueabi" "$STAGING/usr/"
    test -f "$STAGING/usr/arm-linux-gnueabi/lib/libc.so"
    printf '%s\n' "$IMAGE_ID" > "$STAGING/builder-image-id"
    mv "$STAGING" "$DEST/sysroot"
fi
test -f "$DEST/sysroot/usr/arm-linux-gnueabi/lib/libc.so"
test -s "$DEST/sysroot/builder-image-id"
mkdir -p "$DEST/bin"
install -m 755 "$SCRIPTS/webos-native-cc.sh" "$DEST/bin/webos-native-cc"
install -m 755 "$SCRIPTS/webos-native-build.sh" "$DEST/bin/webos-native-build"
echo "Native tools ready at $DEST/bin; subsequent native builds do not use Docker."
