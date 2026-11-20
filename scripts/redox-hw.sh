#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

catalogs=scripts/locales
. linux/dist/messages.sh
language=$(language)

vendor=04f3
product=0c4b

redoxer=$(command -v redoxer || echo "$HOME/.cargo/bin/redoxer")
[[ -x $redoxer ]] || { text dev-no-redoxer >&2; exit 1; }

node=$(lsusb -d "$vendor:$product" | sed -nE 's#^Bus ([0-9]+) Device ([0-9]+).*#/dev/bus/usb/\1/\2#p' | head -n1)
[[ -n $node ]] || { text dev-no-reader device="$vendor:$product" >&2; exit 1; }
[[ -w $node ]] || { text dev-no-access node="$node" >&2; exit 1; }

root=$PWD
stage=$root/target/redox-hw
rm -rf "$stage"
mkdir -p "$stage/root" "$stage/bin" "$stage/drivers" "$stage/xhcid.d"

(cd redox && "$redoxer" build --release --locked)
built=redox/target/x86_64-unknown-redox/release
cp "$built/elanfp" "$stage/bin/"
cp "$built/elanfpd" "$stage/drivers/"
cp redox/dist/xhcid.d/elanfp.toml "$stage/xhcid.d/"

if [[ $# -gt 0 ]]; then
  session="$*"
else
  session="elanfp enroll right-index-finger --uid 1000 && elanfp verify --uid 1000 && elanfp list --uid 1000"
fi

cat > "$stage/root/session.sh" <<SCRIPT
export LANG=${LANG:-en_US.UTF-8}
n=0
until elanfp list >/dev/null 2>&1 || [ \$n -ge 50 ]; do
  sleep 0.2
  n=\$((n+1))
done
$session
SCRIPT

cd "$stage/root"
REDOXER_QEMU_ARGS="-device e1000,netdev=net0 -device isa-debugcon,chardev=log -device isa-debug-exit -device qemu-xhci,id=xhci -device usb-host,bus=xhci.0,vendorid=0x$vendor,productid=0x$product" \
  exec "$redoxer" exec --folder .:/root --folder "$stage/bin/:/usr/bin" --folder "$stage/drivers/:/usr/lib/drivers" --folder "$stage/xhcid.d/:/usr/lib/xhcid.d" bash /root/session.sh
