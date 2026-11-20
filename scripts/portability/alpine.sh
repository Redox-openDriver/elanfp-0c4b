#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."

catalogs=scripts/locales
. linux/dist/messages.sh
language=$(language)

have() { command -v "$1" >/dev/null 2>&1; }
have docker || { text dev-no-docker >&2; exit 1; }

device=
node=$(lsusb -d 04f3:0c4b 2>/dev/null | sed -nE 's#^Bus ([0-9]+) Device ([0-9]+).*#/dev/bus/usb/\1/\2#p' | head -n 1)
if [ "${1:-}" = --with-reader ] && [ -n "$node" ]; then
  device="--device=$node"
fi

image=elanfp-alpine-test
docker image inspect "$image" >/dev/null 2>&1 || docker build -q -t "$image" scripts/portability >/dev/null

exec docker run --rm $device \
  -v "$PWD:/src/elanfp-0c4b:ro" \
  -v elanfp-alpine-cargo:/root/.cargo \
  -v elanfp-alpine-target:/work/target \
  -e CARGO_TARGET_DIR=/work/target \
  -e READER_NODE="${device:+$node}" \
  "$image" sh /src/elanfp-0c4b/scripts/portability/inside-alpine.sh
