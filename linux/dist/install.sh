#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."

dist=linux/dist
state=/var/lib/elanfp
credential=/etc/credstore.encrypted/elanfp-master
key_file=/etc/elanfp/master.key
development_rule=/etc/udev/rules.d/70-redox-passthrough.rules
pcrs=${ELANFP_TPM2_PCRS:-}
manager=${ELANFP_SERVICE_MANAGER:-}
bridge=${ELANFP_FPRINTD_BRIDGE:-yes}
notices=${ELANFP_NOTIFY:-yes}

catalogs=$dist/locales
. "$dist/messages.sh"
language=$(language)

say() { text "$@"; }
item() { printf '  %s\n' "$(text "$@")"; }
fail() { text "$@" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
. "$dist/trust.sh"
running() { root_process "$1" >/dev/null; }
present() {
  case $(as_root sh -c 'if [ -e "$1" ]; then echo present; else echo absent; fi' sh "$1" 2>/dev/null || true) in
    present) return 0 ;;
    absent) return 1 ;;
    *) fail setup-elevation-failed ;;
  esac
}
trap 'status=$?; [ "$status" -eq 0 ] || text setup-stopped script=install.sh status="$status" >&2' EXIT

if [ "$(id -u)" -eq 0 ]; then
  [ -z "${SUDO_USER:-}${DOAS_USER:-}" ] || fail setup-not-with-sudo
  as_root() { "$@"; }
elif have sudo; then
  as_root() { sudo "$@"; }
elif have doas; then
  as_root() { doas "$@"; }
else
  fail setup-root-needed
fi

detect_manager() {
  init=$(cat /proc/1/comm 2>/dev/null || true)
  if [ -d /run/systemd/system ]; then
    echo systemd
  elif [ -e /run/openrc/softlevel ] || [ "$init" = openrc-init ]; then
    echo openrc
  elif [ "$init" = runit ] || running runsvdir; then
    echo runit
  elif [ "$init" = dinit ] || running dinit; then
    echo dinit
  elif [ "$init" = s6-svscan ] || running s6-svscan; then
    echo s6
  else
    echo none
  fi
}

account() {
  if getent passwd "$1" >/dev/null 2>&1 || grep -q "^$1:" /etc/passwd; then
    return
  fi
  shell=/sbin/nologin
  [ -x "$shell" ] || shell=/usr/sbin/nologin
  [ -x "$shell" ] || shell=/bin/false
  if have useradd; then
    getent group "$1" >/dev/null 2>&1 || as_root groupadd -r "$1"
    as_root useradd -r -g "$1" -d "$2" -s "$shell" -M "$1"
  elif have adduser; then
    grep -q "^$1:" /etc/group || as_root addgroup -S "$1"
    as_root adduser -S -D -H -h "$2" -s "$shell" -G "$1" "$1"
  else
    fail setup-no-account-tool account="$1"
  fi
}

start_service() {
  name=$1
  case $manager in
    systemd)
      as_root systemctl enable "$name.service" >/dev/null 2>&1
      as_root systemctl restart "$name.service"
      sleep 1
      as_root systemctl is-active --quiet "$name.service" || return 1
      ;;
    openrc)
      as_root install -Dm755 "$dist/openrc/$name" "/etc/init.d/$name"
      as_root rc-update add "$name" default >/dev/null
      as_root rc-service "$name" restart >/dev/null
      sleep 1
      as_root rc-service "$name" status >/dev/null 2>&1 || return 1
      ;;
    runit)
      if [ -z "$scan" ] || [ ! -d "$scan" ]; then
        return 2
      fi
      as_root ln -sfn "$services/$name" "$scan/$name"
      sleep 6
      as_root env SVDIR="$scan" sv restart "$name" >/dev/null 2>&1 || true
      sleep 1
      as_root env SVDIR="$scan" sv status "$name" 2>/dev/null | grep -q '^run:' || return 1
      ;;
    dinit)
      as_root dinitctl enable "$name" >/dev/null 2>&1 || true
      as_root dinitctl restart "$name" >/dev/null 2>&1 || as_root dinitctl start "$name" >/dev/null
      sleep 1
      as_root dinitctl status "$name" 2>/dev/null | grep -q 'STARTED' || return 1
      ;;
    *)
      return 2
      ;;
  esac
}

[ -n "$manager" ] || manager=$(detect_manager)
case $manager in
  systemd | openrc | runit | dinit | s6 | none) ;;
  *) fail setup-bad-manager ;;
esac
case $bridge in
  yes | no) ;;
  *) fail setup-bad-bridge ;;
esac
case $notices in
  yes | no) ;;
  *) fail setup-bad-notify ;;
esac
case $pcrs in
  *[!0-9+]* | +* | *+ | *++*) fail setup-bad-pcrs ;;
esac
have cargo || fail setup-no-cargo

seal=file
if [ "$manager" = systemd ] && have systemd-creds && systemd-analyze has-tpm2 >/dev/null 2>&1; then
  seal=tpm
fi
if [ "$manager" = systemd ] && have systemd-creds && present "$credential"; then
  seal=tpm
