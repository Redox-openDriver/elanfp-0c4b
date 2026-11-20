#!/bin/sh
set -eu
umask 077

kind=$1
path=$2
fresh=$path.new

trap 'rm -f "$fresh"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

rm -f "$fresh"
case $kind in
  tpm)
    head -c 32 /dev/urandom | systemd-creds encrypt --with-key=host+tpm2 --tpm2-pcrs="$3" --name=elanfp-master - "$fresh"
    [ -s "$fresh" ]
    ;;
  file)
    head -c 32 /dev/urandom > "$fresh"
    [ "$(wc -c < "$fresh")" -eq 32 ]
    chown "$3" "$fresh"
    chmod 0400 "$fresh"
    ;;
  *)
    exit 2
    ;;
esac
sync
ln "$fresh" "$path"
