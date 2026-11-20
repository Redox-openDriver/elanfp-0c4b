#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

catalogs=scripts/locales
. linux/dist/messages.sh
language=$(language)

redoxer=$(command -v redoxer || echo "$HOME/.cargo/bin/redoxer")
[[ -x $redoxer ]] || { text dev-no-redoxer >&2; exit 1; }

root=$PWD
stage=$root/target/redox-run
rm -rf "$stage"
mkdir -p "$stage"

collect() {
  local prefix=$1
  shift
  "$redoxer" test --no-run --release --message-format=json "$@" \
    | python3 -I -c '
import json, sys
for line in sys.stdin:
    try:
        message = json.loads(line)
    except ValueError:
        continue
    if message.get("reason") == "compiler-artifact" and message.get("executable") and message.get("profile", {}).get("test"):
        print(message["target"]["name"], message["executable"])
' | while read -r name executable; do
      cp "$executable" "$stage/t_${prefix}_${name}"
    done
}

collect driver -p elanfp-core -p elanfp-ipc -p elanfp-engine
(cd redox && collect redox --features simulator --test scheme)

cat > "$stage/run.sh" <<'SCRIPT'
status=0
for t in /root/t_*; do
  echo "=== $t"
  "$t" --test-threads=1 || status=1
done
echo "=== /root/t_redox_scheme again in the same boot"
/root/t_redox_scheme --test-threads=1 || status=1
echo "=== redox-test exit $status"
exit $status
SCRIPT

ls -1 "$stage"
cd "$stage"
exec "$redoxer" exec --folder .:/root bash /root/run.sh