elif present "$key_file"; then
  seal=file
fi
[ -z "$pcrs" ] || [ "$seal" = tpm ] || fail setup-pcrs-need-tpm
if [ "$bridge" = yes ] && [ ! -S /run/dbus/system_bus_socket ] && [ ! -S /var/run/dbus/system_bus_socket ]; then
  bridge=absent
fi

cargo build --release --locked -p elanfp-linux -p pam-elanfp -p elanfp-fprintd -p elanfp-notify
built=$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release
for artifact in elanfpd elanfp elanfp-fprintd elanfp-notify libpam_elanfp.so; do
  [ -f "$built/$artifact" ] || fail setup-no-artifact path="$built/$artifact"
done

if [ "$seal" = tpm ]; then
  key=$credential
else
  key=$key_file
fi
if ! present "$key" && as_root find "$state" -name '*.papv' 2>/dev/null | grep -q .; then
  text setup-key-lost state="$state" key="$key" >&2
  fail setup-key-lost-advice
fi

if have systemd-sysusers; then
  as_root install -Dm644 "$dist/systemd/elanfp.sysusers" /etc/sysusers.d/elanfp.conf
  as_root systemd-sysusers /etc/sysusers.d/elanfp.conf
else
  account elanfp "$state"
  account elanfp-fprint /
fi

as_root install -Dm755 "$built/elanfpd" /usr/local/bin/elanfpd
as_root install -Dm755 "$built/elanfp" /usr/local/bin/elanfp
as_root install -Dm644 "$built/libpam_elanfp.so" /usr/local/lib/security/pam_elanfp.so
if [ "$notices" = yes ]; then
  as_root install -Dm755 "$built/elanfp-notify" /usr/local/bin/elanfp-notify
  as_root install -Dm644 "$dist/notify/elanfp-notify.desktop" /etc/xdg/autostart/elanfp-notify.desktop
else
  as_root rm -f /usr/local/bin/elanfp-notify /etc/xdg/autostart/elanfp-notify.desktop
fi
as_root install -Dm644 README.md /usr/local/share/doc/elanfp/README.md
as_root install -Dm644 SECURITY.md /usr/local/share/doc/elanfp/SECURITY.md

sealed_now=no
if ! present "$key"; then
  if [ "$seal" = tpm ]; then
    as_root install -d -m 0700 /etc/credstore.encrypted
    as_root sh "$dist/newkey.sh" tpm "$credential" "$pcrs" || fail setup-key-not-created path="$credential"
    sealed_now=yes
  else
    as_root install -d -m 0755 /etc/elanfp
    as_root sh "$dist/newkey.sh" file "$key_file" elanfp:elanfp || fail setup-key-not-created path="$key_file"
  fi
fi
if [ "$seal" = file ]; then
  as_root chown elanfp:elanfp "$key_file"
  as_root chmod 0400 "$key_file"
fi

device_access=udev
if [ -d /etc/udev/rules.d ] && have udevadm; then
  as_root install -Dm644 "$dist/udev/70-elanfp.rules" /etc/udev/rules.d/70-elanfp.rules
  if as_root udevadm control --reload 2>/dev/null; then
    as_root udevadm trigger --subsystem-match=usb --attr-match=idVendor=04f3 --attr-match=idProduct=0c4b || true
  else
    device_access=reboot
  fi
else
  device_access=manual
fi

services=/etc/sv
scan=
case $manager in
  systemd)
    if [ "$seal" = tpm ]; then
      as_root install -Dm644 "$dist/systemd/elanfpd.service" /etc/systemd/system/elanfpd.service
    else
      sed '/^LoadCredentialEncrypted=/d' "$dist/systemd/elanfpd.service" | as_root tee /etc/systemd/system/elanfpd.service >/dev/null
      as_root chmod 0644 /etc/systemd/system/elanfpd.service
    fi
    as_root install -Dm644 "$dist/systemd/elanfp-fprintd.service" /etc/systemd/system/elanfp-fprintd.service
    as_root systemctl daemon-reload
    ;;
  runit)
    [ -d "$services" ] || [ ! -d /etc/runit/sv ] || services=/etc/runit/sv
    scan=$(watched_by runsvdir || true)
    as_root install -Dm755 "$dist/runit/run" "$services/elanfpd/run"
    as_root install -Dm755 "$dist/runit-fprint/run" "$services/elanfp-fprintd/run"
    ;;
  dinit)
    as_root install -Dm644 "$dist/dinit/elanfpd" /etc/dinit.d/elanfpd
    as_root install -Dm644 "$dist/dinit/elanfpd-dirs" /etc/dinit.d/elanfpd-dirs
    as_root install -Dm644 "$dist/dinit/elanfp-fprintd" /etc/dinit.d/elanfp-fprintd
    ;;
  s6)
    as_root install -Dm755 "$dist/s6/run" /etc/elanfp/s6/elanfpd/run
    as_root install -Dm644 "$dist/s6/type" /etc/elanfp/s6/elanfpd/type
    as_root install -Dm755 "$dist/s6-fprint/run" /etc/elanfp/s6/elanfp-fprintd/run
    as_root install -Dm644 "$dist/s6-fprint/type" /etc/elanfp/s6/elanfp-fprintd/type
    ;;
