#!/usr/bin/env bash
set -euo pipefail
SDK="${WEBOS_SDK:-$HOME/.local/share/webosbrew-native-toolchain/arm-webos-linux-gnueabi_sdk-buildroot}"
SDK="$(cd "$SDK" && pwd)"
ROOT="$(cd "$(dirname "$0")/.." && pwd)/sysroot"
test -f "$ROOT/usr/arm-linux-gnueabi/lib/libc.so" || { echo "Missing Buster sysroot; run make native-setup." >&2; exit 1; }
# Use Buster libc, not the SDK's older libc or compatibility polyfills.
exec "$SDK/bin/arm-webos-linux-gnueabi-gcc" -tno-lgtv-compat \
  --sysroot="$ROOT" -B"$ROOT/usr/arm-linux-gnueabi/lib/" \
  -isystem "$ROOT/usr/arm-linux-gnueabi/include" "$@"
