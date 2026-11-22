#!/bin/sh
set -eu

here=$(dirname "$0")
catalogs=$here/locales
. "$here/messages.sh"
language=$(language)
mode=${1:-}

say() { text "$@"; }
fail() { text "$@" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

case $mode in
  '' | --purge | --bridge) ;;
  *) fail remove-usage ;;
esac

if [ "$(id -u)" -eq 0 ]; then
  as_root() { "$@"; }
elif have sudo; then
  as_root() { sudo "$@"; }
elif have doas; then
  as_root() { doas "$@"; }
else
  fail remove-root-needed
fi

. "$here/trust.sh"
watched=$(watched_by runsvdir || true)

retire() {
  name=$1
  if [ -e "/etc/systemd/system/$name.service" ]; then
    as_root systemctl disable --now "$name.service" 2>/dev/null || true
    as_root rm -f "/etc/systemd/system/$name.service"
  fi
  if [ -e "/etc/init.d/$name" ]; then
    as_root rc-service "$name" stop 2>/dev/null || true
    as_root rc-update del "$name" default 2>/dev/null || true
    as_root rm -f "/etc/init.d/$name"
  fi
  for services in /etc/sv /etc/runit/sv; do
    if [ -e "$services/$name" ]; then
      for scan in "$watched" /var/service /run/runit/service /etc/service /etc/runit/runsvdir/default; do
        trusted_dir "$scan" && [ -L "$scan/$name" ] || continue
        as_root env SVDIR="$scan" sv down "$name" 2>/dev/null || true
        as_root rm -f "$scan/$name"
      done
      as_root rm -rf "$services/$name"
    fi
  done
  if [ -e "/etc/dinit.d/$name" ]; then
    as_root dinitctl stop "$name" 2>/dev/null || true
    as_root dinitctl disable "$name" 2>/dev/null || true
    as_root rm -f "/etc/dinit.d/$name"
  fi
  as_root rm -rf "/etc/elanfp/s6/$name"
}

retire_bridge() {
  retire elanfp-fprintd
  as_root rm -f /usr/local/bin/elanfp-fprintd /etc/dbus-1/system.d/elanfp-fprint.conf /usr/share/polkit-1/actions/org.elanfp.policy
  if have dbus-send; then
    as_root dbus-send --system --type=method_call --dest=org.freedesktop.DBus / org.freedesktop.DBus.ReloadConfig 2>/dev/null || true
  fi
}

if [ "$mode" = --bridge ]; then
  retire_bridge
  if [ -d /run/systemd/system ]; then
    as_root systemctl daemon-reload
  fi
  say remove-bridge-done
  exit 0
fi

pam_users=$(as_root grep -rls pam_elanfp /etc/pam.d /etc/pam.conf /usr/lib/pam.d /usr/etc/pam.d /lib/pam.d 2>/dev/null || true)
if [ -n "$pam_users" ]; then
  say remove-pam-first >&2
  printf '%s\n' "$pam_users" >&2
  exit 1
fi

retire_bridge
retire elanfpd
as_root rm -f /etc/dinit.d/elanfpd-dirs /etc/sysusers.d/elanfp.conf
as_root rm -rf /etc/elanfp/s6
if [ -d /run/systemd/system ]; then
  as_root systemctl daemon-reload
fi
as_root rm -f /usr/local/bin/elanfpd /usr/local/bin/elanfp /usr/local/bin/elanfp-notify /usr/local/lib/security/pam_elanfp.so \
  /etc/udev/rules.d/70-elanfp.rules /etc/xdg/autostart/elanfp-notify.desktop
as_root rm -rf /usr/local/share/doc/elanfp

left=
if [ "$mode" = --purge ]; then
  as_root rm -rf /var/lib/elanfp /etc/credstore.encrypted/elanfp-master /etc/credstore.encrypted/elanfp-master.new /etc/elanfp
  for account in elanfp-fprint elanfp; do
    if getent passwd "$account" >/dev/null 2>&1 || grep -q "^$account:" /etc/passwd; then
      if have userdel; then
        as_root userdel "$account" || left="$left $account"
      elif have deluser; then
        as_root deluser "$account" || left="$left $account"
      else
        left="$left $account"
      fi
    fi
  done
fi

if have udevadm; then
  as_root udevadm control --reload 2>/dev/null || true
  as_root udevadm trigger --subsystem-match=usb --attr-match=idVendor=04f3 --attr-match=idProduct=0c4b 2>/dev/null || true
fi

if [ -n "$left" ]; then
  say remove-accounts-left accounts="${left# }" >&2
  exit 1
elif [ "$mode" = --purge ]; then
  say remove-purged
else
  say remove-kept state=/var/lib/elanfp
fi