esac

started=yes
start_service elanfpd || { [ $? -eq 2 ] && started=manual || started=no; }
[ "$started" != no ] || fail setup-service-failed manager="$manager"

name_guard=unknown
authority=yes
if [ "$bridge" = no ]; then
  sh "$dist/uninstall.sh" --bridge >/dev/null
fi
if [ "$bridge" = yes ]; then
  as_root install -Dm755 "$built/elanfp-fprintd" /usr/local/bin/elanfp-fprintd
  as_root install -Dm644 "$dist/fprint/elanfp-fprint.conf" /etc/dbus-1/system.d/elanfp-fprint.conf
  if [ -d /usr/share/polkit-1/actions ]; then
    as_root install -Dm644 "$dist/fprint/org.elanfp.policy" /usr/share/polkit-1/actions/org.elanfp.policy
  else
    authority=no
  fi
  if have dbus-send; then
    as_root dbus-send --system --type=method_call --dest=org.freedesktop.DBus / org.freedesktop.DBus.ReloadConfig 2>/dev/null || true
  fi
  if [ "$manager" = systemd ]; then
    as_root systemctl stop fprintd.service 2>/dev/null || true
  else
    as_root pkill -x fprintd 2>/dev/null || true
  fi
  if [ "$started" = yes ]; then
    start_service elanfp-fprintd || bridge=failed
  else
    bridge=manual
  fi
  if have dbus-send; then
    claim=$(as_root dbus-send --system --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.RequestName string:net.reactivated.Fprint uint32:4 2>&1 || true)
    case $claim in
      *AccessDenied*) name_guard=yes ;;
      *uint32*) name_guard=no ;;
    esac
  fi
fi

encrypted=unknown
if have findmnt && have lsblk; then
  source=$(findmnt -no SOURCE -T /etc 2>/dev/null | sed 's/\[.*//' || true)
  if [ -n "$source" ]; then
    if lsblk -sno TYPE "$source" 2>/dev/null | grep -q crypt; then encrypted=yes; else encrypted=no; fi
  fi
fi

echo
say setup-manager manager="$manager"
if [ "$seal" != tpm ]; then
  say setup-key-file path="$key_file"
elif [ "$sealed_now" = no ]; then
  say setup-key-tpm-kept path="$credential"
  [ -z "$pcrs" ] || say setup-key-pcrs-kept pcrs="$pcrs"
elif [ -n "$pcrs" ]; then
  say setup-key-tpm-pcrs path="$credential" pcrs="$pcrs"
else
  say setup-key-tpm path="$credential"
fi
case $encrypted-$seal in
  yes-*) say setup-disk-yes ;;
  no-tpm) say setup-disk-no-tpm ;;
  no-file) say setup-disk-no-file ;;
  *) say setup-disk-unknown ;;
esac
case $device_access in
  manual) say setup-access-manual ;;
  reboot) say setup-access-reboot ;;
  *) say setup-access-udev ;;
esac
case $started-$manager in
  manual-s6) say setup-service-s6 path=/etc/elanfp/s6 ;;
  manual-runit) say setup-service-runit path="$services/elanfpd" ;;
  manual-*) say setup-service-none program=/usr/local/bin/elanfpd runtime=/run/elanfp state="$state" ;;
  *) say setup-service-running ;;
esac
case $bridge in
  yes) say setup-bridge-yes ;;
  manual) say setup-bridge-manual program=/usr/local/bin/elanfp-fprintd ;;
  failed) say setup-bridge-failed ;;
  absent) say setup-bridge-absent ;;
  no) say setup-bridge-no ;;
esac
case $bridge-$authority in
  yes-no | manual-no | failed-no) say setup-bridge-no-polkit ;;
esac
[ "$name_guard" != no ] || say setup-bridge-name-open
case $notices in
  yes) say setup-notify-yes program=/usr/local/bin/elanfp-notify ;;
  no) say setup-notify-no ;;
esac
if [ -e "$development_rule" ]; then
  echo
  say setup-dev-rule path="$development_rule"
  say setup-dev-rule-advice
fi

elevate=sudo
have sudo || ! have doas || elevate=doas
logind=no
for library in /usr/lib/libsystemd.so.0 /usr/lib64/libsystemd.so.0 /lib/libsystemd.so.0 /lib64/libsystemd.so.0 /usr/lib/*/libsystemd.so.0 \
  /usr/lib/libelogind.so.0 /usr/lib64/libelogind.so.0 /lib/libelogind.so.0 /lib64/libelogind.so.0 /usr/lib/*/libelogind.so.0; do
  [ -e "$library" ] && logind=yes
done

echo
say setup-next
item setup-next-enroll elevate="$elevate"
item setup-next-try
item setup-next-pam elevate="$elevate" file="/etc/pam.d/$elevate"
printf '       %s\n' "auth sufficient /usr/local/lib/security/pam_elanfp.so"
if [ "$logind" = no ]; then
  echo
  say setup-no-logind
  say setup-no-logind-advice
fi
